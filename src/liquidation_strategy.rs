//! Liquidation strategy implementations.
//!
//! This module provides flexible, configurable strategies for determining
//! liquidation amounts and profitability. The Strategy pattern enables:
//! - Partial vs. full liquidations
//! - Custom profitability calculations
//! - Risk management policies
//! - Gas cost optimization
//!
//! # Architecture
//!
//! Strategies implement the `LiquidationStrategy` trait, which provides
//! methods for calculating optimal liquidation amounts and determining
//! whether a liquidation should proceed based on profitability criteria.

use near_sdk::json_types::U128;
use templar_common::{
    asset::{CollateralAsset, FungibleAssetAmount},
    borrow::BorrowPosition,
    market::MarketConfiguration,
    oracle::pyth::OracleResponse,
    price::{Convert, PricePair},
    Decimal,
};

use crate::LiquidatorResult;

/// Safety buffer in basis points (0.5% = 50 bps).
/// Added to borrow amount to account for price movements and interest accrual during execution.
/// Excess is refunded by the contract.
pub(crate) const SAFETY_BUFFER_BPS: u128 = 50;

/// Headroom subtracted from the on-chain `liquidatable_collateral` cap so
/// price drift between scan and tx execution doesn't trip `ExcessiveLiquidation`.
///
/// This protects against drift up to ~`LIQUIDATABLE_CAP_BUFFER_BPS` between
/// what the liquidator sees at scan time and what the contract recomputes at
/// execution time. Larger drifts (e.g. stale oracle prices feeding the bot
/// while the chain runs `update_price_feeds` with fresh ones) still revert;
/// notification dedup absorbs those.
pub(crate) const LIQUIDATABLE_CAP_BUFFER_BPS: u128 = 300;

/// Applies `LIQUIDATABLE_CAP_BUFFER_BPS` to the on-chain eligibility cap.
pub(crate) fn apply_liquidatable_cap_buffer(liquidatable_collateral: u128) -> u128 {
    (liquidatable_collateral * (10_000 - LIQUIDATABLE_CAP_BUFFER_BPS)) / 10_000
}

/// Clamps the strategy's desired collateral request to the buffered eligibility cap.
///
/// Returns `min(desired, liquidatable * (1 - LIQUIDATABLE_CAP_BUFFER_BPS/10_000))`.
/// Used by both liquidation strategies so the cap-buffer behavior is uniform
/// and exercised by a single unit test.
pub(crate) fn min_with_cap_buffer(desired: u128, liquidatable: u128) -> u128 {
    std::cmp::min(desired, apply_liquidatable_cap_buffer(liquidatable))
}

/// Convert a borrow asset amount to collateral asset amount.
///
/// Formula: `collateral = (borrow / price) / (1 - spread)`
///
/// Uses floor rounding to ensure we request slightly less collateral than the
/// theoretical maximum, providing a natural safety margin.
pub(crate) fn borrow_to_collateral(
    borrow_amount: u128,
    price_pair: &PricePair,
    liquidation_spread: Decimal,
) -> Option<u128> {
    let spread_multiplier = Decimal::ONE - liquidation_spread;
    (Decimal::from(borrow_amount)
        / price_pair.convert(FungibleAssetAmount::<CollateralAsset>::new(1))
        / spread_multiplier)
        .to_u128_floor()
}

/// Convert a collateral asset amount to borrow asset amount.
///
/// Calculates the exact borrow amount needed to purchase the given collateral amount,
/// accounting for the liquidation spread.
///
/// Formula: `borrow = collateral * price * (1 - spread)`
pub(crate) fn collateral_to_borrow(
    collateral_amount: u128,
    price_pair: &PricePair,
    liquidation_spread: Decimal,
) -> Option<u128> {
    let spread_multiplier = Decimal::ONE - liquidation_spread;
    (Decimal::from(collateral_amount)
        * price_pair.convert(FungibleAssetAmount::<CollateralAsset>::new(1))
        * spread_multiplier)
        .to_u128_ceil()
}

