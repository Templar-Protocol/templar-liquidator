//! Oracle price fetching module.
//!
//! Scan-time prices are **off-chain only**: proxy-oracle feeds are composed
//! from each feed's configured sources in order, taking the first leg that
//! yields a fresh price — the RedStone public price API ([`crate::redstone`])
//! for RedStone sources, the token-gated Pyth Pro price API ([`crate::lazer`])
//! for Pyth Pro sources, transformer inputs by free view call. There is no
//! on-chain price read at scan time: an on-chain price is either stale or
//! costs a paid push every scan. Pyth Core sources are not composable (Pyth
//! Core is not integrated — see the CHANGELOG), and a market whose feeds have
//! no off-chain source is filtered at registration
//! ([`OracleFetcher::offchain_priceable`]), never failed per scan. Every
//! composed price is bounded by the market's freshness window before use.
//!
//! Execution-time pricing is separate: before a live liquidation this module
//! pushes fresh Pyth Pro payloads to each Pyth Pro–sourced adapter and
//! re-aggregates the proxy ([`OracleFetcher::update_onchain_prices`]); the
//! market contract then reads its own on-chain oracle. RedStone adapters are
//! pushed by the RedStone leg ([`crate::redstone_push`]) for the feeds the
//! Pyth Pro push cannot refresh — those without a Pyth Pro source, and every
//! feed when no Pyth Pro push is available — nothing else keeps those
//! adapters fresh.

use near_sdk::AccountId;
use std::collections::{HashMap, HashSet};
use templar_common::{
    oracle::pyth::{self, OracleResponse, PriceIdentifier},
    Decimal,
};
use templar_gateway_client::SigningClient;
use templar_gateway_core::GatewayContext;
use templar_gateway_methods_spec::{contract, proxy_oracle, redstone};
use templar_gateway_oracle_updates_dispatch::{Dispatch as OracleUpdatesDispatch, WithLazerSource};
use templar_gateway_oracle_updates_spec::oracle as oracle_updates;
use templar_gateway_types::{
    common::ContractArgs, Base64Bytes, ContractMethodName, OperationStatus,
};
use templar_proxy_oracle_near_common::{
    input::Source,
    price_transformer::{Action, Call},
    request::OracleRequest,
};

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
    Lazer { oracle_id: AccountId, feed_id: u32 },
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
/// Composed proxy prices must pass this bot-side — the on-chain read they
/// replace enforces the same bound on-chain.
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

