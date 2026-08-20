//! Liquidator service lifecycle management.
//!
//! This module handles the bot's main operational loop including:
//! - Registry refresh (discovering and validating markets)
//! - Inventory refresh (updating asset balances)
//! - Liquidation rounds (scanning and executing liquidations)

use std::{collections::HashMap, num::NonZeroU32, sync::Arc, time::Duration};

use near_sdk::AccountId;
use templar_gateway_client::{
    collect_paginated, Client, Network, NetworkConfigBuilder, SigningClient,
};
use templar_gateway_core::GatewayContextBuilder;
use templar_gateway_methods_spec::{contract, market, registry};
use templar_gateway_oracle_updates_dispatch::GatewayContextBuilderOracleExt as _;
use templar_gateway_types::common::Pagination;
use tokio::{
    select,
    sync::RwLock,
    time::{interval, sleep, Duration as TokioDuration, MissedTickBehavior},
};
use tracing::Instrument;

use templar_common::{
    asset::{BorrowAsset, FungibleAsset},
    oracle::pyth::PriceIdentifier,
};

use crate::{
    inventory::InventoryManager, liquidation_strategy::LiquidationStrategy, metrics::Metrics,
    oracle::PythUpdatesClient, swap::SwapProvider, CollateralStrategy, Liquidator, LiquidatorError,
    RoundSummary,
};

/// Page size for listing registry deployments.
#[allow(
    clippy::unwrap_used,
    reason = "compile-time const; a zero literal would fail to compile"
)]
const DEPLOYMENTS_PAGE_SIZE: NonZeroU32 = NonZeroU32::new(500).unwrap();

/// Information needed to price a collateral asset and determine its swap target.
#[derive(Debug, Clone)]
pub struct CollateralPriceInfo {
    /// Oracle account to query for price
    pub oracle_account: AccountId,
    /// Price identifier for this collateral asset in the oracle
    pub price_id: PriceIdentifier,
    /// Decimals for amount conversion (from oracle config)
    pub decimals: i32,
    /// Target borrow asset to swap into (derived from market config)
    pub target_borrow_asset: FungibleAsset<BorrowAsset>,
}

/// Configuration for the liquidator service
///
/// `Debug` is implemented by hand rather than derived: this struct holds the
/// signer secret key, and `near_crypto`'s own `Debug` prints an ed25519 secret
/// in full (it redacts some other key types, so deriving here would leak the
/// key the first time anything formatted the config).
#[allow(clippy::struct_excessive_bools)]
pub struct ServiceConfig {
    /// Market registries to monitor
    pub registries: Vec<AccountId>,
    /// Signer key for transactions
    pub signer_key: near_crypto::SecretKey,
    /// Signer account ID
    pub signer_account: AccountId,
    /// Network to operate on
    pub network: Network,
    /// Custom RPC URL (overrides the network default)
    pub near_rpc_url: Option<String>,
    /// API key for the RPC endpoint, sent as an `Authorization` header
    pub near_rpc_api_key: Option<String>,
    /// Transaction timeout in seconds
    pub transaction_timeout: u64,
    /// Interval between liquidation scans in seconds
    pub liquidation_scan_interval: u64,
    /// Registry refresh interval in seconds
    pub registry_refresh_interval: u64,
    /// Execution mode: continuous loop or single cycle
    pub run_mode: crate::config::RunMode,
    /// Concurrency for registry deployment listing
    pub concurrency: usize,
    /// Maximum positions evaluated/liquidated concurrently within one
    /// market's round (1 = sequential with pacing; see `config::Args`)
    pub position_concurrency: usize,
    /// Liquidation strategy
    pub strategy: Arc<dyn LiquidationStrategy>,
    /// Collateral strategy
    pub collateral_strategy: CollateralStrategy,
    /// Dry run mode - scan without executing
    pub dry_run: bool,
    /// `OneClick` API token for swap authentication
    pub oneclick_api_token: Option<String>,
    /// Collateral asset allowlist for market filtering
    pub allowed_collateral_assets:
        Vec<templar_common::asset::FungibleAsset<templar_common::asset::CollateralAsset>>,
    /// Collateral assets to ignore in market filtering
    pub ignored_collateral_assets:
        Vec<templar_common::asset::FungibleAsset<templar_common::asset::CollateralAsset>>,
    /// Market account IDs to ignore
    pub ignored_markets: Vec<near_sdk::AccountId>,
    /// Enable loop liquidation - repeatedly liquidate until position is healthy
    pub loop_liquidation: bool,
    /// Maximum iterations for loop liquidation (safety limit)
    pub max_loop_iterations: u32,
    /// Pyth Hermes API URL for fetching price data
    pub hermes_url: url::Url,
    /// RedStone public price API for scan-side proxy price composition
    pub redstone_api_url: url::Url,
    /// Minimum USD value to attempt a swap (JIT or batch)
    pub min_swap_value_usd: f64,
    /// Enable batch swap of accumulated collateral at round start
    pub batch_swap_on_cycle_start: bool,
    /// Retry configuration for transient swap errors
    pub swap_retry_config: crate::swap::SwapRetryConfig,
    /// Shared notifier for Telegram alerts
    pub notifier: crate::notifier::SharedNotifier,
    /// Consecutive scan failures before sending a Telegram alert (0 = disabled)
    pub scan_failure_notify_threshold: u32,
    /// Port for the optional `/healthz` + `/metrics` HTTP endpoint (disabled
    /// when unset). Never started in `RunMode::Once`.
    pub http_port: Option<u16>,
    /// Bind address for the optional HTTP endpoint. Defaults to loopback.
    pub http_bind_addr: std::net::IpAddr,
}

