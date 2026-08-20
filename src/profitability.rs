//! Profitability calculation module.
//!
//! Handles cost/profit calculations for liquidations including:
//! - Gas cost conversions
//! - Collateral value conversions
//! - Profitability metrics

use near_sdk::json_types::U128;
use templar_common::{
    market::MarketConfiguration,
    oracle::pyth::{self, OracleResponse},
};

use crate::{LiquidatorError, LiquidatorResult};

/// Profitability calculator for liquidations.
///
/// Responsible for:
/// - Converting gas costs to borrow asset units
/// - Converting collateral to borrow asset value
/// - Calculating profit metrics
pub struct ProfitabilityCalculator;

impl ProfitabilityCalculator {
    /// Default gas cost estimate in USD
    /// ~$0.05 USD for a liquidation transaction (conservative estimate for 0.01 NEAR at ~$5)
    pub const DEFAULT_GAS_COST_USD: f64 = 0.05;

    /// Projects an oracle price (`mantissa × 10^expo`) to USD, rejecting
    /// zero, negative, and non-finite values: dividing by any of them yields
    /// inf, and `inf as u128` saturates to a plausible-looking `u128::MAX`
    /// that sails through the profitability gate.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn price_to_usd(price: &pyth::Price) -> LiquidatorResult<f64> {
        let usd = (price.price.0 as f64) * 10f64.powi(price.expo);
        if !usd.is_finite() || usd <= 0.0 {
            return Err(LiquidatorError::StrategyError(format!(
                "unusable oracle price: {} × 10^{}",
                price.price.0, price.expo
            )));
        }
        Ok(usd)
    }

    /// Converts an f64 amount to `u128`, rejecting values the bare cast would
    /// silently saturate: non-finite, negative, or beyond `u128::MAX`.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub(crate) fn checked_f64_to_u128(value: f64) -> LiquidatorResult<u128> {
        if !value.is_finite() || value < 0.0 || value >= u128::MAX as f64 {
            return Err(LiquidatorError::StrategyError(format!(
                "amount not representable as u128: {value}"
            )));
        }
        Ok(value as u128)
    }

    /// Treats a raw borrow-asset amount as a USD figure by scaling out the
    /// asset's decimals. This is a *proxy*, valid only to the extent the
    /// borrow asset is a USD stablecoin — used for the JIT-swap USD threshold
    /// check, never for on-chain amounts.
    #[allow(clippy::cast_precision_loss)]
    pub fn borrow_units_to_usd(value: u128, borrow_decimals: i32) -> f64 {
        (value as f64) / 10f64.powi(borrow_decimals)
    }

    /// Converts USD gas cost estimate to borrow asset units using oracle prices.
    ///
    /// Formula: `gas_cost_borrow_asset = gas_cost_usd / borrow_asset_usd_price * 10^borrow_decimals`
    ///
    /// # Arguments
    ///
    /// * `gas_cost_usd` - Gas cost in USD (e.g., 0.05 for $0.05)
    /// * `oracle_response` - Oracle price data containing borrow asset/USD price
    /// * `configuration` - Market configuration containing borrow asset price ID and decimals
    ///
    /// # Returns
    ///
    /// Gas cost denominated in borrow asset base units
    ///
    /// # Errors
    ///
    /// Returns an error if the borrow asset price is not found in the oracle
    /// response, is present but unusable (zero, negative, or non-finite), or
    /// the converted amount is not representable as `u128`.
    pub fn convert_gas_cost_to_borrow_asset(
        gas_cost_usd: f64,
        oracle_response: &OracleResponse,
        configuration: &MarketConfiguration,
    ) -> LiquidatorResult<U128> {
        // Get borrow asset price from oracle configuration
        let borrow_price_id = configuration
            .price_oracle_configuration
            .borrow_asset_price_id;
        let borrow_decimals = configuration
            .price_oracle_configuration
            .borrow_asset_decimals;

        let borrow_price = oracle_response
            .get(&borrow_price_id)
            .and_then(|opt| opt.as_ref())
            .ok_or_else(|| {
                LiquidatorError::StrategyError("Borrow asset price not found in oracle".to_string())
            })?;

        let borrow_usd = Self::price_to_usd(borrow_price)?;
        let gas_cost_borrow = (gas_cost_usd / borrow_usd) * 10f64.powi(borrow_decimals);
        Ok(U128(Self::checked_f64_to_u128(gas_cost_borrow)?))
    }

    /// Converts collateral asset amount to borrow asset units using oracle prices.
    ///
    /// Formula: `borrow_value = (collateral_amount * collateral_usd_price) / borrow_usd_price`
    ///
    /// # Arguments
    ///
    /// * `collateral_amount` - Amount in collateral asset base units
    /// * `oracle_response` - Oracle price data containing both asset prices
    /// * `configuration` - Market configuration containing price IDs and decimals
    ///
    /// # Returns
    ///
    /// Collateral value denominated in borrow asset base units
    ///
    /// # Errors
    ///
    /// Returns an error if either price is not found in the oracle response,
    /// is present but unusable (zero, negative, or non-finite), or the
    /// converted amount is not representable as `u128`.
    pub fn convert_collateral_to_borrow_asset(
        collateral_amount: U128,
        oracle_response: &OracleResponse,
        configuration: &MarketConfiguration,
    ) -> LiquidatorResult<U128> {
        let oracle_config = &configuration.price_oracle_configuration;

        // Get collateral price
        let collateral_price = oracle_response
            .get(&oracle_config.collateral_asset_price_id)
            .and_then(|opt| opt.as_ref())
            .ok_or_else(|| {
                LiquidatorError::StrategyError(
                    "Collateral asset price not found in oracle".to_string(),
                )
            })?;

        // Get borrow price
        let borrow_price = oracle_response
            .get(&oracle_config.borrow_asset_price_id)
            .and_then(|opt| opt.as_ref())
            .ok_or_else(|| {
                LiquidatorError::StrategyError("Borrow asset price not found in oracle".to_string())
            })?;

        let collateral_usd = Self::price_to_usd(collateral_price)?;
        let borrow_usd = Self::price_to_usd(borrow_price)?;

        // Convert collateral to borrow asset units
        // Step 1: Convert collateral to USD value
        #[allow(clippy::cast_precision_loss)]
        let collateral_amount_f64 = collateral_amount.0 as f64;
        let collateral_decimals = oracle_config.collateral_asset_decimals;
        let collateral_value_usd =
            (collateral_amount_f64 / 10f64.powi(collateral_decimals)) * collateral_usd;

        // Step 2: Convert USD value to borrow asset units
        let borrow_decimals = oracle_config.borrow_asset_decimals;
        let borrow_value = (collateral_value_usd / borrow_usd) * 10f64.powi(borrow_decimals);
        Ok(U128(Self::checked_f64_to_u128(borrow_value)?))
    }

    /// Calculates detailed profitability metrics for a liquidation.
    ///
    /// Returns (`net_profit`, `profit_percentage`)
    pub fn calculate_profit_metrics(
        liquidation_amount: U128,
        expected_collateral_value: U128,
        gas_cost: U128,
    ) -> (u128, u64) {
        let liquidation_cost = liquidation_amount.0;
        let gas_cost_u128 = gas_cost.0;
        let total_cost = liquidation_cost + gas_cost_u128;
        let expected_revenue = expected_collateral_value.0;

        let net_profit = expected_revenue.saturating_sub(total_cost);

        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let profit_percentage = if total_cost > 0 {
            ((net_profit as f64 / total_cost as f64) * 100.0) as u64
        } else {
            0
        };

        (net_profit, profit_percentage)
    }
}