/// Classifies an [`OracleRequest`] as its scan-time pricing request, or
/// `None` when the source has no off-chain leg: Pyth Core always (not
/// integrated), and Pyth Pro when no API token is configured — there is
/// deliberately no on-chain fallback for either.
fn classify_offchain_request(
    request: &OracleRequest,
    pyth_pro_api_configured: bool,
) -> Option<OffchainRequest> {
    match request {
        OracleRequest::RedStone(req) => Some(OffchainRequest::RedStone(req.price_id.to_string())),
        OracleRequest::Lazer(req) if pyth_pro_api_configured => Some(OffchainRequest::Lazer {
            oracle_id: req.oracle_id.clone(),
            feed_id: req.feed_id,
        }),
        OracleRequest::Pyth(_) | OracleRequest::Lazer(_) => None,
    }
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
            OffchainRequest::Lazer { oracle_id, feed_id } => {
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
    pyth_pro_api_configured: bool,
) -> Vec<OffchainPriceSource> {
    sources
        .filter_map(|source| match source {
            Source::Request(request) => classify_offchain_request(request, pyth_pro_api_configured)
                .map(OffchainPriceSource::Direct),
            Source::Transformer(transformer) => {
                classify_offchain_request(&transformer.request, pyth_pro_api_configured).map(
                    |request| OffchainPriceSource::Transformed {
                        request,
                        call: transformer.call.clone(),
                        action: transformer.action.clone(),
                    },
                )
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

/// Collects one feed's RedStone adapters and feed ids — only when the Pyth
/// Pro push cannot refresh the feed: it has no Pyth Pro source, or the Pyth
/// Pro push is not available (no `LAZER_API_TOKEN`). A feed the Pyth Pro
/// push does refresh is skipped: the proxy aggregates from one fresh source
/// (`min_sources` 1), so a RedStone push there would cost a transaction for
/// nothing. Transformers are unwrapped.
pub(crate) fn collect_redstone_targets_for_feed<'a>(
    sources: impl Iterator<Item = &'a Source>,
    pyth_pro_push_available: bool,
    targets: &mut HashMap<AccountId, std::collections::BTreeSet<RedStoneFeedId>>,
) {
    let requests: Vec<&OracleRequest> = sources
        .map(|source| match source {
            Source::Request(request) => request,
            Source::Transformer(transformer) => &transformer.request,
        })
        .collect();
    if pyth_pro_push_available
        && requests
            .iter()
            .any(|request| matches!(request, OracleRequest::Lazer(_)))
    {
        return;
    }
    for request in requests {
        if let OracleRequest::RedStone(req) = request {
            targets
                .entry(req.oracle_id.clone())
                .or_default()
                .insert(req.price_id.clone());
        }
    }
}

/// Shared cache of detected proxy oracle accounts.
pub type ProxyOracleCache =
    std::sync::Arc<tokio::sync::RwLock<std::collections::HashSet<AccountId>>>;

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
    /// RedStone public price API, for composing proxy prices off-chain at
    /// scan time.
    redstone_api: crate::redstone::RedStoneApiClient,
    /// Pyth Pro price API, for composing Pyth Pro–sourced proxy prices
    /// off-chain at scan time. `None` when no access token is configured —
    /// such feeds are then not priceable and their markets are filtered at
    /// registration. The config type's constructor enforces HTTPS, so this
    /// client can never send the bearer token over cleartext.
    lazer_api: Option<crate::lazer::LazerApiClient>,
    /// RedStone push leg: `None` when disabled (`REDSTONE_PUSH=false`) or
    /// when its no-redirect client could not be built.
    redstone_push: Option<crate::redstone_push::RedStonePushClient>,
    /// Last successful RedStone write, or in-flight reservation, per
    /// (adapter, feed) — see [`RedStonePushMemo`].
    redstone_push_memo: RedStonePushMemo,
}

/// A RedStone feed id as the adapter and the proxy sources name it.
pub(crate) type RedStoneFeedId = templar_common::oracle::redstone::FeedId;

/// Shared memo per (adapter, feed) of the last successful RedStone write —
/// or of an in-flight reservation, taken when a push finds the feed due and
/// released if the build or write fails — so a market round with several
/// positions, or several markets on one adapter, does not resubmit inside
/// the adapter's minimum interval and burn gas on writes the contract
/// rejects. A push future dropped between reserve and release leaves its
/// entry behind; the cost is bounded — the refresh is suppressed for at most
/// one `min_interval_between_updates_ms` — and equals what the rejected
/// duplicate write would have cost in delay, without the gas.
pub type RedStonePushMemo =
    std::sync::Arc<std::sync::Mutex<HashMap<(AccountId, RedStoneFeedId), std::time::Instant>>>;

/// Push targets of one proxy, resolved from a single read of its sources.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UpdateTargets {
    pub pyth_pro: HashMap<AccountId, Vec<u32>>,
    pub redstone: HashMap<AccountId, Vec<RedStoneFeedId>>,
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
        redstone_push_memo: Option<RedStonePushMemo>,
    ) -> Self {
        let crate::OracleApis {
            redstone_api_url,
            lazer_api,
            redstone_push,
        } = apis;
        Self {
            redstone_push_memo: redstone_push_memo.unwrap_or_default(),
            redstone_push: redstone_push.and_then(|config| {
                match crate::redstone_push::RedStonePushClient::new(config) {
                    Ok(client) => Some(client),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            "Could not build the no-redirect RedStone gateway client; RedStone-only feeds will not be pushed"
                        );
                        None
                    }
                }
            }),
            client,
            pyth_pro_updates,
            proxy_oracle_cache: proxy_oracle_cache.unwrap_or_else(|| {
                std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashSet::new()))
            }),
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

    /// Returns a clone of the shared RedStone last-push memo.
    pub fn redstone_push_memo(&self) -> RedStonePushMemo {
        self.redstone_push_memo.clone()
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

    /// Resolves both push legs' targets from one read of the proxy's sources
    /// per price id: the Pyth Pro adapters (and feed ids) whose payloads this
    /// bot pushes, and the RedStone adapters (and feed ids) of feeds the
    /// Pyth Pro push cannot refresh. A non-proxy oracle has nothing to push.
    ///
    /// # Errors
    ///
    /// A proxy-config read failure is propagated rather than skipped, so an
    /// incomplete target set is never pushed.
    pub async fn resolve_update_targets(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> LiquidatorResult<UpdateTargets> {
        if !self.is_proxy_oracle(oracle).await? {
            return Ok(UpdateTargets::default());
        }
        let mut pyth_pro: HashMap<AccountId, std::collections::BTreeSet<u32>> = HashMap::new();
        let mut redstone: HashMap<AccountId, std::collections::BTreeSet<RedStoneFeedId>> =
            HashMap::new();
        let pyth_pro_push_available = self.pyth_pro_updates.is_some();
        for &pid in price_ids {
            let result = self
                .client
                .read(proxy_oracle::GetProxy::new(oracle.clone(), pid))
                .await
                .map_err(|error| LiquidatorError::PriceFetchError(error.into()))?;
            if let Some(proxy) = result.proxy {
                for source in proxy.sources() {
                    Self::collect_pyth_pro_targets_from_source(source, &mut pyth_pro);
                }
                collect_redstone_targets_for_feed(
                    proxy.sources(),
                    pyth_pro_push_available,
                    &mut redstone,
                );
            }
        }
        Ok(UpdateTargets {
            pyth_pro: pyth_pro
                .into_iter()
                .map(|(adapter, feeds)| (adapter, feeds.into_iter().collect()))
                .collect(),
            redstone: redstone
                .into_iter()
                .map(|(adapter, feeds)| (adapter, feeds.into_iter().collect()))
                .collect(),
        })
    }

    /// The Pyth Pro half of [`Self::resolve_update_targets`].
    ///
    /// # Errors
    ///
    /// As [`Self::resolve_update_targets`].
    pub async fn resolve_pyth_pro_update_targets(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> LiquidatorResult<HashMap<AccountId, Vec<u32>>> {
        Ok(self
            .resolve_update_targets(oracle, price_ids)
            .await?
            .pyth_pro)
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
            // Pyth Core is nobody's to push on NEAR; RedStone has its own leg
            // (`collect_redstone_targets_for_feed`).
            Source::Request(OracleRequest::Pyth(_) | OracleRequest::RedStone(_)) => {}
            Source::Transformer(transformer) => Self::collect_pyth_pro_targets_from_source(
                &Source::Request(transformer.request.clone()),
                targets,
            ),
        }
    }

    fn release_redstone_reservation(&self, adapter: &AccountId, feeds: &[RedStoneFeedId]) {
        let mut memo = self
            .redstone_push_memo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::redstone_push::release(&mut memo, adapter, feeds);
    }

    /// Builds a payload for `feed_ids` against the adapter's own rules
    /// (`get_config`: signer set, threshold, timestamp window) and submits
    /// `write_prices`. Best-effort: any failure is logged and the feeds stay
    /// as they are on-chain, which the market contract re-validates
    /// fail-closed.
    async fn update_redstone_prices(
        &self,
        adapter: &AccountId,
        feed_ids: &[RedStoneFeedId],
        packages: &HashMap<String, Vec<crate::redstone_push::SignedPackage>>,
    ) -> bool {
        let rules = match self
            .client
            .read(redstone::GetConfig {
                oracle_id: adapter.clone(),
            })
            .await
        {
            Ok(result) => crate::redstone_push::AdapterRules::from(&result.config),
            Err(error) => {
                tracing::warn!(%adapter, %error, "Failed to read the RedStone adapter config; nothing pushed to it");
                return false;
            }
        };
        let min_interval = std::time::Duration::from_millis(rules.min_interval_ms);
        let now = std::time::Instant::now();
        // Reserve under the lock: a concurrent liquidation must not find the
        // same feeds due; a failed build or write releases them below.
        let due: Vec<RedStoneFeedId> = {
            let mut memo = self
                .redstone_push_memo
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            crate::redstone_push::reserve_due(&mut memo, adapter, feed_ids, min_interval, now)
        };
        if due.is_empty() {
            tracing::debug!(%adapter, feeds = ?feed_ids, "RedStone feeds pushed within the adapter's minimum interval; skipping");
            return false;
        }
        let names: Vec<String> = due.iter().map(|feed| feed.as_ref().to_string()).collect();
        let payload = match crate::redstone_push::build_payload(
            packages,
            &names,
            &rules,
            unix_now_ms(),
        ) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(%adapter, %error, "RedStone payload not built; its feeds stay as they are on-chain");
                self.release_redstone_reservation(adapter, &due);
                return false;
            }
        };
        match self
            .client
            .execute(redstone::WritePrices {
                oracle_id: adapter.clone(),
                feed_ids: due.clone(),
                payload: Base64Bytes(payload),
            })
            .await
        {
            Ok(result) if result.operation.status == OperationStatus::Succeeded => {
                // Refresh the reservation to the time the write landed: the
                // adapter's interval runs from the write, not from the
                // moment the feed was found due.
                let landed = std::time::Instant::now();
                let mut memo = self
                    .redstone_push_memo
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                for feed in &due {
                    memo.insert((adapter.clone(), feed.clone()), landed);
                }
                tracing::info!(%adapter, feeds = ?names, operation_id = %result.operation.id.0, "Pushed RedStone prices");
                true
            }
            Ok(result) => {
                tracing::warn!(%adapter, feeds = ?names, status = ?result.operation.status, "RedStone push did not succeed");
                self.release_redstone_reservation(adapter, &due);
                false
            }
            Err(error) => {
                tracing::warn!(%adapter, feeds = ?names, %error, "RedStone push failed");
                self.release_redstone_reservation(adapter, &due);
                false
            }
        }
    }

    /// Pushes fresh Pyth Pro payloads on-chain for every resolved adapter,
    /// RedStone packages for the feeds the Pyth Pro push cannot refresh —
    /// those without a Pyth Pro source, and every feed when no Pyth Pro push
    /// is available — (when the RedStone leg is enabled), then re-aggregates
    /// the proxy. Returns
    /// `Ok(true)` if any update was sent. Best-effort per adapter: a failed
    /// push is logged and the liquidation proceeds against existing on-chain
    /// state, which the market contract re-validates fail-closed.
    pub async fn update_onchain_prices(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> LiquidatorResult<bool> {
        if !self.is_proxy_oracle(oracle).await? {
            tracing::debug!(%oracle, "Non-proxy oracle: nothing to push");
            return Ok(false);
        }
        let mut any_updated = false;
        // One read of the proxy's sources serves both legs. Without either
        // leg there is nothing to resolve for. (A mixed-source feed is
        // admitted tokenless; the RedStone leg then covers its RedStone
        // source, since the Pyth Pro adapter cannot be pushed.)
        if self.pyth_pro_updates.is_some() || self.redstone_push.is_some() {
            // Best-effort like the pushes themselves: a transient proxy-config
            // read failure must not skip the re-aggregation below.
            match self.resolve_update_targets(oracle, price_ids).await {
                Ok(targets) => {
                    if let Some(client) = &self.pyth_pro_updates {
                        for (adapter, feed_ids) in &targets.pyth_pro {
                            if self.update_pyth_pro_prices(client, adapter, feed_ids).await {
                                any_updated = true;
                            }
                        }
                    }
                    if let Some(push) = &self.redstone_push {
                        if !targets.redstone.is_empty() {
                            match push.fetch_packages().await {
                                Ok(packages) => {
                                    for (adapter, feed_ids) in &targets.redstone {
                                        if self
                                            .update_redstone_prices(adapter, feed_ids, &packages)
                                            .await
                                        {
                                            any_updated = true;
                                        }
                                    }
                                }
                                Err(error) => tracing::warn!(
                                    %oracle,
                                    %error,
                                    "Failed to fetch RedStone packages; RedStone-only feeds stay as they are on-chain"
                                ),
                            }
                        }
                    }
                }
                Err(e) => tracing::warn!(
                    %oracle,
                    error = %e,
                    "Failed to resolve push targets; re-aggregating the proxy from existing adapter state"
                ),
            }
        }
        any_updated |= self.update_proxy_prices(oracle, price_ids).await?;
        Ok(any_updated)
    }

    /// Reads the proxy's cached EMA publish times through the same
    /// `list_ema_prices_no_older_than` view the market contract reads, with
    /// a wide bound so age is measured rather than filtered. This is the one
    /// on-chain price read in the crate and it is a diagnostic
    /// (`RUN_MODE=push-check`), never scan pricing: it shows what the market
    /// would see, before and after a push. A feed the proxy has never cached
    /// comes back as `None`.
    ///
    /// # Errors
    ///
    /// The view call failing (transport, or the contract rejecting the
    /// call) is a [`LiquidatorError::PriceFetchError`].
    pub async fn onchain_publish_times(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> LiquidatorResult<HashMap<PriceIdentifier, Option<i64>>> {
        // Ten years: wide enough that the view never filters, small enough
        // that the contract's seconds→nanoseconds conversion cannot overflow.
        const WIDE_BOUND_SECS: u64 = 10 * 365 * 24 * 60 * 60;
        let args = near_sdk::serde_json::to_vec(&near_sdk::serde_json::json!({
            "price_ids": price_ids,
            "age": WIDE_BOUND_SECS,
        }))
        .map_err(|error| LiquidatorError::PriceFetchError(RpcError::DeserializeError(error)))?;
        let result = self
            .client
            .read(contract::ViewFunction {
                contract_id: oracle.clone(),
                method_name: ContractMethodName("list_ema_prices_no_older_than".to_string()),
                args: ContractArgs::Raw(Base64Bytes(args)),
            })
            .await
            .map_err(|error| LiquidatorError::PriceFetchError(error.into()))?;
        let response: OracleResponse = near_sdk::serde_json::from_value(result.value)
            .map_err(|error| LiquidatorError::PriceFetchError(RpcError::DeserializeError(error)))?;
        Ok(response
            .into_iter()
            .map(|(id, price)| (id, price.map(|price| price.publish_time.as_secs())))
            .collect())
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

    /// Fetches current oracle prices for a proxy oracle by off-chain
    /// composition — the only supported oracle kind, and the only pricing
    /// path: no on-chain price read happens here.
    #[tracing::instrument(skip(self), level = "debug")]
    pub async fn get_oracle_prices(
        &self,
        oracle: AccountId,
        price_ids: &[PriceIdentifier],
        age: u32,
    ) -> LiquidatorResult<OracleResponse> {
        if !self.is_proxy_oracle(&oracle).await? {
            // Registration never admits a non-proxy oracle (see
            // `offchain_priceable`); reaching here is a bug, not a market state.
            return Err(LiquidatorError::PriceFetchError(
                crate::rpc::RpcError::WrongResponseKind(format!(
                    "oracle {oracle} is not a proxy oracle; only off-chain-composable proxy oracles are supported"
                )),
            ));
        }
        // Off-chain only: feeds no candidate could price stay missing, and
        // the caller's `covers_all` gate fails the market scan loudly — the
        // same fail-closed semantics a stale price already had.
        Ok(self
            .compose_proxy_prices_offchain(&oracle, price_ids, age)
            .await)
    }

    /// Registration-time admission: scan prices are off-chain only, so a
    /// market is admitted only when its oracle is a proxy and every feed has
    /// at least one off-chain-composable source (RedStone; Pyth Pro when the
    /// API leg is configured). Filtering here — one log per refresh — beats
    /// failing the market's scan every round and degrading `/healthz`. A
    /// transient proxy-config read failure filters the market for one
    /// refresh cycle (it is re-probed next refresh); a probe error on the
    /// proxy interface itself is propagated. An oracle without the proxy
    /// interface is filtered outright — only proxy oracles can be priced and
    /// pushed — and its reason names the usual cause on NEAR (a Pyth Core–
    /// backed oracle, direct or LST, which can never be refreshed again since
    /// the 2026-08-26 16:00 UTC cutover) and the fix, since the market
    /// returns by itself at the refresh after it is re-pointed to a proxy.
    pub async fn offchain_priceable(
        &self,
        oracle: &AccountId,
        price_ids: &[PriceIdentifier],
    ) -> LiquidatorResult<Option<&'static str>> {
        if !self.is_proxy_oracle(oracle).await? {
            return Ok(Some(
                "oracle has no proxy interface (`list_proxies`): only proxy oracles can be priced off-chain and pushed by the bot; on NEAR this is usually a Pyth Core–backed oracle, which can never be refreshed again since Pyth Core dropped NEAR (2026-08-26 16:00 UTC) — re-point the market to a proxy oracle",
            ));
        }
        let plans = self.resolve_offchain_plans(oracle, price_ids).await;
        Ok(offchain_admission(price_ids, &plans))
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
                    let candidates =
                        plan_offchain_sources(proxy.sources(), self.lazer_api.is_some());
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

    /// Composes proxy-oracle prices off-chain from each feed's configured
    /// sources, in order — the first candidate yielding a fresh price wins:
    /// RedStone via the public price API, Pyth Pro via its price API,
    /// transformers applied with their on-chain input (a free view call).
    ///
    /// Returns only the feeds it could price; the rest stay missing and the
    /// caller's coverage gate decides. Costs no gas anywhere: proxy configs
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
                requested = price_ids.len(),
                "Composed proxy prices off-chain"
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

/// Milliseconds since the Unix epoch, saturating — for the RedStone
/// timestamp window only.
fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
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

    /// An oracle without the proxy interface is filtered with a reason that
    /// states the requirement, the usual cause on NEAR (a Pyth Core–backed
    /// oracle, which can never be refreshed again since the cutover), and the
    /// fix (re-pointing the market to a proxy oracle), so the log reads as an
    /// action, not a state.
    #[tokio::test]
    async fn non_proxy_oracle_is_filtered_with_the_near_cutover_reason() {
        let not_found = r#"{"jsonrpc":"2.0","id":"x","error":{"name":"HANDLER_ERROR","cause":{"name":"CONTRACT_EXECUTION_ERROR","info":{"vm_error":"FunctionCallError(MethodResolveError(MethodNotFound))","block_height":1,"block_hash":"11111111111111111111111111111111"}},"code":-32000,"message":"Server error","data":"wasm execution failed with error: FunctionCallError(MethodResolveError(MethodNotFound))"}}"#;
        let (url, requests) =
            crate::rpc::test_support::scripted_server(vec![(200, not_found.to_string()); 2]).await;
        let fetcher = crate::rpc::test_support::oracle_fetcher_for(url.as_str());

        let reason = fetcher
            .offchain_priceable(
                &"pyth-oracle.near".parse().unwrap(),
                &[PriceIdentifier([0xAA; 32])],
            )
            .await
            .unwrap()
            .expect("a non-proxy oracle is filtered");

        assert!(
            reason.contains("Pyth Core")
                && reason.contains("NEAR")
                && reason.contains("proxy oracle"),
            "reason must name the cause and the fix: {reason}"
        );
        assert_eq!(requests.lock().unwrap().len(), 1, "one probe, no retries");
    }

    /// RedStone is pushed only for feeds the Pyth Pro push cannot refresh:
    /// with the Pyth Pro push available, a feed with any Pyth Pro source
    /// yields no RedStone target (one transaction fewer per liquidation); a
    /// feed with only Pyth Core and/or RedStone sources yields its RedStone
    /// adapters and feed ids, transformers unwrapped; and without the Pyth
    /// Pro push (no token) even a mixed feed is RedStone-pushed.
    #[test]
    fn redstone_targets_cover_only_feeds_without_a_pyth_pro_source() {
        let mut targets = HashMap::new();
        collect_redstone_targets_for_feed(
            [&lazer_source(), &redstone_source()].into_iter(),
            true,
            &mut targets,
        );
        assert!(
            targets.is_empty(),
            "a Pyth Pro–sourced feed needs no RedStone push"
        );

        collect_redstone_targets_for_feed(
            [&pyth_source(), &redstone_source()].into_iter(),
            true,
            &mut targets,
        );
        let adapter: AccountId = "redstone.near".parse().unwrap();
        assert_eq!(
            targets[&adapter],
            std::collections::BTreeSet::from([RedStoneFeedId::from("LTC")])
        );

        let mut under_transformer = HashMap::new();
        collect_redstone_targets_for_feed(
            [&transformer_source(redstone_request())].into_iter(),
            true,
            &mut under_transformer,
        );
        assert!(under_transformer[&adapter].contains(&RedStoneFeedId::from("LTC")));

        // Without a Pyth Pro push (no token) the Pyth Pro source refreshes
        // nothing, so the mixed feed's RedStone source is the only way to a
        // fresh price and is pushed.
        let mut tokenless = HashMap::new();
        collect_redstone_targets_for_feed(
            [&lazer_source(), &redstone_source()].into_iter(),
            false,
            &mut tokenless,
        );
        assert!(tokenless[&adapter].contains(&RedStoneFeedId::from("LTC")));
    }

    /// The push-check freshness read goes through the proxy's own
    /// `list_ema_prices_no_older_than` view with a wide bound, so what the
    /// operator sees is exactly what the market would read; a feed the proxy
    /// has never cached comes back as `None`, not as an error.
    #[tokio::test]
    async fn onchain_publish_times_read_the_proxy_cache() {
        let a = PriceIdentifier([0xAA; 32]);
        let b = PriceIdentifier([0xBB; 32]);
        let view = format!(
            r#"{{"{}":{{"price":"7887689348217","conf":"2256651783","expo":-8,"publish_time":1787699242}},"{}":null}}"#,
            hex::encode(a.0),
            hex::encode(b.0)
        );
        let bytes: Vec<String> = view.bytes().map(|x| x.to_string()).collect();
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":"x","result":{{"result":[{}],"logs":[],"block_height":1,"block_hash":"11111111111111111111111111111111"}}}}"#,
            bytes.join(",")
        );
        let (url, requests) = crate::rpc::test_support::scripted_server(vec![(200, body)]).await;
        let fetcher = crate::rpc::test_support::oracle_fetcher_for(url.as_str());

        let times = fetcher
            .onchain_publish_times(&"proxy.test.near".parse().unwrap(), &[a, b])
            .await
            .expect("view read succeeds");

        assert_eq!(times.get(&a), Some(&Some(1_787_699_242)));
        assert_eq!(times.get(&b), Some(&None));
        let sent = requests.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(
            sent[0].contains(r#""method_name":"list_ema_prices_no_older_than""#),
            "{}",
            sent[0]
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
        let plans = plan_offchain_sources([&lazer_source(), &redstone_source()].into_iter(), true);
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

    /// Scan prices are off-chain only: Pyth Core sources contribute no
    /// candidate (not integrated), and Pyth Pro sources contribute one only
    /// when the API leg is configured — there is no adapter read to fall
    /// back to, so without a token the feed is simply not priceable.
    #[test]
    fn only_offchain_composable_sources_yield_candidates() {
        assert_eq!(classify_offchain_request(&pyth_request(), true), None);
        assert_eq!(classify_offchain_request(&lazer_request(), false), None);
        assert!(matches!(
            classify_offchain_request(&redstone_request(), false),
            Some(OffchainRequest::RedStone(_))
        ));
        assert!(matches!(
            classify_offchain_request(&lazer_request(), true),
            Some(OffchainRequest::Lazer { feed_id: 7, .. })
        ));
        // A proxy whose only source is Pyth Core composes nothing.
        assert!(plan_offchain_sources([&pyth_source()].into_iter(), true).is_empty());
    }

    #[test]
    fn plan_is_empty_when_no_source_is_configured() {
        assert!(plan_offchain_sources([].into_iter(), true).is_empty());
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
                plan_offchain_sources([&redstone_source()].into_iter(), true),
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
        let plan = plan_offchain_sources([&transformer_source(inner)].into_iter(), true)
            .into_iter()
            .next()
            .expect("transformer over a Pyth Pro feed is priceable off-chain");
        match plan {
            OffchainPriceSource::Transformed { request, .. } => {
                assert_eq!(
                    request,
                    OffchainRequest::Lazer {
                        oracle_id: "pyth-lazer.near".parse().unwrap(),
                        feed_id: 9
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
            true,
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
                    }
                );
            }
            other @ OffchainPriceSource::Direct(_) => panic!("expected Transformed, got {other:?}"),
        }
    }
}