impl std::fmt::Debug for ServiceConfig {
    /// Redacts every field that carries credentials: the signer key, the RPC
    /// API key, the RPC URL (an authenticated endpoint embeds the key as a
    /// query parameter) and the 1-Click token. Presence is still reported so a
    /// dump remains useful for diagnosing configuration, without printing the
    /// values. The Telegram token is held by the notifier, which redacts itself.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        /// Reports whether an optional secret was supplied, never its value.
        fn shown(value: Option<&impl std::fmt::Debug>) -> &'static str {
            if value.is_some() {
                "<set>"
            } else {
                "<unset>"
            }
        }

        f.debug_struct("ServiceConfig")
            .field("registries", &self.registries)
            .field("signer_key", &"<redacted>")
            .field("signer_account", &self.signer_account)
            .field("network", &self.network)
            .field("near_rpc_url", &shown(self.near_rpc_url.as_ref()))
            .field("near_rpc_api_key", &shown(self.near_rpc_api_key.as_ref()))
            .field("transaction_timeout", &self.transaction_timeout)
            .field("liquidation_scan_interval", &self.liquidation_scan_interval)
            .field("registry_refresh_interval", &self.registry_refresh_interval)
            .field("run_mode", &self.run_mode)
            .field("concurrency", &self.concurrency)
            .field("position_concurrency", &self.position_concurrency)
            .field("collateral_strategy", &self.collateral_strategy)
            .field("dry_run", &self.dry_run)
            .field(
                "oneclick_api_token",
                &shown(self.oneclick_api_token.as_ref()),
            )
            .field("allowed_collateral_assets", &self.allowed_collateral_assets)
            .field("ignored_collateral_assets", &self.ignored_collateral_assets)
            .field("ignored_markets", &self.ignored_markets)
            .field("loop_liquidation", &self.loop_liquidation)
            .field("max_loop_iterations", &self.max_loop_iterations)
            .field("hermes_url", &self.hermes_url)
            .field("redstone_api_url", &self.redstone_api_url)
            .field("min_swap_value_usd", &self.min_swap_value_usd)
            .field("batch_swap_on_cycle_start", &self.batch_swap_on_cycle_start)
            .field("swap_retry_config", &self.swap_retry_config)
            .field(
                "scan_failure_notify_threshold",
                &self.scan_failure_notify_threshold,
            )
            .field("http_port", &self.http_port)
            .field("http_bind_addr", &self.http_bind_addr)
            .finish_non_exhaustive()
    }
}

/// Liquidator service that manages the bot lifecycle
pub struct LiquidatorService {
    config: ServiceConfig,
    client: SigningClient,
    /// Shares the methods client's operation driver; see [`LiquidatorService::new`].
    pyth_updates: PythUpdatesClient,
    inventory: Arc<RwLock<InventoryManager>>,
    markets: HashMap<AccountId, Liquidator>,
    /// Swap provider passed to liquidators for immediate post-liquidation swaps
    oneclick_provider: Option<crate::swap::SwapProviderImpl>,
    /// Oracle fetcher for batch swap USD threshold checks
    oracle_fetcher: crate::OracleFetcher,
    /// Collateral asset → price / swap target info (built from market configs)
    collateral_price_map: HashMap<String, CollateralPriceInfo>,
    /// Consecutive scan failure count per market (reset on success)
    scan_failure_counts: HashMap<AccountId, u32>,
    /// Operational counters served by the optional `/metrics` endpoint.
    metrics: Arc<Metrics>,
}