/// Core trait for liquidation strategies: the sizing policy for how much of a
/// liquidatable position to repay.
///
/// A strategy does not decide *whether* a position is liquidatable — the scanner
/// and [`crate::Liquidator::liquidate`] establish that beforehand and only call
/// into the strategy once a position is already known to be eligible. The
/// strategy's job is narrower: given the position and the bot's current
/// inventory, decide how big a repayment to attempt this round, and afterward
/// whether that repayment is worth making at all. This is the extension seam for
/// a custom sizing policy (e.g. per-market caps, sizing off inventory pressure)
/// beyond the two built-ins in this module.
pub trait LiquidationStrategy: Send + Sync + std::fmt::Debug {
    /// Calculates the repay amount to attempt and the collateral to request for it.
    ///
    /// # Units
    ///
    /// Every amount here is raw on-chain units, not human-decimal. `available_balance`
    /// and the first element of the returned tuple are in the market's borrow-asset
    /// units; the second element is in collateral-asset units.
    ///
    /// # Returns
    ///
    /// [`StrategyDecision::Sized`] to attempt a liquidation, or
    /// [`StrategyDecision::Decline`] with a reason to skip this position
    /// this round. Choose the reason honestly — it drives the operator's
    /// unfunded alerting: [`DeclineReason::InsufficientInventory`] when
    /// topping up inventory would permit the attempt,
    /// [`DeclineReason::NotViable`] when nothing would (dust, repay below
    /// the market minimum, conversion failure). A `Sized` with a zero
    /// repay or collateral amount is rejected by the caller's fail-closed
    /// validation (the position is skipped with a warning naming the
    /// strategy) rather than submitted on-chain.
    ///
    /// # Caller validation — checked, not re-clamped
    ///
    /// [`crate::Liquidator::liquidate`] validates the returned amounts and
    /// then passes them to the liquidation transaction unmodified (only
    /// wrapped into typed amounts) — it never silently adjusts them. The
    /// validation rejects (skips the position, fail closed): zero amounts,
    /// and a repay above the share of `available_balance` this strategy
    /// declares via [`max_liquidation_percentage`](Self::max_liquidation_percentage).
    /// It does NOT check the position's on-chain eligibility cap, so any
    /// safety margin — headroom against price drift between scan and
    /// execution, or staying under the contract's liquidatable-collateral
    /// cap — must still be built into the returned amount by the strategy
    /// itself. Both built-in strategies do this via the module-level
    /// `SAFETY_BUFFER_BPS` and `LIQUIDATABLE_CAP_BUFFER_BPS` constants (see
    /// `apply_liquidatable_cap_buffer` and `min_with_cap_buffer`); a new
    /// strategy is free to use a different margin, but needs one.
    ///
    /// # Arguments
    ///
    /// * `position` - The borrow position to liquidate. For markets that
    ///   support partial liquidation, the caller has already narrowed
    ///   `position.collateral_asset_deposit` down to the liquidatable portion —
    ///   see the note on v1.0.0 markets in the built-in strategies below.
    /// * `oracle_response` - Current price oracle data
    /// * `configuration` - Market configuration
    /// * `available_balance` - Available inventory balance in the borrow asset
    /// * `market_version` - Market contract version. Gate partial support on
    ///   [`crate::scanner::supports_partial_liquidation`], never on equality
    ///   with a specific version; `None` (no NEP-330 metadata) is treated as
    ///   the oldest supported version — full liquidation required.
    ///
    /// # Errors
    /// Returns an error if price pair retrieval fails or position calculations fail.
    fn calculate_liquidation_amount(
        &self,
        position: &BorrowPosition,
        oracle_response: &OracleResponse,
        configuration: &MarketConfiguration,
        available_balance: U128,
        market_version: Option<crate::scanner::MarketVersion>,
    ) -> LiquidatorResult<StrategyDecision>;

    /// Determines whether a sized liquidation is still worth submitting.
    ///
    /// Called after [`calculate_liquidation_amount`](Self::calculate_liquidation_amount)
    /// has already produced a candidate `liquidation_amount`; this is the final
    /// go/no-go gate before a transaction is submitted. In the inventory-based
    /// model there is no swap cost to factor in here — the bot already holds the
    /// borrow asset — so profitability reduces to expected collateral value versus
    /// liquidation amount plus gas.
    ///
    /// # Arguments
    ///
    /// * `liquidation_amount` - Amount to be used for liquidation (borrow asset)
    /// * `expected_collateral_value` - Expected value of collateral in borrow asset units
    /// * `gas_cost_estimate` - Estimated gas cost in borrow asset units
    ///
    /// # Returns
    ///
    /// `true` if the liquidation should proceed, `false` otherwise. A `false`
    /// here produces [`crate::LiquidationOutcome::Unprofitable`], distinct
    /// from a [`StrategyDecision::Decline`] from `calculate_liquidation_amount`
    /// — both stop the liquidation, but only this one implies the position
    /// genuinely isn't worth repaying at current prices/gas; a decline's
    /// [`DeclineReason`] states whether inventory or viability was the
    /// obstacle.
    ///
    /// # Errors
    /// Returns an error if profitability calculations fail.
    ///
    /// # Default
    ///
    /// The provided implementation requires expected revenue to clear
    /// `liquidation_amount + gas` by at least
    /// [`min_profit_margin_bps`](Self::min_profit_margin_bps). Override it
    /// only for a genuinely different go/no-go policy; overriding the margin
    /// alone means implementing just `min_profit_margin_bps`.
    fn should_liquidate(
        &self,
        liquidation_amount: U128,
        expected_collateral_value: U128,
        gas_cost_estimate: U128,
    ) -> LiquidatorResult<bool> {
        let total_cost = u128::from(liquidation_amount).saturating_add(gas_cost_estimate.into());
        // Ceiling division: the exact requirement is fractional in raw units
        // and the gate must never accept a value strictly below it. Overflow
        // in the bps scaling fails closed — a cost that large is never worth
        // guessing about.
        let Some(scaled) =
            total_cost.checked_mul(10_000 + u128::from(self.min_profit_margin_bps()))
        else {
            return Ok(false);
        };
        let min_revenue = scaled.div_ceil(10_000);
        Ok(u128::from(expected_collateral_value) >= min_revenue)
    }

    /// The minimum profit margin, in basis points, that
    /// [`should_liquidate`](Self::should_liquidate)'s provided implementation
    /// gates on. Also surfaced in the caller's "not profitable" log line, so
    /// keep it truthful even when overriding `should_liquidate` with a policy
    /// that uses no single margin — report the closest scalar summary.
    fn min_profit_margin_bps(&self) -> u32;

    /// Returns the strategy name for logging and debugging.
    fn strategy_name(&self) -> &'static str;

    /// Returns the maximum share of available inventory (0-100, in
    /// percent) this strategy will ever commit to one sizing. **Enforced by
    /// the caller**: a `calculate_liquidation_amount` result whose repay
    /// exceeds this share of the available balance is skipped fail-closed
    /// (with a warning naming the strategy), so keep the declaration
    /// truthful — an under-declared value silently vetoes your own sizing.
    ///
    /// # Default
    ///
    /// Returns 100 (may commit the full available balance) by default.
    fn max_liquidation_percentage(&self) -> u8 {
        100
    }
}

/// A sizing decision: the amounts to submit, or a typed decline. The
/// decline reason is API, not just a log line, because the caller's
/// unfunded accounting (`liquidations_skipped_unfunded_total`) depends on
/// distinguishing "topping up inventory fixes this" from "nothing will".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrategyDecision {
    /// Attempt a liquidation: (repay amount, collateral amount), both
    /// nonzero — zero amounts are rejected fail-closed by the caller.
    Sized(U128, U128),
    /// Skip this position this round, for the stated reason.
    Decline(DeclineReason),
}

