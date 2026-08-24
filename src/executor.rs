//! Liquidation transaction executor module.
//!
//! Handles the creation and submission of liquidation transactions,
//! including inventory management and immediate collateral swapping.

use near_sdk::{json_types::U128, AccountId};
use templar_common::asset::{
    BorrowAsset, BorrowAssetAmount, CollateralAsset, CollateralAssetAmount, FungibleAsset,
    FungibleAssetAmount,
};
use templar_gateway_client::SigningClient;
use templar_gateway_methods_spec::{market, tx};
use templar_gateway_types::{common::TxExecutionStatus, OperationStatus};

use crate::{
    inventory, swap::SwapProvider, CollateralStrategy, LiquidationOutcome, LiquidatorError,
    LiquidatorResult,
};

/// Swap issue that occurred after a successful liquidation.
/// Returned to the caller so notifications can be sent in the right order
/// (liquidation success first, then swap issue).
#[derive(Debug)]
pub enum SwapIssue {
    /// Swap provider doesn't support this asset pair.
    Unsupported {
        from: String,
        to: String,
        amount: String,
    },
    /// Swap failed with an error.
    Failed {
        from: String,
        to: String,
        amount: String,
        error: String,
    },
}

/// A market's asset decimals, from its validated on-chain oracle
/// configuration (gated by the registry's sanity check at registration).
#[derive(Debug, Clone, Copy)]
pub struct MarketDecimals {
    pub borrow: i32,
    pub collateral: i32,
}

/// How one execution settles inventory — the mode and the token are one
/// value, so "dry-run holding a live reservation" and "live execution with
/// nothing reserved" (which would leave spent tokens counted as available
/// until the next refresh) are unrepresentable rather than checked.
#[derive(Debug)]
pub enum Settlement {
    /// Live execution: the caller reserved this amount before its oracle
    /// push; the executor settles the token on every exit path. A dry-run
    /// executor refuses this variant — the executor's own mode remains the
    /// authority on whether money moves.
    Live(inventory::Reservation),
    /// Simulation: no inventory was touched and nothing settles.
    DryRun,
}

/// The amounts one liquidation execution sends and expects: what the sized,
/// gate-approved plan resolved to, in on-chain units.
#[derive(Debug, Clone, Copy)]
pub struct ExecutionRequest {
    /// Borrow-asset amount to repay.
    pub liquidation_amount: BorrowAssetAmount,
    /// Collateral amount requested in return.
    pub collateral_amount: CollateralAssetAmount,
    /// The collateral's expected value in borrow-asset units (drives the
    /// JIT-swap USD threshold).
    pub expected_collateral_value: BorrowAssetAmount,
}

/// Liquidation transaction executor.
///
/// Responsible for:
/// - Creating liquidation transactions
/// - Managing inventory reservations
/// - Executing transactions
/// - Immediately swapping collateral based on strategy
pub struct LiquidationExecutor {
    client: SigningClient,
    inventory: inventory::SharedInventory,
    market: AccountId,
    dry_run: bool,
    collateral_strategy: CollateralStrategy,
    swap_provider: Option<crate::swap::SwapProviderImpl>,
    swap_retry_config: crate::swap::SwapRetryConfig,
    min_swap_value_usd: f64,
    collateral_decimals: i32,
    /// Borrow-asset decimals, for treating borrow-denominated values as a USD
    /// proxy in the JIT-swap threshold check.
    borrow_decimals: i32,
}

impl LiquidationExecutor {
    /// Creates a new liquidation executor.
    pub fn new(
        client: SigningClient,
        inventory: inventory::SharedInventory,
        market: AccountId,
        dry_run: bool,
        swap: crate::SwapConfig,
        decimals: MarketDecimals,
    ) -> Self {
        let crate::SwapConfig {
            provider: swap_provider,
            retry: swap_retry_config,
            min_swap_value_usd,
            collateral_strategy,
        } = swap;
        let MarketDecimals {
            collateral: collateral_decimals,
            borrow: borrow_decimals,
        } = decimals;
        Self {
            client,
            inventory,
            market,
            dry_run,
            collateral_strategy,
            swap_provider,
            swap_retry_config,
            min_swap_value_usd,
            collateral_decimals,
            borrow_decimals,
        }
    }