#[cfg(test)]
mod tests {
    use near_sdk::json_types::U128;

    use super::{pyth, MarketConfiguration, OracleResponse, ProfitabilityCalculator};

    /// A real mainnet `get_configuration` payload (linear-usdt market),
    /// deserialized through the same serde path production uses. Borrow asset
    /// has 6 decimals, collateral 24; price ids are 0xbb…/0xcc… repeated.
    const MARKET_CONFIG_JSON: &str = r#"{"time_chunk_configuration":{"duration_ms":"600000"},"borrow_asset":{"Nep141":"usdt.tether-token.near"},"collateral_asset":{"Nep141":"linear-protocol.near"},"price_oracle_configuration":{"account_id":"proxy-oracle-linear-usdt.v1.tmplr.near","collateral_asset_price_id":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","collateral_asset_decimals":24,"borrow_asset_price_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","borrow_asset_decimals":6,"price_maximum_age_s":60},"borrow_mcr_maintenance":"1.40000000000000000000000000000000000001","borrow_mcr_liquidation":"1.33333333333333333333333333333333333334","borrow_asset_maximum_usage_ratio":"0.99000000000000000000000000000000000001","borrow_origination_fee":{"Flat":"0"},"borrow_interest_rate_strategy":{"Piecewise":{"base":"0","optimal":"0.90000000000000000000000000000000000001","rate_1":"0.08888888888888888888888888888888888889","rate_2":"2.40000000000000000000000000000000000001"}},"borrow_maximum_duration_ms":null,"borrow_range":{"minimum":"1","maximum":null},"supply_range":{"minimum":"40000","maximum":null},"supply_withdrawal_range":{"minimum":"40000","maximum":null},"supply_withdrawal_fee":{"fee":{"Flat":"0"},"duration":"0","behavior":"Fixed"},"yield_weights":{"supply":4,"static":{"revenue.tmplr.near":1}},"protocol_account_id":"revenue.tmplr.near","liquidation_maximum_spread":"0.1"}"#;