impl LiquidatorService {
    /// Create a new liquidator service
    ///
    /// # Panics
    ///
    /// Panics if the RPC URL or signing key cannot be parsed, or if the gateway
    /// client cannot be constructed for the configured signer.
    #[allow(clippy::expect_used)]
    pub fn new(config: ServiceConfig) -> Self {
        // Don't log the resolved RPC URL: an embedded `apiKey` makes it
        // secret-bearing.
        tracing::info!(
            network = %config.network,
            custom_rpc = config.near_rpc_url.is_some(),
            "Connecting to RPC"
        );

        let network = NetworkConfigBuilder::new(config.network)
            .rpc_url(config.near_rpc_url.as_deref())
            .expect("invalid NEAR_RPC_URL")
            .api_key(config.near_rpc_api_key.clone())
            .build();

        // The gateway client signs with `near_api::SecretKey`; the CLI parses a
        // `near_crypto::SecretKey`, so round-trip through the shared string form.
        let secret_key: near_api::SecretKey = config
            .signer_key
            .to_string()
            .parse()
            .expect("invalid signer secret key");

        // The methods client and the oracle-updates client share one operation driver
        // (store, signer pool, executor) and a base context, so idempotency and replay
        // span both.
        let (base_context, driver, signer_account_ids) = Client::builder(network)
            .secret_key(config.signer_account.clone(), secret_key)
            .expect("failed to register gateway signer")
            .build_parts()
            .expect("failed to construct gateway client");
        let client = Client::from_parts(
            base_context.clone(),
            driver.clone(),
            signer_account_ids.clone(),
        )
        .into_signing(config.signer_account.clone())
        .expect("failed to bind gateway signer");
        let pyth_updates: PythUpdatesClient = Client::from_parts(
            GatewayContextBuilder::new(base_context)
                .with_pyth_source(config.hermes_url.clone())
                .build(),
            driver,
            signer_account_ids,
        )
        .into_signing(config.signer_account.clone())
        .expect("failed to bind gateway signer");

        let inventory = Arc::new(RwLock::new(InventoryManager::new(
            client.clone(),
            config.signer_account.clone(),
        )));

        // Create swap provider for executor
        let oneclick_provider = Self::create_swap_provider(&config, &client);

        // Create oracle fetcher for batch swap price checks
        let oracle_fetcher = crate::OracleFetcher::new(
            client.clone(),
            pyth_updates.clone(),
            config.hermes_url.clone(),
            config.redstone_api_url.clone(),
            None,
        );

        // Log swap configuration
        tracing::info!(
            min_swap_usd = %config.min_swap_value_usd,
            batch_swap_enabled = %config.batch_swap_on_cycle_start,
            retry_attempts = %config.swap_retry_config.max_attempts,
            retry_base_delay_ms = %config.swap_retry_config.base_delay_ms,
            "Swap configuration loaded"
        );

        Self {
            config,
            client,
            pyth_updates,
            inventory,
            markets: HashMap::new(),
            oneclick_provider,
            oracle_fetcher,
            collateral_price_map: HashMap::new(),
            scan_failure_counts: HashMap::new(),
            metrics: Arc::new(Metrics::default()),
        }
    }

    /// Creates the swap provider for collateral rebalancing (`None` for the
    /// Hold strategy, which never swaps).
    fn create_swap_provider(
        config: &ServiceConfig,
        client: &SigningClient,
    ) -> Option<crate::swap::SwapProviderImpl> {
        use crate::swap::{OneClickSwap, SwapProviderImpl};

        // No swap provider needed for Hold strategy
        if matches!(config.collateral_strategy, CollateralStrategy::Hold) {
            tracing::info!("Collateral strategy is Hold, no swap provider needed");
            return None;
        }

        let oneclick = OneClickSwap::new(client.clone(), None, config.oneclick_api_token.clone());
        if config.oneclick_api_token.is_some() {
            tracing::info!("1-Click API provider initialized with authentication");
        } else {
            tracing::warn!(
                "1-Click API provider initialized without authentication (0.1% fee applies)"
            );
        }
        Some(SwapProviderImpl::oneclick(oneclick))
    }

    /// Run the service event loop
    pub async fn run(mut self) {
        // Optional operational HTTP surface. Loop mode only: a `run_once`
        // process exits before anything could scrape it. A bind failure is
        // logged loudly but does not abort the bot — trading continues
        // without a health/metrics endpoint rather than being taken down by
        // a failure in an observability-only surface.
        if let Some(port) = self.config.http_port {
            if let Err(e) = crate::http::spawn(
                self.config.http_bind_addr,
                port,
                Arc::clone(&self.metrics),
                self.config.liquidation_scan_interval.saturating_mul(3),
            )
            .await
            {
                tracing::error!(
                    error = %e,
                    bind_addr = %self.config.http_bind_addr,
                    port,
                    "metrics/health endpoint FAILED TO START — bot continues WITHOUT /healthz or /metrics; \
                     a scraper will see connection-refused, not bot failure"
                );
            }
        }

        // Create intervals for periodic tasks
        let mut registry_interval = interval(TokioDuration::from_secs(
            self.config.registry_refresh_interval,
        ));
        registry_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut liquidation_interval = interval(TokioDuration::from_secs(
            self.config.liquidation_scan_interval,
        ));
        liquidation_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        // Load 1-Click supported tokens cache
        if let Some(ref provider) = self.oneclick_provider {
            provider.load_supported_tokens().await;
        }

        // Run initial registry refresh immediately
        match self.refresh_registry().await {
            Ok(()) => {
                tracing::info!("Initial registry refresh completed successfully");
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "Initial registry refresh failed, will retry later"
                );
            }
        }

        // Reset the registry interval to start timing from now
        registry_interval.reset();

