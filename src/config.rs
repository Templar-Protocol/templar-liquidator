//! Configuration management for the liquidator bot.
//!
//! This module handles CLI argument parsing and service configuration creation.

use std::{
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
};

use clap::Parser;
use near_sdk::AccountId;
use templar_gateway_client::Network;
use url::Url;

use crate::{
    notifier::{Notifier, TelegramConfig},
    service::ServiceConfig,
    swap::SwapRetryConfig,
    CollateralStrategy,
};

/// Parse a string into `Option<i64>`, treating empty/whitespace as `None`.
/// Panics if the value is non-empty and not a valid integer.
fn parse_optional_i64(s: &str) -> Option<i64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(
        trimmed
            .parse::<i64>()
            .unwrap_or_else(|_| panic!("TELEGRAM_THREAD_ID '{trimmed}' is not a valid integer")),
    )
}

/// Execution mode for the service.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum RunMode {
    /// Continuous operation: periodic registry refreshes and liquidation rounds.
    #[default]
    Loop,
    /// One registry refresh + one liquidation round, then exit.
    /// For cron / Cloud Run Jobs / K8s CronJob deployments.
    Once,
}

fn validate_percentage(s: &str) -> Result<u8, String> {
    let value: u8 = s
        .parse()
        .map_err(|_| format!("'{s}' is not a valid number"))?;
    if value == 0 || value > 100 {
        return Err(format!(
            "Partial percentage must be between 1 and 100, got {value}"
        ));
    }
    Ok(value)
}

/// Rejects zero at the config boundary: a `0`-attempt retry loop never runs
/// the operation at all and returns a confusing "Retry loop exhausted"
/// error instead of the underlying failure. `1` means "try once, no
/// retries" and is the correct way to disable retrying.
fn validate_retry_attempts(s: &str) -> Result<u32, String> {
    let value: u32 = s
        .parse()
        .map_err(|_| format!("'{s}' is not a valid number"))?;
    if value == 0 {
        return Err(
            "Swap retry attempts must be at least 1 (0 would never attempt the swap); use 1 to disable retrying".to_string(),
        );
    }
    Ok(value)
}

/// Collateral handling strategy after a liquidation, as accepted on the CLI /
/// env var. Maps 1:1 to the domain [`CollateralStrategy`] in
/// `Args::parse_collateral_strategy` (private, so not linked).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum CollateralStrategyArg {
    /// Keep collateral as received (no swaps back to a borrow asset).
    Hold,
    /// Swap collateral back into the market's borrow asset.
    SwapToBorrow,
}

/// Command-line arguments for the liquidator bot.
///
/// `Debug` is implemented by hand rather than derived: this struct holds the
/// signer key (as a string — see the field doc below) plus three API
/// tokens, and a derived `Debug` would print all four in full the first time
/// anything formatted `Args` (a `tracing::debug!(?args)`, a panic payload,
/// etc). See the impl below for the redaction and [`ServiceConfig`]'s own
/// hand-written `Debug` for the same pattern applied to the built config.
#[derive(Clone, Parser)]
#[command(name = "templar-liquidator")]
#[command(about = "Inventory-based liquidator bot for Templar Protocol")]
#[allow(clippy::struct_excessive_bools)]
pub struct Args {
    /// Market registries to run liquidations for
    #[arg(short, long, env = "REGISTRY_ACCOUNT_IDS")]
    pub registries: Vec<AccountId>,

    /// Signer key to use for signing transactions, as `ed25519:<base58>`
    ///
    /// Held as a string and parsed in [`Args::build_config`] rather than by
    /// clap: clap renders the offending value in its `invalid value '…'`
    /// error, and a mistyped key is still near-complete key material that
    /// would land in stderr and from there into logs.
    #[arg(short = 'k', long, env = "SIGNER_KEY")]
    pub signer_key: String,

    /// Signer account ID
    #[arg(short, long, env = "SIGNER_ACCOUNT_ID")]
    pub signer_account: AccountId,

    /// Network to run liquidations on
    #[arg(short, long, env = "NEAR_NETWORK", default_value_t = Network::Testnet)]
    pub network: Network,

    /// Custom RPC URL (overrides default network RPC).
    #[arg(long, env = "NEAR_RPC_URL")]
    pub near_rpc_url: Option<String>,

    /// API key for the RPC endpoint, sent as an `Authorization` header. May also
    /// be supplied as an `apiKey` query parameter on `--near-rpc-url`.
    #[arg(long, env = "NEAR_RPC_API_KEY")]
    pub near_rpc_api_key: Option<String>,

    /// Transaction timeout in seconds
    #[arg(long, env = "TRANSACTION_TIMEOUT", default_value_t = 60)]
    pub transaction_timeout: u64,

    /// Interval between liquidation scans in seconds
    #[arg(long, env = "LIQUIDATION_SCAN_INTERVAL", default_value_t = 600)]
    pub liquidation_scan_interval: u64,

    /// Registry refresh interval in seconds
    #[arg(long, env = "REGISTRY_REFRESH_INTERVAL", default_value_t = 3600)]
    pub registry_refresh_interval: u64,

    /// Execution mode: `loop` runs continuously, `once` performs a single
    /// scan-and-liquidate cycle and exits
    #[arg(long, env = "RUN_MODE", value_enum, default_value_t = RunMode::Loop)]
    pub run_mode: RunMode,

    /// Shorthand for `--run-mode once`. Forces once mode; takes precedence
    /// over `--run-mode`.
    #[arg(long, default_value_t = false)]
    pub once: bool,

    /// Concurrency for registry deployment listing
    #[arg(short, long, env = "CONCURRENCY", default_value_t = 10)]
    pub concurrency: usize,

    /// Maximum positions evaluated/liquidated concurrently within one market's
    /// round. Defaults to 1: fully sequential with a 1-second pause between
    /// positions, which is what free public RPC endpoints tolerate. Raising it
    /// drops the pause and fans position evaluation out — the scale lever for
    /// operators with a dedicated RPC endpoint; expect each in-flight position
    /// to cost several RPC reads (and in live mode, possibly an oracle push).
    #[arg(long, env = "POSITION_CONCURRENCY", default_value_t = 1)]
    pub position_concurrency: usize,