    fn market_config() -> MarketConfiguration {
        near_sdk::serde_json::from_str(MARKET_CONFIG_JSON).expect("fixture parses")
    }

    fn fixture_response(
        borrow: Option<pyth::Price>,
        collateral: Option<pyth::Price>,
    ) -> OracleResponse {
        let mut r = OracleResponse::new();
        r.insert(
            templar_common::oracle::pyth::PriceIdentifier([0xbb; 32]),
            borrow,
        );
        r.insert(
            templar_common::oracle::pyth::PriceIdentifier([0xcc; 32]),
            collateral,
        );
        r
    }

    /// The public conversions must route through the validation — helper-only
    /// coverage would not catch either function being un-wired from it.
    #[test]
    fn conversions_reject_unusable_prices_end_to_end() {
        let cfg = market_config();

        // Zero borrow price: both conversions must error, not saturate.
        let r = fixture_response(Some(price(0, -8)), Some(price(100_000_000, -8)));
        assert!(ProfitabilityCalculator::convert_gas_cost_to_borrow_asset(0.05, &r, &cfg).is_err());
        assert!(
            ProfitabilityCalculator::convert_collateral_to_borrow_asset(U128(1), &r, &cfg).is_err()
        );

        // Zero collateral price: the collateral conversion must error.
        let r = fixture_response(Some(price(100_000_000, -8)), Some(price(0, -8)));
        assert!(
            ProfitabilityCalculator::convert_collateral_to_borrow_asset(U128(1), &r, &cfg).is_err()
        );

        // A result too large for u128 must error rather than saturate.
        let r = fixture_response(Some(price(100_000_000, -8)), Some(price(i64::MAX, 10)));
        assert!(ProfitabilityCalculator::convert_collateral_to_borrow_asset(
            U128(u128::MAX),
            &r,
            &cfg
        )
        .is_err());
    }

    /// Sanity of the happy path against the real market fixture: $0.05 gas at
    /// a $1.00 borrow price and 6 decimals is 50_000 raw units; one whole
    /// 24-decimal collateral token at $1.00 each converts to 10^6 raw borrow
    /// units.
    #[test]
    fn conversions_produce_expected_values_end_to_end() {
        let cfg = market_config();
        let one_usd = price(100_000_000, -8);
        let r = fixture_response(Some(one_usd.clone()), Some(one_usd));

        let gas = ProfitabilityCalculator::convert_gas_cost_to_borrow_asset(0.05, &r, &cfg)
            .expect("usable prices");
        assert_eq!(gas.0, 50_000);

        let value = ProfitabilityCalculator::convert_collateral_to_borrow_asset(
            U128(1_000_000_000_000_000_000_000_000),
            &r,
            &cfg,
        )
        .expect("usable prices");
        assert_eq!(value.0, 1_000_000);
    }

