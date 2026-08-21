//! Inventory-based liquidator for Templar Protocol lending markets on NEAR.
//!
//! The bot holds an inventory of borrow-asset tokens, repays the debt of underwater
//! positions directly from that inventory, and receives their collateral at a
//! discount in return. Received collateral can optionally be swapped back into
//! borrow assets so the same inventory is available for the next round. This crate
//! is published as a **public reference implementation**: the expected way to adapt
//! it is to fork the repository, configure it via CLI flags / env vars (see
//! [`config::Args`]), and — for behavior configuration can't express — implement one
//! of the three extension seams below directly in the fork.
//!
//! # Pipeline
//!
//! Each liquidation round moves through the following stages in order. Every stage
//! is owned by a single module, so a fork that only needs to change one piece of
//! behavior can read (or replace) just that module rather than the whole crate:
//!
//! 1. **Registry refresh** — discover deployed markets across the configured
//!    registries and validate them ([`crate::service`]).
//! 2. **Position scan** — read borrow positions for each market, screen them
//!    locally against oracle prices with the contract's own status logic, and
//!    confirm apparent candidates on-chain ([`crate::scanner`]); per-position
//!    RPC scales with liquidatable positions, not with market size.
//! 3. **Strategy sizing** — decide how much of a liquidatable position to repay
//!    given available inventory ([`crate::liquidation_strategy`]).
//! 4. **Profitability gate** — reject the sizing decision unless the discounted
//!    collateral received is expected to cover the repay amount plus gas, with the
//!    configured margin ([`crate::profitability`]).
//! 5. **Execution** — submit the liquidation transaction and confirm every receipt
//!    in it actually succeeded ([`crate::executor`]).
//! 6. **Collateral handling** — hold the received collateral, or swap it back to
//!    the borrow asset through a [`crate::swap::SwapProvider`]
//!    ([`crate::executor`], [`crate::swap`]).
//! 7. **Notification** — report the round's outcome, or any failure along the way,
//!    to the configured channel ([`crate::notifier`]).
//!
//! # Extension seams
//!
//! A fork that needs behavior beyond what configuration exposes implements one of
//! these three, in-tree:
//!
//! - [`crate::swap::SwapProvider`] — a DEX/aggregator integration for converting
//!   collateral back into borrow assets. Reach for this when the bot needs to route
//!   through a venue other than the built-in 1-Click provider.
//! - [`crate::liquidation_strategy::LiquidationStrategy`] — the policy for how much
//!   of a position to repay each round. Reach for this when the built-in
//!   percentage-of-inventory and fixed-USD-amount strategies don't match the sizing
//!   policy you want (e.g. per-market caps, sizing off inventory pressure).
//! - [`crate::notifier::Notifier`] — currently sends Telegram messages only, with
//!   no trait boundary in front of it. Reach for this when alerts need to go
//!   somewhere Telegram can't (another chat platform, a paging system, a metrics
//!   sink), which today means extending or replacing this type directly.
//!
//! # Run modes
//!
//! [`config::RunMode`] selects how the bot drives the pipeline above:
//!
//! - `loop` (the default) — runs indefinitely, alternating registry refreshes and
//!   liquidation rounds on independent timers.
//! - `--run-mode once` (equivalently `--once`) — performs exactly one registry
//!   refresh and one liquidation round, then exits. Intended for cron-style
//!   schedulers (Cloud Run Jobs, Kubernetes CronJobs) rather than a long-lived
//!   process.
//!
//! # Dry-run is the default
//!
//! The bot **simulates unless told otherwise**: `DRY_RUN` defaults to `true`, so a
//! fresh checkout scans markets and logs what it would do without submitting any
//! transaction. Live trading requires explicitly setting `DRY_RUN=false` — there is
//! no other way to opt in. This is deliberate: the bot moves real inventory and is
//! run unsupervised by operators who may not have read every line of it first.

use std::sync::Arc;

use near_sdk::{json_types::U128, AccountId};
use templar_common::{
    borrow::{BorrowPosition, BorrowStatus},
    market::MarketConfiguration,
    oracle::pyth::OracleResponse,
};
use templar_gateway_client::SigningClient;

use crate::liquidation_strategy::{LiquidationStrategy, SAFETY_BUFFER_BPS};

// Modules
pub mod config;
pub mod executor;
pub mod format;
pub mod http;
pub mod inventory;
pub mod liquidation_strategy;
pub mod metrics;
pub mod notifier;
pub mod oracle;
pub mod profitability;
pub mod redstone;
pub mod rpc;
pub mod scanner;
pub mod service;
pub mod swap;

// Re-exports for convenience
pub use config::{Args, RunMode};
pub use executor::LiquidationExecutor;
pub use inventory::InventoryManager;
pub use oracle::OracleFetcher;
pub use profitability::ProfitabilityCalculator;
pub use scanner::MarketScanner;
pub use service::{LiquidatorService, ServiceConfig};

// Error conversions
use crate::rpc::AppError;

impl From<AppError> for LiquidatorError {
    fn from(err: AppError) -> Self {
        LiquidatorError::SwapProviderError(err)
    }
}

impl From<inventory::InventoryError> for LiquidatorError {
    fn from(err: inventory::InventoryError) -> Self {
        match err {
            inventory::InventoryError::InsufficientBalance { .. } => {
                LiquidatorError::InsufficientBalance
            }
            _ => LiquidatorError::StrategyError(err.to_string()),
        }
    }
}

/// Tally of a liquidation round, additive across every market scanned in it.
///
/// Feeds the optional Prometheus counters in [`crate::metrics`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RoundSummary {
    /// Positions that reached profitability evaluation or a submitted
    /// transaction this round. Excludes positions skipped for insufficient
    /// inventory (bucketed as `not_liquidatable` alongside genuinely healthy
    /// positions — see [`LiquidationOutcome::Skipped`]) and any
    /// scan/preparation-phase error before evaluation (e.g. a failed RPC
    /// read), since in both cases it isn't known whether the position was
    /// actually liquidatable. This undercounts true underwater positions;
    /// splitting `Skipped` into "healthy" vs. "liquidatable but unfunded" is
    /// tracked as follow-up work, not done here.
    pub candidates: u64,
    /// Liquidation transactions submitted (or simulated in dry-run).
    pub attempted: u64,
    /// Liquidations that landed successfully.
    pub succeeded: u64,
    /// Liquidations that failed after a transaction was submitted.
    pub failed: u64,
    /// Markets whose scan completed this round without error.
    pub markets_scanned_ok: u64,
    /// Markets whose scan failed this round. A single round can contribute
    /// more than one, since it iterates every configured market.
    pub markets_failed: u64,
}

impl RoundSummary {
    /// Folds another round's tally into this one.
    pub fn merge(&mut self, other: Self) {
        self.candidates += other.candidates;
        self.attempted += other.attempted;
        self.succeeded += other.succeeded;
        self.failed += other.failed;
        self.markets_scanned_ok += other.markets_scanned_ok;
        self.markets_failed += other.markets_failed;
    }
}

/// Result of a liquidation attempt
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiquidationOutcome {
    /// Position was successfully liquidated
    Liquidated,
    /// Position is no longer in a liquidatable state on-chain (became healthy
    /// or was liquidated by someone else). Distinct from `Skipped` —
    /// `Healthy` means the chain confirmed the position is OK.
    Healthy,
    /// We chose not to liquidate this round (insufficient inventory, below
    /// contract minimum, strategy returned no target, etc.). The position
    /// may still be liquidatable.
    Skipped,
    /// Position is liquidatable but unprofitable
    Unprofitable,
}

