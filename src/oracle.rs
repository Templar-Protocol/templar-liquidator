//! Oracle price fetching module.
//!
//! Handles fetching prices from various oracle types including:
//! - Pyth oracles (via Hermes HTTP API)
//! - LST oracles with price transformers
//! - Proxy oracles — composed off-chain at scan time from each feed's
//!   configured source (Hermes for Pyth sources, the RedStone public price
//!   API via [`crate::redstone`] for RedStone sources, a free adapter view
//!   read for Lazer sources, transformer inputs by free view call), with the
//!   proxy's on-chain price cache as fallback for feeds whose leg fails or
//!   reads stale. Every composed price is bounded by the market's freshness
//!   window before use.
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

/// A proxy source request the bot can price at scan time without the proxy's
/// own on-chain cache: Pyth via Hermes, RedStone via the public price API
/// ([`crate::redstone`]), Lazer via a free view read of its adapter contract.
/// The Lazer adapter is still a push-fed store — the view read prices the
/// feed only while someone maintains those pushes; a stale adapter fails
/// freshness and falls back to the proxy cache like every other leg.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OffchainRequest {
    Pyth(PriceIdentifier),
    RedStone(String),
    Lazer { oracle_id: AccountId, feed_id: u32 },
}

/// The batched backend results one composition round prices its candidates
/// from: one Hermes call, one RedStone call, one view read per Lazer adapter.
struct FetchedBackends {
    pyth: OracleResponse,
    redstone: HashMap<String, pyth::Price>,
    lazer: HashMap<(AccountId, u32), pyth::Price>,
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
    // Checked: a timestamp extreme enough to overflow the subtraction is
    // upstream junk and reads as not-fresh, never a panic or wrap.
    let Some(age_secs) = now_secs.checked_sub(publish_time_secs) else {
        return false;
    };
    age_secs <= i64::from(max_age_secs) && age_secs >= -(crate::redstone::MAX_FUTURE_SKEW_MS / 1000)
}

/// Seconds since the epoch, `0` if the clock predates it — which makes every
/// quote look future-dated and fail freshness, the safe (fail-closed)
/// direction for a pricing path.
fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed())
}

/// Projects a stored Lazer feed to its EMA price — the same projection the
/// on-chain proxy's `Lazer` source consumes, and consistent with the Hermes
/// leg (which also feeds EMA) — rejecting it under the market's freshness
/// bound like every other composed leg. The adapter applies no age filter on
/// reads, so the bound is enforced entirely here.
fn lazer_feed_to_fresh_price(
    feed: &templar_common::oracle::lazer::FeedData,
    now_secs: i64,
    max_age_secs: u32,
) -> Option<pyth::Price> {
    feed.to_ema_price()
        .filter(|price| publish_time_is_fresh(price.publish_time.as_secs(), now_secs, max_age_secs))
}

/// True when every requested feed has a `Some` price in the response.
///
/// Deliberately a caller-side decision, not enforced inside
/// [`OracleFetcher::get_oracle_prices`]: the market scan needs its full feed
/// pair and gates on this (a partial pair reaches per-position status checks
/// and fails each with "Missing price"), while batch callers pricing many
/// unrelated feeds tolerate partial responses per-asset — one stale feed must
/// not blank the rest of the batch.
pub(crate) fn covers_all(response: &OracleResponse, price_ids: &[PriceIdentifier]) -> bool {
    price_ids
        .iter()
        .all(|id| matches!(response.get(id), Some(Some(_))))
}

/// Drops entries whose publish time falls outside the market's freshness
/// bound. `None` entries pass through — they already mean "no price".
fn retain_fresh(response: &mut OracleResponse, now_secs: i64, max_age_secs: u32) {
    response.retain(|_, price| {
        price
            .as_ref()
            .is_none_or(|p| publish_time_is_fresh(p.publish_time.as_secs(), now_secs, max_age_secs))
    });
}