        loop {
            select! {
                _ = registry_interval.tick() => {
                    match self.refresh_registry().await {
                        Ok(()) => {
                            tracing::info!("Registry refresh completed successfully");
                        }
                        Err(e) => {
                            if is_rate_limit_error(&e) {
                                tracing::error!(
                                    error = %e,
                                    "Rate limit hit during registry refresh, will retry in 60 seconds"
                                );
                                // Reset interval to retry in 60 seconds
                                registry_interval.reset_after(TokioDuration::from_secs(60));
                            } else {
                                tracing::error!(
                                    error = %e,
                                    "Registry refresh failed, will retry in 5 minutes"
                                );
                                // Reset interval to retry in 5 minutes
                                registry_interval.reset_after(TokioDuration::from_secs(300));
                            }

                            if self.markets.is_empty() {
                                tracing::warn!("No markets available yet, skipping liquidation round");
                            }
                        }
                    }
                }
                _ = liquidation_interval.tick() => {
                    self.liquidation_cycle().await;
                    tracing::debug!(
                        interval_seconds = self.config.liquidation_scan_interval,
                        "Next liquidation scan interval"
                    );
                }
            }
        }
    }

    /// One liquidation cycle: collateral refresh, optional batch swap,
    /// inventory refresh, liquidation round.
    async fn liquidation_cycle(&mut self) {
        self.metrics.inc_scan();

        // Refresh collateral inventory (detect accumulated dust from prior runs)
        if let Err(e) = self.inventory.write().await.refresh_collateral().await {
            tracing::warn!(error = ?e, "Failed to refresh collateral inventory before batch swap");
        }

        // Batch swap accumulated collateral before liquidation round
        if self.config.batch_swap_on_cycle_start {
            self.batch_swap_collateral().await;
        }

        // Refresh borrow asset inventory before liquidations
        self.refresh_inventory().await;

        // Run liquidation round
        let round_summary = self.run_liquidation_round().await;
        self.metrics.add_round(&round_summary);

        // Refresh collateral inventory after liquidations (for tracking)
        match self.inventory.write().await.refresh_collateral().await {
            Ok(balances) => {
                let count = balances.len();
                if count > 0 {
                    tracing::debug!(
                        collateral_asset_count = count,
                        "Collateral inventory refreshed"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    "Failed to refresh collateral inventory"
                );
            }
        }

        tracing::info!("Liquidation round completed");

        // A cycle only counts as a successful scan if at least one market
        // scanned cleanly — reaching this line doesn't imply that (every
        // per-market error inside `run_liquidation_round` is swallowed so
        // the cycle can move on to the next market). An empty registry
        // (zero configured markets) also does not count: `healthz` must not
        // report healthy for a bot that isn't reaching any market.
        if round_summary.markets_scanned_ok > 0 {
            self.metrics.mark_scan_success();
        }
    }

    /// Runs a single scan-and-liquidate cycle, then returns.
    ///
    /// Returns `Ok` even when individual markets fail to scan or liquidate;
    /// fatal only when the registry listing fails or yields zero markets.
    pub async fn run_once(mut self) -> Result<(), LiquidatorError> {
        // `refresh_registry` reloads the 1-Click supported-tokens cache
        // unconditionally at its tail, so no separate pre-call is needed here.
        let result = match self.refresh_registry().await {
            Ok(()) if self.markets.is_empty() => Err(LiquidatorError::NoMarkets),
            Ok(()) => {
                self.liquidation_cycle().await;
                tracing::info!("Run-once cycle completed");
                Ok(())
            }
            Err(e) => Err(e),
        };

        // Single-cycle runs must drain before returning: the tokio runtime
        // shutting down after `main` returns would otherwise cancel any
        // Telegram POST still in flight, on both the success and error paths.
        self.config.notifier.drain(Duration::from_secs(10)).await;

        result
    }

    /// Check if a market should be processed based on asset filtering rules.
    ///
    /// Returns (`should_process`, `reason_if_filtered`).
    /// Note: ignored markets are checked earlier (before RPC calls) in `refresh_registry`.
    fn should_process_market(
        &self,
        config: &templar_common::market::MarketConfiguration,
    ) -> (bool, Option<String>) {
        // Reject markets whose configured asset decimals can't be used safely
        // in conversions — see `decimals_are_sane`.
        let oracle_cfg = &config.price_oracle_configuration;
        if !decimals_are_sane(
            oracle_cfg.borrow_asset_decimals,
            oracle_cfg.collateral_asset_decimals,
        ) {
            return (
                false,
                Some(format!(
                    "asset decimals out of sane range [0, 38]: borrow={}, collateral={}",
                    oracle_cfg.borrow_asset_decimals, oracle_cfg.collateral_asset_decimals
                )),
            );
        }

        let collateral_asset = &config.collateral_asset;

        // Check ignored collateral assets list
        if !self.config.ignored_collateral_assets.is_empty() {
            for ignored_asset in &self.config.ignored_collateral_assets {
                if collateral_asset == ignored_asset {
                    return (
                        false,
                        Some(format!("collateral '{collateral_asset}' is in ignore list")),
                    );
                }
            }
        }

        // Check allow list
        if !self.config.allowed_collateral_assets.is_empty() {
            let is_allowed = self
                .config
                .allowed_collateral_assets
                .iter()
                .any(|allowed_asset| collateral_asset == allowed_asset);

            if !is_allowed {
                return (
                    false,
                    Some(format!("collateral '{collateral_asset}' not in allowlist")),
                );
            }
        }

        (true, None)
    }

    /// Refresh the market registry
    #[allow(clippy::too_many_lines)]
    async fn refresh_registry(&mut self) -> Result<(), LiquidatorError> {
        let refresh_span = tracing::debug_span!("registry_refresh");

        async {
            tracing::info!(
                registries = ?self.config.registries,
                "Refreshing registry deployments"
            );

            let all_markets = list_all_deployments(
                &self.client,
                self.config.registries.clone(),
                self.config.concurrency,
            )
            .await
            .map_err(|e| LiquidatorError::ListDeploymentsError(e.into()))?;

            tracing::info!(
                market_count = all_markets.len(),
                markets = ?all_markets,
                "Found deployments from registries"
            );

            // Filter deployments using registry metadata, then fetch market configs
            let mut market_configs = Vec::new();
            for market in &all_markets {
                // Step 0: Skip ignored markets before any RPC calls
                if self.config.ignored_markets.contains(market) {
                    tracing::info!(
                        market = %market,
                        "Market filtered out (ignored market)"
                    );
                    continue;
                }

                // Step 1: Fetch market configuration — this is the definitive check
                // for whether a deployment is a market contract. Non-market contracts
                // (proxy-oracles, redstone-adapters) won't have this method.
                let config = match self
                    .client
                    .read(market::GetConfiguration::new(market.clone()))
                    .await
                {
                    Ok(config) => {
                        tracing::debug!(
                            market = %market,
                            borrow_asset = %config.borrow_asset,
                            collateral_asset = %config.collateral_asset,
                            "Fetched market configuration"
                        );
                        config
                    }
                    Err(e) => {
                        if crate::rpc::gateway_is_method_not_found(&e) {
                            tracing::info!(
                                deployment = %market,
                                "Skipping non-market deployment (no get_configuration method)"
                            );
                        } else {
                            tracing::warn!(
                                market = %market,
                                error = %e,
                                "Failed to fetch market configuration, skipping"
                            );
                        }
                        continue;
                    }
                };

                // Step 2: Check contract version using NEP-330
                let version_result = self
                    .client
                    .read(contract::GetVersion::new(market.clone()))
                    .await
                    .ok()
                    .map(|result| result.version_string);

                if let Some(version) = version_result {
                    let parts: Vec<&str> = version.split('.').collect();
                    let is_supported = if let [maj, min, _patch] = parts.as_slice() {
                        let major = maj.parse::<u32>().unwrap_or(0);
                        let minor = min.parse::<u32>().unwrap_or(0);
                        (major, minor) >= (1, 0)
                    } else {
                        tracing::warn!(
                            market = %market,
                            version = %version,
                            "Invalid semver format, skipping"
                        );
                        false
                    };

                    if !is_supported {
                        tracing::info!(
                            market = %market,
                            version = %version,
                            min_required = "1.0.0",
                            "Skipping market - unsupported version"
                        );
                        continue;
                    }
                } else {
                    tracing::info!(
                        market = %market,
                        "Contract missing NEP-330 metadata, skipping"
                    );
                    continue;
                }

                // Step 3: Detect proxy oracle via account name prefix convention
                let oracle_account = &config.price_oracle_configuration.account_id;
                self.oracle_fetcher
                    .detect_and_register_proxy_oracle(oracle_account)
                    .await;

                // Step 4: Apply market filtering rules
                let (should_process, filter_reason) = self.should_process_market(&config);

                if should_process {
                    market_configs.push((market.clone(), config));
                } else {
                    tracing::info!(
                        market = %market,
                        collateral_asset = %config.collateral_asset,
                        reason = filter_reason.unwrap_or_default(),
                        "Market filtered out"
                    );
                }
            }

            // Discover assets from all market configurations
            {
                let mut inventory_guard = self.inventory.write().await;
                inventory_guard.discover_assets(market_configs.iter().map(|(_, config)| config));
                inventory_guard
                    .discover_collateral_assets(market_configs.iter().map(|(_, config)| config));
            }

            // Create liquidators for each market
            let mut supported_markets = HashMap::new();
            let mut unsupported_markets = Vec::new();

            for (market, config) in market_configs {
                tracing::debug!(market = %market, "Creating liquidator for market");

                let mut liquidator = Liquidator::new(
                    &self.client,
                    &self.pyth_updates,
                    &self.inventory,
                    market.clone(),
                    config,
                    self.config.strategy.clone(),
                    self.config.collateral_strategy.clone(),
                    self.config.dry_run,
                    self.oneclick_provider.clone(),
                    self.config.loop_liquidation,
                    self.config.max_loop_iterations,
                    self.config.hermes_url.clone(),
                    self.config.redstone_api_url.clone(),
                    self.config.swap_retry_config.clone(),
                    self.config.min_swap_value_usd,
                    Some(self.oracle_fetcher.proxy_oracle_cache()),
                    self.config.notifier.clone(),
                );

                // Fetch market version for version-specific liquidation logic
                liquidator.fetch_market_version().await;

                // Test market compatibility
                match liquidator.scanner().check_market_compatibility().await {
                    Ok(()) => {
                        supported_markets.insert(market, liquidator);
                    }
                    Err(_) => {
                        unsupported_markets.push(market);
                    }
                }
            }

            if !unsupported_markets.is_empty() {
                tracing::debug!(
                    unsupported_count = unsupported_markets.len(),
                    unsupported = ?unsupported_markets,
                    "Filtered out unsupported markets"
                );
            }

            self.markets = supported_markets;

            // Rebuild collateral price map from new market set
            self.collateral_price_map = Self::build_collateral_price_map(&self.markets);
            tracing::info!(
                collateral_assets = self.collateral_price_map.len(),
                "Built collateral price map"
            );

            // Refresh 1-Click supported tokens cache
            if let Some(ref provider) = self.oneclick_provider {
                provider.load_supported_tokens().await;
            }

            Ok(())
        }
        .instrument(refresh_span)
        .await
    }

    /// Refresh inventory balances
    async fn refresh_inventory(&self) {
        let inventory_span = tracing::debug_span!("inventory_refresh");

        async {
            // Refresh borrow assets
            match self.inventory.write().await.refresh().await {
                Ok(refreshed) => {
                    tracing::debug!(
                        refreshed_count = refreshed,
                        "Borrow inventory refresh completed"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = ?e,
                        "Failed to refresh borrow inventory"
                    );
                }
            }
        }
        .instrument(inventory_span)
        .await;
    }

    /// Build collateral price map from current market configurations.
    ///
    /// For each unique collateral asset, stores the oracle info and the preferred
    /// borrow asset to swap into (preferring USDC variants as they're most liquid).
    fn build_collateral_price_map(
        markets: &HashMap<AccountId, Liquidator>,
    ) -> HashMap<String, CollateralPriceInfo> {
        let mut map = HashMap::new();
        for liquidator in markets.values() {
            let config = liquidator.market_configuration();
            let collateral_key = config.collateral_asset.to_string();

            let existing_is_usdc = map
                .get(&collateral_key)
                .is_some_and(|info: &CollateralPriceInfo| is_usdc_asset(&info.target_borrow_asset));

            // Prefer USDC as target borrow asset when the same collateral appears in
            // multiple markets (most liquid for rebalancing).
            if !existing_is_usdc {
                map.insert(
                    collateral_key,
                    CollateralPriceInfo {
                        oracle_account: config.price_oracle_configuration.account_id.clone(),
                        price_id: config.price_oracle_configuration.collateral_asset_price_id,
                        decimals: config.price_oracle_configuration.collateral_asset_decimals,
                        target_borrow_asset: config.borrow_asset.clone(),
                    },
                );
            }
        }
        map
    }

    /// Fetch USD prices for a set of collateral assets, batching by oracle.
    async fn fetch_collateral_usd_prices(&self, asset_keys: &[String]) -> HashMap<String, f64> {
        // Group by oracle for efficient batching
        let mut oracle_to_assets: HashMap<AccountId, Vec<(String, PriceIdentifier)>> =
            HashMap::new();

        for key in asset_keys {
            if let Some(info) = self.collateral_price_map.get(key) {
                oracle_to_assets
                    .entry(info.oracle_account.clone())
                    .or_default()
                    .push((key.clone(), info.price_id));
            }
        }

        let mut prices = HashMap::new();

        for (oracle, assets) in oracle_to_assets {
            let price_ids: Vec<_> = assets.iter().map(|(_, id)| *id).collect();
            let response = self
                .oracle_fetcher
                .get_oracle_prices(oracle.clone(), &price_ids, 120)
                .await;

            let response = match response {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        oracle = %oracle,
                        error = ?e,
                        "Failed to fetch collateral prices from oracle"
                    );
                    continue;
                }
            };

            for (asset_key, price_id) in assets {
                if let Some(Some(price)) = response.get(&price_id) {
                    #[allow(clippy::cast_precision_loss)]
                    let usd_price = (price.price.0 as f64) * 10f64.powi(price.expo);
                    prices.insert(asset_key, usd_price);
                }
            }
        }

        prices
    }

    /// Calculate USD value of a raw collateral amount.
    fn collateral_usd_value(&self, asset_key: &str, raw_amount: u128, usd_price: f64) -> f64 {
        let decimals = self
            .collateral_price_map
            .get(asset_key)
            .map_or(8, |i| i.decimals);

        #[allow(clippy::cast_precision_loss)]
        let amount = (raw_amount as f64) / 10f64.powi(decimals);
        amount * usd_price
    }

    /// Attempt to batch-swap accumulated collateral holdings at cycle start.
    ///
    /// For each collateral asset with non-zero balance:
    /// 1. Fetch its USD price from the oracle
    /// 2. Skip if below `min_swap_value_usd`
    /// 3. Swap to the target borrow asset with retry
    ///
    /// In dry-run mode, logs what would be swapped without executing.
    #[allow(clippy::too_many_lines)]
    async fn batch_swap_collateral(&self) {
        let holdings = self.inventory.read().await.collateral_holdings();
        if holdings.is_empty() {
            tracing::debug!("No collateral holdings to batch swap");
            return;
        }

        let asset_keys: Vec<String> = holdings.iter().map(|(a, _)| a.to_string()).collect();
        let prices = self.fetch_collateral_usd_prices(&asset_keys).await;

        let dry_run = self.config.dry_run;
        let retry_config = &self.config.swap_retry_config;
        let mut swapped = 0u32;
        let mut skipped = 0u32;

        for (collateral_asset, balance) in &holdings {
            let asset_key = collateral_asset.to_string();

            // Lookup USD price
            let Some(&usd_price) = prices.get(&asset_key) else {
                tracing::debug!(asset = %asset_key, "No price available, skipping batch swap");
                skipped += 1;
                continue;
            };

            let usd_value = self.collateral_usd_value(&asset_key, balance.0, usd_price);

            if usd_value < self.config.min_swap_value_usd {
                tracing::debug!(
                    asset = %asset_key,
                    balance = balance.0,
                    usd_value = format!("${usd_value:.2}"),
                    threshold = format!("${:.2}", self.config.min_swap_value_usd),
                    "Collateral below USD threshold, skipping batch swap"
                );
                skipped += 1;
                continue;
            }

            let Some(info) = self.collateral_price_map.get(&asset_key) else {
                skipped += 1;
                continue;
            };

            // Skip if swap provider doesn't support this asset pair
            if let Some(ref provider) = self.oneclick_provider {
                use crate::swap::SwapProvider;
                if !provider.supports_assets(collateral_asset, &info.target_borrow_asset) {
                    tracing::info!(
                        from = %asset_key,
                        to = %info.target_borrow_asset,
                        "Swap provider does not support asset pair, skipping batch swap"
                    );
                    skipped += 1;
                    continue;
                }
            }

            // Dry run: log what would happen, skip actual swap
            if dry_run {
                tracing::info!(
                    from = %asset_key,
                    to = %info.target_borrow_asset,
                    amount = balance.0,
                    usd_value = format!("${usd_value:.2}"),
                    "[DRY RUN] Would batch swap collateral"
                );
                swapped += 1;
                continue;
            }

            let Some(ref swap_provider) = self.oneclick_provider else {
                tracing::debug!("No swap provider, skipping batch swap");
                return;
            };

            tracing::info!(
                from = %asset_key,
                to = %info.target_borrow_asset,
                amount = balance.0,
                usd_value = format!("${usd_value:.2}"),
                "Attempting batch swap of accumulated collateral"
            );

            let swap_amount = templar_common::asset::FungibleAssetAmount::from(
                near_sdk::json_types::U128(balance.0),
            );

            let swap_name = format!("batch:{asset_key}");
            let provider = swap_provider.clone();
            let coll = collateral_asset.clone();
            let borrow = info.target_borrow_asset.clone();

            let result = crate::swap::retry::swap_with_retry(retry_config, &swap_name, || {
                let provider = provider.clone();
                let coll = coll.clone();
                let borrow = borrow.clone();
                async move {
                    provider
                        .swap(&coll, &borrow, swap_amount)
                        .await
                        .map_err(|e| {
                            // Classify the AppError into SwapError
                            let msg = e.to_string();
                            let kind = if msg.contains("Amount is too low") {
                                crate::swap::SwapErrorKind::AmountTooLow { message: msg }
                            } else if msg.contains("Failed to get quote") {
                                crate::swap::SwapErrorKind::QuoteFailed { message: msg }
                            } else {
                                crate::swap::SwapErrorKind::Unknown { message: msg }
                            };
                            crate::swap::SwapError::new(kind, "Batch swap")
                        })
                }
            })
            .await;

            match result {
                Ok(()) => {
                    tracing::info!(
                        from = %asset_key,
                        usd_value = format!("${usd_value:.2}"),
                        "Batch swap succeeded"
                    );
                    swapped += 1;
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("Amount too low") {
                        tracing::debug!(
                            from = %asset_key,
                            error = %e,
                            "Batch swap skipped - amount too low for provider"
                        );
                    } else if msg.contains("Quote failed") {
                        tracing::debug!(
                            from = %asset_key,
                            "No swap route available for asset, skipping batch swap"
                        );
                    } else {
                        tracing::info!(
                            from = %asset_key,
                            error = %e,
                            "Batch swap failed, will retry next cycle"
                        );
                    }
                    skipped += 1;
                }
            }
        }

        if swapped > 0 || skipped > 0 {
            if dry_run {
                tracing::info!(
                    swapped,
                    skipped,
                    "[DRY RUN] Batch swap round summary (no swaps executed)"
                );
            } else {
                tracing::info!(swapped, skipped, "Batch swap round completed");
            }
        }
    }

    /// Run a single liquidation round across all markets, returning the
    /// combined tally for the optional `/metrics` endpoint.
    async fn run_liquidation_round(&mut self) -> RoundSummary {
        let liquidation_span = tracing::debug_span!("liquidation_round");
        let market_ids: Vec<AccountId> = self.markets.keys().cloned().collect();
        let market_count = market_ids.len();
        let threshold = self.config.scan_failure_notify_threshold;
        let mut round_summary = RoundSummary::default();

        async {
            for (i, market) in market_ids.iter().enumerate() {
                let Some(liquidator) = self.markets.get(market) else {
                    continue;
                };
                let market_span = tracing::debug_span!("market", market = %market);

                let result = async {
                    tracing::info!(market = %market, "Scanning market for liquidations");
                    liquidator
                        .run_liquidations(self.config.position_concurrency)
                        .await
                }
                .instrument(market_span)
                .await;

                // Handle errors gracefully and track consecutive failures
                match result {
                    Ok(summary) => {
                        round_summary.merge(summary);
                        round_summary.markets_scanned_ok += 1;
                        // Reset failure counter; notify recovery if we previously alerted
                        let prev = self
                            .scan_failure_counts
                            .remove(market)
                            .unwrap_or(0);
                        if threshold > 0 && prev >= threshold {
                            tracing::info!(market = %market, prev_failures = prev, "Market recovered");
                            self.config.notifier.notify_scan_recovered(
                                market.as_ref(),
                                prev,
                            );
                        }
                        tracing::info!(market = %market, "Market scan completed");
                    }
                    Err(e) => {
                        round_summary.markets_failed += 1;
                        self.metrics.inc_market_scan_failure();

                        let phase = e.phase();
                        if is_rate_limit_error(&e) {
                            tracing::error!(
                                market = %market,
                                phase = %phase,
                                error = %e,
                                "Rate limited — sleeping 60s before next market"
                            );
                            sleep(Duration::from_secs(60)).await;
                        } else {
                            tracing::warn!(
                                market = %market,
                                phase = %phase,
                                error = %e,
                                "Market skipped"
                            );
                        }

                        // Track consecutive scan failures and notify at threshold
                        let count = self
                            .scan_failure_counts
                            .entry(market.clone())
                            .or_insert(0);
                        *count += 1;
                        if threshold > 0 && *count == threshold {
                            self.config.notifier.notify_scan_failures(
                                market.as_ref(),
                                *count,
                                &e.to_string(),
                            );
                        }
                    }
                }

                // Add delay between markets to avoid rate limiting (except after last market)
                if i < market_count - 1 {
                    let delay_seconds = 5;
                    tracing::debug!(
                        "Waiting {}s before next market to avoid rate limits",
                        delay_seconds
                    );
                    sleep(Duration::from_secs(delay_seconds)).await;
                }
            }
        }
        .instrument(liquidation_span)
        .await;

        round_summary
    }
}