/// Errors that can occur during liquidation operations.
#[derive(Debug, thiserror::Error)]
pub enum LiquidatorError {
    #[error("Failed to fetch borrow status: {0}")]
    FetchBorrowStatus(rpc::RpcError),
    #[error("Failed to serialize data: {0}")]
    SerializeError(#[from] near_sdk::serde_json::Error),
    #[error("Price pair retrieval error: {0}")]
    PricePairError(#[from] templar_common::market::error::RetrievalError),
    #[error("Swap provider error: {0}")]
    SwapProviderError(AppError),
    #[error("Failed to get market configuration: {0}")]
    GetConfigurationError(rpc::RpcError),
    #[error("Failed to fetch oracle prices: {0}")]
    PriceFetchError(rpc::RpcError),
    #[error("Failed to update on-chain oracle prices: {0}")]
    OracleUpdateError(String),
    #[error("Failed to get access key data: {0}")]
    AccessKeyDataError(rpc::RpcError),
    #[error("Liquidation transaction error: {0}")]
    LiquidationTransactionError(rpc::RpcError),
    #[error("Transaction failed: {0}")]
    TransactionFailed(String),
    #[error("Operation completed without a transaction hash: {0}")]
    MissingTransactionHash(String),
    #[error("Failed to list borrow positions: {0}")]
    ListBorrowPositionsError(rpc::RpcError),
    #[error("Failed to fetch balance: {0}")]
    FetchBalanceError(rpc::RpcError),
    #[error("Failed to list deployments: {0}")]
    ListDeploymentsError(rpc::RpcError),
    #[error("Strategy error: {0}")]
    StrategyError(String),
    #[error("Insufficient balance for liquidation")]
    InsufficientBalance,
    /// Registry refresh succeeded but discovered zero supported markets —
    /// enumeration worked, there was simply nothing to scan. Single-cycle
    /// runs treat this as fatal so a scheduled job records a failure instead
    /// of a silent no-op success.
    #[error("Registry refresh yielded zero supported markets")]
    NoMarkets,
}

/// Classifies where in the liquidation pipeline an error occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorPhase {
    /// Reading on-chain state, fetching prices, listing positions.
    Scan,
    /// Decided to liquidate but haven't submitted a tx yet (nonce, serialization, strategy).
    Preparation,
    /// Liquidation or swap transaction was submitted to the network.
    Execution,
}

impl ErrorPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scan => "scan",
            Self::Preparation => "preparation",
            Self::Execution => "execution",
        }
    }
}

impl std::fmt::Display for ErrorPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stable, low-cardinality classification of failure kinds. Used as a dedup
/// bucket for repeat-failure notifications.
///
/// A typed enum (rather than a free-form string) prevents accidental
/// fragmentation of dedup state if a caller mistypes a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationKind {
    ExcessiveLiquidation,
    OfferTooLow,
    NotEligible,
    ValueCalcFailure,
    TxTimeout,
    TxFailedOther,
    TxSubmissionError,
    SwapError,
    FetchBorrowStatus,
    PricePair,
    PriceFetch,
    ListPositions,
    ListDeployments,
    GetConfiguration,
    FetchBalance,
    AccessKey,
    Serialize,
    Strategy,
    InsufficientBalance,
    OracleUpdate,
    NoMarkets,
}

impl NotificationKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExcessiveLiquidation => "excessive_liquidation",
            Self::OfferTooLow => "offer_too_low",
            Self::NotEligible => "not_eligible",
            Self::ValueCalcFailure => "value_calc_failure",
            Self::TxTimeout => "tx_timeout",
            Self::TxFailedOther => "tx_failed_other",
            Self::TxSubmissionError => "tx_submission_error",
            Self::SwapError => "swap_error",
            Self::FetchBorrowStatus => "fetch_borrow_status",
            Self::PricePair => "price_pair",
            Self::PriceFetch => "price_fetch",
            Self::ListPositions => "list_positions",
            Self::ListDeployments => "list_deployments",
            Self::GetConfiguration => "get_configuration",
            Self::FetchBalance => "fetch_balance",
            Self::AccessKey => "access_key",
            Self::Serialize => "serialize",
            Self::Strategy => "strategy",
            Self::InsufficientBalance => "insufficient_balance",
            Self::OracleUpdate => "oracle_update",
            Self::NoMarkets => "no_markets",
        }
    }
}

impl std::fmt::Display for NotificationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl LiquidatorError {
    /// Classifies the error by pipeline phase.
    ///
    /// Only `Execution` errors trigger the "Liquidation Failed" Telegram
    /// notification (successful liquidations and swap issues have their own
    /// dedicated notifications sent elsewhere).
    /// `OracleUpdateError` is classified as `Preparation` because oracle price
    /// pushes are best-effort and swallowed before execution; they never
    /// propagate to callers in practice.
    pub const fn phase(&self) -> ErrorPhase {
        match self {
            Self::FetchBorrowStatus(_)
            | Self::PricePairError(_)
            | Self::PriceFetchError(_)
            | Self::ListBorrowPositionsError(_)
            | Self::ListDeploymentsError(_)
            | Self::GetConfigurationError(_)
            | Self::FetchBalanceError(_)
            | Self::NoMarkets => ErrorPhase::Scan,

            Self::AccessKeyDataError(_)
            | Self::SerializeError(_)
            | Self::StrategyError(_)
            | Self::InsufficientBalance
            | Self::OracleUpdateError(_) => ErrorPhase::Preparation,

            Self::LiquidationTransactionError(_)
            | Self::TransactionFailed(_)
            | Self::MissingTransactionHash(_)
            | Self::SwapProviderError(_) => ErrorPhase::Execution,
        }
    }

    /// Classifies the error into a stable dedup bucket for failure notifications.
    ///
    /// `TransactionFailed` is further classified by the contract panic
    /// substring so a "wrong amount" failure and an "offer too low" failure
    /// each fire their own notification once.
    #[must_use]
    pub fn notification_kind(&self) -> NotificationKind {
        match self {
            Self::TransactionFailed(msg) => classify_transaction_failure(msg),
            Self::LiquidationTransactionError(rpc::RpcError::TimeoutError(_, _)) => {
                NotificationKind::TxTimeout
            }
            Self::LiquidationTransactionError(_) | Self::MissingTransactionHash(_) => {
                NotificationKind::TxSubmissionError
            }
            Self::SwapProviderError(_) => NotificationKind::SwapError,
            Self::FetchBorrowStatus(_) => NotificationKind::FetchBorrowStatus,
            Self::PricePairError(_) => NotificationKind::PricePair,
            Self::PriceFetchError(_) => NotificationKind::PriceFetch,
            Self::ListBorrowPositionsError(_) => NotificationKind::ListPositions,
            Self::ListDeploymentsError(_) => NotificationKind::ListDeployments,
            Self::GetConfigurationError(_) => NotificationKind::GetConfiguration,
            Self::FetchBalanceError(_) => NotificationKind::FetchBalance,
            Self::AccessKeyDataError(_) => NotificationKind::AccessKey,
            Self::SerializeError(_) => NotificationKind::Serialize,
            Self::StrategyError(_) => NotificationKind::Strategy,
            Self::InsufficientBalance => NotificationKind::InsufficientBalance,
            Self::OracleUpdateError(_) => NotificationKind::OracleUpdate,
            Self::NoMarkets => NotificationKind::NoMarkets,
        }
    }
}