/// Classifies an [`OracleRequest`] as its scan-time pricing request. Every
/// source kind classifies; what varies is where the price comes from.
fn classify_offchain_request(request: &OracleRequest) -> OffchainRequest {
    match request {
        OracleRequest::Pyth(req) => OffchainRequest::Pyth(req.price_id),
        OracleRequest::RedStone(req) => OffchainRequest::RedStone(req.price_id.to_string()),
        OracleRequest::Lazer(req) => OffchainRequest::Lazer {
            oracle_id: req.oracle_id.clone(),
            feed_id: req.feed_id,
        },
    }
}

/// Deduplicates the plans' underlying requests into one want-list per
/// backend. Set-based; the order of a batch request carries no meaning.
#[allow(clippy::type_complexity)]
fn collect_offchain_wants(
    plans: &[(PriceIdentifier, Vec<OffchainPriceSource>)],
) -> (
    Vec<PriceIdentifier>,
    Vec<String>,
    HashMap<AccountId, HashSet<u32>>,
) {
    let mut pyth_id_set: HashSet<PriceIdentifier> = HashSet::new();
    let mut redstone_symbol_set: HashSet<String> = HashSet::new();
    let mut lazer_wanted: HashMap<AccountId, HashSet<u32>> = HashMap::new();
    for plan in plans.iter().flat_map(|(_, candidates)| candidates) {
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
            OffchainRequest::Lazer { oracle_id, feed_id } => {
                lazer_wanted
                    .entry(oracle_id.clone())
                    .or_default()
                    .insert(*feed_id);
            }
        }
    }
    (
        pyth_id_set.into_iter().collect(),
        redstone_symbol_set.into_iter().collect(),
        lazer_wanted,
    )
}

