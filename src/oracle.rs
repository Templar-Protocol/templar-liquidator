//! Oracle price fetching module.
//!
//! Handles fetching prices from various oracle types including:
//! - Pyth oracles (via Hermes HTTP API)
//! - LST oracles with price transformers
//! - Proxy oracles — composed off-chain at scan time from each feed's
//!   configured source (Hermes for Pyth sources, the RedStone public price
//!   API via [`crate::redstone`] for RedStone sources, transformer inputs by
//!   free view call), with the proxy's on-chain price cache as fallback for
//!   anything not composable off-chain (e.g. Lazer sources). Every composed
//!   price is bounded by the market's freshness window before use.
//!
//! Execution-time pricing is separate and unchanged: the market contract
//! reads its own on-chain oracle, which this module refreshes via
//! [`OracleFetcher::update_onchain_prices`] before a live liquidation.

use near_sdk::AccountId;
use std::collections::{HashMap, HashSet};
use templar_common::{
    oracle::pyth::{self, OracleResponse, PriceIdentifier},
    Decimal,
};
use templar_gateway_client::SigningClient;
use templar_gateway_core::GatewayContext;
use templar_gateway_methods_spec::{contract, lst_oracle, proxy_oracle, pyth as pyth_spec};
use templar_gateway_oracle_updates_dispatch::{Dispatch as OracleUpdatesDispatch, WithPythSource};
use templar_gateway_oracle_updates_spec::oracle as oracle_updates;
use templar_gateway_types::{
    common::ContractArgs, Base64Bytes, ContractMethodName, OperationStatus,
};
use templar_proxy_oracle_near_common::{
    input::Source,
    price_transformer::{Action, Call, PriceTransformer},
    request::OracleRequest,
};
use url::Url;

use crate::{
    rpc::{gateway_is_method_not_found, RpcError},
    LiquidatorError, LiquidatorResult,
};

// ── Hermes (Pyth) gateway types ──────────────────────────────────────────────

/// Parsed response from Pyth Hermes `/v2/updates/price/latest?parsed=true`.
#[derive(serde::Deserialize)]
struct HermesResponse {
    parsed: Option<Vec<HermesParsedFeed>>,
}

#[derive(serde::Deserialize)]
struct HermesParsedFeed {
    id: String,
    ema_price: HermesParsedPrice,
}

#[derive(serde::Deserialize)]
struct HermesParsedPrice {
    price: String,
    conf: String,
    expo: i32,
    publish_time: i64,
}

// ── Off-chain proxy price composition ────────────────────────────────────────

/// A proxy source request the bot can price without any on-chain oracle
/// state: Pyth via Hermes, RedStone via the public price API
/// ([`crate::redstone`]). Lazer never classifies — a Lazer feed lives only in
/// its on-chain adapter contract, which is a cache someone must push to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OffchainRequest {
    Pyth(PriceIdentifier),
    RedStone(String),
}

/// One proxy feed's scan-time pricing plan: a direct off-chain source, or a
/// transformer applied over one (the transformer input is a free view call).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OffchainPriceSource {
    Direct(OffchainRequest),
    Transformed {
        request: OffchainRequest,
        call: Call,
        action: Action,
    },
}

/// Reports whether a quote's publish time is usable under the market's
/// freshness bound: no older than `max_age_secs`, and no further ahead of
/// `now_secs` than clock skew explains ([`crate::redstone::MAX_FUTURE_SKEW_MS`];
/// a future-dated quote has negative age and passes every staleness bound).
/// Composed proxy prices must pass this bot-side — the on-chain read they
/// replace enforces the same bound on-chain.
fn publish_time_is_fresh(publish_time_secs: i64, now_secs: i64, max_age_secs: u32) -> bool {
    let age_secs = now_secs - publish_time_secs;
    age_secs <= i64::from(max_age_secs) && age_secs >= -(crate::redstone::MAX_FUTURE_SKEW_MS / 1000)
}

/// Classifies an [`OracleRequest`] as off-chain pricable, or `None` for Lazer.
fn classify_offchain_request(request: &OracleRequest) -> Option<OffchainRequest> {
    match request {
        OracleRequest::Pyth(req) => Some(OffchainRequest::Pyth(req.price_id)),
        OracleRequest::RedStone(req) => Some(OffchainRequest::RedStone(req.price_id.to_string())),
        OracleRequest::Lazer(_) => None,
    }
}

/// Picks the first source in the proxy's configured order that can be priced
/// off-chain.
///
/// Primary-source semantics:
/// the proxy's own aggregation and circuit breakers are on-chain policy the
/// scan does not replicate (and the kernel keeps them private), which is safe
/// because scan-time prices are advisory — execution still pushes fresh prices
/// on-chain and the market contract re-validates against its own oracle.
pub(crate) fn plan_offchain_source<'a>(
    sources: impl Iterator<Item = &'a Source>,
) -> Option<OffchainPriceSource> {
    for source in sources {
        match source {
            Source::Request(request) => {
                if let Some(request) = classify_offchain_request(request) {
                    return Some(OffchainPriceSource::Direct(request));
                }
            }
            Source::Transformer(transformer) => {
                if let Some(request) = classify_offchain_request(&transformer.request) {
                    return Some(OffchainPriceSource::Transformed {
                        request,
                        call: transformer.call.clone(),
                        action: transformer.action.clone(),
                    });
                }
            }
        }
    }
    None
}