/// Fetch every deployment across `registries` (paginated, concurrently).
///
/// Propagates the first read error encountered, matching the pre-migration
/// behaviour where a registry failure fails the whole refresh.
async fn list_all_deployments(
    client: &SigningClient,
    registries: Vec<AccountId>,
    concurrency: usize,
) -> templar_gateway_core::GatewayResult<Vec<AccountId>> {
    use futures::{StreamExt, TryStreamExt};

    futures::stream::iter(registries)
        .map(|registry| {
            let client = client.clone();
            async move { list_deployments(&client, registry).await }
        })
        .buffer_unordered(concurrency)
        .try_concat()
        .await
}

/// List all deployments from a single registry, paginating until a short page.
async fn list_deployments(
    client: &SigningClient,
    registry: AccountId,
) -> templar_gateway_core::GatewayResult<Vec<AccountId>> {
    collect_paginated(DEPLOYMENTS_PAGE_SIZE, move |offset, count| {
        let client = client.clone();
        let registry = registry.clone();
        async move {
            let result = client
                .read(registry::ListDeployments {
                    registry_id: registry,
                    args: Pagination {
                        offset: Some(offset),
                        limit: Some(count),
                    },
                })
                .await?;
            Ok(result.account_ids)
        }
    })
    .await
}

/// Check if an asset is a USDC variant (preferred swap target for rebalancing).
/// Non-critical: only used as a heuristic when the same collateral appears in
/// multiple markets to prefer swapping into USDC for better liquidity. If no
/// variant is recognized, batch swap still works but may pick a less liquid target.
fn is_usdc_asset(asset: &FungibleAsset<BorrowAsset>) -> bool {
    let s = asset.to_string().to_lowercase();
    s.contains("usdc")
        || s.contains("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48") // ETH USDC
        || s.contains("17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1") // native NEAR USDC
        || s.contains("5ce3bf3a31af18be40ba30f721101b4341690186") // Solana USDC
        || s.contains("1100_111bzqbb65gxapavoxqmmcgyo5os3txhqs1uh1cgahkquetujq1tju")
    // Stellar USDC
}

