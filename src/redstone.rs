//! RedStone authenticated-gateway client: the one source of RedStone data,
//! for both legs.
//!
//! RedStone switched its public gateways and its public price API
//! (`api.redstone.finance`) off for good on 2026-10-29 00:00 UTC; those hosts
//! answer `403 public_gateway_rejected` and name the replacement:
//! *authenticated gateways*, each a URL plus an API key sent as `x-api-key`
//! — the RedStone SDK's `authenticatedGateways` parameter, in the same
//! `{url, apiKey}` shape. They serve one endpoint,
//! `GET {url}/v2/data-packages/latest-by-data-feeds/{service}?dataFeedIds=A,B`,
//! answering with the feed → signed-packages map
//! `redstone_push::parse_gateway_response` reads. Both legs use it:
//!
//! - scan time (`scan_prices`): each feed's median across its signers.
//!   Advisory, like every scan price — execution re-prices on-chain, where
//!   the market contract re-validates fail-closed;
//! - execution time ([`crate::redstone_push`]): every signature recovered
//!   locally against the adapter's own signer set before a write.
//!
//! Keys are rate-limited, so one client is shared by every market's fetcher:
//! every fetch asks for the union of the feeds ever asked of it, and its
//! answer serves every caller whose feeds it covered for `PACKAGE_TTL`
//! (10 s). Package timestamps fall on 10 s boundaries — the data service
//! signs a new set every 10 s — so asking sooner returns the packages already
//! held. Once every market's feeds have been asked for once, a scan round
//! costs a request per TTL window, not one per market.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use templar_common::oracle::pyth;
use url::Url;

use crate::redstone_push::{parse_gateway_response, scale_value, SignedPackage};

/// Exponent used for synthesized [`pyth::Price`] values: mantissa × 10⁻⁸.
/// Matches Pyth's own convention for USD feeds, so downstream conversions
/// (`create_price_pair`, profitability) treat both sources identically —
/// and RedStone's own 8-decimal value encoding, so a scaled package value
/// is the mantissa as-is.
pub(crate) const SYNTH_EXPO: i32 = -8;

/// How far ahead of our clock a quote's publish time may sit before it is
/// rejected as implausible rather than trusted as fresh. Staleness checks
/// compare `now - publish_time` against a bound, and a future-dated quote
/// yields a negative age that passes every bound there is.
pub(crate) const MAX_FUTURE_SKEW_MS: i64 = 30_000;

/// How long one fetch answers for the feeds it asked about. The data service
/// signs a new package set every 10 s, so a shorter TTL buys no fresher
/// packages, only requests against a rate-limited key.
pub(crate) const PACKAGE_TTL: Duration = Duration::from_secs(10);

/// How long a failed fetch is answered from memory for scan callers before
/// the gateways are asked again. Without it, every market in a round re-runs
/// the full failover chain against gateways that just failed it. The
/// execution-time push never reads this memo (`packages_for_push`).
pub(crate) const FAILURE_BACKOFF: Duration = Duration::from_secs(10);

/// Per-request timeout. A fetch tries each gateway in turn, so the worst
/// case is this times the gateway count.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// What a gateway answers when none of the requested feeds is tracked
/// ("Data packages response is empty"). A request naming some untracked
/// feeds answers 200 without them, so 513 means "nothing to price", not a
/// gateway failure — the other gateway would answer the same.
const STATUS_NO_TRACKED_FEEDS: u16 = 513;

/// Signed packages per feed id, as one fetch returned them.
pub type PackageMap = HashMap<String, Vec<SignedPackage>>;

/// One authenticated gateway: an `https` URL (origin plus an optional path
/// prefix — no userinfo, query or fragment) and the API key it expects.
/// Validated at construction; `Debug` renders the origin only and never the
/// key.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthenticatedGateway {
    url: Url,
    api_key: String,
}

impl std::fmt::Debug for AuthenticatedGateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthenticatedGateway")
            .field("url", &self.url.origin().ascii_serialization())
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl AuthenticatedGateway {
    /// The origin, for logs and error messages — never the full URL, which
    /// is where an operator's mistake would put a credential.
    fn origin(&self) -> String {
        self.url.origin().ascii_serialization()
    }
}

/// One `{url, apiKey}` entry as `REDSTONE_AUTHENTICATED_GATEWAYS` spells it.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawGateway {
    url: String,
    api_key: String,
}

/// What the client is built from: the gateways in failover order (primary
/// first) and the data service whose packages both legs read.
#[derive(Clone, PartialEq, Eq)]
pub struct RedStoneGatewayConfig {
    gateways: Vec<AuthenticatedGateway>,
    data_service_id: String,
}

impl std::fmt::Debug for RedStoneGatewayConfig {
    /// Delegates to [`AuthenticatedGateway`]'s redacting `Debug`; this is
    /// what `ServiceConfig`'s `Debug` prints.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedStoneGatewayConfig")
            .field("gateways", &self.gateways)
            .field("data_service_id", &self.data_service_id)
            .finish()
    }
}