// ── Shared types ─────────────────────────────────────────────────────────────

/// A gateway client bound to the oracle-updates dispatcher, over a context carrying
/// the in-process Hermes payload source that `oracle.updatePyth` fetches from.
pub type PythUpdatesClient = SigningClient<OracleUpdatesDispatch, WithPythSource<GatewayContext>>;

/// Shared cache of detected proxy oracle accounts.
pub type ProxyOracleCache =
    std::sync::Arc<tokio::sync::RwLock<std::collections::HashSet<AccountId>>>;

/// Oracle price fetcher.
///
/// Fetches Pyth prices directly from Hermes, LST oracle prices via
/// transformers, and proxy-oracle prices by off-chain composition with the
/// proxy's on-chain cache as fallback — see the module docs for the full
/// scan-vs-execution pricing split.
pub struct OracleFetcher {
    client: SigningClient,
    pyth_updates: PythUpdatesClient,
    /// Cache of which oracles are LST oracles (`oracle_account` -> `underlying_oracle`)
    lst_oracle_cache: std::sync::Arc<tokio::sync::RwLock<HashMap<AccountId, Option<AccountId>>>>,
    /// Cache of detected proxy oracles (oracles that use cross-contract calls).
    /// Shared across all `OracleFetcher` instances so detection during registry
    /// refresh propagates to per-market fetchers.
    proxy_oracle_cache: ProxyOracleCache,
    /// HTTP client for API calls
    http_client: reqwest::Client,
    /// Pyth Hermes API URL (e.g., <https://hermes.pyth.network>)
    hermes_url: Url,
    /// RedStone public price API, for composing proxy prices off-chain at
    /// scan time.
    redstone_api: crate::redstone::RedStoneApiClient,
}