    /// Percentage of available borrow-asset inventory to deploy per liquidation (1-100)
    /// If not set and --fixed-liquidation-amount-usd is also not set, defaults to 100%
    /// Mutually exclusive with --fixed-liquidation-amount-usd
    #[arg(long, env = "PARTIAL_LIQUIDATION_PERCENTAGE", value_parser = validate_percentage)]
    pub partial_percentage: Option<u8>,

    /// Fixed liquidation amount in USD
    /// Example: 100.0 for $100 USD (works across all USD-based markets with any decimals)
    /// Only supports USD-based borrow assets (USDC, USDT, DAI, etc.)
    /// Mutually exclusive with --partial-percentage
    #[arg(
        long,
        env = "FIXED_LIQUIDATION_AMOUNT_USD",
        conflicts_with = "partial_percentage"
    )]
    pub fixed_liquidation_amount_usd: Option<f64>,

    /// Minimum profit margin in basis points
    #[arg(long, env = "MIN_PROFIT_BPS", default_value_t = 50)]
    pub min_profit_bps: u32,

    /// Dry run mode - scan and log without executing transactions.
    /// Defaults to `true`: a public bot that moves money must simulate
    /// unless the operator explicitly opts into live mode with `DRY_RUN=false`.
    /// Accepts an optional value so live mode is reachable from argv-only
    /// surfaces (compose `command:` arrays, Cloud Run Job args): bare
    /// `--dry-run` means true, `--dry-run=false` / `--dry-run false` opt out.
    #[arg(
        long,
        env = "DRY_RUN",
        action = clap::ArgAction::Set,
        num_args = 0..=1,
        default_value_t = true,
        default_missing_value = "true"
    )]
    pub dry_run: bool,

    /// Collateral strategy: "hold" or "swap-to-borrow"
    #[arg(
        long,
        env = "COLLATERAL_STRATEGY",
        value_enum,
        default_value_t = CollateralStrategyArg::Hold
    )]
    pub collateral_strategy: CollateralStrategyArg,

    /// `OneClick` API token for swap authentication
    #[arg(long, env = "ONECLICK_API_TOKEN")]
    pub oneclick_api_token: Option<String>,

    /// Ref Finance contract address
    #[arg(long, env = "REF_CONTRACT")]
    pub ref_contract: Option<String>,

    /// Collateral asset allowlist for market filtering
    #[arg(long, env = "ALLOWED_COLLATERAL_ASSETS", value_delimiter = ',')]
    pub allowed_collateral_assets: Vec<String>,

    /// Collateral assets to ignore in market filtering
    #[arg(long, env = "IGNORED_COLLATERAL_ASSETS", value_delimiter = ',')]
    pub ignored_collateral_assets: Vec<String>,

    /// Market account IDs to ignore (comma-separated)
    #[arg(long, env = "IGNORED_MARKETS", value_delimiter = ',')]
    pub ignored_markets: Vec<String>,

    /// Enable loop liquidation - repeatedly liquidate until position is healthy
    #[arg(long, env = "LOOP_LIQUIDATION", default_value_t = false)]
    pub loop_liquidation: bool,

    /// Maximum iterations for loop liquidation (safety limit)
    #[arg(long, env = "MAX_LOOP_ITERATIONS", default_value_t = 10)]
    pub max_loop_iterations: u32,

    /// Pyth Hermes API URL for fetching price data. Defaults to Pyth's endpoint for
    /// `--network`.
    #[arg(long, env = "PYTH_HERMES_URL")]
    pub hermes_url: Option<Url>,

    /// RedStone gateway URL for fetching fresh prices
    #[arg(
        long,
        env = "REDSTONE_GATEWAY_URL",
        default_value = "https://oracle-gateway-1.a.redstone.vip"
    )]
    pub redstone_gateway_url: String,

    /// Minimum USD value to attempt a swap (JIT or batch).
    /// Amounts below this threshold are skipped and left for batch swap.
    #[arg(long, env = "MIN_SWAP_VALUE_USD", default_value_t = 10.0)]
    pub min_swap_value_usd: f64,

    /// Enable batch swap of accumulated collateral at the start of each liquidation round.
    #[arg(long, env = "BATCH_SWAP_ON_CYCLE_START", default_value_t = true)]
    pub batch_swap_on_cycle_start: bool,

    /// Maximum retry attempts for transient swap errors (at least 1)
    #[arg(
        long,
        env = "SWAP_RETRY_ATTEMPTS",
        default_value_t = 3,
        value_parser = validate_retry_attempts
    )]
    pub swap_retry_attempts: u32,

    /// Base delay in milliseconds for swap retry exponential backoff (2s, 4s, 8s …)
    #[arg(long, env = "SWAP_RETRY_BASE_DELAY_MS", default_value_t = 2000)]
    pub swap_retry_base_delay_ms: u64,

    /// Number of consecutive scan failures before sending a Telegram alert.
    /// Set to 0 to disable scan failure notifications.
    #[arg(long, env = "SCAN_FAILURE_NOTIFY_THRESHOLD", default_value_t = 2)]
    pub scan_failure_notify_threshold: u32,

    /// Cooldown in hours for repeated "Liquidation Failed" notifications with
    /// the same (market, borrower, error class). Successful liquidations
    /// reset the cooldown for that borrower immediately.
    #[arg(
        long,
        env = "FAILURE_NOTIFICATION_COOLDOWN_HOURS",
        default_value_t = crate::notifier::DEFAULT_FAILURE_NOTIFY_COOLDOWN_HOURS
    )]
    pub failure_notification_cooldown_hours: u64,

    /// Telegram bot token for notifications (omit to disable). Must be set
    /// together with --telegram-chat-id — one without the other is rejected
    /// at parse time.
    #[arg(long, env = "TELEGRAM_BOT_TOKEN", requires = "telegram_chat_id")]
    pub telegram_bot_token: Option<String>,

    /// Telegram chat/channel ID for notifications. Must be set together
    /// with --telegram-bot-token — one without the other is rejected at
    /// parse time.
    #[arg(long, env = "TELEGRAM_CHAT_ID", requires = "telegram_bot_token")]
    pub telegram_chat_id: Option<String>,

    /// Telegram thread/topic ID for sending to specific threads in supergroups.
    /// Accepts an empty env var gracefully (treated as unset).
    #[arg(long, env = "TELEGRAM_THREAD_ID", default_value = "")]
    pub telegram_thread_id: String,

    /// Port for the optional `/healthz` + `/metrics` HTTP endpoint (disabled when unset)
    #[arg(long, env = "HTTP_PORT")]
    pub http_port: Option<u16>,

    /// Bind address for the optional `/healthz` + `/metrics` HTTP endpoint.
    /// Defaults to loopback: the endpoints expose no secrets, but binding
    /// every interface makes "don't publish this" depend entirely on an
    /// external firewall or port mapping rather than the bot itself. Set to
    /// `0.0.0.0` explicitly to publish it (e.g. for a container platform's
    /// own health-check network).
    #[arg(long, env = "HTTP_BIND_ADDR", default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
    pub http_bind_addr: IpAddr,
}