impl RedStoneGatewayConfig {
    /// Parses `REDSTONE_AUTHENTICATED_GATEWAYS`: a JSON array of
    /// `{"url": "https://…", "apiKey": "…"}` objects, primary first. A blank
    /// value or `[]` is "not configured" (`Ok(None)`).
    ///
    /// # Errors
    ///
    /// A value that is not such an array, or an entry whose URL is not a
    /// plain `https` origin (optionally with a path prefix) or whose key is
    /// empty or carries whitespace. No message echoes the value or any part
    /// of it: the value carries the keys, and serde's own messages quote the
    /// offending input.
    pub fn parse(raw: &str, data_service_id: String) -> Result<Option<Self>, String> {
        const KNOB: &str = "REDSTONE_AUTHENTICATED_GATEWAYS";
        // One surrounding pair of single quotes is dropped: a `.env` line
        // must single-quote this JSON for the run scripts' `source` to keep
        // its double quotes, and `docker run --env-file` passes those quotes
        // through verbatim where bash and Compose strip them. A JSON array
        // never begins with `'`, so nothing valid is reinterpreted.
        let raw = raw.trim();
        let raw = raw
            .strip_prefix('\'')
            .and_then(|inner| inner.strip_suffix('\''))
            .unwrap_or(raw)
            .trim();
        if raw.is_empty() {
            return Ok(None);
        }
        let entries: Vec<RawGateway> = near_sdk::serde_json::from_str(raw).map_err(|error| {
            format!(
                "{KNOB} must be a JSON array of {{\"url\": \"https://…\", \"apiKey\": \"…\"}} objects ({:?} error at line {}, column {}; value withheld — it carries API keys)",
                error.classify(),
                error.line(),
                error.column()
            )
        })?;
        if entries.is_empty() {
            return Ok(None);
        }
        let mut gateways = Vec::with_capacity(entries.len());
        for (index, entry) in entries.into_iter().enumerate() {
            let url = Url::parse(&entry.url)
                .map_err(|error| format!("{KNOB} entry {index}: url does not parse ({error})"))?;
            if url.scheme() != "https" {
                return Err(format!(
                    "{KNOB} entry {index}: url must be https (got scheme '{}'): the API key would otherwise travel in cleartext",
                    url.scheme()
                ));
            }
            // The endpoint is appended to the URL's path, and only the origin
            // is ever logged, so anything beyond origin and path is either a
            // misplaced credential or a parameter the request would drop.
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(format!(
                    "{KNOB} entry {index}: url must carry no credentials, query or fragment — only an origin and an optional path prefix; the key goes in apiKey"
                ));
            }
            if entry.api_key.is_empty() || !entry.api_key.bytes().all(|b| b.is_ascii_graphic()) {
                return Err(format!(
                    "{KNOB} entry {index}: apiKey must be non-empty printable ASCII with no whitespace (a trailing newline from `echo` is the usual cause; value withheld)"
                ));
            }
            gateways.push(AuthenticatedGateway {
                url,
                api_key: entry.api_key,
            });
        }
        Ok(Some(Self {
            gateways,
            data_service_id,
        }))
    }

    #[must_use]
    pub fn data_service_id(&self) -> &str {
        &self.data_service_id
    }

    #[must_use]
    pub fn gateway_count(&self) -> usize {
        self.gateways.len()
    }
}

/// The by-feeds URL on one gateway: the endpoint is appended to the
/// configured path (a prefix is kept), each segment encoded, so a data
/// service id can never add a segment of its own; the feed ids go in one
/// encoded `dataFeedIds` parameter (the gateway accepts the encoded comma),
/// so an id from on-chain config can never append a parameter.
pub(crate) fn by_feeds_url(
    gateway_url: &Url,
    data_service_id: &str,
    feeds: &BTreeSet<String>,
) -> Url {
    let mut url = gateway_url.clone();
    url.set_query(None);
    url.set_fragment(None);
    // An https URL always has a base, so this never declines.
    if let Ok(mut segments) = url.path_segments_mut() {
        segments.pop_if_empty().extend([
            "v2",
            "data-packages",
            "latest-by-data-feeds",
            data_service_id,
        ]);
    }
    let joined = feeds
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(",");
    url.query_pairs_mut().append_pair("dataFeedIds", &joined);
    url
}

/// The last fetch and what it was asked for.
struct Snapshot {
    fetched_at: tokio::time::Instant,
    requested: BTreeSet<String>,
    packages: Arc<PackageMap>,
}

#[derive(Default)]
struct FetchState {
    /// Every feed ever asked of this client. Each fetch asks for all of
    /// them, so the markets of one round share a request.
    wanted: BTreeSet<String>,
    last: Option<Snapshot>,
    last_failure: Option<(tokio::time::Instant, String)>,
}