/// Reports whether a market's configured asset decimals are usable.
///
/// Decimals arrive from on-chain configs unvalidated and feed `10f64::powi`
/// in every conversion — out-of-range values silently become zero or
/// infinity. 38 is the ceiling under which `10^d` fits a `u128`; real tokens
/// use 0–24.
fn decimals_are_sane(borrow_decimals: i32, collateral_decimals: i32) -> bool {
    const SANE: std::ops::RangeInclusive<i32> = 0..=38;
    SANE.contains(&borrow_decimals) && SANE.contains(&collateral_decimals)
}

/// Check if an error is a rate limit error
fn is_rate_limit_error(error: &LiquidatorError) -> bool {
    let error_msg = error.to_string();
    error_msg.contains("TooManyRequests")
        || error_msg.contains("429")
        || error_msg.contains("rate limit")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Out-of-range decimals would silently corrupt every conversion —
    /// reject the market at registration instead.
    #[test]
    fn decimals_sanity_bounds() {
        assert!(decimals_are_sane(6, 8));
        assert!(decimals_are_sane(0, 0));
        assert!(decimals_are_sane(38, 38));
        assert!(!decimals_are_sane(-1, 8));
        assert!(!decimals_are_sane(6, -1));
        assert!(!decimals_are_sane(39, 8));
        assert!(!decimals_are_sane(6, 39));
    }
}