/// Maps the proxy's sources, in configured order, to scan-time pricing
/// candidates. Composition tries them in order and takes the first leg that
/// yields a fresh price — whether a given leg is usable (a Lazer adapter's
/// staleness, a Hermes outage) is only knowable after its fetch, so the
/// fall-through lives at composition time, not here.
///
/// The proxy's own aggregation and circuit breakers are on-chain policy the
/// scan does not replicate (and the kernel keeps them private), which is safe
/// because scan-time prices are advisory — execution still pushes fresh prices
/// on-chain and the market contract re-validates against its own oracle.
pub(crate) fn plan_offchain_sources<'a>(
    sources: impl Iterator<Item = &'a Source>,
) -> Vec<OffchainPriceSource> {
    sources
        .map(|source| match source {
            Source::Request(request) => {
                OffchainPriceSource::Direct(classify_offchain_request(request))
            }
            Source::Transformer(transformer) => OffchainPriceSource::Transformed {
                request: classify_offchain_request(&transformer.request),
                call: transformer.call.clone(),
                action: transformer.action.clone(),
            },
        })
        .collect()
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
    /// Lazer (Pyth Pro) price API, for composing Lazer-sourced proxy prices
    /// off-chain at scan time. `None` when no access token is configured —
    /// the Lazer leg then reads the on-chain adapter instead.
    lazer_api: Option<crate::lazer::LazerApiClient>,
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
        lazer_api: Option<(Url, String)>,
        proxy_oracle_cache: Option<ProxyOracleCache>,
    ) -> Self {
        let http_client = reqwest::Client::new();
        Self {
            client,
            pyth_updates,
            lst_oracle_cache: std::sync::Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            proxy_oracle_cache: proxy_oracle_cache.unwrap_or_else(|| {
                std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashSet::new()))
            }),
            hermes_url,
            redstone_api: crate::redstone::RedStoneApiClient::new(redstone_api_url),
            lazer_api: lazer_api.map(|(url, token)| {
                crate::lazer::LazerApiClient::new(http_client.clone(), url, token)
            }),
            http_client,
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

        // Standard Pyth oracle — fetch from Hermes HTTP API. Hermes carries
        // the publisher's timestamp verbatim, so the market's freshness bound
        // is applied here, as on every other pricing path; a fully-stale
        // response comes back empty and the caller skips the market.
        let mut response = self
            .fetch_pyth_prices_from_hermes(price_ids)
            .await
            .ok_or_else(|| {
                LiquidatorError::PriceFetchError(crate::rpc::RpcError::WrongResponseKind(format!(
                    "Failed to fetch Pyth prices from Hermes for oracle {oracle}"
                )))
            })?;
        retain_fresh(&mut response, unix_now_secs(), age);
        Ok(response)
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
                    // Surface the failure — an empty Ok would read as "no
                    // prices" and hide that the transformer read broke.
                    tracing::warn!(
                        price_id = ?price_id,
                        error = %e,
                        "Failed to get transformer"
                    );
                    return Err(LiquidatorError::PriceFetchError(e.into()));
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

    /// Resolves each feed's ordered pricing candidates from the proxy's
    /// on-chain source config (`GetProxy`, a free view call). Feeds are
    /// omitted only when their config can't be read, the proxy has no entry,
    /// or the entry lists no sources.
    async fn resolve_offchain_plans(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> Vec<(PriceIdentifier, Vec<OffchainPriceSource>)> {
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
                    let candidates = plan_offchain_sources(proxy.sources());
                    if candidates.is_empty() {
                        tracing::debug!(
                            %oracle,
                            ?price_id,
                            "Feed has no configured sources, deferring to on-chain cache"
                        );
                    } else {
                        plans.push((price_id, candidates));
                    }
                }
                Err(error) => {
                    tracing::warn!(%oracle, ?price_id, %error, "Failed to read proxy config");
                }
            }
        }
        plans
    }

    /// Prices the wanted Lazer feeds: from the Lazer price API when an
    /// access token is configured (fresh, independent of anyone pushing the
    /// adapter), then the on-chain adapter view read for whatever the API
    /// didn't cover. Both legs enforce the market's freshness bound; a feed
    /// neither leg can price falls through to the feed's next source.
    async fn fetch_lazer_prices(
        &self,
        mut wanted: HashMap<AccountId, HashSet<u32>>,
        max_age_secs: u32,
    ) -> HashMap<(AccountId, u32), pyth::Price> {
        let mut prices = HashMap::new();
        if let Some(api) = &self.lazer_api {
            let all_ids: Vec<u32> = wanted
                .values()
                .flat_map(|ids| ids.iter().copied())
                .collect::<HashSet<u32>>()
                .into_iter()
                .collect();
            let api_prices = api
                .get_ema_prices(&all_ids, unix_now_secs(), max_age_secs)
                .await;
            for (adapter, feed_ids) in &mut wanted {
                feed_ids.retain(|feed_id| match api_prices.get(feed_id) {
                    Some(price) => {
                        prices.insert((adapter.clone(), *feed_id), price.clone());
                        false
                    }
                    None => true,
                });
            }
            wanted.retain(|_, feed_ids| !feed_ids.is_empty());
        }
        prices.extend(self.fetch_lazer_adapter_prices(wanted, max_age_secs).await);
        prices
    }

    /// View-reads each wanted Lazer adapter once and projects its stored
    /// feeds to prices. Freshness is enforced here (the adapter applies no
    /// age filter on reads); a stale or absent feed is simply unpriced and
    /// falls back to the feed's next source, then the proxy's on-chain cache.
    async fn fetch_lazer_adapter_prices(
        &self,
        wanted: HashMap<AccountId, HashSet<u32>>,
        max_age_secs: u32,
    ) -> HashMap<(AccountId, u32), pyth::Price> {
        let mut prices = HashMap::new();
        for (adapter, feed_ids) in wanted {
            let feed_ids: Vec<u32> = feed_ids.into_iter().collect();
            match self
                .client
                .read(templar_gateway_methods_spec::lazer::GetFeedsData {
                    oracle_id: adapter.clone(),
                    feed_ids,
                })
                .await
            {
                Ok(result) => {
                    // Clock sampled after the read, so in-flight view-call
                    // latency counts against the feed's age.
                    let now_secs = unix_now_secs();
                    for (feed_id, feed) in result.feeds {
                        match feed
                            .as_ref()
                            .and_then(|f| lazer_feed_to_fresh_price(f, now_secs, max_age_secs))
                        {
                            Some(price) => {
                                prices.insert((adapter.clone(), feed_id), price);
                            }
                            None => {
                                tracing::debug!(
                                    %adapter,
                                    feed_id,
                                    "Lazer adapter feed absent or stale, deferring to on-chain cache"
                                );
                            }
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%adapter, %error, "Failed to read Lazer adapter feeds");
                }
            }
        }
        prices
    }

    /// Prices one candidate from the pre-fetched backends, applying its
    /// transformer when configured. `None` — a missing or stale backend
    /// entry, or a failed transformer — sends the caller to the feed's next
    /// candidate.
    async fn price_one_candidate(
        &self,
        oracle: &AccountId,
        price_id: PriceIdentifier,
        plan: OffchainPriceSource,
        backends: &FetchedBackends,
        max_age_secs: u32,
    ) -> Option<pyth::Price> {
        let (request, transform) = match plan {
            OffchainPriceSource::Direct(request) => (request, None),
            OffchainPriceSource::Transformed {
                request,
                call,
                action,
            } => (request, Some((call, action))),
        };
        let underlying = match &request {
            // Freshness enforced per leg (the RedStone client applies the
            // same guards at parse time; the Lazer leg at read time), plus
            // once more at consumption in the caller.
            OffchainRequest::Pyth(id) => backends.pyth.get(id).cloned().flatten().filter(|price| {
                publish_time_is_fresh(price.publish_time.as_secs(), unix_now_secs(), max_age_secs)
            }),
            OffchainRequest::RedStone(symbol) => backends.redstone.get(symbol).cloned(),
            OffchainRequest::Lazer { oracle_id, feed_id } => {
                backends.lazer.get(&(oracle_id.clone(), *feed_id)).cloned()
            }
        };
        let Some(underlying) = underlying else {
            tracing::debug!(%oracle, ?price_id, ?request, "Candidate source has no usable price, trying next");
            return None;
        };
        match transform {
            None => Some(underlying),
            Some((call, action)) => match self.fetch_transformer_input(&call).await {
                Ok(input) => action.apply(underlying, input),
                Err(error) => {
                    tracing::warn!(%oracle, ?price_id, %error, "Failed to fetch transformer input");
                    None
                }
            },
        }
    }

    /// Composes proxy-oracle prices off-chain from each feed's configured
    /// sources, in order — the first candidate yielding a fresh price wins:
    /// Pyth via Hermes, RedStone via the public price API, Lazer via its
    /// adapter view read, transformers applied with their on-chain input (a
    /// free view call).
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

        // Batch the underlying fetches: one Hermes call, one RedStone call,
        // one adapter view read per Lazer adapter account. Every candidate of
        // every feed is fetched up front, so falling through to a later
        // candidate below costs no extra backend round-trip.
        let (pyth_ids, redstone_symbols, lazer_wanted) = collect_offchain_wants(&plans);
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
        let lazer_prices = self.fetch_lazer_prices(lazer_wanted, max_age_secs).await;
        let backends = FetchedBackends {
            pyth: pyth_prices,
            redstone: redstone_prices,
            lazer: lazer_prices,
        };

        // Compose each feed from its first candidate that yields a price —
        // a leg's usability (stale Lazer adapter, Hermes outage) is only
        // knowable here, so this is where source order falls through.
        let mut response = OracleResponse::new();
        for (price_id, candidates) in plans {
            let mut composed = None;
            for plan in candidates {
                if let Some(price) = self
                    .price_one_candidate(oracle, price_id, plan, &backends, max_age_secs)
                    .await
                {
                    composed = Some(price);
                    break;
                }
            }
            // Fail-closed at consumption: the backend fetches and transformer
            // view calls take real time, so a price that aged past the bound
            // while they ran must not be inserted on the strength of an
            // earlier clock sample.
            let now_secs = unix_now_secs();
            match composed
                .filter(|p| publish_time_is_fresh(p.publish_time.as_secs(), now_secs, max_age_secs))
            {
                Some(price) => {
                    response.insert(price_id, Some(price));
                }
                None => {
                    tracing::debug!(
                        %oracle,
                        ?price_id,
                        "No candidate source yielded a fresh price, deferring to on-chain cache"
                    );
                }
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

    /// The plan preserves the proxy's full source order. Composition tries
    /// the candidates in order and takes the first fresh leg — so a feed
    /// configured `[Lazer, Pyth]` with a stale Lazer adapter still prices
    /// from Hermes, instead of dying on its first source and falling to an
    /// on-chain cache a standalone deployment doesn't maintain.
    #[test]
    fn plan_keeps_every_source_in_configured_order() {
        let plans = plan_offchain_sources([&lazer_source(), &redstone_source()].into_iter());
        assert_eq!(
            plans,
            vec![
                OffchainPriceSource::Direct(OffchainRequest::Lazer {
                    oracle_id: "pyth-lazer.near".parse().unwrap(),
                    feed_id: 7,
                }),
                OffchainPriceSource::Direct(OffchainRequest::RedStone("LTC".to_string())),
            ]
        );
    }

    #[test]
    fn plan_classifies_pyth_and_redstone_sources() {
        let plans = plan_offchain_sources([&pyth_source(), &redstone_source()].into_iter());
        assert_eq!(
            plans[0],
            OffchainPriceSource::Direct(OffchainRequest::Pyth(PriceIdentifier([0xAA; 32])))
        );
        assert_eq!(plans.len(), 2);
    }

    #[test]
    fn plan_is_empty_when_no_source_is_configured() {
        assert!(plan_offchain_sources([].into_iter()).is_empty());
    }

    fn lazer_feed(publish_secs: u64) -> templar_common::oracle::lazer::FeedData {
        near_sdk::serde_json::from_str(&format!(
            r#"{{"price":"123456","conf":"50",
                 "ema":{{"price":"120000","conf":"40"}},
                 "expo":-8,"publish_time_ns":"{}"}}"#,
            publish_secs * 1_000_000_000
        ))
        .expect("feed fixture parses")
    }

    /// The Lazer leg projects the EMA price — the same projection the
    /// on-chain proxy's Lazer source consumes, and consistent with the
    /// Hermes leg (which also feeds EMA) — and enforces the market's
    /// freshness bound like every other composed leg.
    #[test]
    fn lazer_feed_projects_fresh_ema_and_rejects_stale() {
        let now = 1_700_000_000_i64;

        let fresh = lazer_feed(1_699_999_990);
        let price = lazer_feed_to_fresh_price(&fresh, now, 60).expect("10s old is fresh");
        assert_eq!(price.price.0, 120_000);
        assert_eq!(price.conf.0, 40);
        assert_eq!(price.expo, -8);

        let stale = lazer_feed(1_699_998_000);
        assert!(
            lazer_feed_to_fresh_price(&stale, now, 60).is_none(),
            "2000s old against a 60s bound must be unpriced"
        );
    }

    #[test]
    fn plan_carries_transformers_over_pricable_inners() {
        let inner = OracleRequest::pyth(
            "pyth-oracle.near".parse().unwrap(),
            PriceIdentifier([0xBB; 32]),
        );
        let plan = plan_offchain_sources([&transformer_source(inner)].into_iter())
            .into_iter()
            .next()
            .expect("transformer over pyth is pricable off-chain");
        match plan {
            OffchainPriceSource::Transformed { request, .. } => {
                assert_eq!(request, OffchainRequest::Pyth(PriceIdentifier([0xBB; 32])));
            }
            other @ OffchainPriceSource::Direct(_) => panic!("expected Transformed, got {other:?}"),
        }
    }

    fn price_at(publish_secs: i64) -> pyth::Price {
        pyth::Price {
            price: near_sdk::json_types::I64(100),
            conf: near_sdk::json_types::U64(0),
            expo: -8,
            publish_time: pyth::PythTimestamp::from_secs(publish_secs),
        }
    }

    /// A partial direct-Pyth response must not escape: with no fallback
    /// path, it would reach per-position status checks and fail each one
    /// with "Missing price" instead of skipping the market once.
    #[test]
    fn covers_all_requires_a_some_price_for_every_requested_feed() {
        let now = 1_755_600_000_i64;
        let mut response: OracleResponse = HashMap::new();
        response.insert(PriceIdentifier([1; 32]), Some(price_at(now)));
        response.insert(PriceIdentifier([2; 32]), Some(price_at(now)));
        let both = [PriceIdentifier([1; 32]), PriceIdentifier([2; 32])];

        assert!(covers_all(&response, &both));
        // A feed answered with None is not covered.
        response.insert(PriceIdentifier([2; 32]), None);
        assert!(!covers_all(&response, &both));
        // An absent feed is not covered.
        response.remove(&PriceIdentifier([2; 32]));
        assert!(!covers_all(&response, &both));
    }

    /// Direct-Pyth responses must be bounded by the market's freshness window
    /// too, not just composed proxy prices: a stale entry is dropped (absent
    /// = unpriced, failing closed at the market view), a fresh one kept, and
    /// a `None` entry passed through unchanged.
    #[test]
    fn retain_fresh_drops_only_stale_entries() {
        let now = 1_755_600_000_i64;
        let mut response: OracleResponse = HashMap::new();
        response.insert(PriceIdentifier([1; 32]), Some(price_at(now - 30)));
        response.insert(PriceIdentifier([2; 32]), Some(price_at(now - 3_000)));
        response.insert(PriceIdentifier([3; 32]), None);

        retain_fresh(&mut response, now, 60);

        assert!(
            response.contains_key(&PriceIdentifier([1; 32])),
            "fresh kept"
        );
        assert!(
            !response.contains_key(&PriceIdentifier([2; 32])),
            "stale dropped"
        );
        assert!(
            response.contains_key(&PriceIdentifier([3; 32])),
            "None passed through"
        );
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
        // A timestamp that overflows the age subtraction is upstream junk —
        // it must read as not-fresh, never panic or wrap.
        assert!(!publish_time_is_fresh(i64::MIN, now, 120));
        assert!(!publish_time_is_fresh(i64::MAX, now, 120));
    }

    /// A transformer over a Lazer inner is fully pricable at scan time: the
    /// transformer input is a view call and the Lazer underlying is an
    /// adapter view read. This is the linear-usdt collateral-feed shape —
    /// the case the Lazer leg exists for.
    #[test]
    fn plan_carries_transformers_over_lazer_inners() {
        let inner = OracleRequest::lazer("pyth-lazer.near".parse().unwrap(), 9);
        let plan =
            plan_offchain_sources([&transformer_source(inner), &redstone_source()].into_iter())
                .into_iter()
                .next()
                .expect("transformer over lazer is pricable off-chain");
        match plan {
            OffchainPriceSource::Transformed { request, .. } => {
                assert_eq!(
                    request,
                    OffchainRequest::Lazer {
                        oracle_id: "pyth-lazer.near".parse().unwrap(),
                        feed_id: 9,
                    }
                );
            }
            other @ OffchainPriceSource::Direct(_) => panic!("expected Transformed, got {other:?}"),
        }
    }
}