impl std::fmt::Debug for Args {
    /// Redacts every field that carries credentials: the signer key and the
    /// three token fields (`near_rpc_api_key`, `oneclick_api_token`,
    /// `telegram_bot_token`). Presence is still reported for the optional
    /// ones so a dump remains useful for diagnosing configuration, without
    /// printing the values. Mirrors [`ServiceConfig`]'s hand-written `Debug`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        /// Reports whether an optional secret was supplied, never its value.
        fn shown(value: Option<&impl std::fmt::Debug>) -> &'static str {
            if value.is_some() {
                "<set>"
            } else {
                "<unset>"
            }
        }

        f.debug_struct("Args")
            .field("registries", &self.registries)
            .field("signer_key", &"<redacted>")
            .field("signer_account", &self.signer_account)
            .field("network", &self.network)
            .field("near_rpc_url", &self.near_rpc_url)
            .field("near_rpc_api_key", &shown(self.near_rpc_api_key.as_ref()))
            .field("transaction_timeout", &self.transaction_timeout)
            .field("liquidation_scan_interval", &self.liquidation_scan_interval)
            .field("registry_refresh_interval", &self.registry_refresh_interval)
            .field("run_mode", &self.run_mode)
            .field("once", &self.once)
            .field("concurrency", &self.concurrency)
            .field("position_concurrency", &self.position_concurrency)
            .field("partial_percentage", &self.partial_percentage)
            .field(
                "fixed_liquidation_amount_usd",
                &self.fixed_liquidation_amount_usd,
            )
            .field("min_profit_bps", &self.min_profit_bps)
            .field("dry_run", &self.dry_run)
            .field("collateral_strategy", &self.collateral_strategy)
            .field(
                "oneclick_api_token",
                &shown(self.oneclick_api_token.as_ref()),
            )
            .field("ref_contract", &self.ref_contract)
            .field("allowed_collateral_assets", &self.allowed_collateral_assets)
            .field("ignored_collateral_assets", &self.ignored_collateral_assets)
            .field("ignored_markets", &self.ignored_markets)
            .field("loop_liquidation", &self.loop_liquidation)
            .field("max_loop_iterations", &self.max_loop_iterations)
            .field("hermes_url", &self.hermes_url)
            .field("redstone_gateway_url", &self.redstone_gateway_url)
            .field("min_swap_value_usd", &self.min_swap_value_usd)
            .field("batch_swap_on_cycle_start", &self.batch_swap_on_cycle_start)
            .field("swap_retry_attempts", &self.swap_retry_attempts)
            .field("swap_retry_base_delay_ms", &self.swap_retry_base_delay_ms)
            .field(
                "scan_failure_notify_threshold",
                &self.scan_failure_notify_threshold,
            )
            .field(
                "failure_notification_cooldown_hours",
                &self.failure_notification_cooldown_hours,
            )
            .field(
                "telegram_bot_token",
                &shown(self.telegram_bot_token.as_ref()),
            )
            .field("telegram_chat_id", &self.telegram_chat_id)
            .field("telegram_thread_id", &self.telegram_thread_id)
            .field("http_port", &self.http_port)
            .field("http_bind_addr", &self.http_bind_addr)
            .finish()
    }
}

impl Args {
    /// Parse command-line arguments
    pub fn parse_args() -> Self {
        Self::parse()
    }

    /// Create a liquidation strategy from the arguments
    pub fn create_strategy(&self) -> Arc<dyn crate::liquidation_strategy::LiquidationStrategy> {
        match (self.partial_percentage, self.fixed_liquidation_amount_usd) {
            // clap's `conflicts_with` on `fixed_liquidation_amount_usd` rejects
            // both being set at parse time, so this combination can only be
            // reached by constructing `Args` directly (bypassing clap), which
            // only tests do.
            (Some(_), Some(_)) => unreachable!(
                "clap's conflicts_with enforces --partial-percentage and --fixed-liquidation-amount-usd are mutually exclusive"
            ),
            (None, Some(fixed_amount_usd)) => {
                tracing::info!(
                    fixed_amount_usd = fixed_amount_usd,
                    "Using FixedAmountLiquidationStrategy (USD-based, works across all USD markets)"
                );
                Arc::new(
                    crate::liquidation_strategy::FixedAmountLiquidationStrategy::new(
                        fixed_amount_usd,
                        self.min_profit_bps,
                    ),
                )
            }
            (percentage, None) => {
                let pct = percentage.unwrap_or(100);
                tracing::info!(
                    percentage = pct,
                    "Using PercentageLiquidationStrategy ({}% of available liquidatable collateral, 100% = full liquidation)",
                    pct
                );
                Arc::new(
                    crate::liquidation_strategy::PercentageLiquidationStrategy::new(
                        pct,
                        self.min_profit_bps,
                    ),
                )
            }
        }
    }

    /// Maps the CLI-level [`CollateralStrategyArg`] to the domain
    /// [`CollateralStrategy`]. clap's `value_enum` already rejects anything
    /// but the two accepted spellings at parse time, so this is an
    /// infallible translation, not a validation step.
    fn parse_collateral_strategy(&self) -> CollateralStrategy {
        match self.collateral_strategy {
            CollateralStrategyArg::SwapToBorrow => {
                tracing::info!("Using SwapToBorrow strategy");
                CollateralStrategy::SwapToBorrow
            }
            CollateralStrategyArg::Hold => {
                tracing::info!("Using Hold strategy (keep collateral as received)");
                CollateralStrategy::Hold
            }
        }
    }

