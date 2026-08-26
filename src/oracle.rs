//! Oracle price fetching module.
//!
//! Scan-time prices are **off-chain only**: proxy-oracle feeds are composed
//! from each feed's configured sources in order, taking the first leg that
//! yields a fresh price — the RedStone public price API ([`crate::redstone`])
//! for RedStone sources, the token-gated Pyth Pro price API ([`crate::lazer`])
//! for Pyth Pro sources, transformer inputs by free view call. There is no
//! on-chain price read at scan time: an on-chain price is either stale or
//! costs a paid push every scan. Pyth Core–configured feeds — direct-Pyth and
//! LST oracles, and Pyth Core sources inside a proxy — are priced from the
//! Pyth Pro API over the Pyth Core → Pyth Pro symbol bridge
//! ([`SharedSymbolMap`], built from Pyth Pro's public symbols endpoint and
//! refreshed with the registry); a feed with no Pyth Pro counterpart has no
//! off-chain source. A market whose feeds have no off-chain source is
//! filtered at registration ([`OracleFetcher::offchain_priceable`]), never
//! failed per scan. Every composed price is bounded by the market's
//! freshness window before use.
//!
//! Execution-time pricing is separate: before a live liquidation this module
//! pushes fresh Pyth Pro payloads to each Pyth Pro–sourced adapter and
//! re-aggregates the proxy ([`OracleFetcher::update_onchain_prices`]); the
//! market contract then reads its own on-chain oracle. RedStone adapters are
//! relayer-pushed, not pushed by this bot, and neither is a Pyth Core oracle
//! (its updates are Wormhole VAAs) — a direct-Pyth/LST market relies on an
//! external pusher at execution time, which admission logs.

use near_sdk::AccountId;
use std::collections::{HashMap, HashSet};
use templar_common::{
    oracle::pyth::{self, OracleResponse, PriceIdentifier},
    Decimal,
};
use templar_gateway_client::SigningClient;
use templar_gateway_core::GatewayContext;
use templar_gateway_methods_spec::{contract, lst_oracle, proxy_oracle};
use templar_gateway_oracle_updates_dispatch::{Dispatch as OracleUpdatesDispatch, WithLazerSource};
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

// ── Off-chain proxy price composition ────────────────────────────────────────

/// A proxy source request the bot can price at scan time, off-chain only:
/// RedStone via the public price API ([`crate::redstone`]), Pyth Pro via
/// the token-gated Pyth Pro API ([`crate::lazer`]). There is no on-chain
/// fallback: a stale or failed leg falls through to the feed's next
/// configured source, and a feed no candidate can price stays missing for
/// the caller's coverage gate to judge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OffchainRequest {
    RedStone(String),
    Lazer {
        oracle_id: AccountId,
        feed_id: u32,
        /// Reached over the Pyth Core → Pyth Pro bridge: the source is a
        /// Pyth Core request, whose on-chain price the bot cannot push.
        bridged: bool,
    },
}

/// The batched backend results one composition round prices its candidates
/// from: one RedStone API call and one Pyth Pro API call.
struct FetchedBackends {
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
/// Composed prices must pass this bot-side — the on-chain read they replace
/// enforces the same bound on-chain.
pub(crate) fn publish_time_is_fresh(
    publish_time_secs: i64,
    now_secs: i64,
    max_age_secs: u32,
) -> bool {
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
pub(crate) fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed())
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

/// What scan-time classification needs to know: whether the Pyth Pro API
/// leg exists, and the Pyth Core → Pyth Pro symbol bridge snapshot.
#[derive(Clone, Copy)]
pub(crate) struct OffchainCtx<'a> {
    pub(crate) pyth_pro_api_configured: bool,
    pub(crate) core_to_pro: &'a crate::lazer::PythCoreToProMap,
}

/// Classifies an [`OracleRequest`] as its scan-time pricing request, or
/// `None` when the source has no off-chain leg. Pyth Pro sources need the
/// API token; Pyth Core sources additionally need their price id in the
/// symbol bridge, through which they are priced as Pyth Pro feeds. There is
/// deliberately no on-chain fallback for any of them.
fn classify_offchain_request(
    request: &OracleRequest,
    ctx: &OffchainCtx<'_>,
) -> Option<OffchainRequest> {
    match request {
        OracleRequest::RedStone(req) => Some(OffchainRequest::RedStone(req.price_id.to_string())),
        OracleRequest::Lazer(req) if ctx.pyth_pro_api_configured => Some(OffchainRequest::Lazer {
            oracle_id: req.oracle_id.clone(),
            feed_id: req.feed_id,
            bridged: false,
        }),
        OracleRequest::Pyth(req) if ctx.pyth_pro_api_configured => ctx
            .core_to_pro
            .get(&req.price_id)
            .map(|&feed_id| OffchainRequest::Lazer {
                oracle_id: req.oracle_id.clone(),
                feed_id,
                bridged: true,
            }),
        OracleRequest::Pyth(_) | OracleRequest::Lazer(_) => None,
    }
}

/// One Pyth Core–priced feed of a non-proxy oracle: the market's price id,
/// the Pyth Core id actually priced (the same id for a direct-Pyth oracle;
/// the transformer's underlying for an LST oracle), and the transformer
/// applied on top, if any.
#[derive(Debug, Clone)]
pub(crate) struct PythCoreFeed {
    pub(crate) market_id: PriceIdentifier,
    pub(crate) underlying_id: PriceIdentifier,
    pub(crate) transformer: Option<PriceTransformer>,
}

/// Plans a non-proxy oracle's feeds as off-chain candidates over the symbol
/// bridge, keyed by the market's own price id. An unmapped underlying yields
/// an empty candidate list so admission can name it.
pub(crate) fn plan_pyth_core_feeds(
    pyth_oracle: &AccountId,
    feeds: Vec<PythCoreFeed>,
    ctx: &OffchainCtx<'_>,
) -> Vec<(PriceIdentifier, Vec<OffchainPriceSource>)> {
    feeds
        .into_iter()
        .map(|feed| {
            let request = OracleRequest::pyth(pyth_oracle.clone(), feed.underlying_id);
            let candidates = match (classify_offchain_request(&request, ctx), feed.transformer) {
                (Some(request), None) => vec![OffchainPriceSource::Direct(request)],
                (Some(request), Some(transformer)) => vec![OffchainPriceSource::Transformed {
                    request,
                    call: transformer.call,
                    action: transformer.action,
                }],
                (None, _) => Vec::new(),
            };
            (feed.market_id, candidates)
        })
        .collect()
}