    /// Get reference to the shared inventory
    pub fn inventory(&self) -> &inventory::SharedInventory {
        &self.inventory
    }

    /// Check if executor is in dry run mode
    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// Executes a liquidation transaction.
    ///
    /// # Reservation contract
    ///
    /// In live mode the caller must have **already reserved**
    /// `liquidation_amount` of `borrow_asset` before calling
    /// ([`crate::Liquidator::liquidate`] reserves ahead of its paid oracle
    /// push, so an inventory-race loser fails before spending gas). This
    /// method owns the reservation's end of life: released on every failure
    /// path, consumed (debited) on success. Dry-run touches no inventory on
    /// either side.
    ///
    /// # Flow
    /// 1. Create and submit transaction (caller holds the reservation)
    /// 2. Handle collateral based on strategy
    /// 3. Consume the reservation on success, release it on failure
    #[tracing::instrument(skip(self, borrow_asset, collateral_asset), level = "info")]
    #[allow(clippy::too_many_lines)]
    pub async fn execute_liquidation(
        &self,
        borrow_account: &AccountId,
        borrow_asset: &FungibleAsset<BorrowAsset>,
        collateral_asset: &FungibleAsset<CollateralAsset>,
        request: ExecutionRequest,
        settlement: Settlement,
    ) -> LiquidatorResult<(LiquidationOutcome, Option<SwapIssue>)> {
        let ExecutionRequest {
            liquidation_amount,
            collateral_amount,
            expected_collateral_value,
        } = request;
        // The settlement variant selects the dry-run branch, but it is only
        // the token's carrier: `self.dry_run` gates each direction below,
        // so a mismatched pair fails closed rather than moving funds or
        // fabricating a result.
        if matches!(settlement, Settlement::DryRun) {
            // A live executor must not report a simulated success as a
            // liquidation — that would send non-dry-run notifications and
            // clear failure dedup with nothing executed.
            if !self.dry_run {
                return Err(LiquidatorError::StrategyError(
                    "dry-run settlement passed to a live executor; liquidation aborted".to_string(),
                ));
            }
            // Log JIT swap intent if applicable
            if matches!(self.collateral_strategy, CollateralStrategy::SwapToBorrow)
                && self.swap_provider.is_some()
                && collateral_asset.to_string() != borrow_asset.to_string()
            {
                let usd_estimate =
                    crate::profitability::ProfitabilityCalculator::borrow_units_to_usd(
                        u128::from(expected_collateral_value),
                        self.borrow_decimals,
                    );

                if usd_estimate >= self.min_swap_value_usd {
                    tracing::info!(
                        borrower = %borrow_account,
                        from = %collateral_asset,
                        to = %borrow_asset,
                        collateral_amount = %u128::from(collateral_amount),
                        usd_value = format!("${usd_estimate:.2}"),
                        "[DRY RUN] Would JIT swap collateral after liquidation"
                    );
                } else {
                    tracing::info!(
                        borrower = %borrow_account,
                        from = %collateral_asset,
                        collateral_amount = %u128::from(collateral_amount),
                        usd_value = format!("${usd_estimate:.2}"),
                        threshold = format!("${:.2}", self.min_swap_value_usd),
                        "[DRY RUN] Would skip JIT swap (below threshold), batch later"
                    );
                }
            }
            return Ok((LiquidationOutcome::Liquidated, None));
        }
        let Settlement::Live(reservation) = settlement else {
            unreachable!("dry-run returned above");
        };
        // The Settlement variant carries the token, but it must not become
        // the sole authority on whether money moves: DRY_RUN=true is the
        // crate's safety invariant, so a live settlement handed to a
        // dry-run executor fails closed instead of submitting.
        if self.dry_run {
            self.inventory.write().await.release(reservation);
            return Err(LiquidatorError::StrategyError(
                "live settlement passed to a dry-run executor; reservation released, liquidation aborted"
                    .to_string(),
            ));
        }
        // Fail closed on a mismatched token — wrong amount *or wrong asset*:
        // the transaction would spend one thing while settlement debited
        // another, silently mis-accounting inventory until the next refresh.
        if !reservation.covers(borrow_asset, liquidation_amount) {
            let reserved = u128::from(reservation.amount());
            self.inventory.write().await.release(reservation);
            return Err(LiquidatorError::StrategyError(format!(
                "reservation (amount {reserved}) does not cover {} of {borrow_asset}; reservation released, liquidation aborted",
                u128::from(liquidation_amount)
            )));
        }
        // The reservation settles exactly once on whichever live path this
        // function exits through; `take()` makes each settle site
        // self-evidently the only one to run.
        let mut reservation = Some(reservation);

        // Execute liquidation transaction through the gateway. The driver signs,
        // submits, and polls to finality; a reverted on-chain transaction comes
        // back as `Ok` with a `Failed` operation status (not an `Err`), so the
        // status is checked explicitly below.
        tracing::info!(
            borrower = %borrow_account,
            liquidation_amount = %u128::from(liquidation_amount),
            expected_collateral_value = %u128::from(expected_collateral_value),
            collateral_amount = %u128::from(collateral_amount),
            "Submitting liquidation transaction"
        );

        let tx_start = std::time::Instant::now();
        let tx_result = self
            .client
            .execute(market::Liquidate::new(
                self.market.clone(),
                borrow_account.clone(),
                liquidation_amount,
                Some(collateral_amount),
            ))
            .await;

        match tx_result {
            Ok(operation_result) => {
                let tx_duration = tx_start.elapsed();

                match operation_result.operation.status {
                    OperationStatus::Succeeded => {
                        // The operation status reflects only the transaction's
                        // final receipt. `market::Liquidate` is an
                        // `ft_transfer_call`, so a liquidation the market rejects
                        // makes the receiver callback panic while
                        // `ft_resolve_transfer` refunds and the top-level
                        // transaction still succeeds. Treat any failed receipt as
                        // a failed liquidation.
                        let failed_receipt =
                            match self.first_failed_receipt(&operation_result).await {
                                Ok(failed_receipt) => failed_receipt,
                                Err(error) => {
                                    // Inventory was reserved by the caller (see the
                                    // reservation contract above); release it before
                                    // surfacing the inspection error, like the other
                                    // failure paths.
                                    if let Some(r) = reservation.take() {
                                        self.inventory.write().await.release(r);
                                    }
                                    return Err(error);
                                }
                            };

                        if let Some(failed_on) = failed_receipt {
                            if let Some(r) = reservation.take() {
                                self.inventory.write().await.release(r);
                            }

                            let operation_id = operation_result.operation.id.0.clone();
                            let error_msg = format!(
                                "Liquidation operation {operation_id} succeeded at top level but a receipt on {failed_on} failed (likely rejected and refunded)"
                            );
                            tracing::error!(
                                borrower = %borrow_account,
                                liquidation_amount = %u128::from(liquidation_amount),
                                operation_id = %operation_id,
                                failed_receipt_account = %failed_on,
                                "Liquidation reverted in a receipt despite top-level success, inventory released"
                            );
                            return Err(LiquidatorError::TransactionFailed(error_msg));
                        }

                        tracing::info!(
                            borrower = %borrow_account,
                            liquidation_amount = %u128::from(liquidation_amount),
                            expected_collateral_value = %u128::from(expected_collateral_value),
                            collateral_amount = %u128::from(collateral_amount),
                            tx_duration_ms = tx_duration.as_millis(),
                            "Liquidation executed successfully (all receipts succeeded)"
                        );

                        // Tokens have left our account: consume (debit +
                        // un-reserve), never release — a bare release would
                        // count the spent amount as available until the next
                        // RPC refresh.
                        if let Some(r) = reservation.take() {
                            self.inventory.write().await.consume(r);
                        }

                        // Handle collateral based on strategy
                        let (swap_succeeded, swap_issue) = match &self.collateral_strategy {
                            CollateralStrategy::Hold => (false, None),
                            CollateralStrategy::SwapToBorrow => {
                                // Estimate USD value for threshold check.
                                // The expected_collateral_value is denominated in borrow asset
                                // (often a USD stablecoin), so it serves as a rough USD proxy
                                // once the asset's decimals are scaled out.
                                let usd_estimate = Some(
                                    crate::profitability::ProfitabilityCalculator::borrow_units_to_usd(
                                        u128::from(expected_collateral_value),
                                        self.borrow_decimals,
                                    ),
                                );
                                // Immediately swap collateral back to borrow asset
                                self.swap_collateral_to_borrow(
                                    collateral_asset,
                                    borrow_asset,
                                    collateral_amount,
                                    usd_estimate,
                                )
                                .await
                                .unwrap_or((false, None))
                            }
                        };

                        // If swap succeeded, refresh inventory to get updated balance
                        if swap_succeeded {
                            if let Err(e) = self
                                .inventory
                                .write()
                                .await
                                .refresh_asset(borrow_asset)
                                .await
                            {
                                tracing::warn!(
                                    borrow_asset = %borrow_asset,
                                    error = ?e,
                                    "Failed to refresh inventory after swap, continuing with stale balance"
                                );
                            }
                        }

                        Ok((LiquidationOutcome::Liquidated, swap_issue))
                    }
                    failed_status => {
                        // Operation did not succeed (reverted receipt, or did not
                        // reach finality) - release reserved inventory.
                        if let Some(r) = reservation.take() {
                            self.inventory.write().await.release(r);
                        }

                        let operation_id = operation_result.operation.id.0.clone();
                        let error_msg = format!(
                            "Liquidation operation {operation_id} ended with status {failed_status:?}"
                        );

                        tracing::error!(
                            borrower = %borrow_account,
                            liquidation_amount = %u128::from(liquidation_amount),
                            operation_id = %operation_id,
                            status = ?failed_status,
                            "Liquidation transaction did not succeed, inventory released"
                        );
                        Err(LiquidatorError::TransactionFailed(error_msg))
                    }
                }
            }
            Err(e) => {
                // Release reserved inventory on submission failure
                if let Some(r) = reservation.take() {
                    self.inventory.write().await.release(r);
                }

                tracing::error!(
                    borrower = %borrow_account,
                    liquidation_amount = %u128::from(liquidation_amount),
                    error = %e,
                    "Liquidation gateway call failed, inventory released"
                );
                Err(LiquidatorError::LiquidationTransactionError(e.into()))
            }
        }
    }