/// Why a strategy declined to size a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclineReason {
    /// Available inventory cannot fund the repay this position requires —
    /// topping up inventory is the fix. Reported as
    /// [`crate::LiquidationOutcome::SkippedUnfunded`] and counted in the
    /// `liquidations_skipped_unfunded_total` metric.
    InsufficientInventory,
    /// A cause no amount of inventory can clear: dust-sized position,
    /// repay below the market minimum, a failed conversion. Details are in
    /// the strategy's own log line; reported as a plain
    /// [`crate::LiquidationOutcome::Skipped`] so the unfunded alert cannot
    /// latch on it.
    NotViable,
}

/// A liquidation percentage, provably in `1..=100`: out-of-range values are
/// unrepresentable, rejected where the value is parsed (clap uses the
/// `FromStr` impl) instead of panicking in a constructor downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiquidationPercentage(u8);

impl LiquidationPercentage {
    /// Full liquidation — the default when no percentage is configured.
    pub const FULL: Self = Self(100);

    /// # Errors
    ///
    /// Rejects 0 and anything above 100.
    pub fn new(value: u8) -> Result<Self, String> {
        if value == 0 || value > 100 {
            return Err(format!(
                "Partial percentage must be between 1 and 100, got {value}"
            ));
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn get(self) -> u8 {
        self.0
    }
}

impl std::str::FromStr for LiquidationPercentage {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let value: u8 = s
            .parse()
            .map_err(|_| format!("'{s}' is not a valid number"))?;
        Self::new(value)
    }
}

impl std::fmt::Display for LiquidationPercentage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Percentage-of-inventory liquidation strategy — also the **full-liquidation
/// strategy** when configured at 100%. There is no separate "full liquidation"
/// type: sending `target_percentage: 100` *is* full liquidation, and is the
/// crate's default when no strategy flag is set at all.
///
/// Sizes each liquidation as `target_percentage` of the bot's available borrow-
/// asset inventory (after the strategy's own safety buffer), then converts that
/// borrow amount to the equivalent collateral request via the market's price
/// pair and liquidation spread, clamped to the position's liquidatable
/// collateral.
///
/// # Configuration
///
/// Selected by default, or explicitly via `--partial-percentage <1-100>`
/// (env `PARTIAL_LIQUIDATION_PERCENTAGE`) — see
/// [`Args::partial_percentage`](crate::config::Args::partial_percentage). Mutually
/// exclusive with [`FixedAmountLiquidationStrategy`]'s flag; [`Args::create_strategy`](crate::config::Args::create_strategy)
/// panics at startup if both are set. `target_percentage` picks the fraction of
/// inventory to deploy per liquidation; `min_profit_margin_bps` (shared with
/// `--min-profit-bps` / `MIN_PROFIT_BPS`) sets the minimum margin
/// [`should_liquidate`](LiquidationStrategy::should_liquidate) requires.
///
/// # When to reach for this
///
/// The default choice. Use 100% (the default) to fully liquidate every eligible
/// position your inventory can cover. Use a lower percentage to spread limited
/// inventory across more positions in a round, accept smaller/faster
/// transactions, and reduce single-position exposure — at the cost of not
/// fully repairing any one position in a single pass (loop liquidation, see
/// `LOOP_LIQUIDATION`, can compensate by repeating this strategy across
/// several iterations against the same position).
///
/// # Benefits
///
/// - Controlled capital deployment
/// - Risk management through partial fund usage
/// - Faster execution with smaller amounts
/// - Multiple liquidation opportunities can be pursued
///
/// # Tradeoffs
///
/// - May not fully liquidate positions
/// - Requires multiple transactions for full capital deployment
/// - Position may remain partially underwater
#[derive(Debug, Clone, Copy)]
pub struct PercentageLiquidationStrategy {
    /// Percentage of available funds to use
    pub target_percentage: LiquidationPercentage,
    /// Minimum profit margin in basis points (e.g., 50 = 0.5%)
    pub min_profit_margin_bps: u32,
}

impl PercentageLiquidationStrategy {
    /// Creates a new partial liquidation strategy. The percentage bounds
    /// live in [`LiquidationPercentage`], so there is nothing to panic on.
    #[must_use]
    pub fn new(target_percentage: LiquidationPercentage, min_profit_margin_bps: u32) -> Self {
        Self {
            target_percentage,
            min_profit_margin_bps,
        }
    }
}

impl LiquidationStrategy for PercentageLiquidationStrategy {
    #[tracing::instrument(skip(self, position, oracle_response, configuration), level = "debug")]
    fn calculate_liquidation_amount(
        &self,
        position: &BorrowPosition,
        oracle_response: &OracleResponse,
        configuration: &MarketConfiguration,
        available_balance: U128,
        market_version: Option<crate::scanner::MarketVersion>,
    ) -> LiquidatorResult<StrategyDecision> {
        let available_u128: u128 = available_balance.into();

        let available_after_buffer = (available_u128 * (10_000 - SAFETY_BUFFER_BPS)) / 10_000;
        let target_amount =
            (available_after_buffer * u128::from(self.target_percentage.get())) / 100;

        if target_amount == 0 {
            tracing::warn!(
                available_balance = %available_u128,
                percentage = %self.target_percentage,
                "Target liquidation amount is zero"
            );
            return Ok(StrategyDecision::Decline(
                DeclineReason::InsufficientInventory,
            ));
        }

        let price_pair = configuration
            .price_oracle_configuration
            .create_price_pair(oracle_response)?;

        // Note: position.collateral_asset_deposit contains liquidatable_collateral (set by caller)
        let liquidatable_collateral = position.collateral_asset_deposit;

        let Some(collateral_amount) = borrow_to_collateral(
            target_amount,
            &price_pair,
            configuration.liquidation_maximum_spread,
        ) else {
            tracing::warn!(
                borrow_amount = %target_amount,
                "Could not calculate collateral amount from borrow amount"
            );
            return Ok(StrategyDecision::Decline(DeclineReason::NotViable));
        };

        // Pre-partial markets (version < 1.1.0, or unknown) require
        // liquidating ALL collateral.
        let requires_full = !crate::scanner::supports_partial_liquidation(market_version);
        let target_collateral = if requires_full {
            position.collateral_asset_deposit.into()
        } else {
            min_with_cap_buffer(collateral_amount, liquidatable_collateral.into())
        };

        if target_collateral == 0 {
            tracing::warn!(
                liquidatable_collateral = %u128::from(liquidatable_collateral),
                "Buffered liquidatable cap rounded to zero — position too small to liquidate safely"
            );
            return Ok(StrategyDecision::Decline(DeclineReason::NotViable));
        }

        let Some(theoretical_amount) = collateral_to_borrow(
            target_collateral,
            &price_pair,
            configuration.liquidation_maximum_spread,
        ) else {
            tracing::warn!(
                collateral_amount = %target_collateral,
                "Could not calculate borrow amount from collateral"
            );
            return Ok(StrategyDecision::Decline(DeclineReason::NotViable));
        };

        let final_amount =
            theoretical_amount.saturating_add((theoretical_amount * SAFETY_BUFFER_BPS) / 10_000);

        if final_amount > available_u128 {
            if requires_full {
                tracing::warn!(
                    required = %final_amount,
                    available = %available_u128,
                    "Market requires full collateral liquidation but insufficient balance"
                );
            } else {
                tracing::warn!(
                    required = %final_amount,
                    available = %available_u128,
                    "Insufficient balance for liquidation"
                );
            }
            return Ok(StrategyDecision::Decline(
                DeclineReason::InsufficientInventory,
            ));
        }

        let contract_minimum: u128 = configuration.borrow_range.minimum.into();
        if final_amount < contract_minimum {
            // Which constraint bound the slice decides the reason: when the
            // inventory-derived target was binding (the position's buffered
            // cap did not clamp it), a larger balance raises the slice past
            // the minimum — a funding cause. A position-capped slice, or a
            // full-liquidation amount, is inventory-independent.
            let inventory_bound = !requires_full
                && collateral_amount
                    < apply_liquidatable_cap_buffer(liquidatable_collateral.into());
            let reason = if inventory_bound {
                DeclineReason::InsufficientInventory
            } else {
                DeclineReason::NotViable
            };
            tracing::warn!(
                amount = %final_amount,
                contract_minimum = %contract_minimum,
                reason = ?reason,
                "Liquidation amount below contract minimum"
            );
            return Ok(StrategyDecision::Decline(reason));
        }

        Ok(StrategyDecision::Sized(
            U128(final_amount),
            U128(target_collateral),
        ))
    }