/// What the round loop does with a position's locally computed status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalScreen {
    /// Apparent liquidation: confirm on-chain and attempt it.
    Candidate,
    /// Healthy: skip without any RPC, clearing failure-dedup state exactly
    /// as an on-chain Healthy answer would.
    SkipHealthy,
    /// Below maintenance but not liquidatable: skip without any RPC.
    SkipMaintenance,
}

/// Forward margin applied to the expiry leg of the local screen, covering
/// wall-clock skew against block time plus a round's duration. A position
/// expiring within the margin screens as a candidate and gets the
/// authoritative on-chain check — so a lagging clock can only cost an extra
/// RPC read, never a missed expiry liquidation.
const EXPIRY_SCREEN_MARGIN_MS: u64 = 15 * 60 * 1000;

/// The timestamp the local screen's expiry check runs against: now plus
/// [`EXPIRY_SCREEN_MARGIN_MS`], saturating.
fn expiry_screen_timestamp(now_ms: u64) -> u64 {
    now_ms.saturating_add(EXPIRY_SCREEN_MARGIN_MS)
}

/// Screens a locally computed [`BorrowStatus`] into a round-loop action.
///
/// The screen only decides which positions pay for an on-chain status read —
/// every apparent liquidation is still confirmed by the market contract
/// inside [`Liquidator::liquidate`] before anything is sized or submitted,
/// so a false positive costs one RPC read and a false negative cannot occur
/// for any status this returns `Candidate` for.
fn screen_status(status: BorrowStatus) -> LocalScreen {
    match status {
        BorrowStatus::Liquidation(_) => LocalScreen::Candidate,
        BorrowStatus::Healthy => LocalScreen::SkipHealthy,
        BorrowStatus::MaintenanceRequired => LocalScreen::SkipMaintenance,
    }
}

/// Maps a contract-level `TransactionFailed` message to a stable kind.
///
/// The match is on substrings of the contract panic so the categorization
/// survives small wording changes and surrounding receipt-id boilerplate.
fn classify_transaction_failure(msg: &str) -> NotificationKind {
    if msg.contains("Attempt to liquidate more collateral") {
        NotificationKind::ExcessiveLiquidation
    } else if msg.contains("Liquidation offer too low") {
        NotificationKind::OfferTooLow
    } else if msg.contains("not eligible for liquidation") {
        NotificationKind::NotEligible
    } else if msg.contains("Failed to calculate value of collateral") {
        NotificationKind::ValueCalcFailure
    } else if msg.contains("Timeout") || msg.contains("timeout") {
        NotificationKind::TxTimeout
    } else {
        NotificationKind::TxFailedOther
    }
}

pub type LiquidatorResult<T = ()> = Result<T, LiquidatorError>;

/// Collateral management strategy
#[derive(Debug, Clone)]
pub enum CollateralStrategy {
    /// Hold collateral as received (default)
    Hold,
    /// Swap collateral back to borrow assets (assets used for liquidations)
    SwapToBorrow,
}

/// Production-grade liquidator with modular architecture.
///
/// This liquidator orchestrates specialized modules:
/// - Scanner: Fetches and evaluates borrow positions
/// - Oracle: Fetches price data
/// - Profitability: Calculates costs and profits
/// - Executor: Executes liquidation transactions
/// - Inventory: Manages asset balances
pub struct Liquidator {
    /// Market scanner for position fetching
    scanner: scanner::MarketScanner,
    /// Oracle fetcher for price data
    oracle_fetcher: oracle::OracleFetcher,
    /// Liquidation executor
    executor: executor::LiquidationExecutor,
    /// Market contract to liquidate positions in
    pub market: AccountId,
    /// Market configuration (cached)
    market_config: MarketConfiguration,
    /// Liquidation strategy
    strategy: Arc<dyn LiquidationStrategy>,
    /// Enable loop liquidation - repeatedly liquidate until position is healthy
    loop_liquidation: bool,
    /// Maximum iterations for loop liquidation (safety limit)
    max_loop_iterations: u32,
    /// Market version (major, minor, patch) - used for version-specific liquidation logic
    market_version: Option<(u32, u32, u32)>,
    /// Shared notifier for Telegram alerts
    notifier: crate::notifier::SharedNotifier,
}