    /// Return the first contract whose receipt failed for a completed operation,
    /// if any.
    ///
    /// The gateway's operation status only reflects the transaction's final
    /// receipt, so a rejected `ft_transfer_call` — whose receiver callback
    /// panicked and was refunded by `ft_resolve_transfer` — reports success.
    /// This fetches the receipt outcomes so the caller can detect that.
    ///
    /// Fails closed if the completed operation carries no transaction hash:
    /// without it the receipts can't be inspected, so success must not be
    /// assumed.
    async fn first_failed_receipt(
        &self,
        result: &templar_gateway_types::common::WriteOperationResult,
    ) -> LiquidatorResult<Option<String>> {
        let Some(tx_hash) = result.operation.latest_tx_hash() else {
            return Err(LiquidatorError::MissingTransactionHash(format!(
                "liquidation operation {} reported terminal without a transaction hash; cannot inspect receipts",
                result.operation.id.0
            )));
        };
        let detail = self
            .client
            .read(tx::Get {
                tx_hash,
                sender_account_id: result.operation.signer_account_id.0.clone(),
                wait_until: Some(TxExecutionStatus::Executed),
                encoding: tx::ValueEncoding::Json,
            })
            .await
            .map_err(|error| LiquidatorError::LiquidationTransactionError(error.into()))?;
        Ok(detail.failed_receipts.first().map(ToString::to_string))
    }