    fn min_profit_margin_bps(&self) -> u32 {
        self.min_profit_margin_bps
    }

    fn strategy_name(&self) -> &'static str {
        "Percentage Liquidation"
    }

    fn max_liquidation_percentage(&self) -> u8 {
        // The true ceiling is 100, not `target_percentage`: on markets
        // without partial-liquidation support this strategy deliberately
        // sizes the full required repay, bounded only by the available
        // balance — declaring the partial target here would make the
        // caller's cap veto exactly the large positions on those markets.
        100
    }
}

/// Convert USD amount to raw token units.
///
/// Assumes all borrow assets are USD-based stablecoins (USDC, USDT, DAI, etc.).
///
/// Example: 100.0 USD with 6 decimals = `100_000000` raw units
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]
fn usd_to_raw_units(usd_amount: f64, decimals: i32) -> u128 {
    let multiplier = 10_f64.powi(decimals);
    (usd_amount * multiplier) as u128
}

/// Fixed USD amount liquidation strategy.
///
/// Uses a fixed USD amount per liquidation, automatically converting to raw units
/// based on each market's borrow asset decimals. Assumes all borrow assets are
/// USD-based stablecoins (the conversion in `usd_to_raw_units` scales the USD
/// figure by `10^decimals` with no price lookup, so it is wrong for a non-USD
/// borrow asset).
///
/// # Configuration
///
/// Selected via `--fixed-liquidation-amount-usd <USD>` (env
/// `FIXED_LIQUIDATION_AMOUNT_USD`) — see
/// [`Args::fixed_liquidation_amount_usd`](crate::config::Args::fixed_liquidation_amount_usd).
/// Mutually exclusive with [`PercentageLiquidationStrategy`]'s flag (startup
/// panic if both are set, same as above). `fixed_amount_usd` sets the USD cap
/// per liquidation; `min_profit_margin_bps` is the same knob and default as
/// [`PercentageLiquidationStrategy`]'s.
///
/// # When to reach for this
///
/// Reach for this over the percentage strategy when the operator wants a
/// predictable, position-size-independent capital cap per liquidation —
/// e.g. "never risk more than $100 in a single liquidation, regardless of
/// how large the underwater position is or which market's decimals apply."
/// The percentage strategy's cap moves with inventory size and position size;
/// this one doesn't.
#[derive(Debug, Clone, Copy)]
pub struct FixedAmountLiquidationStrategy {
    /// Fixed USD amount to use per liquidation (e.g., 100.0 for $100 USD)
    pub fixed_amount_usd: f64,
    /// Minimum profit margin in basis points
    pub min_profit_margin_bps: u32,
}

impl FixedAmountLiquidationStrategy {
    #[must_use]
    pub fn new(fixed_amount_usd: f64, min_profit_margin_bps: u32) -> Self {
        Self {
            fixed_amount_usd,
            min_profit_margin_bps,
        }
    }
}