/// Admission of Pyth Core–priced feeds (direct-Pyth and LST oracles): names
/// what is missing — the token, the symbol map, or the specific unmapped
/// price id — so "get a token" and "this asset is not on Pyth Pro" read
/// differently in the log.
pub(crate) fn pyth_core_admission(
    underlying_ids: &[PriceIdentifier],
    pyth_pro_api_configured: bool,
    core_to_pro: &crate::lazer::PythCoreToProMap,
) -> Option<String> {
    if !pyth_pro_api_configured {
        return Some(
            "Pyth Pro API not configured (LAZER_API_TOKEN): Pyth Core price ids cannot be priced off-chain"
                .to_string(),
        );
    }
    if core_to_pro.is_empty() {
        return Some(
            "Pyth Pro symbol map unavailable (retried at the next registry refresh)".to_string(),
        );
    }
    underlying_ids
        .iter()
        .find(|id| !core_to_pro.contains_key(id))
        .map(|id| {
            format!(
                "no Pyth Pro feed for Pyth Core price id {}",
                hex::encode(id.0)
            )
        })
}

/// Deduplicates the plans' underlying requests into one want-list per
/// backend. Set-based; the order of a batch request carries no meaning.
fn collect_offchain_wants(
    plans: &[(PriceIdentifier, Vec<OffchainPriceSource>)],
) -> (Vec<String>, HashMap<AccountId, HashSet<u32>>) {
    let mut redstone_symbol_set: HashSet<String> = HashSet::new();
    let mut lazer_wanted: HashMap<AccountId, HashSet<u32>> = HashMap::new();
    for plan in plans.iter().flat_map(|(_, candidates)| candidates) {
        let request = match plan {
            OffchainPriceSource::Direct(request)
            | OffchainPriceSource::Transformed { request, .. } => request,
        };
        match request {
            OffchainRequest::RedStone(symbol) => {
                redstone_symbol_set.insert(symbol.clone());
            }
            OffchainRequest::Lazer {
                oracle_id, feed_id, ..
            } => {
                lazer_wanted
                    .entry(oracle_id.clone())
                    .or_default()
                    .insert(*feed_id);
            }
        }
    }
    (redstone_symbol_set.into_iter().collect(), lazer_wanted)
}

/// Maps the proxy's sources, in configured order, to scan-time pricing
/// candidates. Composition tries them in order and takes the first leg that
/// yields a fresh price — whether a given leg is usable (a Lazer adapter's
/// staleness, an API outage) is only knowable after its fetch, so the
/// fall-through lives at composition time, not here.
///
/// The proxy's own aggregation and circuit breakers are on-chain policy the
/// scan does not replicate (and the kernel keeps them private), which is safe
/// because scan-time prices are advisory — execution still pushes fresh prices
/// on-chain and the market contract re-validates against its own oracle.
pub(crate) fn plan_offchain_sources<'a>(
    sources: impl Iterator<Item = &'a Source>,
    ctx: &OffchainCtx<'_>,
) -> Vec<OffchainPriceSource> {
    sources
        .filter_map(|source| match source {
            Source::Request(request) => {
                classify_offchain_request(request, ctx).map(OffchainPriceSource::Direct)
            }
            Source::Transformer(transformer) => {
                classify_offchain_request(&transformer.request, ctx).map(|request| {
                    OffchainPriceSource::Transformed {
                        request,
                        call: transformer.call.clone(),
                        action: transformer.action.clone(),
                    }
                })
            }
        })
        .collect()
}

/// The pure half of [`OracleFetcher::offchain_priceable`]: every requested
/// feed must have at least one composable candidate.
pub(crate) fn offchain_admission(
    price_ids: &[PriceIdentifier],
    plans: &[(PriceIdentifier, Vec<OffchainPriceSource>)],
) -> Option<&'static str> {
    let all_covered = price_ids.iter().all(|price_id| {
        plans
            .iter()
            .any(|(id, candidates)| id == price_id && !candidates.is_empty())
    });
    if all_covered {
        None
    } else {
        Some("no off-chain price source for a feed")
    }
}

// ── Shared types ─────────────────────────────────────────────────────────────

/// A gateway client bound to the oracle-updates dispatcher, over a context
/// carrying the in-process Pyth Pro websocket payload source that
/// `oracle.updateLazer` fetches from.
pub type PythProUpdatesClient =
    SigningClient<OracleUpdatesDispatch, WithLazerSource<GatewayContext>>;

/// Shared cache of proxy-oracle probe verdicts (`true` = proxy). Negative
/// verdicts are cached too: a direct-Pyth or LST market is scanned every
/// round, and re-probing `list_proxies` each time would be a view call per
/// market per scan for an answer that does not change.
pub type ProxyOracleCache = std::sync::Arc<tokio::sync::RwLock<HashMap<AccountId, bool>>>;

/// Shared Pyth Core → Pyth Pro symbol bridge, refreshed at each registry
/// refresh and read by every per-market fetcher.
pub type SharedSymbolMap = std::sync::Arc<tokio::sync::RwLock<crate::lazer::PythCoreToProMap>>;

/// What kind of oracle a market was admitted with — decides whether the bot
/// can push prices for it before a liquidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OracleKind {
    /// Proxy oracle: composed off-chain, Pyth Pro adapters pushed by the bot.
    Proxy,
    /// Direct Pyth Core oracle: priced off-chain over the symbol bridge;
    /// the bot cannot push to it (Pyth Core needs Wormhole VAAs), so
    /// execution relies on the on-chain price being kept fresh externally.
    DirectPyth,
    /// LST oracle over a Pyth Core underlying: same as `DirectPyth`, with the
    /// transformer applied off-chain.
    Lst,
}

/// A market's admission verdict: its oracle kind, and how many of its feeds
/// are priced over the Pyth Core → Pyth Pro bridge — feeds whose on-chain
/// Pyth Core price the bot cannot push (Pyth Core updates are Wormhole
/// VAAs), so a liquidation relies on an external pusher for them. Every feed
/// of a `DirectPyth`/`Lst` oracle is bridged; a proxy feed is bridged when
/// it is sourced from Pyth Core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admitted {
    pub kind: OracleKind,
    pub bridged_feeds: usize,
}

/// Counts the feeds whose plan has a candidate reached over the bridge.
pub(crate) fn bridged_feeds(plans: &[(PriceIdentifier, Vec<OffchainPriceSource>)]) -> usize {
    plans
        .iter()
        .filter(|(_, candidates)| {
            candidates.iter().any(|candidate| {
                let request = match candidate {
                    OffchainPriceSource::Direct(request)
                    | OffchainPriceSource::Transformed { request, .. } => request,
                };
                matches!(request, OffchainRequest::Lazer { bridged: true, .. })
            })
        })
        .count()
}