    fn price(mantissa: i64, expo: i32) -> templar_common::oracle::pyth::Price {
        templar_common::oracle::pyth::Price {
            price: near_sdk::json_types::I64(mantissa),
            conf: near_sdk::json_types::U64(0),
            expo,
            publish_time: templar_common::oracle::pyth::PythTimestamp::from_secs(1_755_600_000),
        }
    }

    /// A zero, negative, or non-finite oracle price must be an error, never a
    /// divisor: dividing by it yields inf, and `inf as u128` saturates to a
    /// plausible-looking u128::MAX that sails through the profitability gate.
    #[test]
    fn price_to_usd_rejects_unusable_prices() {
        assert!(ProfitabilityCalculator::price_to_usd(&price(0, -8)).is_err());
        assert!(ProfitabilityCalculator::price_to_usd(&price(-100, -8)).is_err());
        // Extreme exponent overflows f64 to infinity.
        assert!(ProfitabilityCalculator::price_to_usd(&price(i64::MAX, 300)).is_err());

        let usd = ProfitabilityCalculator::price_to_usd(&price(6_435_012_000_000, -8)).unwrap();
        assert!((usd - 64_350.12).abs() < 1e-6);
    }

    /// The f64→u128 cast saturates instead of failing; conversions must catch
    /// non-finite, negative, and out-of-range values before the cast.
    #[test]
    fn checked_f64_to_u128_rejects_unrepresentable_values() {
        assert!(ProfitabilityCalculator::checked_f64_to_u128(f64::INFINITY).is_err());
        assert!(ProfitabilityCalculator::checked_f64_to_u128(f64::NAN).is_err());
        assert!(ProfitabilityCalculator::checked_f64_to_u128(-1.0).is_err());
        assert!(ProfitabilityCalculator::checked_f64_to_u128(3.5e38).is_err());
        assert_eq!(
            ProfitabilityCalculator::checked_f64_to_u128(1_000_000.9).unwrap(),
            1_000_000
        );
        assert_eq!(
            ProfitabilityCalculator::checked_f64_to_u128(0.0).unwrap(),
            0
        );
    }