/// Client for RedStone's authenticated gateways. Built once and shared
/// (`Arc`) by every market's fetcher: the cache is what keeps a scan round
/// within the keys' rate limit.
///
/// Follows no redirects: every request carries an API key, and the packages'
/// signatures do not decide whose view of the feed set is fetched — a
/// redirect is a failed request, and the next gateway is tried.
pub struct RedStoneGatewayClient {
    http: reqwest::Client,
    config: RedStoneGatewayConfig,
    ttl: Duration,
    failure_backoff: Duration,
    /// Held across the fetch: concurrent callers wait for one request and
    /// then read its result, rather than each sending their own.
    state: tokio::sync::Mutex<FetchState>,
}

impl std::fmt::Debug for RedStoneGatewayClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedStoneGatewayClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl RedStoneGatewayClient {
    /// # Errors
    ///
    /// The no-redirect client failing to build — the caller disables
    /// RedStone rather than fall back to a redirect-following client.
    pub fn new(config: RedStoneGatewayConfig) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| {
                format!(
                    "RedStone gateway client could not be built: {}",
                    error.without_url()
                )
            })?;
        Ok(Self {
            http,
            config,
            ttl: PACKAGE_TTL,
            failure_backoff: FAILURE_BACKOFF,
            state: tokio::sync::Mutex::new(FetchState::default()),
        })
    }

    #[must_use]
    pub fn config(&self) -> &RedStoneGatewayConfig {
        &self.config
    }

    /// Signed packages for `feeds`: from the last fetch when it asked for
    /// all of them within the TTL, otherwise from a fresh fetch of every
    /// wanted feed. A feed the service does not track is absent from the
    /// map — callers treat absent as unpriced, never as zero.
    ///
    /// # Errors
    ///
    /// Every gateway failing (transport, a non-success status other than
    /// 513, a redirect, an unparseable body). The failure is answered from
    /// memory for `FAILURE_BACKOFF` (10 s). Messages name gateways by origin and
    /// never carry a key or a response body.
    pub async fn packages(&self, feeds: &[String]) -> Result<Arc<PackageMap>, String> {
        self.packages_inner(feeds, true).await
    }

    /// [`Self::packages`] for the execution-time push: a fresh snapshot is
    /// still reused, but a recent failure is never replayed. A liquidation
    /// is waiting on this read, so it asks the gateways itself rather than
    /// inherit a failure some other market's scan recorded up to 10 s ago
    /// against gateways that may have recovered since. Its own failure is
    /// recorded for the scan callers.
    ///
    /// # Errors
    ///
    /// As [`Self::packages`], without the replay.
    pub async fn packages_for_push(&self, feeds: &[String]) -> Result<Arc<PackageMap>, String> {
        self.packages_inner(feeds, false).await
    }

    async fn packages_inner(
        &self,
        feeds: &[String],
        replay_failure: bool,
    ) -> Result<Arc<PackageMap>, String> {
        if feeds.is_empty() {
            return Ok(Arc::default());
        }
        let mut state = self.state.lock().await;
        state.wanted.extend(feeds.iter().cloned());
        let now = tokio::time::Instant::now();
        if let Some(last) = &state.last {
            if now.saturating_duration_since(last.fetched_at) < self.ttl
                && feeds.iter().all(|feed| last.requested.contains(feed))
            {
                return Ok(Arc::clone(&last.packages));
            }
        }
        if let Some((failed_at, error)) = state.last_failure.as_ref().filter(|_| replay_failure) {
            if now.saturating_duration_since(*failed_at) < self.failure_backoff {
                return Err(error.clone());
            }
        }
        let requested = state.wanted.clone();
        match self.fetch(&requested).await {
            Ok(packages) => {
                let packages = Arc::new(packages);
                state.last = Some(Snapshot {
                    fetched_at: tokio::time::Instant::now(),
                    requested,
                    packages: Arc::clone(&packages),
                });
                state.last_failure = None;
                Ok(packages)
            }
            Err(error) => {
                state.last_failure = Some((tokio::time::Instant::now(), error.clone()));
                Err(error)
            }
        }
    }

    /// One fetch: each gateway in order until one answers.
    async fn fetch(&self, feeds: &BTreeSet<String>) -> Result<PackageMap, String> {
        let mut failures = Vec::with_capacity(self.config.gateways.len());
        for gateway in &self.config.gateways {
            match self.fetch_from(gateway, feeds).await {
                Ok(packages) => {
                    tracing::debug!(
                        gateway = %gateway.origin(),
                        requested = feeds.len(),
                        served = packages.len(),
                        "Fetched RedStone packages"
                    );
                    return Ok(packages);
                }
                Err(error) => {
                    tracing::warn!(gateway = %gateway.origin(), %error, "RedStone gateway failed");
                    failures.push(format!("{}: {error}", gateway.origin()));
                }
            }
        }
        Err(format!(
            "every RedStone gateway failed ({})",
            failures.join("; ")
        ))
    }

    async fn fetch_from(
        &self,
        gateway: &AuthenticatedGateway,
        feeds: &BTreeSet<String>,
    ) -> Result<PackageMap, String> {
        let mut key = reqwest::header::HeaderValue::from_str(&gateway.api_key)
            .map_err(|_| "API key is not a valid header value".to_string())?;
        key.set_sensitive(true);
        let url = by_feeds_url(&gateway.url, &self.config.data_service_id, feeds);
        let response = self
            .http
            .get(url)
            .header("x-api-key", key)
            .send()
            .await
            .map_err(|error| format!("request failed: {}", error.without_url()))?;
        let status = response.status();
        if status.as_u16() == STATUS_NO_TRACKED_FEEDS {
            // Not a gateway failure — every gateway serves the same service,
            // so failing over would only repeat it — but rarely healthy
            // either: the usual cause is a data service that tracks none of
            // these feeds. Said here, since nothing else on the RedStone path
            // would say why every RedStone-priced feed is unpriced; once per
            // fetch, so at most once per TTL window.
            tracing::warn!(
                gateway = %gateway.origin(),
                data_service = %self.config.data_service_id,
                requested = feeds.len(),
                "RedStone gateway tracks none of the requested feeds (513); check REDSTONE_DATA_SERVICE_ID"
            );
            return Ok(PackageMap::new());
        }
        // The body of a refused authenticated request is never logged or
        // returned: only the status says what happened.
        if !status.is_success() {
            return Err(format!("returned {status}"));
        }
        let body = response
            .text()
            .await
            .map_err(|error| format!("body could not be read: {}", error.without_url()))?;
        parse_gateway_response(&body)
    }

    #[cfg(test)]
    fn with_timing(mut self, ttl: Duration, failure_backoff: Duration) -> Self {
        self.ttl = ttl;
        self.failure_backoff = failure_backoff;
        self
    }
}