impl OracleFetcher {
    /// Creates a new oracle fetcher.
    ///
    /// `proxy_oracle_cache` allows sharing the proxy oracle cache across multiple
    /// `OracleFetcher` instances. Pass `None` to create a standalone cache.
    ///
    /// On-chain price pushes are signed by the account bound to the shared
    /// [`SigningClient`].
    pub fn new(
        client: SigningClient,
        pyth_updates: PythUpdatesClient,
        hermes_url: Url,
        redstone_api_url: Url,
        proxy_oracle_cache: Option<ProxyOracleCache>,
    ) -> Self {
        Self {
            client,
            pyth_updates,
            lst_oracle_cache: std::sync::Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            proxy_oracle_cache: proxy_oracle_cache.unwrap_or_else(|| {
                std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashSet::new()))
            }),
            http_client: reqwest::Client::new(),
            hermes_url,
            redstone_api: crate::redstone::RedStoneApiClient::new(redstone_api_url),
        }
    }

    /// Returns a clone of the shared proxy oracle cache handle.
    pub fn proxy_oracle_cache(&self) -> ProxyOracleCache {
        self.proxy_oracle_cache.clone()
    }

    /// Detects whether an oracle is a proxy oracle by probing its view interface.
    pub async fn detect_and_register_proxy_oracle(&self, oracle: &AccountId) {
        if let Err(error) = self.is_proxy_oracle(oracle).await {
            tracing::warn!(%oracle, %error, "Failed to detect proxy oracle interface");
        }
    }

    #[tracing::instrument(skip(self), level = "debug")]
    async fn is_proxy_oracle(&self, oracle: &AccountId) -> LiquidatorResult<bool> {
        if self.proxy_oracle_cache.read().await.contains(oracle) {
            return Ok(true);
        }

        match self
            .client
            .read(proxy_oracle::ListProxies {
                oracle_id: oracle.clone(),
                offset: None,
                count: Some(1),
            })
            .await
        {
            Ok(_) => {
                if self.proxy_oracle_cache.write().await.insert(oracle.clone()) {
                    tracing::info!(%oracle, "Registered proxy oracle");
                }
                Ok(true)
            }
            Err(error) if gateway_is_method_not_found(&error) => Ok(false),
            Err(error) => Err(LiquidatorError::PriceFetchError(error.into())),
        }
    }

    /// Checks if the oracle is an LST oracle by attempting to fetch its underlying oracle ID.
    #[tracing::instrument(skip(self), level = "debug")]
    async fn is_lst_oracle(&self, oracle: &AccountId) -> LiquidatorResult<Option<AccountId>> {
        // Check cache first
        {
            let cache = self.lst_oracle_cache.read().await;
            if let Some(cached) = cache.get(oracle) {
                return Ok(cached.clone());
            }
        }

        // Try to fetch underlying oracle ID. Only a missing `oracle_id` method
        // means "not an LST oracle"; any other error (transient RPC, decode) is
        // propagated and NOT cached, so a blip can't permanently misroute an LST
        // oracle through the direct Pyth path until restart.
        let result = match self
            .client
            .read(lst_oracle::GetOracleId::new(oracle.clone()))
            .await
        {
            Ok(response) => {
                tracing::debug!(
                    oracle = %oracle,
                    underlying = %response.pyth_oracle_id,
                    "Detected LST oracle"
                );
                Some(response.pyth_oracle_id)
            }
            Err(error) if gateway_is_method_not_found(&error) => {
                tracing::debug!(oracle = %oracle, "Standard Pyth oracle (no oracle_id method)");
                None
            }
            Err(error) => return Err(LiquidatorError::PriceFetchError(error.into())),
        };

        // Cache the result
        {
            let mut cache = self.lst_oracle_cache.write().await;
            cache.insert(oracle.clone(), result.clone());
        }

        Ok(result)
    }

    // ── Pyth / Hermes ────────────────────────────────────────────────────────

    /// Fetches EMA prices from the Pyth Hermes HTTP API.
    ///
    /// Returns an `OracleResponse` keyed by `PriceIdentifier`.
    #[tracing::instrument(skip(self), level = "debug")]
    async fn fetch_pyth_prices_from_hermes(
        &self,
        price_ids: &[PriceIdentifier],
    ) -> Option<OracleResponse> {
        let url = format!(
            "{}/v2/updates/price/latest",
            self.hermes_url.as_str().trim_end_matches('/')
        );
        let mut query_params: Vec<(&str, String)> = price_ids
            .iter()
            .map(|id| ("ids[]", id.to_string()))
            .collect();
        query_params.push(("parsed", "true".to_string()));

        let response = self
            .http_client
            .get(&url)
            .query(&query_params)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| {
                tracing::debug!(error = %e, "Hermes HTTP request failed");
            })
            .ok()?;

        if !response.status().is_success() {
            tracing::debug!(status = %response.status(), "Hermes returned error status");
            return None;
        }

        let body: HermesResponse = response
            .json()
            .await
            .map_err(|e| {
                tracing::debug!(error = %e, "Failed to parse Hermes response");
            })
            .ok()?;

        let parsed = body.parsed?;
        let mut result = OracleResponse::new();

        for feed in &parsed {
            // Parse the hex ID back to a PriceIdentifier
            let Ok(id_bytes) = hex::decode(&feed.id).map_err(|e| {
                tracing::warn!(id = %feed.id, error = %e, "Invalid hex price ID from Hermes");
            }) else {
                continue;
            };
            if id_bytes.len() != 32 {
                continue;
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&id_bytes);
            let price_id = PriceIdentifier(arr);

            let (Ok(price_val), Ok(conf_val)) = (
                feed.ema_price.price.parse::<i64>(),
                feed.ema_price.conf.parse::<u64>(),
            ) else {
                tracing::warn!(id = %feed.id, "Invalid Hermes price payload, skipping feed");
                continue;
            };

            result.insert(
                price_id,
                Some(pyth::Price {
                    price: near_sdk::json_types::I64(price_val),
                    conf: near_sdk::json_types::U64(conf_val),
                    expo: feed.ema_price.expo,
                    publish_time: pyth::PythTimestamp::from_secs(feed.ema_price.publish_time),
                }),
            );
        }

        tracing::debug!(
            price_count = result.len(),
            "Fetched Pyth EMA prices from Hermes"
        );

        Some(result)
    }

    // ── On-chain price updates ────────────────────────────────────────────────

    /// Resolves the market-facing oracle account + price IDs to the actual
    /// underlying Pyth oracle and feed IDs that need `update_price_feeds`.
    ///
    /// - **Direct Pyth oracle**: returns as-is.
    /// - **LST oracle**: resolves via `oracle_id()` + transformers to get
    ///   the underlying Pyth oracle and transformed feed IDs.
    /// - **Proxy oracle**: reads proxy entries, collects all
    ///   `OracleRequest::Pyth` targets (`oracle_id` + `price_id`).
    ///
    /// Returns a map of `pyth_oracle_account` → `Vec<feed_ids>`.
    pub async fn resolve_pyth_update_targets(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> LiquidatorResult<HashMap<AccountId, Vec<PriceIdentifier>>> {
        let mut targets: HashMap<AccountId, HashSet<PriceIdentifier>> = HashMap::new();

        // LST oracle: resolve underlying oracle + transform price IDs. A read
        // failure is propagated (not silently mapped to the original feed), so we
        // never refresh the wrong Pyth feeds; fall back to `pid` only when the
        // contract explicitly reports no transformer.
        if let Some(underlying_oracle) = self.is_lst_oracle(oracle).await? {
            let mut underlying_ids = Vec::new();
            for &pid in price_ids {
                let result = self
                    .client
                    .read(lst_oracle::GetTransformer::new(oracle.clone(), pid))
                    .await
                    .map_err(|error| LiquidatorError::PriceFetchError(error.into()))?;
                match result.transformer {
                    Some(transformer) => underlying_ids.push(transformer.price_id),
                    None => underlying_ids.push(pid),
                }
            }
            targets
                .entry(underlying_oracle)
                .or_default()
                .extend(underlying_ids);
            return Ok(targets
                .into_iter()
                .map(|(oracle_id, feed_ids)| (oracle_id, feed_ids.into_iter().collect()))
                .collect());
        }

        // Proxy oracle: collect Pyth entries from proxy config. A read failure is
        // propagated rather than skipped, so we don't refresh an incomplete set
        // of feeds. Probe via `is_proxy_oracle` (not the raw cache) so a direct
        // caller that hasn't warmed the cache still classifies correctly; the
        // probe checks the cache first, keeping the hot path cheap.
        if self.is_proxy_oracle(oracle).await? {
            for &pid in price_ids {
                let result = self
                    .client
                    .read(proxy_oracle::GetProxy::new(oracle.clone(), pid))
                    .await
                    .map_err(|error| LiquidatorError::PriceFetchError(error.into()))?;
                if let Some(proxy) = result.proxy {
                    for source in proxy.sources() {
                        Self::collect_pyth_targets_from_source(source, &mut targets);
                    }
                }
            }
            return Ok(targets
                .into_iter()
                .map(|(oracle_id, feed_ids)| (oracle_id, feed_ids.into_iter().collect()))
                .collect());
        }

        // Direct Pyth oracle
        targets
            .entry(oracle.clone())
            .or_default()
            .extend(price_ids.iter().copied());
        Ok(targets
            .into_iter()
            .map(|(oracle_id, feed_ids)| (oracle_id, feed_ids.into_iter().collect()))
            .collect())
    }

    /// Collects Pyth oracle targets from a proxy source entry.
    fn collect_pyth_targets_from_source(
        source: &Source,
        targets: &mut HashMap<AccountId, HashSet<PriceIdentifier>>,
    ) {
        match source {
            Source::Request(OracleRequest::Pyth(pyth_req)) => {
                targets
                    .entry(pyth_req.oracle_id.clone())
                    .or_default()
                    .insert(pyth_req.price_id);
            }
            // RedStone and Lazer prices are pushed elsewhere (RedStone by the
            // relayer; Lazer by the gateway that holds the Lazer payload source),
            // not by the liquidator.
            Source::Request(OracleRequest::RedStone(_) | OracleRequest::Lazer(_)) => {}
            Source::Transformer(transformer) => {
                // Transformer wraps an underlying request — extract its Pyth target
                Self::collect_pyth_targets_from_source(
                    &Source::Request(transformer.request.clone()),
                    targets,
                );
            }
        }
    }

    /// Resolves market-level oracle config to underlying Pyth targets and pushes
    /// fresh prices on-chain for each. Returns `Ok(true)` if any update was sent.
    pub async fn update_onchain_prices(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> LiquidatorResult<bool> {
        let is_proxy_oracle = self.is_proxy_oracle(oracle).await?;
        let targets = self.resolve_pyth_update_targets(oracle, price_ids).await?;

        if targets.is_empty() && !is_proxy_oracle {
            tracing::debug!("No Pyth targets to update on-chain");
            return Ok(false);
        }

        let mut any_updated = false;
        for (pyth_oracle, feed_ids) in &targets {
            match self.update_pyth_prices(pyth_oracle, feed_ids).await {
                Ok(true) => any_updated = true,
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(
                        oracle = %pyth_oracle,
                        error = %e,
                        "Failed to update on-chain Pyth prices; proceeding with existing on-chain state"
                    );
                }
            }
        }

        if is_proxy_oracle {
            any_updated |= self.update_proxy_prices(oracle, price_ids).await?;
        }

        Ok(any_updated)
    }

    /// Refreshes a proxy oracle cache by invoking its on-chain `update_prices` flow.
    ///
    /// Best-effort: the gateway write returns only the operation status, not the
    /// per-feed cache result, so an unsuccessful operation surfaces as an error
    /// that the caller logs and swallows.
    #[tracing::instrument(skip(self), level = "info")]
    async fn update_proxy_prices(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> LiquidatorResult<bool> {
        let result = self
            .client
            .execute(proxy_oracle::UpdatePrices {
                oracle_id: oracle.clone(),
                price_ids: price_ids.to_vec(),
            })
            .await
            .map_err(|e| {
                LiquidatorError::OracleUpdateError(format!("Proxy oracle update failed: {e}"))
            })?;

        if result.operation.status != OperationStatus::Succeeded {
            return Err(LiquidatorError::OracleUpdateError(format!(
                "Proxy oracle update operation {} ended with status {:?}",
                result.operation.id.0, result.operation.status
            )));
        }

        tracing::info!(oracle = %oracle, price_ids = ?price_ids, operation_id = %result.operation.id.0, "Successfully updated proxy oracle prices");
        Ok(true)
    }

    /// Pushes fresh Pyth prices on-chain via `oracle.updatePyth`, which fetches the
    /// VAA covering `price_ids` from Hermes inside the gateway.
    ///
    /// The market contract reads prices from the on-chain oracle during
    /// liquidation execution, so prices must be fresh there — not just in the
    /// liquidator's local HTTP-fetched view.
    ///
    /// Returns `Ok(true)` if the update was applied on-chain.
    #[tracing::instrument(skip(self), level = "info")]
    async fn update_pyth_prices(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> LiquidatorResult<bool> {
        tracing::info!(
            oracle = %oracle,
            price_ids = ?price_ids,
            "Requesting on-chain Pyth price update"
        );

        match self
            .pyth_updates
            .execute(oracle_updates::UpdatePyth {
                oracle_id: oracle.clone(),
                price_ids: price_ids.to_vec(),
            })
            .await
        {
            Ok(result) if result.operation.status == OperationStatus::Succeeded => {
                tracing::info!(
                    operation_id = %result.operation.id.0,
                    oracle = %oracle,
                    "Successfully updated on-chain Pyth prices"
                );
                Ok(true)
            }
            Ok(result) => {
                tracing::error!(
                    operation_id = %result.operation.id.0,
                    status = ?result.operation.status,
                    oracle = %oracle,
                    "Pyth price update did not succeed"
                );
                Err(LiquidatorError::OracleUpdateError(format!(
                    "Pyth price update operation {} ended with status {:?}",
                    result.operation.id.0, result.operation.status
                )))
            }
            Err(e) => {
                tracing::error!(
                    error = %e,
                    oracle = %oracle,
                    "Failed to submit price update transaction"
                );
                Err(LiquidatorError::OracleUpdateError(format!(
                    "Transaction failed: {e}"
                )))
            }
        }
    }

    // ── Main entry point ─────────────────────────────────────────────────────

    /// Fetches current oracle prices.
    ///
    /// Detects oracle type and uses the appropriate method:
    /// - LST oracles: Fetch from underlying oracle and apply transformers
    /// - Proxy oracles: Read cached on-chain proxy oracle prices
    /// - Pyth oracles: Hermes HTTP API
    #[tracing::instrument(skip(self), level = "debug")]
    pub async fn get_oracle_prices(
        &self,
        oracle: AccountId,
        price_ids: &[PriceIdentifier],
        age: u32,
    ) -> LiquidatorResult<OracleResponse> {
        // Check proxy interface first so protected proxy feeds cannot be bypassed by cache misses
        // or nonstandard account naming.
        if self.is_proxy_oracle(&oracle).await? {
            // Scan-side: compose prices off-chain first — no gas, and no
            // dependency on a keeper having recently pushed the proxy's
            // cache — then fall back to the on-chain cache for any feed that
            // couldn't be composed (e.g. Lazer-sourced). Execution is
            // unaffected: `update_onchain_prices` still pushes before a live
            // liquidation and the market contract reads its own oracle.
            let mut response = self
                .compose_proxy_prices_offchain(&oracle, price_ids, age)
                .await;
            let missing: Vec<PriceIdentifier> = price_ids
                .iter()
                .filter(|price_id| !response.contains_key(price_id))
                .copied()
                .collect();
            if missing.is_empty() {
                return Ok(response);
            }
            // Propagate the error: a partial response would make every
            // position's status check panic with "Missing price", noisier
            // than skipping the market once.
            let cached = self.get_proxy_oracle_prices(oracle, &missing, age).await?;
            response.extend(cached);
            return Ok(response);
        }

        // Check if this is an LST oracle upfront
        if let Some(underlying_oracle) = self.is_lst_oracle(&oracle).await? {
            tracing::debug!(
                oracle = %oracle,
                underlying = %underlying_oracle,
                "Using LST oracle approach with transformers"
            );
            return self
                .get_oracle_prices_with_transformers(oracle, price_ids, age, underlying_oracle)
                .await;
        }

        // Standard Pyth oracle — fetch from Hermes HTTP API
        self.fetch_pyth_prices_from_hermes(price_ids)
            .await
            .ok_or_else(|| {
                LiquidatorError::PriceFetchError(crate::rpc::RpcError::WrongResponseKind(format!(
                    "Failed to fetch Pyth prices from Hermes for oracle {oracle}"
                )))
            })
    }

    // ── LST oracle ───────────────────────────────────────────────────────────

    /// Fetches prices from LST oracle by calling underlying Pyth oracle and applying transformers.
    #[tracing::instrument(skip(self), level = "debug")]
    #[allow(clippy::too_many_lines)]
    async fn get_oracle_prices_with_transformers(
        &self,
        lst_oracle: AccountId,
        price_ids: &[PriceIdentifier],
        age: u32,
        underlying_oracle: AccountId,
    ) -> LiquidatorResult<OracleResponse> {
        tracing::info!(
            oracle = %lst_oracle,
            underlying = %underlying_oracle,
            "Fetching LST oracle prices with transformers"
        );

        // Get transformers for each price ID
        let mut transformers: HashMap<PriceIdentifier, PriceTransformer> = HashMap::new();
        let mut underlying_price_ids: Vec<PriceIdentifier> = Vec::new();

        for &price_id in price_ids {
            match self
                .client
                .read(lst_oracle::GetTransformer {
                    oracle_id: lst_oracle.clone(),
                    price_identifier: price_id,
                })
                .await
            {
                Ok(result) => {
                    if let Some(transformer) = result.transformer {
                        tracing::debug!(
                            price_id = ?price_id,
                            underlying_id = ?transformer.price_id,
                            "Found price transformer"
                        );
                        underlying_price_ids.push(transformer.price_id);
                        transformers.insert(price_id, transformer);
                    } else {
                        tracing::debug!(price_id = ?price_id, "No transformer, using price ID as-is");
                        underlying_price_ids.push(price_id);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        price_id = ?price_id,
                        error = %e,
                        "Failed to get transformer, skipping market"
                    );
                    return Ok(HashMap::new());
                }
            }
        }

        tracing::debug!(
            underlying_oracle = %underlying_oracle,
            underlying_price_ids = ?underlying_price_ids,
            "Fetching prices from underlying Pyth oracle"
        );

        // Fetch prices from underlying Pyth oracle
        let mut underlying_prices =
            Box::pin(self.get_oracle_prices(underlying_oracle.clone(), &underlying_price_ids, age))
                .await?;

        if underlying_prices.is_empty() {
            tracing::warn!("Underlying oracle returned no prices, skipping market");
            return Ok(HashMap::new());
        }

        // Apply transformers to get final prices
        let mut final_prices: OracleResponse = HashMap::new();

        for (&original_price_id, transformer) in &transformers {
            if let Some(Some(underlying_price)) = underlying_prices.remove(&transformer.price_id) {
                // Fetch the input value for transformation
                match self.fetch_transformer_input(&transformer.call).await {
                    Ok(input) => {
                        if let Some(transformed_price) =
                            transformer.action.apply(underlying_price, input)
                        {
                            tracing::debug!(
                                price_id = ?original_price_id,
                                "Successfully transformed price"
                            );
                            final_prices.insert(original_price_id, Some(transformed_price));
                        } else {
                            tracing::warn!(
                                price_id = ?original_price_id,
                                "Price transformation returned None"
                            );
                            final_prices.insert(original_price_id, None);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            price_id = ?original_price_id,
                            error = %e,
                            "Failed to fetch transformer input"
                        );
                        final_prices.insert(original_price_id, None);
                    }
                }
            } else {
                tracing::warn!(
                    price_id = ?original_price_id,
                    underlying_id = ?transformer.price_id,
                    "Underlying price not found in oracle response"
                );
                final_prices.insert(original_price_id, None);
            }
        }

        // Add prices that didn't need transformation
        for &price_id in price_ids {
            if !transformers.contains_key(&price_id) {
                if let Some(price) = underlying_prices.remove(&price_id) {
                    final_prices.insert(price_id, price);
                }
            }
        }

        tracing::info!(
            oracle = %lst_oracle,
            price_count = final_prices.len(),
            "Successfully fetched and transformed LST oracle prices"
        );

        Ok(final_prices)
    }

    // ── Proxy oracle ─────────────────────────────────────────────────────────

    /// Resolves each feed's off-chain pricing plan from the proxy's on-chain
    /// source config (`GetProxy`, a free view call). Feeds whose config can't
    /// be read or whose sources can't be priced off-chain are omitted.
    async fn resolve_offchain_plans(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> Vec<(PriceIdentifier, OffchainPriceSource)> {
        let mut plans = Vec::new();
        for &price_id in price_ids {
            let result = self
                .client
                .read(proxy_oracle::GetProxy::new(oracle.clone(), price_id))
                .await;
            match result {
                Ok(result) => {
                    let Some(proxy) = result.proxy else {
                        tracing::debug!(%oracle, ?price_id, "Proxy has no entry for price id");
                        continue;
                    };
                    if let Some(plan) = plan_offchain_source(proxy.sources()) {
                        plans.push((price_id, plan));
                    } else {
                        tracing::debug!(
                            %oracle,
                            ?price_id,
                            "No off-chain pricable source for feed (e.g. Lazer-only), deferring to on-chain cache"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(%oracle, ?price_id, %error, "Failed to read proxy config");
                }
            }
        }
        plans
    }

    /// Composes proxy-oracle prices off-chain from each feed's configured
    /// primary source: Pyth via Hermes, RedStone via the public price API,
    /// transformers applied with their on-chain input (a free view call).
    ///
    /// Returns only the feeds it could price — the caller falls back to the
    /// on-chain cache read for the rest. Costs no gas anywhere: proxy configs
    /// and transformer inputs are view calls, prices are HTTP.
    async fn compose_proxy_prices_offchain(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
        max_age_secs: u32,
    ) -> OracleResponse {
        let plans = self.resolve_offchain_plans(oracle, price_ids).await;
        if plans.is_empty() {
            return OracleResponse::new();
        }

        // Batch the underlying fetches: one Hermes call, one RedStone call.
        // Set-based dedup; the order of a batch request carries no meaning.
        let mut pyth_id_set: HashSet<PriceIdentifier> = HashSet::new();
        let mut redstone_symbol_set: HashSet<String> = HashSet::new();
        for (_, plan) in &plans {
            let request = match plan {
                OffchainPriceSource::Direct(request)
                | OffchainPriceSource::Transformed { request, .. } => request,
            };
            match request {
                OffchainRequest::Pyth(id) => {
                    pyth_id_set.insert(*id);
                }
                OffchainRequest::RedStone(symbol) => {
                    redstone_symbol_set.insert(symbol.clone());
                }
            }
        }
        let pyth_ids: Vec<PriceIdentifier> = pyth_id_set.into_iter().collect();
        let redstone_symbols: Vec<String> = redstone_symbol_set.into_iter().collect();
        let pyth_prices = if pyth_ids.is_empty() {
            OracleResponse::new()
        } else {
            self.fetch_pyth_prices_from_hermes(&pyth_ids)
                .await
                .unwrap_or_default()
        };
        let redstone_prices = self
            .redstone_api
            .get_prices(&redstone_symbols, max_age_secs)
            .await;

        // Compose each feed, applying its transformer when one is configured.
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs().cast_signed());
        let mut response = OracleResponse::new();
        for (price_id, plan) in plans {
            let (request, transform) = match plan {
                OffchainPriceSource::Direct(request) => (request, None),
                OffchainPriceSource::Transformed {
                    request,
                    call,
                    action,
                } => (request, Some((call, action))),
            };
            let underlying = match &request {
                // Freshness enforced here (the RedStone leg's client applies
                // the same guards): a stale entry is unpriced and falls
                // through to the on-chain cache read.
                OffchainRequest::Pyth(id) => {
                    pyth_prices.get(id).cloned().flatten().filter(|price| {
                        let fresh = publish_time_is_fresh(
                            price.publish_time.as_secs(),
                            now_secs,
                            max_age_secs,
                        );
                        if !fresh {
                            tracing::debug!(
                                %oracle,
                                ?price_id,
                                publish_time = price.publish_time.as_secs(),
                                "Composed Pyth price is stale or future-dated, deferring to on-chain cache"
                            );
                        }
                        fresh
                    })
                }
                OffchainRequest::RedStone(symbol) => redstone_prices.get(symbol).cloned(),
            };
            let Some(underlying) = underlying else {
                tracing::debug!(%oracle, ?price_id, ?request, "Underlying source returned no usable price");
                continue;
            };
            let price = match transform {
                None => Some(underlying),
                Some((call, action)) => match self.fetch_transformer_input(&call).await {
                    Ok(input) => action.apply(underlying, input),
                    Err(error) => {
                        tracing::warn!(%oracle, ?price_id, %error, "Failed to fetch transformer input");
                        None
                    }
                },
            };
            if let Some(price) = price {
                response.insert(price_id, Some(price));
            }
        }

        if !response.is_empty() {
            tracing::debug!(
                %oracle,
                composed = response.len(),
                requested = price_ids.len(),
                "Composed proxy prices off-chain"
            );
        }
        response
    }

    /// Fetches prices from a proxy oracle cache.
    ///
    /// Proxy oracle aggregation, circuit-breaker evaluation, and cache writes happen in
    /// the proxy contract's `update_prices` flow. This read path intentionally does not
    /// re-run proxy logic off-chain because that would bypass on-chain breaker state.
    #[tracing::instrument(skip(self), level = "debug")]
    async fn get_proxy_oracle_prices(
        &self,
        proxy_oracle: AccountId,
        price_ids: &[PriceIdentifier],
        age: u32,
    ) -> LiquidatorResult<OracleResponse> {
        let result = self
            .client
            .read(pyth_spec::ListEmaPricesNoOlderThan {
                oracle_id: proxy_oracle,
                price_ids: price_ids.to_vec(),
                age: u64::from(age),
            })
            .await
            .map_err(|e| LiquidatorError::PriceFetchError(e.into()))?;

        Ok(result
            .prices
            .into_iter()
            .map(|entry| (entry.price_id, entry.price))
            .collect())
    }

    // ── Transformers ─────────────────────────────────────────────────────────

    /// Fetches the input value needed for price transformation (e.g., LST
    /// redemption rate). Also used when composing proxy prices off-chain —
    /// it is a free view call, not a transaction.
    async fn fetch_transformer_input(&self, call: &Call) -> Result<Decimal, RpcError> {
        let result = self
            .client
            .read(contract::ViewFunction {
                // `near_sdk::AccountId` is a re-export of `near_account_id::AccountId`
                // (the type the spec uses), so no conversion is needed.
                contract_id: call.account_id.clone(),
                method_name: ContractMethodName(call.method_name.clone()),
                args: ContractArgs::Raw(Base64Bytes(call.args.0.clone())),
            })
            .await
            .map_err(RpcError::from)?;

        let value: Decimal =
            near_sdk::serde_json::from_value(result.value).map_err(RpcError::DeserializeError)?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_proxy_oracle_near_common::{
        input::ProxyPriceTransformer, price_transformer::Call, request::OracleRequest,
    };

    fn pyth_source() -> Source {
        Source::Request(OracleRequest::pyth(
            "pyth-oracle.near".parse().unwrap(),
            PriceIdentifier([0xAA; 32]),
        ))
    }

    fn redstone_source() -> Source {
        Source::Request(OracleRequest::redstone(
            "redstone.near".parse().unwrap(),
            "LTC",
        ))
    }

    fn lazer_source() -> Source {
        Source::Request(OracleRequest::lazer("pyth-lazer.near".parse().unwrap(), 7))
    }

    fn transformer_source(inner: OracleRequest) -> Source {
        let rate_contract: AccountId = "meta-pool.near".parse().unwrap();
        Source::Transformer(ProxyPriceTransformer::lst(
            inner,
            24,
            Call::new_simple(&rate_contract, "get_st_near_price"),
        ))
    }

    #[test]
    fn plan_picks_the_first_offchain_pricable_source() {
        let plan = plan_offchain_source([&pyth_source(), &redstone_source()].into_iter())
            .expect("pyth source is pricable off-chain");
        assert_eq!(
            plan,
            OffchainPriceSource::Direct(OffchainRequest::Pyth(PriceIdentifier([0xAA; 32])))
        );
    }

    #[test]
    fn plan_skips_lazer_sources() {
        // Lazer feeds live only in their on-chain adapter — no off-chain API —
        // so the planner must pass over them to the next source rather than
        // failing the whole feed.
        let plan = plan_offchain_source([&lazer_source(), &redstone_source()].into_iter())
            .expect("redstone source is pricable off-chain");
        assert_eq!(
            plan,
            OffchainPriceSource::Direct(OffchainRequest::RedStone("LTC".to_string()))
        );
    }

    #[test]
    fn plan_returns_none_when_no_source_is_pricable() {
        assert!(plan_offchain_source([&lazer_source()].into_iter()).is_none());
        assert!(plan_offchain_source([].into_iter()).is_none());
    }

    #[test]
    fn plan_carries_transformers_over_pricable_inners() {
        let inner = OracleRequest::pyth(
            "pyth-oracle.near".parse().unwrap(),
            PriceIdentifier([0xBB; 32]),
        );
        let plan = plan_offchain_source([&transformer_source(inner)].into_iter())
            .expect("transformer over pyth is pricable off-chain");
        match plan {
            OffchainPriceSource::Transformed { request, .. } => {
                assert_eq!(request, OffchainRequest::Pyth(PriceIdentifier([0xBB; 32])));
            }
            other @ OffchainPriceSource::Direct(_) => panic!("expected Transformed, got {other:?}"),
        }
    }

    /// Composed Pyth prices must honor the market's freshness bound exactly
    /// like the RedStone leg and the on-chain cache read they replace: a
    /// stale (or implausibly future-dated) Hermes entry must fall through to
    /// the on-chain fallback, never price a position off an hour-old number.
    #[test]
    fn publish_time_freshness_matches_the_markets_bound() {
        let now = 1_755_600_000_i64;
        // Fresh, and exactly at the bound: usable.
        assert!(publish_time_is_fresh(now - 10, now, 120));
        assert!(publish_time_is_fresh(now - 120, now, 120));
        // One second past the bound: stale.
        assert!(!publish_time_is_fresh(now - 121, now, 120));
        // Slight future skew is clock drift; beyond the allowance it is
        // implausible and must be refused (a negative age passes every
        // staleness bound there is).
        assert!(publish_time_is_fresh(now + 5, now, 120));
        assert!(!publish_time_is_fresh(now + 31, now, 120));
    }

    #[test]
    fn plan_skips_transformers_over_lazer_inners() {
        let inner = OracleRequest::lazer("pyth-lazer.near".parse().unwrap(), 9);
        let plan =
            plan_offchain_source([&transformer_source(inner), &redstone_source()].into_iter())
                .expect("falls through to the redstone source");
        assert_eq!(
            plan,
            OffchainPriceSource::Direct(OffchainRequest::RedStone("LTC".to_string()))
        );
    }
}