impl Liquidator {
    /// Creates a new liquidator instance.
    ///
    /// # Arguments
    ///
    /// * `client` - JSON-RPC client for blockchain communication
    /// * `signer` - Transaction signer
    /// * `inventory` - Shared inventory manager
    /// * `market` - Market contract account ID
    /// * `market_config` - Market configuration
    /// * `strategy` - Liquidation strategy
    /// * `collateral_strategy` - Collateral management strategy
    /// * `timeout` - Transaction timeout in seconds
    /// * `dry_run` - If true, scan and log without executing liquidations
    /// * `swap_provider` - Optional swap provider for collateral swaps
    /// * `loop_liquidation` - Enable loop liquidation until position is healthy
    /// * `max_loop_iterations` - Maximum iterations for loop liquidation (safety limit)
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: &SigningClient,
        pyth_updates: &oracle::PythUpdatesClient,
        inventory: &inventory::SharedInventory,
        market: AccountId,
        market_config: MarketConfiguration,
        strategy: Arc<dyn LiquidationStrategy>,
        collateral_strategy: CollateralStrategy,
        dry_run: bool,
        swap_provider: Option<crate::swap::SwapProviderImpl>,
        loop_liquidation: bool,
        max_loop_iterations: u32,
        hermes_url: url::Url,
        redstone_api_url: url::Url,
        swap_retry_config: crate::swap::SwapRetryConfig,
        min_swap_value_usd: f64,
        proxy_oracle_cache: Option<oracle::ProxyOracleCache>,
        notifier: crate::notifier::SharedNotifier,
    ) -> Self {
        let scanner = scanner::MarketScanner::new(client.clone(), market.clone());
        let oracle_fetcher = oracle::OracleFetcher::new(
            client.clone(),
            pyth_updates.clone(),
            hermes_url,
            redstone_api_url,
            proxy_oracle_cache,
        );
        let executor = executor::LiquidationExecutor::new(
            client.clone(),
            inventory.clone(),
            market.clone(),
            dry_run,
            collateral_strategy,
            swap_provider,
            swap_retry_config,
            min_swap_value_usd,
            market_config
                .price_oracle_configuration
                .collateral_asset_decimals,
            market_config
                .price_oracle_configuration
                .borrow_asset_decimals,
        );

        Self {
            scanner,
            oracle_fetcher,
            executor,
            market,
            market_config,
            strategy,
            loop_liquidation,
            max_loop_iterations,
            market_version: None,
            notifier,
        }
    }

    /// Get reference to the scanner (for compatibility checks)
    pub fn scanner(&self) -> &scanner::MarketScanner {
        &self.scanner
    }

    /// Get reference to the market configuration
    pub fn market_configuration(&self) -> &MarketConfiguration {
        &self.market_config
    }

    /// Fetches and caches the market version via NEP-330 contract metadata.
    ///
    /// This should be called once after creating the Liquidator to enable version-specific
    /// liquidation logic. The version determines whether to use total collateral (v1.0)
    /// or liquidatable collateral (v1.1+) for liquidation calculations.
    ///
    /// If the market doesn't provide NEP-330 metadata, assumes v1.0 for safety.
    pub async fn fetch_market_version(&mut self) {
        self.market_version = self.scanner.get_market_version().await;
        if let Some((major, minor, patch)) = self.market_version {
            tracing::debug!(
                market = %self.market,
                version = %format!("{major}.{minor}.{patch}"),
                "Fetched market version"
            );
        } else {
            tracing::debug!(
                market = %self.market,
                "Market version unavailable (no NEP-330 metadata), assuming v1.0"
            );
        }
    }

    /// Get formatted asset info for logging (decimals and asset IDs from configuration)
    fn asset_info(&self) -> (i32, String, i32, String) {
        let borrow_decimals = self
            .market_config
            .price_oracle_configuration
            .borrow_asset_decimals;
        let collateral_decimals = self
            .market_config
            .price_oracle_configuration
            .collateral_asset_decimals;
        let borrow_asset_id = self.market_config.borrow_asset.to_string();
        let collateral_asset_id = self.market_config.collateral_asset.to_string();
        (
            borrow_decimals,
            borrow_asset_id,
            collateral_decimals,
            collateral_asset_id,
        )
    }

    /// Gates a liquidation attempt on both oracle conversions succeeding:
    /// no conversions, no attempt. Any fallback here is a wrong-unit number
    /// (raw collateral units, or a decimals-blind gas constant) that would
    /// turn the profitability gate into noise.
    fn require_conversions(
        expected_collateral_value: LiquidatorResult<U128>,
        gas_cost: LiquidatorResult<U128>,
    ) -> Option<(U128, U128)> {
        match (expected_collateral_value, gas_cost) {
            (Ok(value), Ok(gas)) => Some((value, gas)),
            (Err(error), _) => {
                tracing::warn!(%error, "Could not value collateral in borrow-asset units, skipping position");
                None
            }
            (_, Err(error)) => {
                tracing::warn!(%error, "Could not convert gas cost to borrow-asset units, skipping position");
                None
            }
        }
    }

    fn record_price_update_attempt(result: LiquidatorResult<bool>) -> bool {
        match result {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "Failed to update on-chain prices; proceeding and letting the market enforce oracle freshness"
                );
                true
            }
        }
    }

    /// Performs a single liquidation using inventory-based model and modular architecture.
    ///
    /// # Flow
    /// 1. Scanner: Check if position is liquidatable
    /// 2. Strategy: Calculate liquidation amount
    /// 3. Estimate collateral value for profitability check
    /// 4. Profitability: Check if profitable
    /// 5. Executor: Execute liquidation (contract calculates optimal collateral to restore to MCR)
    #[tracing::instrument(skip(self, position, oracle_response), level = "info", fields(
        borrower = %borrow_account,
        market = %self.market
    ))]
    #[allow(clippy::too_many_lines)]
    pub async fn liquidate(
        &self,
        borrow_account: AccountId,
        position: BorrowPosition,
        oracle_response: OracleResponse,
    ) -> Result<LiquidationOutcome, LiquidatorError> {
        // Loop liquidation support - controlled by LOOP_LIQUIDATION parameter
        // In dry run mode, skip looping since position state doesn't change
        // (no actual liquidation happens, so re-checking yields identical results)
        let dry_run = self.executor.is_dry_run();
        let loop_enabled = self.loop_liquidation && !dry_run;
        let mut loop_iteration = 0;
        let max_iterations = if dry_run { 1 } else { self.max_loop_iterations };
        let mut prices_pushed_onchain = false;
        let mut total_liquidated_amount = 0u128;
        let mut total_collateral_received = 0u128;
        let mut position = position;

        loop {
            loop_iteration += 1;

            if loop_enabled && loop_iteration > 1 {
                tracing::debug!(
                    borrower = %borrow_account,
                    iteration = loop_iteration,
                    total_liquidated = total_liquidated_amount,
                    total_collateral = total_collateral_received,
                    "Loop liquidation: checking position again"
                );
            }

            // Step 1: Check liquidation status
            let status = self
                .scanner
                .get_borrow_status(&borrow_account, &oracle_response)
                .await
                .map_err(LiquidatorError::FetchBorrowStatus)?;

            let reason = match status {
                Some(BorrowStatus::Liquidation(r)) => r,
                Some(BorrowStatus::MaintenanceRequired) => {
                    // Position is no longer liquidatable but is still unhealthy
                    // — don't treat as Healthy (it would clear dedup state).
                    tracing::info!(
                        market = %self.market,
                        borrower = %borrow_account,
                        "Position no longer liquidatable but still requires maintenance, skipping"
                    );
                    return Ok(LiquidationOutcome::Skipped);
                }
                Some(BorrowStatus::Healthy) | None => {
                    if loop_iteration > 1 {
                        let (borrow_dec, borrow_asset, coll_dec, coll_asset) = self.asset_info();
                        tracing::info!(
                            market = %self.market,
                            borrower = %borrow_account,
                            iterations = loop_iteration - 1,
                            total_sent = %format::format_amount(total_liquidated_amount, borrow_dec, &borrow_asset),
                            total_received = %format::format_amount(total_collateral_received, coll_dec, &coll_asset),
                            "Loop liquidation completed successfully - position now healthy"
                        );
                    }
                    return Ok(if loop_iteration > 1 {
                        LiquidationOutcome::Liquidated
                    } else {
                        LiquidationOutcome::Healthy
                    });
                }
            };

            // Log position is liquidatable with details
            if loop_iteration == 1 {
                let (borrow_dec, borrow_asset, coll_dec, coll_asset) = self.asset_info();
                let price_pair = self
                    .market_config
                    .price_oracle_configuration
                    .create_price_pair(&oracle_response)?;
                let collateralization_ratio = position.collateralization_ratio(&price_pair);

                tracing::info!(
                    borrower = %borrow_account,
                    reason = ?reason,
                    mcr_liquidation = %self.market_config.borrow_mcr_liquidation,
                    collateralization_ratio = ?collateralization_ratio,
                    total_collateral = %format::format_amount(u128::from(position.collateral_asset_deposit), coll_dec, &coll_asset),
                    total_debt = %format::format_amount(u128::from(position.get_total_borrow_asset_liability()), borrow_dec, &borrow_asset),
                    "Position is liquidatable"
                );
            }

            // If loop liquidation is disabled or strategy doesn't support it, exit after first iteration
            if !loop_enabled && loop_iteration > 1 {
                tracing::info!(
                    borrower = %borrow_account,
                    "Loop liquidation not supported for this strategy, stopping after first liquidation"
                );
                return Ok(LiquidationOutcome::Liquidated);
            }

            // Safety check for max iterations
            if loop_iteration > max_iterations {
                let (borrow_dec, borrow_asset, coll_dec, coll_asset) = self.asset_info();
                tracing::info!(
                    market = %self.market,
                    borrower = %borrow_account,
                    iterations = max_iterations,
                    total_sent = %format::format_amount(total_liquidated_amount, borrow_dec, &borrow_asset),
                    total_received = %format::format_amount(total_collateral_received, coll_dec, &coll_asset),
                    "Loop liquidation stopped - max iterations reached"
                );
                return Ok(LiquidationOutcome::Liquidated);
            }

            // Will log consolidated info after profitability check
            let dry_run_mode = self.executor.is_dry_run();

            // Step 2: Calculate liquidatable collateral
            // This amount determines the maximum collateral that can be liquidated
            // to bring the position to the maintenance collateralization ratio.
            let price_pair = self
                .market_config
                .price_oracle_configuration
                .create_price_pair(&oracle_response)?;
            let liquidatable_collateral = position.liquidatable_collateral(
                &price_pair,
                self.market_config.borrow_mcr_maintenance,
                self.market_config.liquidation_maximum_spread,
            );

            // Step 3: Calculate liquidation amount based on liquidatable collateral
            let available_balance = self
                .executor
                .inventory()
                .read()
                .await
                .get_available_balance(&self.market_config.borrow_asset);

            // Early check: ensure we have at least the contract minimum
            let contract_minimum: u128 = self.market_config.borrow_range.minimum.into();
            if available_balance.0 < contract_minimum {
                let (borrow_dec, borrow_asset, _, _) = self.asset_info();
                tracing::info!(
                    borrower = %borrow_account,
                    available_balance = %format::format_amount(available_balance.0, borrow_dec, &borrow_asset),
                    contract_minimum = %format::format_amount(contract_minimum, borrow_dec, &borrow_asset),
                    "Insufficient inventory: below contract minimum borrow amount, skipping"
                );
                return Ok(LiquidationOutcome::Skipped);
            }

            // v1.0.0 markets: use full position (no partial support)
            // v1.1.0+ markets: adjust position to liquidatable collateral for strategy calculation
            let adjusted_position = if self.market_version == Some((1, 0, 0)) {
                position.clone()
            } else {
                let mut adj = position.clone();
                adj.collateral_asset_deposit = liquidatable_collateral;
                adj
            };

            let (_, _, coll_dec, coll_asset) = self.asset_info();
            tracing::info!(
                borrower = %borrow_account,
                market = %self.market,
                market_version = ?self.market_version,
                liquidatable_collateral = %format::format_amount(liquidatable_collateral.into(), coll_dec, &coll_asset),
                total_collateral = %format::format_amount(position.collateral_asset_deposit.into(), coll_dec, &coll_asset),
                "Using liquidatable collateral for liquidation calculation"
            );

            let Some((liquidation_amount, collateral_amount)) =
                self.strategy.calculate_liquidation_amount(
                    &adjusted_position,
                    &oracle_response,
                    &self.market_config,
                    available_balance,
                    self.market_version,
                )?
            else {
                if loop_iteration > 1 {
                    let (borrow_dec, borrow_asset, _, _) = self.asset_info();
                    tracing::warn!(
                        borrower = %borrow_account,
                        iteration = %format::format_iteration(loop_iteration, max_iterations),
                        available_balance = %format::format_amount(available_balance.0, borrow_dec, &borrow_asset),
                        "Loop liquidation: insufficient balance to continue, stopping"
                    );
                    return Ok(LiquidationOutcome::Liquidated);
                }
                // Strategy already logged the specific reason (insufficient inventory, below minimum, etc.)
                return Ok(LiquidationOutcome::Skipped);
            };

            // Calculate expected value and gas cost for profitability. Both
            // conversions must succeed — see `require_conversions` for why
            // there is deliberately no fallback here.
            let Some((expected_collateral_value, gas_cost)) = Self::require_conversions(
                profitability::ProfitabilityCalculator::convert_collateral_to_borrow_asset(
                    collateral_amount,
                    &oracle_response,
                    &self.market_config,
                ),
                profitability::ProfitabilityCalculator::convert_gas_cost_to_borrow_asset(
                    profitability::ProfitabilityCalculator::DEFAULT_GAS_COST_USD,
                    &oracle_response,
                    &self.market_config,
                ),
            ) else {
                if loop_iteration > 1 {
                    return Ok(LiquidationOutcome::Liquidated);
                }
                return Ok(LiquidationOutcome::Skipped);
            };

            // Calculate what we'd actually get after applying liquidation spread
            // Spread reduces what we receive: value_after_spread = value × (1 - spread)
            let spread = self.market_config.liquidation_maximum_spread;
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let collateral_value_with_spread = {
                let spread_f64 = spread.to_f64_lossy();
                let value_f64 = expected_collateral_value.0 as f64;
                let after_spread = value_f64 * (1.0 - spread_f64);
                after_spread as u128
            };

            // Step 5: Check profitability

            // Calculate detailed profitability metrics
            let (net_profit, _profit_pct) =
                profitability::ProfitabilityCalculator::calculate_profit_metrics(
                    liquidation_amount,
                    expected_collateral_value,
                    gas_cost,
                );

            let theoretical_amount_for_profit =
                U128((liquidation_amount.0 * 10_000) / (10_000 + SAFETY_BUFFER_BPS));

            let is_profitable = self.strategy.should_liquidate(
                theoretical_amount_for_profit,
                expected_collateral_value,
                gas_cost,
            )?;

            // Log consolidated liquidation info with human-readable amounts
            let (borrow_dec, borrow_asset, coll_dec, coll_asset) = self.asset_info();

            if is_profitable {
                // Calculate signed profit for display (can be negative if unprofitable)
                let signed_profit =
                    if expected_collateral_value.0 >= liquidation_amount.0 + gas_cost.0 {
                        i128::try_from(net_profit).unwrap_or(i128::MAX)
                    } else {
                        // Revenue < cost, calculate actual loss
                        let loss = (liquidation_amount.0 + gas_cost.0)
                            .saturating_sub(expected_collateral_value.0);
                        -(i128::try_from(loss).unwrap_or(i128::MAX))
                    };

                let message = if dry_run_mode {
                    "[DRY RUN] Liquidatable position"
                } else {
                    "Liquidatable position"
                };

                // Only show iteration if loop is enabled (for partial/fixed strategies)
                if loop_enabled {
                    tracing::info!(
                        market = %self.market,
                        borrower = %borrow_account,
                        reason = ?reason,
                        iteration = %format::format_iteration(loop_iteration, max_iterations),
                        collateral_total = %format::format_amount(position.collateral_asset_deposit.into(), coll_dec, &coll_asset),
                        collateral_liquidatable = %format::format_amount(liquidatable_collateral.into(), coll_dec, &coll_asset),
                        send = %format::format_amount(liquidation_amount.0, borrow_dec, &borrow_asset),
                        receive = %format::format_amount(collateral_amount.0, coll_dec, &coll_asset),
                        profit = %format::format_profit(signed_profit, liquidation_amount.0, borrow_dec, &borrow_asset),
                        "{}", message
                    );
                } else {
                    tracing::info!(
                        market = %self.market,
                        borrower = %borrow_account,
                        reason = ?reason,
                        collateral_total = %format::format_amount(position.collateral_asset_deposit.into(), coll_dec, &coll_asset),
                        collateral_liquidatable = %format::format_amount(liquidatable_collateral.into(), coll_dec, &coll_asset),
                        send = %format::format_amount(liquidation_amount.0, borrow_dec, &borrow_asset),
                        receive = %format::format_amount(collateral_amount.0, coll_dec, &coll_asset),
                        profit = %format::format_profit(signed_profit, liquidation_amount.0, borrow_dec, &borrow_asset),
                        "{}", message
                    );
                }
            }

            if !is_profitable {
                let (borrow_dec, borrow_asset, coll_dec, coll_asset) = self.asset_info();

                // Calculate actual loss (revenue - cost, will be negative)
                let total_cost = liquidation_amount.0 + gas_cost.0;
                let loss = if expected_collateral_value.0 >= total_cost {
                    i128::try_from(expected_collateral_value.0 - total_cost).unwrap_or(i128::MAX)
                } else {
                    let deficit = total_cost - expected_collateral_value.0;
                    -(i128::try_from(deficit).unwrap_or(i128::MAX))
                };

                // Minimum revenue at the strategy's actual configured margin
                // (not a hardcoded default, which would lie whenever
                // MIN_PROFIT_BPS is set to anything else). Same ceiling
                // arithmetic as the trait's provided `should_liquidate`;
                // saturating here because this value is log-only.
                let min_revenue_required = total_cost
                    .saturating_mul(10_000 + u128::from(self.strategy.min_profit_margin_bps()))
                    .div_ceil(10_000);
                let spread_pct = spread.to_f64_lossy() * 100.0;

                let message = if dry_run_mode {
                    "[DRY RUN] Position not profitable, skipping"
                } else {
                    "Position not profitable, skipping"
                };

                tracing::info!(
                    market = %self.market,
                    borrower = %borrow_account,
                    collateral_total = %format::format_amount(position.collateral_asset_deposit.into(), coll_dec, &coll_asset),
                    collateral_liquidatable = %format::format_amount(liquidatable_collateral.into(), coll_dec, &coll_asset),
                    collateral_requested = %format::format_amount(collateral_amount.0, coll_dec, &coll_asset),
                    send = %format::format_amount(liquidation_amount.0, borrow_dec, &borrow_asset),
                    gas_cost = %format::format_amount(gas_cost.0, borrow_dec, &borrow_asset),
                    total_cost = %format::format_amount(total_cost, borrow_dec, &borrow_asset),
                    receive_value_no_spread = %format::format_amount(expected_collateral_value.0, borrow_dec, &borrow_asset),
                    receive_value_with_spread = %format::format_amount(collateral_value_with_spread, borrow_dec, &borrow_asset),
                    min_revenue_required = %format::format_amount(min_revenue_required, borrow_dec, &borrow_asset),
                    spread = %format!("{:.1}%", spread_pct),
                    loss = %format::format_profit(loss, total_cost, borrow_dec, &borrow_asset),
                    "{}", message
                );

                if loop_iteration > 1 {
                    return Ok(LiquidationOutcome::Liquidated);
                }
                return Ok(LiquidationOutcome::Unprofitable);
            }

            // Reserve BEFORE the paid oracle push: a position that loses an
            // inventory race under POSITION_CONCURRENCY must fail before
            // spending gas. The executor releases the reservation on failure
            // and consumes it on success; dry-run touches no inventory.
            if !dry_run {
                let reserve_result = self.executor.inventory().write().await.reserve(
                    &self.market_config.borrow_asset,
                    templar_common::asset::BorrowAssetAmount::from(liquidation_amount.0),
                );
                if let Err(error) = reserve_result {
                    tracing::info!(
                        borrower = %borrow_account,
                        error = %error,
                        "Inventory no longer covers the sized amount (consumed by a concurrent position), skipping"
                    );
                    if loop_iteration > 1 {
                        return Ok(LiquidationOutcome::Liquidated);
                    }
                    return Ok(LiquidationOutcome::Skipped);
                }
            }

            // Step 6: Push fresh prices to underlying Pyth oracle(s) before first execution.
            // The market contract reads from the on-chain oracle during liquidation,
            // so prices must be fresh there — not just in our HTTP-fetched view.
            // Resolves proxy/LST oracles to their underlying Pyth targets.
            // Only push once per liquidate() call (covers loop iterations too).
            if !prices_pushed_onchain && !dry_run {
                let oracle_account = &self.market_config.price_oracle_configuration.account_id;
                let price_ids = &[
                    self.market_config
                        .price_oracle_configuration
                        .borrow_asset_price_id,
                    self.market_config
                        .price_oracle_configuration
                        .collateral_asset_price_id,
                ];
                prices_pushed_onchain = Self::record_price_update_attempt(
                    self.oracle_fetcher
                        .update_onchain_prices(oracle_account, price_ids)
                        .await,
                );
            }

            // Execute liquidation (contract determines optimal collateral amount)
            let (outcome, swap_issue) = self
                .executor
                .execute_liquidation(
                    &borrow_account,
                    &self.market_config.borrow_asset,
                    &self.market_config.collateral_asset,
                    templar_common::asset::BorrowAssetAmount::from(liquidation_amount.0),
                    templar_common::asset::CollateralAssetAmount::from(collateral_amount.0),
                    templar_common::asset::BorrowAssetAmount::from(expected_collateral_value.0),
                )
                .await?;

            // Notify about successful liquidation (sent first, before any swap issues)
            if outcome == LiquidationOutcome::Liquidated {
                let (borrow_dec, borrow_asset, coll_dec, coll_asset) = self.asset_info();
                let total_cost = liquidation_amount.0 + gas_cost.0;
                let signed_profit = if expected_collateral_value.0 >= total_cost {
                    i128::try_from(expected_collateral_value.0 - total_cost).unwrap_or(i128::MAX)
                } else {
                    -(i128::try_from(total_cost - expected_collateral_value.0).unwrap_or(i128::MAX))
                };
                self.notifier.notify_liquidation(
                    self.market.as_ref(),
                    borrow_account.as_ref(),
                    &format::format_amount_short(liquidation_amount.0, borrow_dec, &borrow_asset),
                    &format::format_amount_short(collateral_amount.0, coll_dec, &coll_asset),
                    &format::format_profit_short(
                        signed_profit,
                        liquidation_amount.0,
                        borrow_dec,
                        &borrow_asset,
                    ),
                    None,
                    dry_run,
                );
            }

            // Notify about swap issues (sent after liquidation notification)
            if let Some(issue) = swap_issue {
                match issue {
                    executor::SwapIssue::Unsupported { from, to, amount } => {
                        self.notifier.notify_swap_unsupported(
                            self.market.as_ref(),
                            &from,
                            &to,
                            &amount,
                        );
                    }
                    executor::SwapIssue::Failed {
                        from,
                        to,
                        amount,
                        error,
                    } => {
                        self.notifier.notify_swap_failed(
                            self.market.as_ref(),
                            &from,
                            &to,
                            &amount,
                            &error,
                        );
                    }
                }
            }

            // Track cumulative amounts
            total_liquidated_amount += liquidation_amount.0;
            total_collateral_received += collateral_amount.0;

            tracing::debug!(
                borrower = %borrow_account,
                iteration = loop_iteration,
                liquidation_amount = %liquidation_amount.0,
                collateral_received = %collateral_amount.0,
                cumulative_liquidated = total_liquidated_amount,
                cumulative_collateral = total_collateral_received,
                "Liquidation iteration completed"
            );

            // If loop liquidation is disabled or strategy doesn't support it, return after first liquidation
            if !loop_enabled {
                return Ok(outcome);
            }

            // Re-fetch position data before next iteration so we have current
            // collateral/debt amounts (the status check at the top of the loop
            // only checks liquidation eligibility, not position amounts).
            match self.scanner.get_borrow_position(&borrow_account).await {
                Ok(Some(updated)) => position = updated,
                Ok(None) => {
                    tracing::info!(
                        borrower = %borrow_account,
                        "Position no longer exists after liquidation, stopping loop"
                    );
                    return Ok(LiquidationOutcome::Liquidated);
                }
                Err(e) => {
                    tracing::warn!(
                        borrower = %borrow_account,
                        error = ?e,
                        "Failed to re-fetch position, stopping loop"
                    );
                    return Ok(LiquidationOutcome::Liquidated);
                }
            }
        }
    }

    /// Runs liquidations for all eligible positions in the market.
    ///
    /// `concurrency` bounds how many positions are evaluated/liquidated in
    /// flight at once. At `1` (the default), positions are processed
    /// sequentially with a 1-second pause between them — the pacing free
    /// public RPC endpoints tolerate. Above `1` the pause is dropped and
    /// evaluation fans out; inventory reservations serialize the actual
    /// capital commitment, so concurrent positions cannot double-spend the
    /// same balance, but each in-flight position costs several RPC reads and
    /// (in live mode) possibly its own oracle push.
    #[tracing::instrument(skip(self, concurrency), level = "info", fields(market = %self.market))]
    #[allow(clippy::too_many_lines)]
    pub async fn run_liquidations(&self, concurrency: usize) -> LiquidatorResult<RoundSummary> {
        let max_percentage = self.strategy.max_liquidation_percentage();

        tracing::info!(
            strategy = %self.strategy.strategy_name(),
            percentage = max_percentage,
            "Starting liquidation run"
        );

        let oracle_account = self
            .market_config
            .price_oracle_configuration
            .account_id
            .clone();
        let price_ids = [
            self.market_config
                .price_oracle_configuration
                .borrow_asset_price_id,
            self.market_config
                .price_oracle_configuration
                .collateral_asset_price_id,
        ];
        let price_max_age = self
            .market_config
            .price_oracle_configuration
            .price_maximum_age_s;

        // Fetch oracle prices via HTTP APIs (Hermes for Pyth, gateway for RedStone)
        let oracle_response = self
            .oracle_fetcher
            .get_oracle_prices(oracle_account.clone(), &price_ids, price_max_age)
            .await?;

        // The scan needs the full feed pair; a partial response would reach
        // every position's status check and fail each with "Missing price"
        // instead of skipping the market once. This is an error, not a clean
        // skip: it must count as a failed market scan so a stale-oracle
        // outage surfaces through the failure counters, the consecutive-
        // failure alerts, and /healthz instead of reading as a healthy round
        // that silently liquidated nothing.
        if !crate::oracle::covers_all(&oracle_response, &price_ids) {
            let missing: Vec<String> = price_ids
                .iter()
                .filter(|id| !matches!(oracle_response.get(*id), Some(Some(_))))
                .map(|id| hex::encode(id.0))
                .collect();
            return Err(LiquidatorError::PriceFetchError(
                rpc::RpcError::WrongResponseKind(format!(
                    "oracle prices missing or stale for feed(s) {missing:?}"
                )),
            ));
        }

        // Scan for positions
        let borrows = self.scanner.get_all_borrows().await?;
        if borrows.is_empty() {
            tracing::info!("No borrow positions found");
            return Ok(RoundSummary::default());
        }

        // Process positions
        let mut liquidated = 0u64;
        let mut not_liquidatable = 0u64;
        let mut unprofitable = 0u64;
        let mut failed = 0u64;
        // Subset of `failed` where a transaction was actually submitted
        // (execution phase) rather than failing before submission.
        let mut failed_execution = 0u64;
        let position_count = borrows.len();

        // Screen positions locally before paying for any per-position RPC:
        // status is computed with the same `MarketConfiguration::borrow_status`
        // the contract runs, from data already in hand (positions, prices,
        // config). Only apparent candidates get the authoritative on-chain
        // check inside `liquidate()`. Wall-clock now stands in for block time,
        // with `EXPIRY_SCREEN_MARGIN_MS` of forward slack on the expiry leg
        // so a lagging clock routes near-expiry positions on-chain instead of
        // skipping them. If the price pair can't be built locally, every
        // position is confirmed on-chain instead.
        // Wall-clock now feeds the expiry leg of the screen; if it can't be
        // computed, skip screening entirely (fail open toward candidacy)
        // rather than screen against a bogus timestamp.
        #[allow(clippy::cast_possible_truncation)]
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_millis() as u64);
        let price_pair = self
            .market_config
            .price_oracle_configuration
            .create_price_pair(&oracle_response);
        let candidates: Vec<(AccountId, BorrowPosition)> = match (price_pair, now_ms) {
            (Ok(price_pair), Some(now_ms)) => borrows
                .into_iter()
                .filter(|(account, position)| {
                    let status = self.market_config.borrow_status(
                        position.collateralization_ratio(&price_pair),
                        position.started_at_block_timestamp_ms,
                        expiry_screen_timestamp(now_ms),
                    );
                    match screen_status(status) {
                        LocalScreen::Candidate => true,
                        LocalScreen::SkipHealthy => {
                            self.notifier
                                .clear_failure_dedup_for(self.market.as_ref(), account.as_ref());
                            not_liquidatable += 1;
                            false
                        }
                        LocalScreen::SkipMaintenance => {
                            not_liquidatable += 1;
                            false
                        }
                    }
                })
                .collect(),
            (Err(error), _) => {
                tracing::warn!(
                    %error,
                    "Could not build price pair for local screening; confirming every position on-chain"
                );
                borrows.into_iter().collect()
            }
            (Ok(_), None) => {
                tracing::warn!(
                    "System clock predates the epoch; confirming every position on-chain"
                );
                borrows.into_iter().collect()
            }
        };

        tracing::info!(
            positions = position_count,
            candidates = candidates.len(),
            "Screened positions locally"
        );

        let total = candidates.len();
        let concurrency = concurrency.max(1);

        use futures::StreamExt as _;
        let mut results = futures::stream::iter(candidates.into_iter().enumerate().map(
            |(i, (account, position))| {
                let oracle_response = oracle_response.clone();
                async move {
                    let result = self
                        .liquidate(account.clone(), position, oracle_response)
                        .await;
                    // Sequential mode paces positions 1s apart (skipped after
                    // the last one); concurrent mode drops the pause — the
                    // operator raising the knob brings an RPC endpoint sized
                    // for the load.
                    if concurrency == 1 && i < total - 1 {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                    (account, result)
                }
            },
        ))
        .buffer_unordered(concurrency);

        while let Some((account, result)) = results.next().await {
            match result {
                Ok(LiquidationOutcome::Liquidated) => {
                    self.notifier
                        .clear_failure_dedup_for(self.market.as_ref(), account.as_ref());
                    liquidated += 1;
                }
                Ok(LiquidationOutcome::Healthy) => {
                    self.notifier
                        .clear_failure_dedup_for(self.market.as_ref(), account.as_ref());
                    not_liquidatable += 1;
                }
                Ok(LiquidationOutcome::Skipped) => not_liquidatable += 1,
                Ok(LiquidationOutcome::Unprofitable) => unprofitable += 1,
                Err(e) => {
                    let phase = e.phase();
                    if phase == ErrorPhase::Execution {
                        failed_execution += 1;
                        tracing::error!(borrower = %account, phase = %phase, error = %e, "Liquidation failed");
                        self.notifier.notify_liquidation_failed(
                            self.market.as_ref(),
                            account.as_ref(),
                            e.notification_kind(),
                            &e.to_string(),
                        );
                    } else {
                        tracing::warn!(borrower = %account, phase = %phase, error = %e, "Skipped position");
                    }
                    failed += 1;
                }
            }
        }

        tracing::info!(
            liquidated,
            not_liquidatable,
            unprofitable,
            failed,
            "Liquidation run completed"
        );

        // `attempted`/`succeeded`/`failed` are keyed off `phase() ==
        // ErrorPhase::Execution`, not off which call inside `liquidate()`
        // raised the error — preparation-phase errors (serialization,
        // strategy failures) must not count as submitted transactions, and
        // only the `phase()` check makes that claim.
        // `unprofitable` positions reached profitability
        // evaluation but never submission, so they count toward `candidates`
        // but not `attempted`. Scan/preparation-phase failures (`failed` minus
        // `failed_execution`) are excluded from `candidates` since it isn't
        // known whether those positions were actually liquidatable.
        // `markets_scanned_ok`/`markets_failed` are round-level bookkeeping
        // the caller (`LiquidatorService::run_liquidation_round`) fills in
        // from the Ok/Err of this whole call — a single market's summary
        // has nothing to report there.
        let round_summary = RoundSummary {
            candidates: liquidated + unprofitable + failed_execution,
            attempted: liquidated + failed_execution,
            succeeded: liquidated,
            failed: failed_execution,
            markets_scanned_ok: 0,
            markets_failed: 0,
        };

        Ok(round_summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_phase_scan() {
        let err = LiquidatorError::FetchBorrowStatus(rpc::RpcError::TimeoutError(30, 30));
        assert_eq!(err.phase(), ErrorPhase::Scan);
    }

    #[test]
    fn test_error_phase_preparation() {
        let err = LiquidatorError::InsufficientBalance;
        assert_eq!(err.phase(), ErrorPhase::Preparation);
    }

    #[test]
    fn test_error_phase_execution() {
        let err = LiquidatorError::TransactionFailed("receipt failed".to_string());
        assert_eq!(err.phase(), ErrorPhase::Execution);
    }

    #[test]
    fn test_error_phase_display() {
        assert_eq!(ErrorPhase::Scan.to_string(), "scan");
        assert_eq!(ErrorPhase::Preparation.to_string(), "preparation");
        assert_eq!(ErrorPhase::Execution.to_string(), "execution");
    }

    #[test]
    fn test_notification_kind_excessive_liquidation() {
        let msg = r#"Receipt 6wy7eW4sLeVAApXmmsyaseK48yfGpJRVrt5etZrsRByp failed: ExecutionError("Smart contract panicked: Attempt to liquidate more collateral than is currently eligible: 37818981 requested > 34516659 available")"#;
        let err = LiquidatorError::TransactionFailed(msg.to_string());
        assert_eq!(
            err.notification_kind(),
            NotificationKind::ExcessiveLiquidation
        );
    }

    #[test]
    fn test_notification_kind_offer_too_low() {
        let err = LiquidatorError::TransactionFailed(
            "Smart contract panicked: Liquidation offer too low: 99 offered < 100".to_string(),
        );
        assert_eq!(err.notification_kind(), NotificationKind::OfferTooLow);
    }

    #[test]
    fn test_notification_kind_not_eligible() {
        let err = LiquidatorError::TransactionFailed(
            "Borrow position is not eligible for liquidation".to_string(),
        );
        assert_eq!(err.notification_kind(), NotificationKind::NotEligible);
    }

    #[test]
    fn test_notification_kind_value_calc_failure() {
        let err = LiquidatorError::TransactionFailed(
            "Smart contract panicked: Failed to calculate value of collateral".to_string(),
        );
        assert_eq!(err.notification_kind(), NotificationKind::ValueCalcFailure);
    }

    #[test]
    fn test_notification_kind_tx_failed_other() {
        let err = LiquidatorError::TransactionFailed("some new failure mode".to_string());
        assert_eq!(err.notification_kind(), NotificationKind::TxFailedOther);
    }

    #[test]
    fn test_notification_kind_tx_submission_timeout() {
        let err = LiquidatorError::LiquidationTransactionError(rpc::RpcError::TimeoutError(30, 30));
        assert_eq!(err.notification_kind(), NotificationKind::TxTimeout);
    }

    #[test]
    fn test_notification_kind_tx_submission_non_timeout() {
        let err = LiquidatorError::LiquidationTransactionError(rpc::RpcError::WrongResponseKind(
            "boom".to_string(),
        ));
        assert_eq!(err.notification_kind(), NotificationKind::TxSubmissionError);
    }

    #[test]
    fn test_notification_kind_non_tx_variants_stable() {
        assert_eq!(
            LiquidatorError::InsufficientBalance.notification_kind(),
            NotificationKind::InsufficientBalance,
        );
        assert_eq!(
            LiquidatorError::FetchBorrowStatus(rpc::RpcError::TimeoutError(30, 30))
                .notification_kind(),
            NotificationKind::FetchBorrowStatus,
        );
    }

    #[test]
    fn test_notification_kind_as_str_stable() {
        // Lock the string representation so dedup state from previous deployments
        // remains valid across rolling restarts.
        assert_eq!(
            NotificationKind::ExcessiveLiquidation.as_str(),
            "excessive_liquidation"
        );
        assert_eq!(NotificationKind::TxTimeout.as_str(), "tx_timeout");
        assert_eq!(
            NotificationKind::InsufficientBalance.as_str(),
            "insufficient_balance"
        );
    }

    /// The local screen decides which positions pay for an on-chain status
    /// read. Any liquidation status — for any reason — must be a candidate
    /// (the on-chain check stays authoritative), healthy positions must skip
    /// *and* clear failure-dedup state (mirroring the on-chain Healthy
    /// handling), and maintenance-required positions skip without clearing.
    #[test]
    fn local_screen_maps_every_status() {
        use templar_common::borrow::LiquidationReason;

        assert_eq!(
            screen_status(BorrowStatus::Liquidation(
                LiquidationReason::Undercollateralization
            )),
            LocalScreen::Candidate,
        );
        assert_eq!(
            screen_status(BorrowStatus::Liquidation(LiquidationReason::Expiration)),
            LocalScreen::Candidate,
        );
        assert_eq!(
            screen_status(BorrowStatus::Healthy),
            LocalScreen::SkipHealthy
        );
        assert_eq!(
            screen_status(BorrowStatus::MaintenanceRequired),
            LocalScreen::SkipMaintenance,
        );
    }

    /// The expiry leg of the screen must look *ahead* by the margin: a
    /// position expiring within it is a candidate (the on-chain check
    /// decides), so a wall clock lagging block time by up to the margin can
    /// only cost an extra RPC read, never a missed expiry liquidation.
    #[test]
    fn expiry_screen_timestamp_adds_the_forward_margin() {
        assert_eq!(expiry_screen_timestamp(0), EXPIRY_SCREEN_MARGIN_MS);
        assert_eq!(
            expiry_screen_timestamp(1_000),
            1_000 + EXPIRY_SCREEN_MARGIN_MS
        );
        assert_eq!(expiry_screen_timestamp(u64::MAX), u64::MAX);
    }

    #[test]
    fn require_conversions_fails_closed_when_either_conversion_errors() {
        // Fail-closed: no conversions, no liquidation attempt. Any fallback
        // would be a wrong-unit number fed into the profitability gate.
        let err = || {
            Err(LiquidatorError::StrategyError(
                "price not found in oracle".to_string(),
            ))
        };

        assert_eq!(
            Liquidator::require_conversions(Ok(U128(100)), Ok(U128(5))),
            Some((U128(100), U128(5)))
        );
        assert_eq!(Liquidator::require_conversions(err(), Ok(U128(5))), None);
        assert_eq!(Liquidator::require_conversions(Ok(U128(100)), err()), None);
        assert_eq!(Liquidator::require_conversions(err(), err()), None);
    }

    #[test]
    fn price_update_failure_is_non_blocking_for_liquidation_attempt() {
        assert!(Liquidator::record_price_update_attempt(Err(
            LiquidatorError::OracleUpdateError("transient update failure".to_string())
        )));
    }
}