    /// Resolves the effective run mode, applying the `--once` override.
    const fn effective_run_mode(&self) -> RunMode {
        if self.once {
            RunMode::Once
        } else {
            self.run_mode
        }
    }

    /// Build service configuration from arguments
    #[allow(clippy::too_many_lines)]
    pub fn build_config(&self) -> ServiceConfig {
        let strategy = self.create_strategy();
        let collateral_strategy = self.parse_collateral_strategy();

        // Parse collateral asset filters
        let allowed_collateral_assets: Vec<_> = self
            .allowed_collateral_assets
            .iter()
            .filter_map(|s| {
                s.parse::<templar_common::asset::FungibleAsset<templar_common::asset::CollateralAsset>>()
                    .map_err(|e| {
                        tracing::warn!(
                            asset = %s,
                            error = ?e,
                            "Failed to parse allowed collateral asset, skipping"
                        );
                        e
                    })
                    .ok()
            })
            .collect();

        let ignored_collateral_assets: Vec<_> = self
            .ignored_collateral_assets
            .iter()
            .filter_map(|s| {
                s.parse::<templar_common::asset::FungibleAsset<templar_common::asset::CollateralAsset>>()
                    .map_err(|e| {
                        tracing::warn!(
                            asset = %s,
                            error = ?e,
                            "Failed to parse ignored collateral asset, skipping"
                        );
                        e
                    })
                    .ok()
            })
            .collect();

        // Log market filtering
        if allowed_collateral_assets.is_empty() {
            tracing::info!("Market filtering: processing all assets");
        } else {
            tracing::info!(
                allowed_assets = ?allowed_collateral_assets,
                "Market filtering enabled with allowlist"
            );
        }

        if !ignored_collateral_assets.is_empty() {
            tracing::info!(
                ignored_assets = ?ignored_collateral_assets,
                "Market filtering: ignoring specified assets"
            );
        }

        let ignored_markets: Vec<AccountId> = self
            .ignored_markets
            .iter()
            .filter_map(|s| {
                s.trim()
                    .parse::<AccountId>()
                    .map_err(|e| {
                        tracing::warn!(
                            market = %s,
                            error = ?e,
                            "Failed to parse ignored market account ID, skipping"
                        );
                        e
                    })
                    .ok()
            })
            .collect();

        if !ignored_markets.is_empty() {
            tracing::info!(
                ignored_markets = ?ignored_markets,
                "Market filtering: ignoring specified markets"
            );
        }

        // Build notifier — require both bot token and chat ID. clap's mutual
        // `requires` on the two fields already rejects one being present
        // without the other at parse time, so `(Some, None)` / `(None, Some)`
        // below can only happen by constructing `Args` directly (bypassing
        // clap), which only tests do. What clap *can't* express is a
        // content-level check across two fields — e.g. one holding a real
        // token and the other holding a present-but-blank string — so that
        // case is still validated here.
        let telegram_config = match (
            self.telegram_bot_token.as_deref(),
            self.telegram_chat_id.as_deref(),
        ) {
            (None, None) => {
                tracing::info!("Telegram notifications disabled");
                None
            }
            (Some(raw_bot_token), Some(raw_chat_id)) => {
                let bot_token = raw_bot_token.trim();
                let chat_id = raw_chat_id.trim();
                match (bot_token.is_empty(), chat_id.is_empty()) {
                    (true, true) => {
                        tracing::info!("Telegram notifications disabled");
                        None
                    }
                    (false, false) => {
                        tracing::info!("Telegram notifications enabled");
                        Some(TelegramConfig {
                            bot_token: bot_token.to_owned().into(),
                            chat_id: chat_id.to_owned(),
                            thread_id: parse_optional_i64(&self.telegram_thread_id),
                        })
                    }
                    _ => {
                        panic!(
                            "TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID must both be set or both be empty"
                        );
                    }
                }
            }
            (Some(_), None) | (None, Some(_)) => unreachable!(
                "clap's mutual requires enforces TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID are both present or both absent"
            ),
        };
        let failure_cooldown =
            std::time::Duration::from_secs(self.failure_notification_cooldown_hours * 3600);
        // A concurrent round can fire up to two notifications per in-flight
        // position (liquidation + swap issue). Scale the notifier's in-flight
        // cap with the knob — at the default cap the busiest rounds would
        // silently drop alerts once the inter-position pause is gone.
        let notification_capacity = crate::notifier::MAX_INFLIGHT_NOTIFICATIONS
            .max(self.position_concurrency.max(1).saturating_mul(2));
        let notifier = Arc::new(Notifier::with_limits(
            telegram_config,
            failure_cooldown,
            notification_capacity,
        ));

        // The error deliberately omits the value: a mistyped key is still
        // near-complete key material, and this message reaches stderr.
        let signer_key: near_crypto::SecretKey = self.signer_key.trim().parse().unwrap_or_else(|_| {
            panic!("SIGNER_KEY is not a valid NEAR secret key (expected `ed25519:<base58>`); value withheld")
        });

        ServiceConfig {
            registries: self.registries.clone(),
            signer_key,
            signer_account: self.signer_account.clone(),
            network: self.network,
            near_rpc_url: self.near_rpc_url.clone(),
            near_rpc_api_key: self.near_rpc_api_key.clone(),
            transaction_timeout: self.transaction_timeout,
            liquidation_scan_interval: self.liquidation_scan_interval,
            registry_refresh_interval: self.registry_refresh_interval,
            run_mode: self.effective_run_mode(),
            // `0` would make `buffer_unordered` hang forever; a refresh/scan with
            // no concurrency makes no sense, so floor both knobs at 1.
            concurrency: self.concurrency.max(1),
            position_concurrency: self.position_concurrency.max(1),
            strategy,
            collateral_strategy,
            dry_run: self.dry_run,
            oneclick_api_token: self.oneclick_api_token.clone(),
            ref_contract: self.ref_contract.clone(),
            allowed_collateral_assets,
            ignored_collateral_assets,
            ignored_markets,
            loop_liquidation: self.loop_liquidation,
            max_loop_iterations: self.max_loop_iterations,
            hermes_url: self
                .hermes_url
                .clone()
                .unwrap_or_else(|| self.network.hermes_url()),
            redstone_gateway_url: self.redstone_gateway_url.clone(),
            min_swap_value_usd: self.min_swap_value_usd,
            batch_swap_on_cycle_start: self.batch_swap_on_cycle_start,
            swap_retry_config: SwapRetryConfig {
                max_attempts: self.swap_retry_attempts,
                base_delay_ms: self.swap_retry_base_delay_ms,
            },
            notifier,
            scan_failure_notify_threshold: self.scan_failure_notify_threshold,
            http_port: self.http_port,
            http_bind_addr: self.http_bind_addr,
        }
    }