    /// The JIT-swap USD proxy must scale by the market's actual borrow-asset
    /// decimals — a hardcoded 10^6 is off by 10^12 for an 18-decimal asset.
    #[test]
    fn test_borrow_units_to_usd_scales_by_decimals() {
        // 6 decimals (USDC): 5_000_000 raw = $5
        assert!((ProfitabilityCalculator::borrow_units_to_usd(5_000_000, 6) - 5.0).abs() < 1e-9);
        // 18 decimals (DAI): 5 * 10^18 raw = $5
        assert!(
            (ProfitabilityCalculator::borrow_units_to_usd(5_000_000_000_000_000_000, 18) - 5.0)
                .abs()
                < 1e-9
        );
        // 0 decimals: raw units are whole dollars
        assert!((ProfitabilityCalculator::borrow_units_to_usd(5, 0) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn test_calculate_profit_metrics_basic() {
        let liquidation_amount = U128(1000);
        let expected_collateral = U128(1200); // 20% profit before gas
        let gas_cost = U128(50);

        let (net_profit, profit_pct) = ProfitabilityCalculator::calculate_profit_metrics(
            liquidation_amount,
            expected_collateral,
            gas_cost,
        );

        // Net profit: 1200 - (1000 + 50) = 150
        assert_eq!(net_profit, 150);
        // Profit %: (150 / 1050) * 100 = 14%
        assert_eq!(profit_pct, 14);
    }

    #[test]
    fn test_calculate_profit_metrics_zero_profit() {
        let liquidation_amount = U128(1000);
        let expected_collateral = U128(1000);
        let gas_cost = U128(0);

        let (net_profit, profit_pct) = ProfitabilityCalculator::calculate_profit_metrics(
            liquidation_amount,
            expected_collateral,
            gas_cost,
        );

        assert_eq!(net_profit, 0);
        assert_eq!(profit_pct, 0);
    }

    #[test]
    fn test_calculate_profit_metrics_loss() {
        let liquidation_amount = U128(1000);
        let expected_collateral = U128(900); // 10% loss
        let gas_cost = U128(50);

        let (net_profit, profit_pct) = ProfitabilityCalculator::calculate_profit_metrics(
            liquidation_amount,
            expected_collateral,
            gas_cost,
        );

        // Loss scenario: 900 - 1050 = -150, but saturating_sub makes it 0
        assert_eq!(net_profit, 0);
        assert_eq!(profit_pct, 0);
    }

    #[test]
    fn test_calculate_profit_metrics_high_profit() {
        let liquidation_amount = U128(1000);
        let expected_collateral = U128(2000); // 100% profit before gas
        let gas_cost = U128(100);

        let (net_profit, profit_pct) = ProfitabilityCalculator::calculate_profit_metrics(
            liquidation_amount,
            expected_collateral,
            gas_cost,
        );

        // Net profit: 2000 - 1100 = 900
        assert_eq!(net_profit, 900);
        // Profit %: (900 / 1100) * 100 = 81%
        assert_eq!(profit_pct, 81);
    }

    #[test]
    fn test_calculate_profit_metrics_zero_cost() {
        let liquidation_amount = U128(0);
        let expected_collateral = U128(1000);
        let gas_cost = U128(0);

        let (net_profit, profit_pct) = ProfitabilityCalculator::calculate_profit_metrics(
            liquidation_amount,
            expected_collateral,
            gas_cost,
        );

        assert_eq!(net_profit, 1000);
        // Division by zero protected, returns 0
        assert_eq!(profit_pct, 0);
    }

    #[test]
    fn test_calculate_profit_metrics_with_gas() {
        let liquidation_amount = U128(10_000);
        let expected_collateral = U128(11_500);
        let gas_cost = U128(500);

        let (net_profit, profit_pct) = ProfitabilityCalculator::calculate_profit_metrics(
            liquidation_amount,
            expected_collateral,
            gas_cost,
        );

        // Net profit: 11500 - (10000 + 500) = 1000
        assert_eq!(net_profit, 1000);
        // Profit %: (1000 / 10500) * 100 = 9%
        assert_eq!(profit_pct, 9);
    }

    #[test]
    fn test_calculate_profit_metrics_large_amounts() {
        let liquidation_amount = U128(1_000_000_000_000); // 1T units
        let expected_collateral = U128(1_100_000_000_000); // 10% profit
        let gas_cost = U128(1_000_000_000); // 1B units

        let (net_profit, profit_pct) = ProfitabilityCalculator::calculate_profit_metrics(
            liquidation_amount,
            expected_collateral,
            gas_cost,
        );

        // Should handle large numbers
        assert!(net_profit > 0);
        assert!(profit_pct < 100);
    }

    #[test]
    fn test_calculate_profit_metrics_minimal_profit() {
        let liquidation_amount = U128(10_000);
        let expected_collateral = U128(10_101); // ~1% profit
        let gas_cost = U128(100);

        let (net_profit, profit_pct) = ProfitabilityCalculator::calculate_profit_metrics(
            liquidation_amount,
            expected_collateral,
            gas_cost,
        );

        // Net profit: 10101 - 10100 = 1
        assert_eq!(net_profit, 1);
        // Profit %: very small, likely rounds to 0
        assert_eq!(profit_pct, 0);
    }

    #[test]
    fn test_calculate_profit_metrics_percentage_rounding() {
        let liquidation_amount = U128(1000);
        let expected_collateral = U128(1550); // 55% profit before gas
        let gas_cost = U128(0);

        let (net_profit, profit_pct) = ProfitabilityCalculator::calculate_profit_metrics(
            liquidation_amount,
            expected_collateral,
            gas_cost,
        );

        assert_eq!(net_profit, 550);
        // Profit %: (550 / 1000) * 100 = 55%
        assert_eq!(profit_pct, 55);
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn test_default_gas_cost_constant() {
        // Verify the default gas cost constant is reasonable
        assert!(ProfitabilityCalculator::DEFAULT_GAS_COST_USD > 0.0);
        assert!(ProfitabilityCalculator::DEFAULT_GAS_COST_USD < 1.0);
    }
}