/// Oracle price fetcher.
///
/// Composes proxy-oracle prices off-chain at scan time and pushes Pyth Pro
/// payloads on-chain at execution time — see the module docs for the full
/// scan-vs-execution pricing split.
pub struct OracleFetcher {
    client: SigningClient,
    /// Pushes Pyth Pro payloads on-chain before a live liquidation. `None`
    /// when no `LAZER_API_TOKEN` is configured — Pyth Pro–sourced adapters
    /// then rely on an external pusher (stated once at startup).
    pyth_pro_updates: Option<PythProUpdatesClient>,
    /// Cache of detected proxy oracles (oracles that use cross-contract calls).
    /// Shared across all `OracleFetcher` instances so detection during registry
    /// refresh propagates to per-market fetchers.
    proxy_oracle_cache: ProxyOracleCache,
    /// Cache of LST-oracle probes (`oracle` → its underlying Pyth Core oracle,
    /// or `None` for a direct oracle).
    lst_oracle_cache: std::sync::Arc<tokio::sync::RwLock<HashMap<AccountId, Option<AccountId>>>>,
    /// The Pyth Core → Pyth Pro symbol bridge, shared like the proxy cache.
    symbol_map: SharedSymbolMap,
    /// Where the bridge is fetched from (public, no bearer; fetched with a
    /// no-redirect client built per refresh).
    lazer_symbols_url: Url,
    /// RedStone public price API, for composing proxy prices off-chain at
    /// scan time.
    redstone_api: crate::redstone::RedStoneApiClient,
    /// Pyth Pro price API, for composing Pyth Pro–sourced proxy prices
    /// off-chain at scan time. `None` when no access token is configured —
    /// such feeds are then not priceable and their markets are filtered at
    /// registration. The config type's constructor enforces HTTPS, so this
    /// client can never send the bearer token over cleartext.
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
        pyth_pro_updates: Option<PythProUpdatesClient>,
        apis: crate::OracleApis,
        proxy_oracle_cache: Option<ProxyOracleCache>,
        symbol_map: Option<SharedSymbolMap>,
    ) -> Self {
        let crate::OracleApis {
            redstone_api_url,
            lazer_api,
            lazer_symbols_url,
        } = apis;
        Self {
            client,
            pyth_pro_updates,
            lst_oracle_cache: std::sync::Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            symbol_map: symbol_map.unwrap_or_default(),
            lazer_symbols_url,
            proxy_oracle_cache: proxy_oracle_cache.unwrap_or_default(),
            redstone_api: crate::redstone::RedStoneApiClient::new(redstone_api_url),
            lazer_api: lazer_api.and_then(|config| {
                match crate::lazer::LazerApiClient::new(config) {
                    Ok(client) => Some(client),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            "Could not build the no-redirect Pyth Pro API client; Pyth Pro–sourced markets cannot be priced off-chain and will be filtered at registration"
                        );
                        None
                    }
                }
            }),
        }
    }

    /// Returns a clone of the shared symbol-bridge handle.
    pub fn symbol_map(&self) -> SharedSymbolMap {
        self.symbol_map.clone()
    }

    /// Refreshes the Pyth Core → Pyth Pro symbol bridge from the public
    /// symbols endpoint. Skipped (returns `Ok(0)`) without a Pyth Pro API
    /// leg — nothing could consume it. On failure — transport, body, a
    /// redirect, or a response with no usable feed — the previous map is
    /// kept and the error surfaces to the caller, which logs it; bridge-priced
    /// markets are filtered only while the map is empty (fail closed until a
    /// first refresh succeeds), never because a later refresh failed.
    pub async fn refresh_pyth_core_to_pro_map(&self) -> Result<usize, String> {
        if self.lazer_api.is_none() {
            return Ok(0);
        }
        let map = crate::lazer::fetch_pyth_core_to_pro_map(&self.lazer_symbols_url).await?;
        let len = map.len();
        *self.symbol_map.write().await = map;
        Ok(len)
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
        if let Some(&verdict) = self.proxy_oracle_cache.read().await.get(oracle) {
            return Ok(verdict);
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
                if self
                    .proxy_oracle_cache
                    .write()
                    .await
                    .insert(oracle.clone(), true)
                    .is_none()
                {
                    tracing::info!(%oracle, "Registered proxy oracle");
                }
                Ok(true)
            }
            Err(error) if gateway_is_method_not_found(&error) => {
                self.proxy_oracle_cache
                    .write()
                    .await
                    .insert(oracle.clone(), false);
                Ok(false)
            }
            Err(error) => Err(LiquidatorError::PriceFetchError(error.into())),
        }
    }

    /// Resolves a proxy oracle's sources to the Pyth Pro adapters (and feed
    /// ids) whose payloads this bot pushes on-chain before a live liquidation.
    /// Pyth Core and RedStone sources are externally pushed and never appear
    /// here; a non-proxy oracle has nothing to push.
    pub async fn resolve_pyth_pro_update_targets(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> LiquidatorResult<HashMap<AccountId, Vec<u32>>> {
        let mut targets: HashMap<AccountId, std::collections::BTreeSet<u32>> = HashMap::new();
        if !self.is_proxy_oracle(oracle).await? {
            return Ok(HashMap::new());
        }
        // A read failure is propagated rather than skipped, so we never push
        // an incomplete set of feeds.
        for &pid in price_ids {
            let result = self
                .client
                .read(proxy_oracle::GetProxy::new(oracle.clone(), pid))
                .await
                .map_err(|error| LiquidatorError::PriceFetchError(error.into()))?;
            if let Some(proxy) = result.proxy {
                for source in proxy.sources() {
                    Self::collect_pyth_pro_targets_from_source(source, &mut targets);
                }
            }
        }
        Ok(targets
            .into_iter()
            .map(|(adapter, feeds)| (adapter, feeds.into_iter().collect()))
            .collect())
    }

    /// Collects Pyth Pro adapter targets from one proxy source entry.
    pub(crate) fn collect_pyth_pro_targets_from_source(
        source: &Source,
        targets: &mut HashMap<AccountId, std::collections::BTreeSet<u32>>,
    ) {
        match source {
            Source::Request(OracleRequest::Lazer(req)) => {
                targets
                    .entry(req.oracle_id.clone())
                    .or_default()
                    .insert(req.feed_id);
            }
            // Pyth Core (Wormhole VAAs) and RedStone (relayer-pushed) are
            // not this bot's to push.
            Source::Request(OracleRequest::Pyth(_) | OracleRequest::RedStone(_)) => {}
            Source::Transformer(transformer) => Self::collect_pyth_pro_targets_from_source(
                &Source::Request(transformer.request.clone()),
                targets,
            ),
        }
    }

    /// Pushes fresh Pyth Pro payloads on-chain for every resolved adapter,
    /// then re-aggregates the proxy. Returns `Ok(true)` if any update was
    /// sent. Best-effort per adapter: a failed push is logged and the
    /// liquidation proceeds against existing on-chain state, which the
    /// market contract re-validates fail-closed.
    pub async fn update_onchain_prices(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> LiquidatorResult<bool> {
        if !self.is_proxy_oracle(oracle).await? {
            tracing::debug!(
                %oracle,
                "Pyth Core oracle: the bot cannot push to it (needs Wormhole VAAs); relying on the external pusher"
            );
            return Ok(false);
        }
        let mut any_updated = false;
        // Without a push client there is nothing to push with, so the per-id
        // proxy reads that resolve targets are skipped outright. (A
        // mixed-source feed is admitted tokenless and does carry a Pyth Pro
        // adapter — it simply cannot be pushed; the startup log says so.)
        if let Some(client) = &self.pyth_pro_updates {
            // Best-effort like the pushes themselves: a transient proxy-config
            // read failure must not skip the re-aggregation below.
            match self
                .resolve_pyth_pro_update_targets(oracle, price_ids)
                .await
            {
                Ok(targets) => {
                    for (adapter, feed_ids) in &targets {
                        if self.update_pyth_pro_prices(client, adapter, feed_ids).await {
                            any_updated = true;
                        }
                    }
                }
                Err(e) => tracing::warn!(
                    %oracle,
                    error = %e,
                    "Failed to resolve Pyth Pro push targets; re-aggregating the proxy from existing adapter state"
                ),
            }
        }
        any_updated |= self.update_proxy_prices(oracle, price_ids).await?;
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

    /// One `oracle.updateLazer` per feed (the gateway op is single-feed):
    /// the gateway fetches the payload from its websocket subscription and
    /// writes it to the adapter's `update_price_feeds`. Best-effort per
    /// feed — a market's two feeds usually share one adapter, and a failed
    /// submit for one must not skip the other. Returns whether any landed.
    #[tracing::instrument(skip(self, client), level = "info")]
    async fn update_pyth_pro_prices(
        &self,
        client: &PythProUpdatesClient,
        adapter: &AccountId,
        feed_ids: &[u32],
    ) -> bool {
        let mut any = false;
        for &feed_id in feed_ids {
            let result = match client
                .execute(oracle_updates::UpdateLazer {
                    oracle_id: adapter.clone(),
                    feed_id,
                })
                .await
            {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!(
                        adapter = %adapter,
                        feed_id,
                        error = %e,
                        "Pyth Pro update submit failed; continuing with the adapter's remaining feeds"
                    );
                    continue;
                }
            };
            if result.operation.status == OperationStatus::Succeeded {
                tracing::info!(
                    adapter = %adapter,
                    feed_id,
                    operation_id = %result.operation.id.0,
                    "Pushed Pyth Pro price on-chain"
                );
                any = true;
            } else {
                tracing::error!(
                    adapter = %adapter,
                    feed_id,
                    operation_id = %result.operation.id.0,
                    status = ?result.operation.status,
                    "Pyth Pro update did not succeed"
                );
            }
        }
        any
    }

    /// Probes whether `oracle` is an LST oracle (a Pyth Core underlying plus
    /// per-feed transformers), returning the underlying oracle. A
    /// method-not-found answer means a direct oracle. Cached per oracle.
    async fn is_lst_oracle(&self, oracle: &AccountId) -> LiquidatorResult<Option<AccountId>> {
        if let Some(cached) = self.lst_oracle_cache.read().await.get(oracle) {
            return Ok(cached.clone());
        }
        let probed = match self
            .client
            .read(lst_oracle::GetOracleId {
                oracle_id: oracle.clone(),
            })
            .await
        {
            Ok(result) => Some(result.pyth_oracle_id),
            Err(error) if gateway_is_method_not_found(&error) => None,
            Err(error) => return Err(LiquidatorError::PriceFetchError(error.into())),
        };
        self.lst_oracle_cache
            .write()
            .await
            .insert(oracle.clone(), probed.clone());
        Ok(probed)
    }

    /// Resolves a non-proxy oracle's feeds to the Pyth Core ids actually
    /// priced: identity for a direct oracle; the transformer's underlying
    /// (plus the transformer) for an LST oracle. Read failures propagate —
    /// a partial set would price the wrong feeds.
    async fn resolve_pyth_core_feeds(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> LiquidatorResult<(AccountId, Vec<PythCoreFeed>)> {
        let Some(underlying_oracle) = self.is_lst_oracle(oracle).await? else {
            let feeds = price_ids
                .iter()
                .map(|&id| PythCoreFeed {
                    market_id: id,
                    underlying_id: id,
                    transformer: None,
                })
                .collect();
            return Ok((oracle.clone(), feeds));
        };
        let mut feeds = Vec::with_capacity(price_ids.len());
        for &market_id in price_ids {
            let result = self
                .client
                .read(lst_oracle::GetTransformer {
                    oracle_id: oracle.clone(),
                    price_identifier: market_id,
                })
                .await
                .map_err(|error| LiquidatorError::PriceFetchError(error.into()))?;
            feeds.push(match result.transformer {
                Some(transformer) => PythCoreFeed {
                    market_id,
                    underlying_id: transformer.price_id,
                    transformer: Some(transformer),
                },
                None => PythCoreFeed {
                    market_id,
                    underlying_id: market_id,
                    transformer: None,
                },
            });
        }
        Ok((underlying_oracle, feeds))
    }

    /// Plans a non-proxy oracle's feeds over the symbol bridge.
    async fn resolve_non_proxy_plans(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> LiquidatorResult<Vec<(PriceIdentifier, Vec<OffchainPriceSource>)>> {
        let (pyth_oracle, feeds) = self.resolve_pyth_core_feeds(oracle, price_ids).await?;
        let map = self.symbol_map.read().await;
        let ctx = OffchainCtx {
            pyth_pro_api_configured: self.lazer_api.is_some(),
            core_to_pro: &map,
        };
        Ok(plan_pyth_core_feeds(&pyth_oracle, feeds, &ctx))
    }

    /// Fetches current oracle prices by off-chain composition — the only
    /// pricing path; no on-chain price read happens here. Proxy oracles
    /// compose from their source configs; direct-Pyth and LST oracles
    /// compose over the Pyth Core → Pyth Pro symbol bridge.
    #[tracing::instrument(skip(self), level = "debug")]
    pub async fn get_oracle_prices(
        &self,
        oracle: AccountId,
        price_ids: &[PriceIdentifier],
        age: u32,
    ) -> LiquidatorResult<OracleResponse> {
        let plans = if self.is_proxy_oracle(&oracle).await? {
            self.resolve_offchain_plans(&oracle, price_ids).await
        } else {
            self.resolve_non_proxy_plans(&oracle, price_ids).await?
        };
        // Off-chain only: feeds no candidate could price stay missing, and
        // the caller's `covers_all` gate fails the market scan loudly — the
        // same fail-closed semantics a stale price already had.
        Ok(self.compose_from_plans(&oracle, plans, age).await)
    }

    /// Registration-time admission: scan prices are off-chain only, so a
    /// market is admitted only when every feed has at least one off-chain
    /// candidate. A proxy oracle admits through `offchain_admission` over
    /// its feeds' configured sources (RedStone; Pyth Pro and bridged Pyth
    /// Core when the API leg is configured) as [`OracleKind::Proxy`]; a
    /// direct-Pyth or LST oracle admits through `pyth_core_admission` over
    /// the symbol bridge as `DirectPyth`/`Lst`. The verdict counts the feeds
    /// priced over the bridge — the ones whose on-chain price the bot cannot
    /// push. Filtering here — one log per refresh — beats failing the
    /// market's scan every round and degrading `/healthz`. A transient
    /// proxy-config read failure filters the market for one refresh cycle
    /// (re-probed next refresh); a probe or transformer read error is
    /// propagated.
    pub async fn offchain_priceable(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> LiquidatorResult<Result<Admitted, String>> {
        if self.is_proxy_oracle(oracle).await? {
            let plans = self.resolve_offchain_plans(oracle, price_ids).await;
            return Ok(match offchain_admission(price_ids, &plans) {
                None => Ok(Admitted {
                    kind: OracleKind::Proxy,
                    bridged_feeds: bridged_feeds(&plans),
                }),
                Some(reason) => Err(reason.to_string()),
            });
        }
        let kind = if self.is_lst_oracle(oracle).await?.is_some() {
            OracleKind::Lst
        } else {
            OracleKind::DirectPyth
        };
        let (_, feeds) = self.resolve_pyth_core_feeds(oracle, price_ids).await?;
        let underlying_ids: Vec<PriceIdentifier> = feeds.iter().map(|f| f.underlying_id).collect();
        let map = self.symbol_map.read().await;
        Ok(
            match pyth_core_admission(&underlying_ids, self.lazer_api.is_some(), &map) {
                // Every feed of a Pyth Core oracle is priced over the bridge.
                None => Ok(Admitted {
                    kind,
                    bridged_feeds: price_ids.len(),
                }),
                Some(reason) => Err(reason),
            },
        )
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
        let map = self.symbol_map.read().await;
        let ctx = OffchainCtx {
            pyth_pro_api_configured: self.lazer_api.is_some(),
            core_to_pro: &map,
        };
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
                    let candidates = plan_offchain_sources(proxy.sources(), &ctx);
                    if candidates.is_empty() {
                        tracing::debug!(
                            %oracle,
                            ?price_id,
                            "Feed has no off-chain-composable source"
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

    /// Prices the wanted Pyth Pro feeds from the Pyth Pro price API — the
    /// only Pyth Pro scan leg (no adapter read: an on-chain price is stale
    /// or costs a push). Freshness is enforced by the client; a feed the API
    /// can't price falls through to the feed's next source.
    async fn fetch_lazer_prices(
        &self,
        wanted: HashMap<AccountId, HashSet<u32>>,
        max_age_secs: u32,
    ) -> HashMap<(AccountId, u32), pyth::Price> {
        let Some(api) = &self.lazer_api else {
            return HashMap::new();
        };
        let all_ids: Vec<u32> = wanted
            .values()
            .flat_map(|ids| ids.iter().copied())
            .collect::<HashSet<u32>>()
            .into_iter()
            .collect();
        let api_prices = api.get_ema_prices(&all_ids, max_age_secs).await;
        let mut prices = HashMap::new();
        for (adapter, feed_ids) in wanted {
            for feed_id in feed_ids {
                if let Some(price) = api_prices.get(&feed_id) {
                    prices.insert((adapter.clone(), feed_id), price.clone());
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
        transformer_inputs: &mut Vec<(Call, Option<Decimal>)>,
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
            // Freshness enforced per leg by each API client at parse time,
            // plus once more at consumption in the caller.
            OffchainRequest::RedStone(symbol) => backends.redstone.get(symbol).cloned(),
            OffchainRequest::Lazer {
                oracle_id, feed_id, ..
            } => backends.lazer.get(&(oracle_id.clone(), *feed_id)).cloned(),
        };
        let Some(underlying) = underlying else {
            tracing::debug!(%oracle, ?price_id, ?request, "Candidate source has no usable price, trying next");
            return None;
        };
        match transform {
            None => Some(underlying),
            Some((call, action)) => {
                // Per-round cache, linear because `Call` is Eq but not Hash
                // and a round carries a handful of transformers at most: two
                // feeds sharing a rate contract, or a feed retrying its next
                // candidate, must not pay a second view call. A failed fetch
                // caches as `None` — retrying within the same round would
                // just repeat the failure.
                let cached = transformer_inputs
                    .iter()
                    .find(|(c, _)| *c == call)
                    .map(|(_, cached)| *cached);
                let input = if let Some(cached) = cached {
                    cached
                } else {
                    let fetched = match self.fetch_transformer_input(&call).await {
                        Ok(input) => Some(input),
                        Err(error) => {
                            tracing::warn!(%oracle, ?price_id, %error, "Failed to fetch transformer input");
                            None
                        }
                    };
                    transformer_inputs.push((call, fetched));
                    fetched
                };
                action.apply(underlying, input?)
            }
        }
    }

    /// Composes prices off-chain from each feed's planned candidates, in
    /// order — the first candidate yielding a fresh price wins:
    /// RedStone via the public price API, Pyth Pro via its price API,
    /// transformers applied with their on-chain input (a free view call).
    ///
    /// Returns only the feeds it could price; the rest stay missing and the
    /// caller's coverage gate decides. Costs no gas anywhere: proxy configs
    /// and transformer inputs are view calls, prices are HTTP.
    async fn compose_from_plans(
        &self,
        oracle: &AccountId,
        plans: Vec<(PriceIdentifier, Vec<OffchainPriceSource>)>,
        max_age_secs: u32,
    ) -> OracleResponse {
        if plans.is_empty() {
            return OracleResponse::new();
        }
        let requested = plans.len();

        // Batch the underlying fetches: one RedStone call, one Pyth Pro API
        // call. Every candidate of every feed is fetched up front, so falling
        // through to a later candidate below costs no extra round-trip.
        let (redstone_symbols, lazer_wanted) = collect_offchain_wants(&plans);
        let redstone_prices = self
            .redstone_api
            .get_prices(&redstone_symbols, max_age_secs)
            .await;
        let lazer_prices = self.fetch_lazer_prices(lazer_wanted, max_age_secs).await;
        let backends = FetchedBackends {
            redstone: redstone_prices,
            lazer: lazer_prices,
        };

        // Compose each feed from its first candidate that yields a price —
        // a leg's usability (a stale or failed API leg) is only
        // knowable here, so this is where source order falls through.
        let mut response = OracleResponse::new();
        let mut transformer_inputs: Vec<(Call, Option<Decimal>)> = Vec::new();
        for (price_id, candidates) in plans {
            let mut composed = None;
            for plan in candidates {
                let Some(price) = self
                    .price_one_candidate(oracle, price_id, plan, &backends, &mut transformer_inputs)
                    .await
                else {
                    continue;
                };
                // Fail-closed at consumption, per candidate: the backend
                // fetches and transformer view calls take real time, so a
                // price that aged past the bound while they ran must not be
                // inserted on the strength of an earlier clock sample — but
                // the next candidate is already prefetched and may still be
                // fresh, so staleness here costs the candidate, not the
                // whole feed.
                if publish_time_is_fresh(
                    price.publish_time.as_secs(),
                    unix_now_secs(),
                    max_age_secs,
                ) {
                    composed = Some(price);
                    break;
                }
                tracing::debug!(
                    %oracle,
                    ?price_id,
                    "Candidate price aged past the freshness bound in flight, trying next"
                );
            }
            match composed {
                Some(price) => {
                    response.insert(price_id, Some(price));
                }
                None => {
                    tracing::debug!(
                        %oracle,
                        ?price_id,
                        "No candidate source yielded a fresh price; feed left unpriced"
                    );
                }
            }
        }

        if !response.is_empty() {
            tracing::debug!(
                %oracle,
                composed = response.len(),
                requested,
                "Composed prices off-chain"
            );
        }
        response
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

    /// The registration probe is a real RPC round-trip; when it fails, the
    /// error must propagate (`PriceFetchError`) rather than silently filter
    /// or admit the market. Exercised against a live client pointed at a
    /// scripted local RPC endpoint.
    #[tokio::test]
    async fn offchain_priceable_propagates_probe_errors() {
        let (url, _requests) = crate::rpc::test_support::scripted_server(vec![
            (
                500,
                r#"{"jsonrpc":"2.0","id":"x","error":{"name":"INTERNAL_ERROR","cause":{"name":"INTERNAL_ERROR"},"code":-32000,"message":"scripted failure","data":"scripted failure"}}"#.to_string(),
            );
            16
        ])
        .await;
        let fetcher = crate::rpc::test_support::oracle_fetcher_for(url.as_str());

        let result = fetcher
            .offchain_priceable(
                &"proxy-oracle.test.near".parse().unwrap(),
                &[PriceIdentifier([0xAA; 32])],
            )
            .await;

        let err = result.expect_err("a failed probe must not silently admit or filter");
        assert!(
            matches!(err, crate::LiquidatorError::PriceFetchError(_)),
            "wrong error class: {err:?}"
        );
    }

    /// The verdict counts only feeds with a bridged candidate — the ones the
    /// bot cannot push for: a native Pyth Pro or RedStone candidate does not
    /// count, a bridged one does whether direct or under a transformer.
    #[test]
    fn bridged_feeds_counts_only_feeds_with_a_bridged_candidate() {
        let oracle: AccountId = "pyth-oracle.near".parse().unwrap();
        let lazer = |bridged| OffchainRequest::Lazer {
            oracle_id: oracle.clone(),
            feed_id: 1,
            bridged,
        };
        let transformer = ProxyPriceTransformer::lst(
            OracleRequest::pyth(oracle.clone(), PriceIdentifier([3; 32])),
            24,
            Call::new_simple(&oracle, "get_st_near_price"),
        );
        let plans = vec![
            (
                PriceIdentifier([1; 32]),
                vec![OffchainPriceSource::Direct(lazer(false))],
            ),
            (
                PriceIdentifier([2; 32]),
                vec![
                    OffchainPriceSource::Direct(OffchainRequest::RedStone("BTC".to_string())),
                    OffchainPriceSource::Direct(lazer(true)),
                ],
            ),
            (
                PriceIdentifier([3; 32]),
                vec![OffchainPriceSource::Transformed {
                    request: lazer(true),
                    call: transformer.call,
                    action: transformer.action,
                }],
            ),
        ];
        assert_eq!(bridged_feeds(&plans), 2);
        assert_eq!(bridged_feeds(&plans[..1]), 0);
    }

    /// A non-proxy oracle is admitted over the symbol bridge. Both probes
    /// (proxy `list_proxies`, LST `oracle_id`) answer method-not-found —
    /// a direct Pyth Core oracle — and with the API leg configured the
    /// verdict follows the bridge: the reason names the unmapped id; mapped,
    /// the market is admitted as `DirectPyth`. Both probes are cached, so
    /// the second admission makes no RPC call.
    #[tokio::test]
    async fn direct_pyth_oracle_is_admitted_over_the_bridge() {
        let not_found = r#"{"jsonrpc":"2.0","id":"x","error":{"name":"HANDLER_ERROR","cause":{"name":"CONTRACT_EXECUTION_ERROR","info":{"vm_error":"FunctionCallError(MethodResolveError(MethodNotFound))","block_height":1,"block_hash":"11111111111111111111111111111111"}},"code":-32000,"message":"Server error","data":"wasm execution failed with error: FunctionCallError(MethodResolveError(MethodNotFound))"}}"#;
        let (url, requests) =
            crate::rpc::test_support::scripted_server(vec![(200, not_found.to_string()); 8]).await;
        let lazer_api = crate::lazer::LazerApiConfig::new(
            "https://pyth-pro.invalid".parse().unwrap(),
            "test-token".to_string(),
        )
        .unwrap();
        let fetcher = crate::rpc::test_support::oracle_fetcher_with(url.as_str(), Some(lazer_api));
        let oracle: AccountId = "pyth-oracle.test.near".parse().unwrap();
        let id = PriceIdentifier([0xAA; 32]);
        fetcher
            .symbol_map()
            .write()
            .await
            .insert(PriceIdentifier([0xBB; 32]), 2);

        let verdict = fetcher.offchain_priceable(&oracle, &[id]).await.unwrap();
        assert_eq!(
            verdict,
            Err(format!(
                "no Pyth Pro feed for Pyth Core price id {}",
                hex::encode([0xAA; 32])
            ))
        );
        let probes = requests.lock().unwrap().len();
        assert_eq!(probes, 2, "one proxy probe and one LST probe");
        {
            let bodies = requests.lock().unwrap();
            assert!(
                bodies.iter().any(|b| b.contains("list_proxies")),
                "{bodies:?}"
            );
            assert!(
                bodies
                    .iter()
                    .any(|b| b.contains("\"method_name\":\"oracle_id\"")),
                "{bodies:?}"
            );
        }

        fetcher.symbol_map().write().await.insert(id, 1);
        let verdict = fetcher.offchain_priceable(&oracle, &[id]).await.unwrap();
        assert_eq!(
            verdict,
            Ok(Admitted {
                kind: OracleKind::DirectPyth,
                bridged_feeds: 1,
            })
        );
        assert_eq!(
            requests.lock().unwrap().len(),
            probes,
            "both probes are cached; the second admission makes no RPC call"
        );
    }

    use templar_proxy_oracle_near_common::{
        input::ProxyPriceTransformer, price_transformer::Call, request::OracleRequest,
    };

    fn pyth_source() -> Source {
        Source::Request(OracleRequest::pyth(
            "pyth-oracle.near".parse().unwrap(),
            PriceIdentifier([0xAA; 32]),
        ))
    }

    static EMPTY_BRIDGE: std::sync::LazyLock<crate::lazer::PythCoreToProMap> =
        std::sync::LazyLock::new(HashMap::new);

    fn ctx_on() -> OffchainCtx<'static> {
        OffchainCtx {
            pyth_pro_api_configured: true,
            core_to_pro: &EMPTY_BRIDGE,
        }
    }

    fn ctx_off() -> OffchainCtx<'static> {
        OffchainCtx {
            pyth_pro_api_configured: false,
            core_to_pro: &EMPTY_BRIDGE,
        }
    }

    fn pyth_request() -> OracleRequest {
        OracleRequest::pyth(
            "pyth-oracle.near".parse().unwrap(),
            PriceIdentifier([0xAA; 32]),
        )
    }

    fn redstone_request() -> OracleRequest {
        OracleRequest::redstone("redstone.near".parse().unwrap(), "LTC")
    }

    fn lazer_request() -> OracleRequest {
        OracleRequest::lazer("pyth-lazer.near".parse().unwrap(), 7)
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
    /// configured `[Pyth Pro, RedStone]` with a failed Pyth Pro API call
    /// still prices from RedStone instead of dying on its first source.
    #[test]
    fn plan_keeps_every_source_in_configured_order() {
        let plans =
            plan_offchain_sources([&lazer_source(), &redstone_source()].into_iter(), &ctx_on());
        assert_eq!(
            plans,
            vec![
                OffchainPriceSource::Direct(OffchainRequest::Lazer {
                    oracle_id: "pyth-lazer.near".parse().unwrap(),
                    feed_id: 7,
                    bridged: false
                }),
                OffchainPriceSource::Direct(OffchainRequest::RedStone("LTC".to_string())),
            ]
        );
    }

    /// Scan prices are off-chain only: an unbridged Pyth Core source
    /// contributes no candidate, and Pyth Pro sources contribute one only
    /// when the API leg is configured — there is no adapter read to fall
    /// back to, so without a token the feed is simply not priceable.
    #[test]
    fn only_offchain_composable_sources_yield_candidates() {
        assert_eq!(classify_offchain_request(&pyth_request(), &ctx_on()), None);
        assert_eq!(
            classify_offchain_request(&lazer_request(), &ctx_off()),
            None
        );
        assert!(matches!(
            classify_offchain_request(&redstone_request(), &ctx_off()),
            Some(OffchainRequest::RedStone(_))
        ));
        assert!(matches!(
            classify_offchain_request(&lazer_request(), &ctx_on()),
            Some(OffchainRequest::Lazer {
                feed_id: 7,
                bridged: false,
                ..
            })
        ));
        // A proxy whose only source is Pyth Core composes nothing.
        assert!(plan_offchain_sources([&pyth_source()].into_iter(), &ctx_on()).is_empty());
    }

    /// A Pyth Core source composes only through the symbol bridge: with the
    /// core id mapped (and the API leg configured) it becomes a Pyth Pro
    /// request; unmapped, it contributes no candidate.
    #[test]
    fn pyth_core_sources_compose_through_the_symbol_bridge() {
        let mut bridge: crate::lazer::PythCoreToProMap = HashMap::new();
        bridge.insert(PriceIdentifier([0xAA; 32]), 1);
        let with_bridge = OffchainCtx {
            pyth_pro_api_configured: true,
            core_to_pro: &bridge,
        };
        assert!(matches!(
            classify_offchain_request(&pyth_request(), &with_bridge),
            Some(OffchainRequest::Lazer {
                feed_id: 1,
                bridged: true,
                ..
            })
        ));
        let empty: crate::lazer::PythCoreToProMap = HashMap::new();
        let no_bridge = OffchainCtx {
            pyth_pro_api_configured: true,
            core_to_pro: &empty,
        };
        assert_eq!(classify_offchain_request(&pyth_request(), &no_bridge), None);
        let no_token = OffchainCtx {
            pyth_pro_api_configured: false,
            core_to_pro: &bridge,
        };
        assert_eq!(classify_offchain_request(&pyth_request(), &no_token), None);
    }

    /// A non-proxy oracle's feeds — direct Pyth Core ids, or an LST oracle's
    /// transformed underlyings — plan as Pyth Pro candidates over the bridge,
    /// keyed by the market's own price id; an unmapped underlying yields an
    /// empty candidate list so admission can name it.
    #[test]
    fn non_proxy_feeds_plan_over_the_bridge_and_keep_the_market_id() {
        let mut bridge: crate::lazer::PythCoreToProMap = HashMap::new();
        let near = PriceIdentifier([0xCC; 32]);
        bridge.insert(near, 27);
        let ctx = OffchainCtx {
            pyth_pro_api_configured: true,
            core_to_pro: &bridge,
        };
        let stnear = PriceIdentifier([0x55; 32]);
        let unmapped = PriceIdentifier([0x99; 32]);
        let pyth_oracle: AccountId = "pyth-oracle.near".parse().unwrap();
        let rate_contract: AccountId = "meta-pool.near".parse().unwrap();
        let transformer =
            templar_proxy_oracle_near_common::price_transformer::PriceTransformer::lst(
                near,
                24,
                Call::new_simple(&rate_contract, "get_st_near_price"),
            );
        let feeds = vec![
            PythCoreFeed {
                market_id: near,
                underlying_id: near,
                transformer: None,
            },
            PythCoreFeed {
                market_id: stnear,
                underlying_id: near,
                transformer: Some(transformer),
            },
            PythCoreFeed {
                market_id: unmapped,
                underlying_id: unmapped,
                transformer: None,
            },
        ];
        let plans = plan_pyth_core_feeds(&pyth_oracle, feeds, &ctx);
        assert_eq!(plans.len(), 3);
        assert_eq!(
            plans[0],
            (
                near,
                vec![OffchainPriceSource::Direct(OffchainRequest::Lazer {
                    oracle_id: pyth_oracle.clone(),
                    feed_id: 27,
                    bridged: true
                })]
            )
        );
        assert!(matches!(
            plans[1].1.as_slice(),
            [OffchainPriceSource::Transformed {
                request: OffchainRequest::Lazer { feed_id: 27, .. },
                ..
            }]
        ));
        assert_eq!(
            plans[1].0, stnear,
            "the transformed price is keyed by the market's id"
        );
        assert!(
            plans[2].1.is_empty(),
            "an unmapped underlying has no candidate"
        );
    }

    /// Admission of Pyth Core–priced feeds names what is missing: the token,
    /// the symbol map, or the specific unmapped price id — so an operator
    /// can tell "get a token" from "this asset is not on Pyth Pro".
    #[test]
    fn pyth_core_admission_names_the_missing_piece() {
        let mut bridge: crate::lazer::PythCoreToProMap = HashMap::new();
        let a = PriceIdentifier([0xAA; 32]);
        let b = PriceIdentifier([0xBB; 32]);
        bridge.insert(a, 1);
        let empty: crate::lazer::PythCoreToProMap = HashMap::new();
        assert!(pyth_core_admission(&[a], false, &bridge)
            .unwrap()
            .contains("LAZER_API_TOKEN"));
        assert!(pyth_core_admission(&[a], true, &empty)
            .unwrap()
            .contains("symbol map"));
        assert_eq!(pyth_core_admission(&[a], true, &bridge), None);
        let reason = pyth_core_admission(&[a, b], true, &bridge).unwrap();
        assert!(
            reason.contains(&hex::encode(b.0)),
            "names the unmapped id: {reason}"
        );
    }

    #[test]
    fn plan_is_empty_when_no_source_is_configured() {
        assert!(plan_offchain_sources([].into_iter(), &ctx_on()).is_empty());
    }

    /// The on-chain push resolves only Pyth Pro adapters from a proxy's
    /// sources: Pyth Core and RedStone are externally pushed, and a
    /// transformer unwraps to its inner request.
    #[test]
    fn push_targets_collect_only_pyth_pro_adapters() {
        let mut targets: HashMap<AccountId, std::collections::BTreeSet<u32>> = HashMap::new();
        OracleFetcher::collect_pyth_pro_targets_from_source(&pyth_source(), &mut targets);
        OracleFetcher::collect_pyth_pro_targets_from_source(&redstone_source(), &mut targets);
        OracleFetcher::collect_pyth_pro_targets_from_source(&lazer_source(), &mut targets);
        OracleFetcher::collect_pyth_pro_targets_from_source(
            &transformer_source(OracleRequest::lazer("pyth-lazer.near".parse().unwrap(), 9)),
            &mut targets,
        );
        let adapter: AccountId = "pyth-lazer.near".parse().unwrap();
        assert_eq!(
            targets.len(),
            1,
            "only the Pyth Pro adapter is a push target"
        );
        assert_eq!(
            targets[&adapter].iter().copied().collect::<Vec<_>>(),
            vec![7, 9]
        );
    }

    /// Admission requires every requested feed to have a composable
    /// candidate — one unpriceable feed filters the market, since a round
    /// needs the full pair.
    #[test]
    fn offchain_admission_requires_every_feed_covered() {
        let a = PriceIdentifier([0xAA; 32]);
        let b = PriceIdentifier([0xBB; 32]);
        let plan_for = |id| {
            (
                id,
                plan_offchain_sources([&redstone_source()].into_iter(), &ctx_on()),
            )
        };
        assert_eq!(
            offchain_admission(&[a, b], &[plan_for(a), plan_for(b)]),
            None
        );
        assert_eq!(
            offchain_admission(&[a, b], &[plan_for(a)]),
            Some("no off-chain price source for a feed")
        );
        assert_eq!(
            offchain_admission(&[a], &[(a, Vec::new())]),
            Some("no off-chain price source for a feed")
        );
    }

    #[test]
    fn plan_carries_transformers_over_priceable_inners() {
        let inner = OracleRequest::lazer("pyth-lazer.near".parse().unwrap(), 9);
        let plan = plan_offchain_sources([&transformer_source(inner)].into_iter(), &ctx_on())
            .into_iter()
            .next()
            .expect("transformer over a Pyth Pro feed is priceable off-chain");
        match plan {
            OffchainPriceSource::Transformed { request, .. } => {
                assert_eq!(
                    request,
                    OffchainRequest::Lazer {
                        oracle_id: "pyth-lazer.near".parse().unwrap(),
                        feed_id: 9,
                        bridged: false
                    }
                );
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

    /// A partial response must not escape: with no fallback path, it would
    /// reach per-position status checks and fail each one
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

    /// Composed prices must honor the market's freshness bound exactly like
    /// the on-chain read the market itself performs at execution: a stale
    /// (or implausibly future-dated) entry must fall through to the feed's
    /// next candidate, never price a position off an hour-old number.
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

    /// A transformer over a Lazer inner is fully priceable at scan time: the
    /// transformer input is a view call and the Lazer underlying is an
    /// adapter view read. This is the linear-usdt collateral-feed shape —
    /// the case the Lazer leg exists for.
    #[test]
    fn plan_carries_transformers_over_lazer_inners() {
        let inner = OracleRequest::lazer("pyth-lazer.near".parse().unwrap(), 9);
        let plan = plan_offchain_sources(
            [&transformer_source(inner), &redstone_source()].into_iter(),
            &ctx_on(),
        )
        .into_iter()
        .next()
        .expect("transformer over lazer is priceable off-chain");
        match plan {
            OffchainPriceSource::Transformed { request, .. } => {
                assert_eq!(
                    request,
                    OffchainRequest::Lazer {
                        oracle_id: "pyth-lazer.near".parse().unwrap(),
                        feed_id: 9,
                        bridged: false
                    }
                );
            }
            other @ OffchainPriceSource::Direct(_) => panic!("expected Transformed, got {other:?}"),
        }
    }
}