    /// Log startup information
    pub fn log_startup(&self) {
        tracing::info!(
            network = %self.network,
            dry_run = self.dry_run,
            run_mode = ?self.effective_run_mode(),
            "Starting liquidator bot"
        );

        if self.dry_run {
            tracing::info!("DRY RUN MODE: Scanning only, no transactions will be executed");
        } else {
            tracing::warn!(
                "LIVE MODE: transactions WILL be submitted on-chain (set DRY_RUN=true to simulate instead)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use templar_gateway_client::Network;

    use super::*;

    /// Signer key shared by all config tests. Any valid ed25519 key works —
    /// only its presence (a required arg) matters here.
    const TEST_SIGNER_KEY: &str =
        "ed25519:5JQFYvABVhxnvvvULXqZUSP8QtEiRBMUi5dHfkqZmJ2FLVJqMn3mEhZpF8p8qvC6SvdZLd5VDSvkeVJdyBDZfGi1";

    fn create_test_args() -> Args {
        Args {
            registries: vec!["registry.testnet".parse().unwrap()],
            signer_key: TEST_SIGNER_KEY.to_string(),
            signer_account: "liquidator.testnet".parse().unwrap(),
            network: Network::Testnet,
            near_rpc_url: None,
            near_rpc_api_key: None,
            transaction_timeout: 60,
            liquidation_scan_interval: 600,
            registry_refresh_interval: 3600,
            run_mode: RunMode::Loop,
            once: false,
            concurrency: 10,
            position_concurrency: 1,
            partial_percentage: Some(50),
            fixed_liquidation_amount_usd: None,
            min_profit_bps: 100,
            // Matches the CLI default (safe-by-default): tests that care about
            // live-mode behavior override this explicitly.
            dry_run: true,
            collateral_strategy: CollateralStrategyArg::Hold,
            oneclick_api_token: None,
            ref_contract: None,
            allowed_collateral_assets: vec![],
            ignored_collateral_assets: vec![],
            ignored_markets: vec![],
            loop_liquidation: false,
            max_loop_iterations: 10,
            hermes_url: None,
            redstone_gateway_url: "https://oracle-gateway-1.a.redstone.vip".to_string(),
            min_swap_value_usd: 10.0,
            batch_swap_on_cycle_start: true,
            swap_retry_attempts: 3,
            swap_retry_base_delay_ms: 2000,
            scan_failure_notify_threshold: 2,
            failure_notification_cooldown_hours:
                crate::notifier::DEFAULT_FAILURE_NOTIFY_COOLDOWN_HOURS,
            telegram_bot_token: None,
            telegram_chat_id: None,
            telegram_thread_id: String::new(),
            http_port: None,
            http_bind_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
        }
    }

    /// Parses `Args` from the minimal required CLI args plus `extra`,
    /// exercising clap's real parser (`try_parse_from`) rather than the
    /// struct-literal helper above — this is what catches CLI-contract
    /// regressions (arg names, env var names, kebab-case values) that a
    /// struct literal can't, since it bypasses clap entirely.
    fn parse_with(extra: &[&str]) -> Args {
        try_parse_with(extra).expect("minimal required args should parse")
    }

    /// Like `parse_with`, but returns the `Result` so failure-path tests
    /// (clap usage errors from `conflicts_with` / `requires` / a rejecting
    /// `value_parser`) can assert on the error instead of unwrapping.
    fn try_parse_with(extra: &[&str]) -> Result<Args, clap::Error> {
        let mut argv = vec![
            "templar-liquidator",
            "--registries",
            "registry.testnet",
            "--signer-key",
            TEST_SIGNER_KEY,
            "--signer-account",
            "liquidator.testnet",
        ];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv)
    }

    /// Reads a declared default off `Args::command()`'s static argument
    /// graph rather than a live parse. clap reads the real process
    /// environment for every `#[arg(env = "…")]` field, so a parse-based
    /// assertion would be vacuous (or fail outright) if the test process
    /// happened to inherit an exported var of the same name; the command's
    /// argument graph carries the declared default independent of it.
    fn declared_default(id: &str) -> Vec<String> {
        use clap::CommandFactory as _;
        Args::command()
            .get_arguments()
            .find(|a| a.get_id().as_str() == id)
            .unwrap_or_else(|| panic!("arg '{id}' exists"))
            .get_default_values()
            .iter()
            .map(|v| v.to_string_lossy().into_owned())
            .collect()
    }

    /// `Args::command().debug_assert()` walks clap's derived argument graph
    /// and panics on internal inconsistencies (conflicting ids, bad defaults,
    /// etc.) that only surface at parse time otherwise.
    #[test]
    fn arg_definitions_are_valid() {
        use clap::CommandFactory as _;
        Args::command().debug_assert();
    }

    #[test]
    fn run_mode_defaults_to_loop() {
        assert_eq!(declared_default("run_mode"), ["loop"]);
        assert_eq!(declared_default("once"), ["false"]);
    }

    #[test]
    fn run_mode_once_parses_and_reaches_config() {
        let args = parse_with(&["--run-mode", "once"]);
        assert_eq!(args.build_config().run_mode, RunMode::Once);
    }

    /// A VAA is only accepted by the Pyth receiver whose guardian set signed it.
    #[test]
    fn hermes_url_defaults_to_the_selected_networks_endpoint() {
        for network in [Network::Mainnet, Network::Testnet] {
            let mut args = create_test_args();
            args.network = network;
            args.hermes_url = None;

            assert_eq!(args.build_config().hermes_url, network.hermes_url());
        }
    }

    #[test]
    fn an_explicit_hermes_url_overrides_the_network_default() {
        let override_url: Url = "https://hermes.example/".parse().expect("a valid endpoint");
        let mut args = create_test_args();
        args.hermes_url = Some(override_url.clone());

        assert_eq!(args.build_config().hermes_url, override_url);
    }

    #[test]
    fn test_parse_collateral_strategy_swap_to_borrow() {
        let mut args = create_test_args();
        args.collateral_strategy = CollateralStrategyArg::SwapToBorrow;

        let strategy = args.parse_collateral_strategy();
        assert!(matches!(strategy, CollateralStrategy::SwapToBorrow));
    }

    #[test]
    fn test_parse_collateral_strategy_hold() {
        let mut args = create_test_args();
        args.collateral_strategy = CollateralStrategyArg::Hold;

        let strategy = args.parse_collateral_strategy();
        assert!(matches!(strategy, CollateralStrategy::Hold));
    }

    /// clap rejects anything outside the two accepted spellings at parse
    /// time now that `collateral_strategy` is a `ValueEnum`, and the
    /// accepted spellings themselves (`hold`, `swap-to-borrow`) must not
    /// drift — they're documented in `docs/configuration.md` and
    /// `.env.example`.
    #[test]
    fn collateral_strategy_accepts_the_documented_spellings_and_rejects_others() {
        assert_eq!(
            parse_with(&["--collateral-strategy", "hold"]).collateral_strategy,
            CollateralStrategyArg::Hold
        );
        assert_eq!(
            parse_with(&["--collateral-strategy", "swap-to-borrow"]).collateral_strategy,
            CollateralStrategyArg::SwapToBorrow
        );
        try_parse_with(&["--collateral-strategy", "swap_to_borrow"])
            .expect_err("underscore spelling is not accepted, only kebab-case");
    }

    #[test]
    fn test_create_strategy_percentage_100() {
        let mut args = create_test_args();
        args.partial_percentage = Some(100);
        args.min_profit_bps = 200;

        let strategy = args.create_strategy();
        assert_eq!(strategy.strategy_name(), "Percentage Liquidation");
        assert_eq!(strategy.max_liquidation_percentage(), 100);
    }

    #[test]
    fn test_create_strategy_percentage_75() {
        let mut args = create_test_args();
        args.partial_percentage = Some(75);
        args.min_profit_bps = 150;

        let strategy = args.create_strategy();
        assert_eq!(strategy.strategy_name(), "Percentage Liquidation");
        assert_eq!(strategy.max_liquidation_percentage(), 75);
    }

    #[test]
    fn test_create_strategy_default_percentage() {
        let mut args = create_test_args();
        args.partial_percentage = None;
        args.fixed_liquidation_amount_usd = None;

        let strategy = args.create_strategy();
        assert_eq!(strategy.strategy_name(), "Percentage Liquidation");
        assert_eq!(strategy.max_liquidation_percentage(), 100);
    }

    #[test]
    fn test_create_strategy_fixed_amount() {
        let mut args = create_test_args();
        args.partial_percentage = None;
        args.fixed_liquidation_amount_usd = Some(100.0);

        let strategy = args.create_strategy();
        assert_eq!(strategy.strategy_name(), "Fixed Amount Liquidation");
    }

    /// `--partial-percentage` and `--fixed-liquidation-amount-usd` are
    /// mutually exclusive via clap's `conflicts_with` now, so setting both
    /// is a parse-time usage error (not a panic reached after startup
    /// logging has already run).
    #[test]
    fn test_create_strategy_mutual_exclusivity() {
        let err = try_parse_with(&[
            "--partial-percentage",
            "50",
            "--fixed-liquidation-amount-usd",
            "100",
        ])
        .expect_err("both strategy flags together should fail to parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn test_build_config() {
        let mut args = create_test_args();
        args.near_rpc_url = Some("https://custom.rpc.url".to_string());
        args.transaction_timeout = 90;
        args.liquidation_scan_interval = 300;
        args.registry_refresh_interval = 1800;
        args.concurrency = 5;
        // The case that matters for build_config is an explicit opt-in to
        // live mode (dry_run = false) propagating through to the config.
        args.dry_run = false;
        args.oneclick_api_token = Some("test_token".to_string());
        args.ref_contract = Some("ref.testnet".to_string());
        args.allowed_collateral_assets = vec!["nep141:usdc.testnet".to_string()];
        args.ignored_collateral_assets = vec!["nep141:scam.testnet".to_string()];

        let config = args.build_config();
        assert_eq!(config.registries.len(), 1);
        assert_eq!(config.network, Network::Testnet);
        assert_eq!(
            config.near_rpc_url,
            Some("https://custom.rpc.url".to_string())
        );
        assert_eq!(config.transaction_timeout, 90);
        assert_eq!(config.liquidation_scan_interval, 300);
        assert_eq!(config.registry_refresh_interval, 1800);
        assert_eq!(config.concurrency, 5);
        assert!(!config.dry_run);
        assert_eq!(config.allowed_collateral_assets.len(), 1);
        assert_eq!(config.ignored_collateral_assets.len(), 1);
    }

    #[test]
    fn test_network_display() {
        assert_eq!(Network::Mainnet.to_string(), "mainnet");
        assert_eq!(Network::Testnet.to_string(), "testnet");
    }

    #[test]
    fn test_percentage_validation() {
        // Valid percentages
        assert_eq!(validate_percentage("1").unwrap(), 1);
        assert_eq!(validate_percentage("50").unwrap(), 50);
        assert_eq!(validate_percentage("100").unwrap(), 100);

        // Invalid percentages
        assert!(validate_percentage("0").is_err());
        assert!(validate_percentage("101").is_err());
        assert!(validate_percentage("abc").is_err());
        assert!(validate_percentage("-5").is_err());
    }

    #[test]
    fn test_swap_retry_attempts_validation() {
        // Valid attempt counts
        assert_eq!(validate_retry_attempts("1").unwrap(), 1);
        assert_eq!(validate_retry_attempts("3").unwrap(), 3);

        // Invalid: a 0-attempt loop would never run the operation
        assert!(validate_retry_attempts("0").is_err());
        assert!(validate_retry_attempts("abc").is_err());
        assert!(validate_retry_attempts("-1").is_err());
    }

    /// `0` is rejected at the config boundary — a live parse, not just the
    /// unit-level `validate_retry_attempts` above — so a misconfigured
    /// deployment fails fast at startup with a clear message instead of
    /// hitting "Retry loop exhausted" deep in a swap attempt.
    #[test]
    fn swap_retry_attempts_zero_is_rejected_at_parse_time() {
        let err = try_parse_with(&["--swap-retry-attempts", "0"])
            .expect_err("zero retry attempts should fail to parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn test_telegram_disabled_both_empty() {
        let args = create_test_args();
        // Both unset → notifier disabled
        let config = args.build_config();
        assert!(!config.notifier.is_enabled());
    }

    #[test]
    fn test_telegram_enabled_both_set() {
        let mut args = create_test_args();
        args.telegram_bot_token = Some("123:ABC".to_string());
        args.telegram_chat_id = Some("-100123".to_string());
        let config = args.build_config();
        assert!(config.notifier.is_enabled());
    }

    #[test]
    fn test_telegram_enabled_with_thread_id() {
        let mut args = create_test_args();
        args.telegram_bot_token = Some("123:ABC".to_string());
        args.telegram_chat_id = Some("-100123".to_string());
        args.telegram_thread_id = "42".to_string();
        let config = args.build_config();
        assert!(config.notifier.is_enabled());
    }

    /// `telegram_bot_token` and `telegram_chat_id` mutually `requires` each
    /// other now, so one without the other is a parse-time usage error
    /// rather than a panic reached after `build_config` runs.
    #[test]
    fn telegram_bot_token_and_chat_id_are_mutually_required_at_parse_time() {
        let err = try_parse_with(&["--telegram-bot-token", "123:ABC"])
            .expect_err("bot token without chat id should fail to parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);

        // `=` form is required: a chat id starts with `-`, so the space-separated
        // form is parsed as flags (the run scripts use `=` for the same reason).
        let err = try_parse_with(&["--telegram-chat-id=-100123"])
            .expect_err("chat id without bot token should fail to parse");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn test_telegram_whitespace_only_treated_as_empty() {
        let mut args = create_test_args();
        args.telegram_bot_token = Some("  ".to_string());
        args.telegram_chat_id = Some("  ".to_string());
        let config = args.build_config();
        assert!(!config.notifier.is_enabled());
    }

    /// clap's `requires` only enforces that both flags are *present*; it
    /// can't validate content across two fields (e.g. one holding a real
    /// token, the other present but blank). That asymmetric case is still
    /// caught in `build_config`, not silently accepted.
    #[test]
    #[should_panic(expected = "TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID must both be set")]
    fn test_telegram_panics_on_asymmetric_blank_value() {
        let mut args = create_test_args();
        args.telegram_bot_token = Some("123:ABC".to_string());
        args.telegram_chat_id = Some("  ".to_string());
        args.build_config();
    }

    #[test]
    fn test_parse_optional_i64_empty() {
        assert_eq!(parse_optional_i64(""), None);
        assert_eq!(parse_optional_i64("  "), None);
    }

    #[test]
    fn test_parse_optional_i64_valid() {
        assert_eq!(parse_optional_i64("42"), Some(42));
        assert_eq!(parse_optional_i64(" -100 "), Some(-100));
    }

    #[test]
    #[should_panic(expected = "not a valid integer")]
    fn test_parse_optional_i64_invalid() {
        parse_optional_i64("abc");
    }

    #[test]
    fn test_telegram_empty_thread_id_env_var() {
        let mut args = create_test_args();
        args.telegram_bot_token = Some("123:ABC".to_string());
        args.telegram_chat_id = Some("-100123".to_string());
        args.telegram_thread_id = String::new();
        let config = args.build_config();
        assert!(config.notifier.is_enabled());
    }

    #[test]
    fn test_scan_failure_threshold_default() {
        let args = create_test_args();
        assert_eq!(args.scan_failure_notify_threshold, 2);
        let config = args.build_config();
        assert_eq!(config.scan_failure_notify_threshold, 2);
    }

    #[test]
    fn test_scan_failure_threshold_disabled() {
        let mut args = create_test_args();
        args.scan_failure_notify_threshold = 0;
        let config = args.build_config();
        assert_eq!(config.scan_failure_notify_threshold, 0);
    }

    #[test]
    fn test_scan_failure_threshold_custom() {
        let mut args = create_test_args();
        args.scan_failure_notify_threshold = 5;
        let config = args.build_config();
        assert_eq!(config.scan_failure_notify_threshold, 5);
    }

    #[test]
    fn http_port_defaults_to_disabled() {
        assert_eq!(parse_with(&[]).http_port, None);
        assert_eq!(create_test_args().build_config().http_port, None);
    }

    #[test]
    fn http_port_reaches_config() {
        let mut args = create_test_args();
        args.http_port = Some(9090);
        assert_eq!(args.build_config().http_port, Some(9090));
    }

    /// The endpoints expose no secrets, but binding every interface makes
    /// "don't publish this" depend entirely on an external firewall/port
    /// mapping rather than the bot itself — loopback must be the default.
    #[test]
    fn http_bind_addr_defaults_to_loopback() {
        assert_eq!(
            parse_with(&[]).http_bind_addr,
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
        assert_eq!(
            create_test_args().build_config().http_bind_addr,
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        );
    }

    #[test]
    fn http_bind_addr_is_configurable_and_reaches_config() {
        let args = parse_with(&["--http-bind-addr", "0.0.0.0"]);
        assert_eq!(args.http_bind_addr, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        assert_eq!(
            args.build_config().http_bind_addr,
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        );
    }

    /// Position-level parallelism must default to 1 — the sequential, gently
    /// paced behavior small operators on public RPC endpoints rely on. Scale
    /// is an explicit opt-in, not a changed default.
    #[test]
    fn position_concurrency_defaults_to_sequential() {
        assert_eq!(declared_default("position_concurrency"), ["1"]);
        assert_eq!(create_test_args().build_config().position_concurrency, 1);
    }

    #[test]
    fn position_concurrency_reaches_config_and_is_floored_at_one() {
        let args = parse_with(&["--position-concurrency", "8"]);
        assert_eq!(args.build_config().position_concurrency, 8);

        // `0` would make `buffer_unordered` hang forever — floor it at 1,
        // same as the registry-listing `concurrency` knob.
        let mut args = create_test_args();
        args.position_concurrency = 0;
        assert_eq!(args.build_config().position_concurrency, 1);
    }

    /// A concurrent round can fire up to two notifications per in-flight
    /// position (liquidation + swap issue); the notifier's in-flight cap must
    /// scale with the knob or the busiest rounds silently drop alerts.
    #[test]
    fn notifier_inflight_capacity_scales_with_position_concurrency() {
        let default_cfg = create_test_args().build_config();
        assert_eq!(
            default_cfg.notifier.max_inflight(),
            crate::notifier::MAX_INFLIGHT_NOTIFICATIONS,
        );

        let mut args = create_test_args();
        args.position_concurrency = 32;
        assert!(args.build_config().notifier.max_inflight() >= 64);
    }

    #[test]
    fn once_flag_forces_once_mode() {
        let mut args = create_test_args();
        args.once = true;
        let config = args.build_config();
        assert_eq!(config.run_mode, RunMode::Once);
    }

    #[test]
    fn dry_run_is_the_default() {
        // A public bot that moves money must simulate unless the operator
        // explicitly opts into live mode. Asserted against the *declared*
        // default (see `declared_default`), not a live parse — the same
        // process-environment concern as `run_mode_defaults_to_loop`.
        assert_eq!(declared_default("dry_run"), ["true"]);
    }

    #[test]
    fn a_malformed_signer_key_is_rejected_without_echoing_it() {
        // clap renders the offending value in `invalid value '…'`, so the key
        // is parsed in build_config instead. A typo'd key is still nearly
        // complete key material and must never reach stderr.
        const TYPO: &str = "ed25519:5JQFYvABVhxnvvvULXqZUSP8QtEiRBMUi5dHfkqZmJ2FLVJqMn3mEhZpF8p8qvC6SvdZLd5VDSvkeVJdyBDZfGi!";

        // clap accepts it (the field is a String) — no value echoed here.
        let mut args = parse_with(&[]);
        args.signer_key = TYPO.to_string();

        let panic = std::panic::catch_unwind(move || args.build_config())
            .expect_err("a malformed key must be rejected");
        // `panic!` with a literal yields a `&'static str` payload; a formatted
        // one yields `String`. Check both so the assertion can't pass vacuously.
        let message = panic.downcast_ref::<String>().map_or_else(
            || (*panic.downcast_ref::<&'static str>().unwrap_or(&"")).to_string(),
            Clone::clone,
        );
        assert!(!message.is_empty(), "panic payload was not a string");

        assert!(message.contains("value withheld"), "got: {message}");
        assert!(
            !message.contains(TYPO) && !message.contains("5JQFYvAB"),
            "key material leaked into the error: {message}"
        );
    }

    #[test]
    fn debug_formatting_never_reveals_the_signer_key() {
        // `near_crypto`'s own Debug prints an ed25519 secret in full, so a
        // derived Debug on ServiceConfig would put a funded key into the logs
        // of any fork that ever formatted its config. ServiceConfig implements
        // Debug by hand to prevent that; this pins it.
        let mut args = create_test_args();
        args.near_rpc_api_key = Some("rpc-api-key-value".to_string());
        args.oneclick_api_token = Some("oneclick-token-value".to_string());
        let rendered = format!("{:?}", args.build_config());

        let secret = TEST_SIGNER_KEY
            .strip_prefix("ed25519:")
            .expect("test key is ed25519");
        assert!(
            !rendered.contains(secret),
            "signer key leaked into Debug output: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
        for value in ["rpc-api-key-value", "oneclick-token-value"] {
            assert!(
                !rendered.contains(value),
                "{value} leaked into Debug output: {rendered}"
            );
        }
    }

    #[test]
    fn args_debug_formatting_never_reveals_secrets() {
        // `Args` implements `Debug` by hand for the same reason
        // `ServiceConfig` does above: the signer key and the three token
        // fields (`near_rpc_api_key`, `oneclick_api_token`,
        // `telegram_bot_token`) must never appear in a
        // `tracing::debug!(?args)` or a panic payload that formats `Args`.
        let mut args = create_test_args();
        args.near_rpc_api_key = Some("rpc-api-key-value".to_string());
        args.oneclick_api_token = Some("oneclick-token-value".to_string());
        args.telegram_bot_token = Some("telegram-bot-token-value".to_string());
        args.telegram_chat_id = Some("-100123".to_string());
        let rendered = format!("{args:?}");

        let secret = TEST_SIGNER_KEY
            .strip_prefix("ed25519:")
            .expect("test key is ed25519");
        assert!(
            !rendered.contains(secret),
            "signer key leaked into Debug output: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
        for value in [
            "rpc-api-key-value",
            "oneclick-token-value",
            "telegram-bot-token-value",
        ] {
            assert!(
                !rendered.contains(value),
                "{value} leaked into Debug output: {rendered}"
            );
        }
        // Non-secret fields must still print normally — a redacted dump
        // that hides everything isn't useful for diagnosing configuration.
        assert!(rendered.contains("-100123"));
    }

    #[test]
    fn dry_run_accepts_bare_and_explicit_forms() {
        // Argv-only surfaces (compose `command:` arrays, Cloud Run Job args)
        // can only ever select MORE simulation unless an explicit false is
        // reachable from the CLI, not just from the environment.
        assert!(parse_with(&["--dry-run"]).dry_run);
        assert!(parse_with(&["--dry-run=true"]).dry_run);
        assert!(!parse_with(&["--dry-run=false"]).dry_run);
        assert!(!parse_with(&["--dry-run", "false"]).dry_run);
    }

    #[test]
    fn explicit_live_mode_opt_out_reaches_config() {
        // The assertion production safety rests on: docker-compose.prod.yml
        // sets DRY_RUN=false, and that opt-out must survive all the way to
        // ServiceConfig, not just to Args.
        assert!(!parse_with(&["--dry-run=false"]).build_config().dry_run);
        assert!(!parse_with(&["--dry-run", "false"]).build_config().dry_run);
    }

    #[test]
    fn create_test_args_matches_the_cli_default() {
        // Pins the claim made where create_test_args() sets dry_run: true —
        // that it mirrors Args's own default rather than an independently
        // chosen value that could drift from it.
        assert_eq!(create_test_args().dry_run, parse_with(&[]).dry_run);
    }
}