    /// Swap collateral immediately after liquidation.
    ///
    /// Returns `Ok((succeeded, swap_issue))` where `swap_issue` is populated
    /// when the swap failed or was unsupported (for notification by the caller).
    #[allow(clippy::too_many_lines)]
    async fn swap_collateral_to_borrow(
        &self,
        collateral_asset: &FungibleAsset<CollateralAsset>,
        borrow_asset: &FungibleAsset<BorrowAsset>,
        collateral_amount: CollateralAssetAmount,
        expected_collateral_value_usd: Option<f64>,
    ) -> LiquidatorResult<(bool, Option<SwapIssue>)> {
        let Some(ref swap_provider) = self.swap_provider else {
            tracing::debug!("No swap provider configured, holding collateral");
            return Ok((false, None));
        };

        // Skip swap if collateral is already the target borrow asset
        if collateral_asset.to_string() == borrow_asset.to_string() {
            tracing::debug!("Collateral is already borrow asset, skipping JIT swap");
            return Ok((false, None));
        }

        // Skip swap if the provider doesn't support this asset pair
        if !swap_provider.supports_assets(collateral_asset, borrow_asset) {
            tracing::info!(
                from = %collateral_asset,
                to = %borrow_asset,
                "Swap provider does not support asset pair, holding collateral"
            );
            return Ok((
                false,
                Some(SwapIssue::Unsupported {
                    from: crate::format::short_asset_name(&collateral_asset.to_string()),
                    to: crate::format::short_asset_name(&borrow_asset.to_string()),
                    amount: crate::format::format_amount_short(
                        u128::from(collateral_amount),
                        self.collateral_decimals,
                        &collateral_asset.to_string(),
                    ),
                }),
            ));
        }

        // Skip swap if value is below threshold — will be picked up by batch swap
        if let Some(usd_value) = expected_collateral_value_usd {
            if usd_value < self.min_swap_value_usd {
                tracing::info!(
                    asset = %collateral_asset,
                    amount_raw = %u128::from(collateral_amount),
                    usd_value = format!("${usd_value:.2}"),
                    threshold = format!("${:.2}", self.min_swap_value_usd),
                    "Skipping JIT swap - below threshold, will batch later"
                );
                return Ok((false, None));
            }
        }

        let from_asset_id = collateral_asset.to_string();
        let to_asset_id = borrow_asset.to_string();

        tracing::info!(
            from = %from_asset_id,
            to = %to_asset_id,
            amount_raw = %u128::from(collateral_amount),
            "JIT swap: collateral→borrow"
        );

        let swap_amount = FungibleAssetAmount::from(U128::from(collateral_amount));
        let swap_name = format!("jit:{from_asset_id}");

        let provider = swap_provider.clone();
        let coll = collateral_asset.clone();
        let borrow = borrow_asset.clone();

        let result =
            crate::swap::retry::swap_with_retry(&self.swap_retry_config, &swap_name, || {
                let provider = provider.clone();
                let coll = coll.clone();
                let borrow = borrow.clone();
                async move {
                    use crate::swap::SwapProvider;
                    provider.swap(&coll, &borrow, swap_amount).await
                }
            })
            .await;

        let amount_short = crate::format::format_amount_short(
            u128::from(collateral_amount),
            self.collateral_decimals,
            &from_asset_id,
        );
        let from_short = crate::format::short_asset_name(&from_asset_id);
        let to_short = crate::format::short_asset_name(&to_asset_id);

        match result {
            Ok(()) => {
                tracing::info!(
                    from = %from_asset_id,
                    to = %to_asset_id,
                    amount_raw = %u128::from(collateral_amount),
                    "JIT swap completed - inventory replenished"
                );
                Ok((true, None))
            }
            Err(e) => {
                let issue = match &e.kind {
                    crate::swap::SwapErrorKind::AmountTooLow { .. } => {
                        tracing::info!(
                            swap = %swap_name,
                            error = %e,
                            "JIT swap skipped - amount below provider minimum, will batch"
                        );
                        None // Not a notification-worthy issue
                    }
                    crate::swap::SwapErrorKind::QuoteFailed { .. } => {
                        tracing::debug!(
                            swap = %swap_name,
                            "No swap route available for asset, holding collateral"
                        );
                        Some(SwapIssue::Failed {
                            from: from_short,
                            to: to_short,
                            amount: amount_short,
                            error: "No swap route available".to_string(),
                        })
                    }
                    crate::swap::SwapErrorKind::Indeterminate { .. } => {
                        tracing::warn!(
                            swap = %swap_name,
                            error = %e,
                            "JIT swap outcome unknown - funds may have moved; not retrying, next inventory refresh reconciles"
                        );
                        Some(SwapIssue::Failed {
                            from: from_short,
                            to: to_short,
                            amount: amount_short,
                            error: e.to_string(),
                        })
                    }
                    // Named, not a wildcard: a new SwapErrorKind variant must
                    // make an explicit funds-safety decision here to compile.
                    crate::swap::SwapErrorKind::NetworkError { .. }
                    | crate::swap::SwapErrorKind::ServerError { .. }
                    | crate::swap::SwapErrorKind::RateLimited
                    | crate::swap::SwapErrorKind::Timeout { .. }
                    | crate::swap::SwapErrorKind::ValidationError { .. }
                    | crate::swap::SwapErrorKind::Unknown { .. } => {
                        tracing::info!(
                            swap = %swap_name,
                            error = %e,
                            "JIT swap failed, holding collateral"
                        );
                        Some(SwapIssue::Failed {
                            from: from_short,
                            to: to_short,
                            amount: amount_short,
                            error: e.to_string(),
                        })
                    }
                };
                Ok((false, issue))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn test_executor(dry_run: bool) -> (LiquidationExecutor, inventory::SharedInventory) {
        let network = near_api::NetworkConfig::from_rpc_url(
            "testnet",
            "https://rpc.testnet.near.org".parse().unwrap(),
        );
        let secret_key: near_api::SecretKey =
            near_crypto::SecretKey::from_seed(near_crypto::KeyType::ED25519, "liquidator-test")
                .to_string()
                .parse()
                .unwrap();
        // No network I/O happens in these tests: both mismatch guards
        // return before the executor issues any read or transaction.
        let client = SigningClient::connect(
            network,
            AccountId::from_str("test.near").unwrap(),
            secret_key,
        )
        .unwrap();
        let account = AccountId::from_str("test.near").unwrap();
        let inventory: inventory::SharedInventory = Arc::new(RwLock::new(
            inventory::InventoryManager::new(client.clone(), account.clone()),
        ));
        let executor = LiquidationExecutor::new(
            client,
            inventory.clone(),
            account,
            dry_run,
            crate::SwapConfig {
                provider: None,
                retry: crate::swap::SwapRetryConfig::default(),
                min_swap_value_usd: 0.0,
                collateral_strategy: CollateralStrategy::Hold,
            },
            MarketDecimals {
                borrow: 6,
                collateral: 8,
            },
        );
        (executor, inventory)
    }

    fn request(amount: u128) -> ExecutionRequest {
        ExecutionRequest {
            liquidation_amount: BorrowAssetAmount::from(amount),
            collateral_amount: CollateralAssetAmount::from(amount),
            expected_collateral_value: BorrowAssetAmount::from(amount),
        }
    }

    /// A live settlement handed to a dry-run executor must fail closed and
    /// release the token — DRY_RUN=true is the crate's safety invariant, and
    /// the executor's own mode stays the authority on whether money moves.
    #[tokio::test]
    async fn dry_run_executor_rejects_live_settlement_and_releases() {
        let (executor, inventory) = test_executor(true);
        let asset: FungibleAsset<BorrowAsset> =
            FungibleAsset::from_str("nep141:usdc.near").unwrap();
        let collateral: FungibleAsset<CollateralAsset> =
            FungibleAsset::from_str("nep141:btc.near").unwrap();
        inventory
            .write()
            .await
            .seed_asset_for_tests(&asset, BorrowAssetAmount::from(1_000));
        let reservation = inventory
            .write()
            .await
            .reserve(&asset, BorrowAssetAmount::from(300))
            .unwrap();

        let borrower = AccountId::from_str("borrower.near").unwrap();
        let result = executor
            .execute_liquidation(
                &borrower,
                &asset,
                &collateral,
                request(300),
                Settlement::Live(reservation),
            )
            .await;

        assert!(result.is_err(), "must refuse to submit under dry-run");
        assert_eq!(
            inventory.read().await.get_reserved_balance(&asset).0,
            0,
            "the rejected token must be released, not leaked"
        );
    }

    /// A dry-run settlement handed to a live executor must fail closed —
    /// reporting a simulated success as Liquidated would send non-dry-run
    /// notifications and clear failure dedup with nothing executed.
    #[tokio::test]
    async fn live_executor_rejects_dry_run_settlement() {
        let (executor, _inventory) = test_executor(false);
        let asset: FungibleAsset<BorrowAsset> =
            FungibleAsset::from_str("nep141:usdc.near").unwrap();
        let collateral: FungibleAsset<CollateralAsset> =
            FungibleAsset::from_str("nep141:btc.near").unwrap();

        let borrower = AccountId::from_str("borrower.near").unwrap();
        let result = executor
            .execute_liquidation(
                &borrower,
                &asset,
                &collateral,
                request(300),
                Settlement::DryRun,
            )
            .await;

        assert!(
            result.is_err(),
            "a live executor must not fabricate Liquidated from a simulation"
        );
    }

    /// A token for the wrong asset (same amount) must fail closed and be
    /// released — spending asset A while debiting asset B would leave A
    /// counted available for concurrent sizing.
    #[tokio::test]
    async fn mismatched_reservation_asset_fails_closed() {
        let (executor, inventory) = test_executor(false);
        let asset_a: FungibleAsset<BorrowAsset> =
            FungibleAsset::from_str("nep141:usdc.near").unwrap();
        let asset_b: FungibleAsset<BorrowAsset> =
            FungibleAsset::from_str("nep141:usdt.near").unwrap();
        let collateral: FungibleAsset<CollateralAsset> =
            FungibleAsset::from_str("nep141:btc.near").unwrap();
        inventory
            .write()
            .await
            .seed_asset_for_tests(&asset_b, BorrowAssetAmount::from(1_000));
        let reservation = inventory
            .write()
            .await
            .reserve(&asset_b, BorrowAssetAmount::from(300))
            .unwrap();

        let borrower = AccountId::from_str("borrower.near").unwrap();
        let result = executor
            .execute_liquidation(
                &borrower,
                &asset_a,
                &collateral,
                request(300),
                Settlement::Live(reservation),
            )
            .await;

        assert!(
            result.is_err(),
            "wrong-asset token must abort the liquidation"
        );
        assert_eq!(
            inventory.read().await.get_reserved_balance(&asset_b).0,
            0,
            "the mismatched token must be released"
        );
    }
}