/// Scan-time prices for `symbols` from gateway packages: per feed, the
/// median value across distinct signers (the mean of the middle two for an
/// even count, rounded half up) and the lower median of their timestamps —
/// one lagging signer neither moves the price nor ages the quote. Values
/// stay exact integers at [`SYNTH_EXPO`] throughout; no float is involved.
///
/// Freshness guards every feed must pass, as for every scan leg:
///
/// - a timestamp of zero — rejected;
/// - older than `max_age_ms` — rejected as stale;
/// - more than [`MAX_FUTURE_SKEW_MS`] ahead of `now_ms` — rejected as
///   implausible.
///
/// No signer quorum is required and no signature is recovered here: a scan
/// price is advisory, and the push leg enforces the adapter's signer set and
/// threshold before anything is written. A symbol with no usable package is
/// absent from the result — unpriced, never zero.
pub(crate) fn scan_prices(
    packages: &PackageMap,
    symbols: &[String],
    now_ms: i64,
    max_age_ms: i64,
) -> HashMap<String, pyth::Price> {
    let mut prices = HashMap::with_capacity(symbols.len());
    for symbol in symbols {
        let Some(list) = packages.get(symbol) else {
            continue;
        };
        let mut signers = BTreeSet::new();
        let mut values = Vec::with_capacity(list.len());
        let mut stamps = Vec::with_capacity(list.len());
        for package in list {
            let Some(value) = scale_value(&package.value_text) else {
                continue;
            };
            if !signers.insert(package.claimed_signer) {
                continue;
            }
            values.push(value);
            stamps.push(package.timestamp_ms);
        }
        if values.is_empty() {
            tracing::debug!(%symbol, "No usable RedStone package, skipping");
            continue;
        }
        values.sort_unstable();
        stamps.sort_unstable();
        let mid = values.len() / 2;
        let median = if values.len() % 2 == 1 {
            Some(values[mid])
        } else {
            values[mid - 1]
                .checked_add(values[mid])
                .and_then(|sum| sum.checked_add(1))
                .map(|sum| sum / 2)
        };
        let Some(mantissa) = median
            .and_then(|median| i64::try_from(median).ok())
            .filter(|mantissa| *mantissa >= 1)
        else {
            tracing::warn!(%symbol, "RedStone median is zero or out of range, skipping");
            continue;
        };
        let Ok(timestamp_ms) = i64::try_from(stamps[(stamps.len() - 1) / 2]) else {
            tracing::warn!(%symbol, "RedStone timestamp out of range, skipping");
            continue;
        };
        if timestamp_ms <= 0 {
            tracing::warn!(%symbol, "RedStone package has no timestamp, skipping");
            continue;
        }
        let age_ms = now_ms - timestamp_ms;
        if age_ms > max_age_ms {
            tracing::debug!(%symbol, age_ms, max_age_ms, "RedStone quote is stale, skipping");
            continue;
        }
        if age_ms < -MAX_FUTURE_SKEW_MS {
            tracing::warn!(%symbol, lead_ms = -age_ms, "RedStone quote is future-dated, skipping");
            continue;
        }
        prices.insert(
            symbol.clone(),
            pyth::Price {
                price: near_sdk::json_types::I64(mantissa),
                conf: near_sdk::json_types::U64(0),
                expo: SYNTH_EXPO,
                publish_time: pyth::PythTimestamp::from_secs(timestamp_ms / 1000),
            },
        );
    }
    prices
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redstone_push::tests::{FIXTURE, FIXTURE_TS_MS};

    const KEY: &str = "test-key-0123456789abcdef";
    const SERVICE: &str = "redstone-primary-prod";

    fn feeds(ids: &[&str]) -> Vec<String> {
        ids.iter().map(ToString::to_string).collect()
    }

    /// A client over plain-http local servers. Built from the struct, not
    /// [`RedStoneGatewayConfig::parse`], which refuses http by design.
    fn client_for(urls: &[&Url]) -> RedStoneGatewayClient {
        RedStoneGatewayClient::new(RedStoneGatewayConfig {
            gateways: urls
                .iter()
                .map(|url| AuthenticatedGateway {
                    url: (*url).clone(),
                    api_key: KEY.to_string(),
                })
                .collect(),
            data_service_id: SERVICE.to_string(),
        })
        .unwrap()
    }

    fn parse(raw: &str) -> Result<Option<RedStoneGatewayConfig>, String> {
        RedStoneGatewayConfig::parse(raw, SERVICE.to_string())
    }

    // ── Configuration ────────────────────────────────────────────────────

    /// The SDK's `authenticatedGateways` shape, in JSON, primary first.
    #[test]
    fn parse_keeps_every_gateway_in_order() {
        let config = parse(
            r#"[{"url":"https://primary.example","apiKey":"k1"},{"url":"https://backup.example/prefix/","apiKey":"k2"}]"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(config.gateway_count(), 2);
        assert_eq!(config.gateways[0].origin(), "https://primary.example");
        assert_eq!(config.gateways[0].api_key, "k1");
        assert_eq!(config.gateways[1].url.path(), "/prefix/");
        assert_eq!(config.data_service_id(), SERVICE);
    }

    #[test]
    fn blank_or_empty_is_not_configured() {
        assert_eq!(parse("").unwrap(), None);
        assert_eq!(parse("  \n").unwrap(), None);
        assert_eq!(parse("[]").unwrap(), None);
        assert_eq!(parse("''").unwrap(), None);
    }

    /// The `.env` form — single-quoted so `source` keeps the JSON's double
    /// quotes — reaches the binary with its quotes under
    /// `docker run --env-file`, and must parse the same as the bare JSON.
    #[test]
    fn a_single_quoted_env_file_value_parses_like_bare_json() {
        let bare = r#"[{"url":"https://primary.example","apiKey":"k1"}]"#;
        assert_eq!(parse(&format!("'{bare}'")).unwrap(), parse(bare).unwrap());
        assert_eq!(
            parse(&format!(" '{bare}'\n")).unwrap(),
            parse(bare).unwrap()
        );
        // Only a matching pair is dropped.
        assert!(parse(&format!("'{bare}")).is_err());
    }

    /// Every rejection names the knob and the entry, never the value: the
    /// value carries the keys, and serde quotes the input it chokes on.
    #[test]
    fn rejections_never_echo_the_value() {
        let secret = "s3cr3t-k3y-material";
        let cases = [
            // Not JSON: the SDK's JS-object form, unquoted keys.
            format!(r#"[{{url: "https://a.example", apiKey: "{secret}"}}]"#),
            // A bare string — serde's own message would quote it.
            format!(r#""{secret}""#),
            format!(r#"[{{"url":"https://a.example","apiKey":{{"v":"{secret}"}}}}]"#),
            format!(r#"[{{"url":"http://a.example","apiKey":"{secret}"}}]"#),
            format!(r#"[{{"url":"https://u:{secret}@a.example","apiKey":"k"}}]"#),
            format!(r#"[{{"url":"https://a.example?key={secret}","apiKey":"k"}}]"#),
            format!(r#"[{{"url":"https://a.example#{secret}","apiKey":"k"}}]"#),
            format!(r#"[{{"url":"https://a.example","apiKey":"{secret}\n"}}]"#),
            format!(r#"[{{"url":"https://a.example","apiKey":"{secret} x"}}]"#),
            r#"[{"url":"https://a.example","apiKey":""}]"#.to_string(),
            format!(r#"[{{"url":"not a url {secret}","apiKey":"k"}}]"#),
        ];
        for raw in cases {
            let error = parse(&raw).unwrap_err();
            assert!(
                error.contains("REDSTONE_AUTHENTICATED_GATEWAYS"),
                "names the knob: {error}"
            );
            assert!(!error.contains(secret), "echoed the value: {error}");
        }
    }

    #[test]
    fn rejections_say_which_entry_and_why() {
        let error = parse(
            r#"[{"url":"https://a.example","apiKey":"k"},{"url":"http://b.example","apiKey":"k"}]"#,
        )
        .unwrap_err();
        assert!(
            error.contains("entry 1") && error.contains("https"),
            "{error}"
        );
    }

    /// Every rendering that can reach a log — the config `ServiceConfig`
    /// prints, a gateway, the client — shows origins and never a key.
    #[test]
    fn debug_never_shows_a_key() {
        let config = parse(
            r#"[{"url":"https://primary.example/p","apiKey":"key-one"},{"url":"https://backup.example","apiKey":"key-two"}]"#,
        )
        .unwrap()
        .unwrap();
        let client = RedStoneGatewayClient::new(config.clone()).unwrap();
        for rendered in [format!("{config:?}"), format!("{client:?}")] {
            assert!(
                !rendered.contains("key-one") && !rendered.contains("key-two"),
                "{rendered}"
            );
            assert!(rendered.contains("https://primary.example"), "{rendered}");
            assert!(rendered.contains("<redacted>"), "{rendered}");
        }
    }

    // ── Request shape ────────────────────────────────────────────────────

    #[test]
    fn by_feeds_url_keeps_a_prefix_and_encodes_what_it_appends() {
        let wanted: BTreeSet<String> = feeds(&["USDC", "BTC"]).into_iter().collect();
        let url = by_feeds_url(&"https://gw.example".parse().unwrap(), SERVICE, &wanted);
        assert_eq!(
            url.as_str(),
            "https://gw.example/v2/data-packages/latest-by-data-feeds/redstone-primary-prod?dataFeedIds=BTC%2CUSDC"
        );
        let prefixed = by_feeds_url(
            &"https://gw.example/redstone/".parse().unwrap(),
            SERVICE,
            &wanted,
        );
        assert_eq!(
            prefixed.path(),
            "/redstone/v2/data-packages/latest-by-data-feeds/redstone-primary-prod"
        );
    }

    /// Feed ids come from on-chain config, so they are not this bot's to
    /// trust: one must not be able to append a parameter, and a service id
    /// must not be able to add a path segment.
    #[test]
    fn by_feeds_url_cannot_be_steered_by_its_inputs() {
        let hostile: BTreeSet<String> = feeds(&["BTC&limit=1"]).into_iter().collect();
        let url = by_feeds_url(
            &"https://gw.example".parse().unwrap(),
            "svc/../x?y",
            &hostile,
        );
        let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        assert_eq!(
            pairs,
            vec![("dataFeedIds".to_string(), "BTC&limit=1".to_string())]
        );
        assert_eq!(url.path_segments().unwrap().count(), 4, "{url}");
    }

    // ── Fetching ─────────────────────────────────────────────────────────

    /// The request is the by-feeds GET, carrying the key in `x-api-key`
    /// and naming every wanted feed.
    #[tokio::test]
    async fn fetch_sends_the_key_and_names_the_feeds() {
        let (url, heads) =
            crate::rpc::test_support::scripted_server_heads(vec![(200, FIXTURE.to_string())]).await;
        let client = client_for(&[&url]);
        let packages = client.packages(&feeds(&["USDC", "BTC"])).await.unwrap();
        assert_eq!(packages["BTC"].len(), 5);
        let heads = heads.lock().unwrap();
        assert_eq!(heads.len(), 1);
        assert!(
            heads[0].starts_with(
                "GET /v2/data-packages/latest-by-data-feeds/redstone-primary-prod?dataFeedIds=BTC%2CUSDC HTTP/1.1\r\n"
            ),
            "{}",
            heads[0]
        );
        assert!(
            heads[0].contains(&format!("x-api-key: {KEY}\r\n")),
            "{}",
            heads[0]
        );
    }

    /// A caller asking only for feeds the last fetch covered, within the
    /// TTL, is answered without a request; one asking for a new feed
    /// triggers a fetch of everything wanted so far.
    #[tokio::test]
    async fn a_round_shares_requests_and_new_feeds_widen_the_next() {
        let (url, heads) = crate::rpc::test_support::scripted_server_heads(vec![
            (200, FIXTURE.to_string()),
            (200, FIXTURE.to_string()),
        ])
        .await;
        let client = client_for(&[&url]);
        client.packages(&feeds(&["USDC", "BTC"])).await.unwrap();
        client.packages(&feeds(&["BTC"])).await.unwrap();
        client.packages(&feeds(&["USDC"])).await.unwrap();
        assert_eq!(
            heads.lock().unwrap().len(),
            1,
            "covered feeds reuse the fetch"
        );

        let packages = client.packages(&feeds(&["CETES"])).await.unwrap();
        assert!(packages.contains_key("CETES"));
        let heads = heads.lock().unwrap();
        assert_eq!(heads.len(), 2);
        assert!(
            heads[1].contains("dataFeedIds=BTC%2CCETES%2CUSDC "),
            "{}",
            heads[1]
        );
    }

    #[tokio::test]
    async fn an_expired_fetch_is_refreshed() {
        let (url, heads) = crate::rpc::test_support::scripted_server_heads(vec![
            (200, FIXTURE.to_string()),
            (200, FIXTURE.to_string()),
        ])
        .await;
        let client = client_for(&[&url]).with_timing(Duration::ZERO, FAILURE_BACKOFF);
        client.packages(&feeds(&["BTC"])).await.unwrap();
        client.packages(&feeds(&["BTC"])).await.unwrap();
        assert_eq!(heads.lock().unwrap().len(), 2);
    }

    /// 513 is the gateway saying none of the feeds is tracked: an empty
    /// answer, not a failure to fail over from.
    #[tokio::test]
    async fn untracked_feeds_read_as_absent() {
        let (url, _) = crate::rpc::test_support::scripted_server_heads(vec![(
            513,
            r#"{"statusCode":513,"message":"Data packages response is empty"}"#.to_string(),
        )])
        .await;
        let packages = client_for(&[&url])
            .packages(&feeds(&["NOT_A_FEED"]))
            .await
            .unwrap();
        assert!(packages.is_empty());
    }

    /// Any non-success status — a 403 for a rejected key, a 429, a
    /// redirect — moves on to the next gateway; the error, once all have
    /// failed, names each by origin with its status and carries no key.
    #[tokio::test]
    async fn a_failing_gateway_fails_over_to_the_next() {
        let (primary, primary_heads) =
            crate::rpc::test_support::scripted_server_heads(vec![(302, String::new())]).await;
        let (backup, backup_heads) =
            crate::rpc::test_support::scripted_server_heads(vec![(200, FIXTURE.to_string())]).await;
        let packages = client_for(&[&primary, &backup])
            .packages(&feeds(&["USDC"]))
            .await
            .unwrap();
        assert_eq!(packages["USDC"].len(), 3);
        assert_eq!(primary_heads.lock().unwrap().len(), 1);
        assert_eq!(backup_heads.lock().unwrap().len(), 1);

        let (a, _) = crate::rpc::test_support::scripted_server_heads(vec![(
            403,
            format!(r#"{{"message":"Forbidden","echo":"{KEY}"}}"#),
        )])
        .await;
        let (b, _) =
            crate::rpc::test_support::scripted_server_heads(vec![(429, String::new())]).await;
        let error = client_for(&[&a, &b])
            .packages(&feeds(&["USDC"]))
            .await
            .unwrap_err();
        assert!(error.contains("403") && error.contains("429"), "{error}");
        assert!(
            !error.contains(KEY),
            "a response body reached the error: {error}"
        );
    }

    /// Inside the backoff a failure is answered from memory: the next
    /// market in the round does not re-run the failover chain.
    #[tokio::test]
    async fn a_failure_is_not_retried_inside_the_backoff() {
        let (url, heads) =
            crate::rpc::test_support::scripted_server_heads(vec![(503, String::new())]).await;
        let client = client_for(&[&url]);
        let first = client.packages(&feeds(&["BTC"])).await.unwrap_err();
        let second = client.packages(&feeds(&["BTC"])).await.unwrap_err();
        assert_eq!(first, second);
        assert_eq!(heads.lock().unwrap().len(), 1);
    }

    /// The execution-time push never inherits a scan's failure: inside the
    /// backoff a scan caller gets the memo, the push asks the gateways — and
    /// its success then serves the scan callers too.
    #[tokio::test]
    async fn the_push_is_never_answered_from_the_failure_memo() {
        let (url, heads) = crate::rpc::test_support::scripted_server_heads(vec![
            (503, String::new()),
            (200, FIXTURE.to_string()),
        ])
        .await;
        let client = client_for(&[&url]);
        let scan_error = client.packages(&feeds(&["BTC"])).await.unwrap_err();
        assert_eq!(
            client.packages(&feeds(&["BTC"])).await.unwrap_err(),
            scan_error,
            "a scan caller inside the backoff gets the memo"
        );
        assert_eq!(heads.lock().unwrap().len(), 1);

        let pushed = client.packages_for_push(&feeds(&["BTC"])).await.unwrap();
        assert_eq!(pushed["BTC"].len(), 5);
        assert_eq!(heads.lock().unwrap().len(), 2, "the push asked the gateway");
        assert!(client.packages(&feeds(&["BTC"])).await.is_ok());
        assert_eq!(
            heads.lock().unwrap().len(),
            2,
            "and its answer serves scans"
        );
    }

    #[tokio::test]
    async fn no_feeds_means_no_request() {
        let (url, heads) = crate::rpc::test_support::scripted_server_heads(vec![]).await;
        assert!(client_for(&[&url]).packages(&[]).await.unwrap().is_empty());
        assert!(heads.lock().unwrap().is_empty());
    }

    // ── Scan pricing ─────────────────────────────────────────────────────

    const MAX_AGE_MS: i64 = 60_000;

    /// RedStone's 8-decimal encoding is the synthesized price's exponent,
    /// so a scaled package value is a mantissa as-is.
    #[test]
    fn synth_expo_matches_the_package_decimals() {
        assert_eq!(
            SYNTH_EXPO,
            -i32::try_from(crate::redstone_push::DECIMALS).unwrap()
        );
    }

    /// Captured packages price to their exact median: BTC's five carry
    /// 78676.8627996 four times and 78676.8577998 once.
    #[test]
    fn scan_prices_take_the_median_across_signers() {
        let packages = parse_gateway_response(FIXTURE).unwrap();
        let now = i64::try_from(FIXTURE_TS_MS).unwrap() + 5_000;
        let prices = scan_prices(
            &packages,
            &feeds(&["BTC", "USDC", "CETES"]),
            now,
            MAX_AGE_MS,
        );
        assert_eq!(prices["BTC"].price.0, 7_867_686_279_960);
        assert_eq!(prices["BTC"].expo, SYNTH_EXPO);
        assert_eq!(prices["BTC"].conf.0, 0);
        assert_eq!(prices["USDC"].price.0, 99_994_000);
        assert_eq!(prices["CETES"].price.0, 6_952_400);
        assert_eq!(
            prices["BTC"].publish_time,
            pyth::PythTimestamp::from_secs(now / 1000 - 5)
        );
    }

    fn package(signer: u8, value: &str, timestamp_ms: u64) -> SignedPackage {
        SignedPackage {
            feed_id: "X".to_string(),
            value_text: value.to_string(),
            timestamp_ms,
            signature: [0; 65],
            claimed_signer: [signer; 20],
        }
    }

    const NOW_MS: i64 = 1_800_000_000_000;

    fn ts(offset_ms: i64) -> u64 {
        u64::try_from(NOW_MS + offset_ms).unwrap()
    }

    /// Even counts average the middle two (half up); a signer counted twice
    /// counts once, so a repeated outlier cannot drag the median; the
    /// timestamp is the lower median, so one lagging signer does not age the
    /// quote.
    #[test]
    fn scan_prices_median_rules() {
        let mut packages = PackageMap::new();
        packages.insert(
            "X".to_string(),
            vec![
                package(1, "1.00000001", ts(-10_000)),
                package(2, "1.00000002", ts(-10_000)),
                package(3, "9", ts(-90_000)),
                package(3, "9", ts(-90_000)),
                package(4, "1.00000004", ts(-10_000)),
            ],
        );
        let price = &scan_prices(&packages, &feeds(&["X"]), NOW_MS, MAX_AGE_MS)["X"];
        // Distinct signers' values: 1.00000001, 1.00000002, 1.00000004, 9 →
        // (100000002 + 100000004 + 1) / 2.
        assert_eq!(price.price.0, 100_000_003);
        // Timestamps sorted: -90s, -10s, -10s, -10s; lower median is -10s.
        assert_eq!(
            price.publish_time,
            pyth::PythTimestamp::from_secs((NOW_MS - 10_000) / 1000)
        );
    }

    #[test]
    fn scan_prices_apply_the_freshness_guards() {
        let mut packages = PackageMap::new();
        packages.insert("GOOD".to_string(), vec![package(1, "2", ts(-1_000))]);
        packages.insert("NO_TS".to_string(), vec![package(1, "2", 0)]);
        packages.insert(
            "STALE".to_string(),
            vec![package(1, "2", ts(-MAX_AGE_MS - 1_000))],
        );
        packages.insert(
            "FUTURE".to_string(),
            vec![package(1, "2", ts(MAX_FUTURE_SKEW_MS + 1_000))],
        );
        packages.insert("JUNK".to_string(), vec![package(1, "junk", ts(-1_000))]);
        packages.insert("ZERO".to_string(), vec![package(1, "0", ts(-1_000))]);
        let wanted = feeds(&["GOOD", "NO_TS", "STALE", "FUTURE", "JUNK", "ZERO", "ABSENT"]);
        let prices = scan_prices(&packages, &wanted, NOW_MS, MAX_AGE_MS);
        assert_eq!(
            prices.keys().collect::<Vec<_>>(),
            vec!["GOOD"],
            "{prices:?}"
        );
        assert_eq!(prices["GOOD"].price.0, 200_000_000);
    }
}