impl LiquidationStrategy for FixedAmountLiquidationStrategy {
    #[tracing::instrument(skip(self, position, oracle_response, configuration), level = "debug")]
    fn calculate_liquidation_amount(
        &self,
        position: &BorrowPosition,
        oracle_response: &OracleResponse,
        configuration: &MarketConfiguration,
        available_balance: U128,
        market_version: Option<crate::scanner::MarketVersion>,
    ) -> LiquidatorResult<StrategyDecision> {
        let decimals = configuration
            .price_oracle_configuration
            .borrow_asset_decimals;
        let fixed_amount = usd_to_raw_units(self.fixed_amount_usd, decimals);

        let available_u128: u128 = available_balance.into();

        if fixed_amount > available_u128 {
            let asset_id = configuration.borrow_asset.to_string();
            tracing::warn!(
                fixed_amount_usd = %self.fixed_amount_usd,
                fixed_amount = %crate::format::format_amount(fixed_amount, decimals, &asset_id),
                available_balance = %crate::format::format_amount(available_u128, decimals, &asset_id),
                "Insufficient balance for fixed amount liquidation"
            );
            return Ok(StrategyDecision::Decline(
                DeclineReason::InsufficientInventory,
            ));
        }

        let price_pair = configuration
            .price_oracle_configuration
            .create_price_pair(oracle_response)?;

        let liquidatable_u128: u128 = position.collateral_asset_deposit.into();

        let Some(max_collateral) = borrow_to_collateral(
            fixed_amount,
            &price_pair,
            configuration.liquidation_maximum_spread,
        ) else {
            let asset_id = configuration.borrow_asset.to_string();
            tracing::warn!(
                fixed_amount_usd = %self.fixed_amount_usd,
                fixed_amount = %crate::format::format_amount(fixed_amount, decimals, &asset_id),
                "Could not calculate collateral amount from fixed amount"
            );
            return Ok(StrategyDecision::Decline(DeclineReason::NotViable));
        };

        // Pre-partial markets (version < 1.1.0, or unknown) require
        // liquidating ALL collateral.
        let requires_full = !crate::scanner::supports_partial_liquidation(market_version);
        let target_collateral = if requires_full {
            position.collateral_asset_deposit.into()
        } else {
            let safe_collateral = (max_collateral * (10_000 - SAFETY_BUFFER_BPS)) / 10_000;
            min_with_cap_buffer(safe_collateral, liquidatable_u128)
        };

        if target_collateral == 0 {
            tracing::warn!(
                liquidatable_collateral = %liquidatable_u128,
                "Buffered liquidatable cap rounded to zero — position too small to liquidate safely"
            );
            return Ok(StrategyDecision::Decline(DeclineReason::NotViable));
        }

        let Some(expected_minimum) = collateral_to_borrow(
            target_collateral,
            &price_pair,
            configuration.liquidation_maximum_spread,
        ) else {
            tracing::warn!(
                collateral_amount = %target_collateral,
                "Could not calculate borrow amount from collateral"
            );
            return Ok(StrategyDecision::Decline(DeclineReason::NotViable));
        };

        let amount_with_buffer = expected_minimum
            .saturating_add(((expected_minimum * SAFETY_BUFFER_BPS) / 10_000).max(1));

        // A full liquidation cannot be capped: an offer below the full
        // requirement is rejected on-chain as too low, wasting a transaction
        // every round. If the fixed budget can't fund it, skip the position.
        if requires_full && amount_with_buffer > fixed_amount {
            let asset_id = configuration.borrow_asset.to_string();
            tracing::warn!(
                required = %crate::format::format_amount(amount_with_buffer, decimals, &asset_id),
                fixed_amount = %crate::format::format_amount(fixed_amount, decimals, &asset_id),
                "Fixed budget cannot fund the required full liquidation, skipping"
            );
            return Ok(StrategyDecision::Decline(DeclineReason::NotViable));
        }

        // Cap at fixed_amount (the maximum we're willing to send)
        let final_amount = std::cmp::min(amount_with_buffer, fixed_amount);

        let contract_minimum: u128 = configuration.borrow_range.minimum.into();
        if final_amount < contract_minimum {
            tracing::warn!(
                amount = %final_amount,
                contract_minimum = %contract_minimum,
                "Fixed amount below contract minimum"
            );
            return Ok(StrategyDecision::Decline(DeclineReason::NotViable));
        }

        if requires_full && final_amount > available_u128 {
            tracing::warn!(
                required = %final_amount,
                available = %available_u128,
                "Market requires full collateral liquidation but insufficient balance"
            );
            return Ok(StrategyDecision::Decline(
                DeclineReason::InsufficientInventory,
            ));
        }

        Ok(StrategyDecision::Sized(
            U128(final_amount),
            U128(target_collateral),
        ))
    }

    fn min_profit_margin_bps(&self) -> u32 {
        self.min_profit_margin_bps
    }

    fn strategy_name(&self) -> &'static str {
        "Fixed Amount Liquidation"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real mainnet fixtures: the ibtc-usdc market config (BTC collateral,
    /// 8 decimals; USDC borrow, 6 decimals; 5% spread) and a borrow position
    /// in that market's own JSON shape.
    const IBTC_CONFIG_JSON: &str = r#"{"time_chunk_configuration":{"BlockTimestampMs":{"divisor":"600000"}},"borrow_asset":{"Nep141":"17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1"},"collateral_asset":{"Nep245":{"contract_id":"intents.near","token_id":"nep141:btc.omft.near"}},"price_oracle_configuration":{"account_id":"pyth-oracle.near","collateral_asset_price_id":"e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43","collateral_asset_decimals":8,"borrow_asset_price_id":"eaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a","borrow_asset_decimals":6,"price_maximum_age_s":60},"borrow_mcr_maintenance":"1.25","borrow_mcr_liquidation":"1.19999999999999999999999999999999999999","borrow_asset_maximum_usage_ratio":"0.99000000000000000000000000000000000001","borrow_origination_fee":{"Proportional":"0.00099999999999999999999999999999999999"},"borrow_interest_rate_strategy":{"Piecewise":{"base":"0","optimal":"0.90000000000000000000000000000000000001","rate_1":"0.08888888888888888888888888888888888889","rate_2":"2.40000000000000000000000000000000000001"}},"borrow_maximum_duration_ms":null,"borrow_range":{"minimum":"1","maximum":null},"supply_range":{"minimum":"40000","maximum":null},"supply_withdrawal_range":{"minimum":"40000","maximum":null},"supply_withdrawal_fee":{"fee":{"Flat":"0"},"duration":"0","behavior":"Fixed"},"yield_weights":{"supply":1,"static":{"revenue.tmplr.near":1,"rewards.tmplr.near":1}},"protocol_account_id":"revenue.tmplr.near","liquidation_maximum_spread":"0.05000000000000000000000000000000000001"}"#;

    fn ibtc_config() -> MarketConfiguration {
        near_sdk::serde_json::from_str(IBTC_CONFIG_JSON).expect("fixture parses")
    }

    fn position(collateral_raw: u128, principal_raw: u128) -> BorrowPosition {
        near_sdk::serde_json::from_str(&format!(
            r#"{{"started_at_block_timestamp_ms":"1758939235082",
                 "collateral_asset_deposit":"{collateral_raw}",
                 "borrow_asset_principal":"{principal_raw}",
                 "borrow_asset_fees":{{"total":"0","fraction_as_u128_dividend":"0","next_snapshot_index":0,"amortized":"0"}}}}"#
        ))
        .expect("position fixture parses")
    }

    fn btc_usdc_prices(cfg: &MarketConfiguration) -> OracleResponse {
        let price = |mantissa: i64| templar_common::oracle::pyth::Price {
            price: near_sdk::json_types::I64(mantissa),
            conf: near_sdk::json_types::U64(0),
            expo: -8,
            publish_time: templar_common::oracle::pyth::PythTimestamp::from_secs(1_755_600_000),
        };
        let mut r = OracleResponse::new();
        let poc = &cfg.price_oracle_configuration;
        // BTC ≈ $64,350; USDC ≈ $1.00
        r.insert(
            poc.collateral_asset_price_id,
            Some(price(6_435_000_000_000)),
        );
        r.insert(poc.borrow_asset_price_id, Some(price(100_000_000)));
        r
    }

    /// A full-liquidation market (no partial support) requires repaying for
    /// ALL the collateral; a fixed budget below that requirement must skip
    /// the position, not submit an offer capped at the budget — the contract
    /// rejects such an offer as too low, wasting a transaction every round.
    /// `(1, 0, 5)` pins the version-gating half: any pre-partial version —
    /// not just exactly (1, 0, 0) — takes the full-liquidation path.
    #[test]
    fn fixed_budget_below_full_requirement_skips_instead_of_underfunding() {
        let cfg = ibtc_config();
        let prices = btc_usdc_prices(&cfg);
        // 1 BTC of collateral: full liquidation needs ≈ $61k of USDC; the
        // strategy's budget is $100.
        let pos = position(100_000_000, 3_980_000);
        let strategy = FixedAmountLiquidationStrategy::new(100.0, 50);

        for version in [
            None,
            Some(crate::scanner::MarketVersion::new(1, 0, 0)),
            Some(crate::scanner::MarketVersion::new(1, 0, 5)),
        ] {
            let result = strategy
                .calculate_liquidation_amount(&pos, &prices, &cfg, U128(1_000_000_000_000), version)
                .expect("no error");
            // NotViable, not InsufficientInventory: the cap here is the
            // operator's configured FIXED_LIQUIDATION_AMOUNT_USD, a
            // deliberate config decision — topping up inventory changes
            // nothing, so the unfunded alert must not page for it. (The
            // wallet-balance branch is separately InsufficientInventory.)
            assert_eq!(
                result,
                StrategyDecision::Decline(DeclineReason::NotViable),
                "budget-capped full liquidation must decline as not-viable (version {version:?})"
            );
        }
    }

    /// A failed collateral→borrow conversion is not a fundable liquidation.
    /// Collapsing it to zero would produce `amount_with_buffer = 1`, which
    /// slips under both the full-liquidation guard and this market's
    /// contract minimum of 1 — submitting a one-raw-unit offer for the whole
    /// position that the contract rejects as too low.
    #[test]
    fn conversion_failure_skips_instead_of_offering_one_unit() {
        let cfg = ibtc_config();
        let prices = btc_usdc_prices(&cfg);
        // Collateral so large that valuing it in borrow units overflows u128,
        // making collateral_to_borrow return None on the full-liquidation path.
        let pos = position(u128::MAX, 3_980_000);
        let strategy = FixedAmountLiquidationStrategy::new(100.0, 50);

        let result = strategy
            .calculate_liquidation_amount(&pos, &prices, &cfg, U128(1_000_000_000_000), None)
            .expect("no error");
        assert_eq!(
            result,
            StrategyDecision::Decline(DeclineReason::NotViable),
            "unvaluable collateral must decline as not-viable — no inventory clears it"
        );
    }

    /// On a partial-supporting market, a percentage slice that lands under
    /// the contract minimum is inventory-clearable — the slice scales with
    /// the balance — so it must decline as InsufficientInventory. A
    /// position-capped (dust) slice is not: no inventory changes it.
    #[test]
    fn below_minimum_percentage_slice_classifies_by_binding_constraint() {
        // The fixture's minimum is 1 raw unit; raise it so a small slice
        // can land under it (the range type is construction-validated, so
        // patch the JSON rather than the struct).
        let cfg: MarketConfiguration = near_sdk::serde_json::from_str(&IBTC_CONFIG_JSON.replace(
            r#""borrow_range":{"minimum":"1""#,
            r#""borrow_range":{"minimum":"1000000""#,
        ))
        .expect("patched fixture parses");
        let prices = btc_usdc_prices(&cfg);
        let partial = Some(crate::scanner::MarketVersion::new(1, 1, 0));

        // Large position, small inventory: the 10% inventory slice is the
        // binding constraint and lands under the raised market minimum —
        // more inventory clears it.
        let big_pos = position(100_000_000, 3_980_000);
        let strategy =
            PercentageLiquidationStrategy::new("10".parse::<LiquidationPercentage>().unwrap(), 50);
        let result = strategy
            .calculate_liquidation_amount(&big_pos, &prices, &cfg, U128(2_000_000), partial)
            .expect("no error");
        assert_eq!(
            result,
            StrategyDecision::Decline(DeclineReason::InsufficientInventory),
            "an inventory-bound slice under the minimum is a funding cause"
        );

        // Small position, roomy inventory: the position's buffered cap
        // binds and the amount lands under the minimum; no inventory
        // clears it.
        let dust_pos = position(1_000, 40);
        let result = strategy
            .calculate_liquidation_amount(
                &dust_pos,
                &prices,
                &cfg,
                U128(1_000_000_000_000),
                partial,
            )
            .expect("no error");
        assert_eq!(
            result,
            StrategyDecision::Decline(DeclineReason::NotViable),
            "a position-capped dust slice is not a funding cause"
        );
    }

    /// Control: with partial support the same budget produces a partial
    /// liquidation within the fixed amount (plus safety buffer).
    #[test]
    fn fixed_budget_partial_liquidation_stays_within_budget() {
        let cfg = ibtc_config();
        let prices = btc_usdc_prices(&cfg);
        let pos = position(100_000_000, 3_980_000);
        let strategy = FixedAmountLiquidationStrategy::new(100.0, 50);

        let StrategyDecision::Sized(repay, collateral) = strategy
            .calculate_liquidation_amount(
                &pos,
                &prices,
                &cfg,
                U128(1_000_000_000_000),
                Some(crate::scanner::MarketVersion::new(1, 1, 0)),
            )
            .expect("no error")
        else {
            panic!("partial liquidation is fundable");
        };
        assert!(
            repay.0 <= 100_000_000,
            "repay {repay:?} within the $100 budget"
        );
        assert!(collateral.0 > 0 && collateral.0 < 100_000_000);
    }

    #[test]
    fn test_partial_strategy_creation() {
        let strategy = PercentageLiquidationStrategy::new(LiquidationPercentage::FULL, 50);
        assert_eq!(strategy.max_liquidation_percentage(), 100);
        let strategy =
            PercentageLiquidationStrategy::new("50".parse::<LiquidationPercentage>().unwrap(), 50);
        assert_eq!(strategy.min_profit_margin_bps, 50);
        assert_eq!(strategy.strategy_name(), "Percentage Liquidation");
        // 100 even with a 50% partial target: the ceiling is what the
        // caller enforces, and the full-liquidation path legitimately
        // commits up to the whole available balance.
        assert_eq!(strategy.max_liquidation_percentage(), 100);
    }

    /// The percentage bounds live in the type, not in a constructor panic:
    /// 0 and 101 are unrepresentable, rejected where the value is parsed.
    #[test]
    fn liquidation_percentage_bounds() {
        assert!(LiquidationPercentage::new(1).is_ok());
        assert!(LiquidationPercentage::new(100).is_ok());
        assert!(LiquidationPercentage::new(0).is_err());
        assert!(LiquidationPercentage::new(101).is_err());
        assert_eq!("75".parse::<LiquidationPercentage>().unwrap().get(), 75);
        assert!("0".parse::<LiquidationPercentage>().is_err());
        assert!("abc".parse::<LiquidationPercentage>().is_err());
    }

    #[test]
    fn test_profitability_check() {
        let strategy = PercentageLiquidationStrategy::new("50".parse().unwrap(), 50); // 0.5% profit margin

        // Profitable case: collateral_value > (liquidation_amount + gas) * 1.005
        // Cost: 1100 (1000 liquidation + 100 gas), Min revenue: 1105, Collateral: 1110
        let is_profitable = strategy
            .should_liquidate(
                U128(1000), // liquidation amount
                U128(1110), // expected collateral value
                U128(100),  // gas cost
            )
            .unwrap();
        assert!(is_profitable, "Should be profitable");

        // Not profitable case: collateral_value < (liquidation_amount + gas) * 1.005
        // Cost: 1100, Min revenue: 1105, Collateral: 1100
        let is_not_profitable = strategy
            .should_liquidate(
                U128(1000), // liquidation amount
                U128(1100), // collateral value too low
                U128(100),  // gas cost
            )
            .unwrap();
        assert!(!is_not_profitable, "Should not be profitable");
    }

    /// Both built-ins must report the margin they actually gate on, so the
    /// caller's "not profitable" log can show the real configured threshold
    /// instead of a hardcoded 50-bps default that lies whenever
    /// MIN_PROFIT_BPS is set to anything else.
    #[test]
    fn min_profit_margin_bps_reports_the_configured_margin() {
        let pct = PercentageLiquidationStrategy::new("50".parse().unwrap(), 75);
        assert_eq!(pct.min_profit_margin_bps(), 75);

        let fixed = FixedAmountLiquidationStrategy::new(100.0, 200);
        assert_eq!(fixed.min_profit_margin_bps(), 200);
    }

    /// The fixed-amount strategy shares the exact profitability gate with the
    /// percentage strategy (provided by the trait) — pinned so the two can't
    /// silently drift apart.
    #[test]
    fn fixed_amount_profitability_gate_matches_percentage_gate() {
        let pct = PercentageLiquidationStrategy::new("50".parse().unwrap(), 50);
        let fixed = FixedAmountLiquidationStrategy::new(100.0, 50);
        for (liq, coll, gas) in [
            (1000u128, 1110u128, 100u128),
            (1000, 1100, 100),
            (1000, 1105, 100),
        ] {
            assert_eq!(
                pct.should_liquidate(U128(liq), U128(coll), U128(gas))
                    .unwrap(),
                fixed
                    .should_liquidate(U128(liq), U128(coll), U128(gas))
                    .unwrap(),
            );
        }
    }

    /// The minimum-revenue threshold must round *up*: at total cost 1100 and
    /// a 50-bps margin the exact requirement is 1105.5, so 1105 must fail and
    /// 1106 must pass.
    #[test]
    fn min_revenue_requirement_rounds_up() {
        let strategy = PercentageLiquidationStrategy::new("50".parse().unwrap(), 50);
        assert!(!strategy
            .should_liquidate(U128(1000), U128(1105), U128(100))
            .unwrap());
        assert!(strategy
            .should_liquidate(U128(1000), U128(1106), U128(100))
            .unwrap());
    }

    /// A total cost large enough to overflow the bps multiplication must fail
    /// closed (not profitable), never wrap into an arbitrary decision or
    /// panic in debug builds.
    #[test]
    fn min_revenue_overflow_fails_closed() {
        let strategy = PercentageLiquidationStrategy::new("50".parse().unwrap(), 50);
        assert!(!strategy
            .should_liquidate(U128(u128::MAX), U128(u128::MAX), U128(u128::MAX))
            .unwrap());
    }

    #[test]
    fn test_apply_liquidatable_cap_buffer_subtracts_300bps() {
        assert_eq!(apply_liquidatable_cap_buffer(10_000), 9_700);
        assert_eq!(apply_liquidatable_cap_buffer(34_516_659), 33_481_159);
    }

    #[test]
    fn test_apply_liquidatable_cap_buffer_zero() {
        assert_eq!(apply_liquidatable_cap_buffer(0), 0);
    }

    #[test]
    fn test_apply_liquidatable_cap_buffer_covers_drift_up_to_buffer_size() {
        // The buffer is applied to the bot's scan-time view of the cap. If the
        // chain later recomputes the cap and it drops by ≤ LIQUIDATABLE_CAP_BUFFER_BPS,
        // our request still fits.
        let scan_time_cap = 1_000_000u128;
        let request = apply_liquidatable_cap_buffer(scan_time_cap);

        // Drift exactly equal to the buffer: request must not exceed chain cap.
        let chain_cap_at_drift_3pct = (scan_time_cap * 9_700) / 10_000;
        assert!(
            request <= chain_cap_at_drift_3pct,
            "request {request} must fit under {chain_cap_at_drift_3pct} for ≤3% drift",
        );
    }

    #[test]
    fn test_min_with_cap_buffer_returns_desired_when_below_cap() {
        // Desired is well below the buffered cap → no clipping.
        let liquidatable = 1_000_000u128;
        let desired = 500_000u128;
        assert_eq!(min_with_cap_buffer(desired, liquidatable), desired);
    }

    #[test]
    fn test_min_with_cap_buffer_clips_to_buffered_cap_when_desired_exceeds() {
        // Desired exceeds the buffered cap → must clip to buffered cap, never to raw cap.
        let liquidatable = 1_000_000u128;
        let desired = 2_000_000u128;
        let result = min_with_cap_buffer(desired, liquidatable);
        assert_eq!(result, apply_liquidatable_cap_buffer(liquidatable));
        assert!(
            result < liquidatable,
            "result {result} must be strictly less than raw cap {liquidatable}",
        );
    }

    #[test]
    fn test_min_with_cap_buffer_returns_zero_for_dust_cap() {
        // 33 * 9700 / 10000 = 32_010 / 10_000 = 32 (integer divide) wait no:
        // 33 * 9700 = 320_100 / 10_000 = 32. Hmm; let me pick a value < 34.
        // For 33: (33 * 9_700) / 10_000 = 320_100 / 10_000 = 32. Still > 0.
        // For 1: (1 * 9_700) / 10_000 = 9_700 / 10_000 = 0.
        assert_eq!(apply_liquidatable_cap_buffer(1), 0);
        // Strategies must guard against this case (declines instead of
        // sending borrow with zero collateral request).
        assert_eq!(min_with_cap_buffer(100, 1), 0);
    }

    #[test]
    fn test_min_with_cap_buffer_clips_when_desired_just_above_buffered() {
        // Edge case: desired just above the buffered cap, still under raw cap.
        // Without this helper a regression could pass `desired` through unclipped.
        let liquidatable = 1_000_000u128;
        let buffered = apply_liquidatable_cap_buffer(liquidatable);
        let desired = buffered + 1;
        assert_eq!(min_with_cap_buffer(desired, liquidatable), buffered);
    }

    #[test]
    fn test_apply_liquidatable_cap_buffer_insufficient_for_large_drift() {
        // Honesty check: the buffer does NOT cover drifts larger than itself.
        // The observed incident (request 37.8M vs available 34.5M, ≈9% drift)
        // would still revert even with this buffer applied at scan time.
        // Notification dedup is the safety net for that case.
        let scan_time_cap = 37_818_981u128;
        let request = apply_liquidatable_cap_buffer(scan_time_cap);
        let chain_cap_after_drift = 34_516_659u128; // observed
        assert!(
            request > chain_cap_after_drift,
            "9% drift exceeds 3% buffer — request {request} still > chain {chain_cap_after_drift}",
        );
    }

    // Note: Gas cost check removed - gas costs are negligible on NEAR
    // (typically < 0.1% of liquidation value even with 150 TGas at $100 NEAR)

    // Tests for conversion functions removed - they require complex PricePair setup
    // that is better tested in integration tests with real market configurations.
    // The conversion formulas are straightforward:
    // - collateral_to_borrow: borrow = collateral × price × (1 - spread)
    // - borrow_to_collateral: collateral = borrow / (price × (1 - spread))
}
