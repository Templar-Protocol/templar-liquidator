//! Pyth Lazer (Pyth Pro) price API client (scan-side, off-chain).
//!
//! Fetches the latest EMA prices for Lazer feed ids from the Lazer price
//! service (`POST /v1/latest_price`, Bearer-token authenticated — unlike
//! the RedStone public API, Pyth Pro is a subscription service).
//! Used only to compose proxy-oracle prices for scan-time position
//! evaluation; execution-time pricing still goes through the on-chain oracle
//! push in [`crate::oracle::OracleFetcher::update_onchain_prices`], so
//! nothing here can bypass on-chain freshness or circuit-breaker state where
//! funds move.
//!
//! EMA (not spot) is requested deliberately: it is the projection the
//! on-chain proxy's `Lazer` source consumes — every scan-side pricing path
//! speaks the same value.

use std::collections::HashMap;

use templar_common::oracle::pyth::{self, PriceIdentifier};
use url::Url;

/// Pyth Core price id → Pyth Pro feed id: the bridge that prices a Pyth
/// Core–configured feed (a direct-Pyth or LST market, or a Pyth Core source
/// inside a proxy) from the Pyth Pro API. Built from Pyth Pro's public
/// symbols endpoint, where each feed carries its Pyth Core id as `hermes_id`.
pub(crate) type PythCoreToProMap = HashMap<PriceIdentifier, u32>;

/// One entry of the public symbols endpoint — only the bridge fields.
#[derive(serde::Deserialize)]
struct SymbolEntry {
    pyth_lazer_id: u32,
    #[serde(default)]
    hermes_id: Option<String>,
    #[serde(default)]
    state: String,
}

/// Fetches the symbol bridge from the public endpoint. The client is built
/// per fetch and follows no redirects: the map decides which Pyth Pro feed
/// prices a Pyth Core id, so an `https → http` downgrade (which reqwest
/// would otherwise follow) must surface as a failed fetch, not a cleartext
/// map; a client build failure fails the fetch the same way. Entries are
/// parsed one by one so a single malformed entry is skipped (with a
/// warning), not the whole response. Only `stable` feeds with a well-formed
/// 32-byte `hermes_id` (an optional `0x` prefix is accepted) enter the map.
/// A transport or body failure, and a response yielding no usable feed,
/// fail the fetch so the caller keeps the previous map.
pub(crate) async fn fetch_pyth_core_to_pro_map(
    symbols_url: &Url,
) -> Result<PythCoreToProMap, String> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|error| format!("symbols client could not be built: {}", error.without_url()))?;
    let response = http
        .get(symbols_url.clone())
        .send()
        .await
        .map_err(|error| format!("symbols request failed: {}", error.without_url()))?;
    if !response.status().is_success() {
        return Err(format!("symbols endpoint returned {}", response.status()));
    }
    let entries: Vec<near_sdk::serde_json::Value> = response
        .json()
        .await
        .map_err(|error| format!("symbols body did not parse: {}", error.without_url()))?;
    let mut map = PythCoreToProMap::new();
    for (index, value) in entries.into_iter().enumerate() {
        let entry: SymbolEntry = match near_sdk::serde_json::from_value(value) {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(index, %error, "Pyth Pro symbol entry did not parse; skipping it in the bridge");
                continue;
            }
        };
        if entry.state != "stable" {
            continue;
        }
        let Some(hex_id) = entry.hermes_id else {
            continue;
        };
        match hex::decode(hex_id.strip_prefix("0x").unwrap_or(&hex_id)) {
            Ok(bytes) if bytes.len() == 32 => {
                let mut id = [0u8; 32];
                id.copy_from_slice(&bytes);
                map.insert(PriceIdentifier(id), entry.pyth_lazer_id);
            }
            _ => tracing::warn!(
                feed_id = entry.pyth_lazer_id,
                hermes_id = %hex_id,
                "Pyth Pro symbol carries a malformed Pyth Core id; skipping it in the bridge"
            ),
        }
    }
    if map.is_empty() {
        return Err("symbols endpoint listed no usable stable feed with a Pyth Core id; keeping the previous map".to_string());
    }
    Ok(map)
}

/// One feed's entry in the `latest_price` parsed payload. Mantissas arrive
/// as decimal strings; `exponent` scales them (`mantissa * 10^exponent`).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LazerFeedPoint {
    price_feed_id: u32,
    #[serde(default)]
    ema_price: Option<String>,
    /// A bare JSON number on the wire (the SDK's string helper covers
    /// `emaPrice` but not `emaConfidence`); accepted as string too, in case
    /// upstream ever unifies them.
    #[serde(default)]
    ema_confidence: Option<near_sdk::serde_json::Value>,
    #[serde(default)]
    exponent: Option<i16>,
    /// Microseconds, a bare JSON number (`TimestampUs` is transparent over
    /// `u64`, unlike the string-encoded envelope timestamp); accepted as
    /// string too. When this feed last actually updated — the envelope
    /// timestamp only says when the response was assembled. Presence-aware:
    /// an absent field (upstream dropped the property → envelope fallback)
    /// is a different case from an explicit JSON `null` (upstream has no
    /// timestamp for this feed → fail closed).
    #[serde(default, deserialize_with = "present_timestamp")]
    feed_update_timestamp: MaybeTimestamp,
}

/// The three states a requested timestamp field can arrive in. Serde's
/// `Option` collapses absent and `null`; this keeps them apart.
#[derive(Default)]
enum MaybeTimestamp {
    /// Field not in the payload at all.
    #[default]
    Absent,
    /// Field present — possibly `null` or malformed, both fail-closed.
    Present(Option<near_sdk::serde_json::Value>),
}

/// Marks a present field as [`MaybeTimestamp::Present`]; an absent field
/// never reaches this and stays [`MaybeTimestamp::Absent`] via
/// `#[serde(default)]`.
fn present_timestamp<'de, D>(deserializer: D) -> Result<MaybeTimestamp, D::Error>
where
    D: near_sdk::serde::Deserializer<'de>,
{
    near_sdk::serde::Deserialize::deserialize(deserializer).map(MaybeTimestamp::Present)
}

/// The `parsed` object of a `latest_price` response: the envelope's own
/// assembly timestamp (microseconds since the epoch, as a decimal string),
/// which prices a feed only as the fallback when that feed carries no
/// `feedUpdateTimestamp` of its own.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LazerParsedPayload {
    timestamp_us: String,
    price_feeds: Vec<LazerFeedPoint>,
}

#[derive(serde::Deserialize)]
struct LazerLatestPriceResponse {
    parsed: Option<LazerParsedPayload>,
}

/// Extracts an integer that upstream serializes inconsistently — some fields
/// arrive as bare JSON numbers, others through a to-string helper.
fn value_as_i64(value: &near_sdk::serde_json::Value) -> Option<i64> {
    match value {
        near_sdk::serde_json::Value::Number(n) => n.as_i64(),
        near_sdk::serde_json::Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

/// Parses a `latest_price` response into per-feed EMA prices, enforcing the
/// market's freshness bound per feed: a feed older than `max_age_secs`, or
/// further ahead of `now_secs` than clock skew explains
/// ([`crate::redstone::MAX_FUTURE_SKEW_MS`]), is skipped without discarding
/// its fresh siblings — absent beats a wrong number. A feed missing its EMA
/// mantissa, its exponent, or a usable EMA confidence is likewise skipped
/// individually, as is one whose `feedUpdateTimestamp` is present but `null`
/// or unparseable — its own age is then unknowable, so it fails closed
/// rather than riding the envelope's assembly time (the confidence and
/// timestamp gates are the easy ones to miss when debugging an empty result
/// against a response that visibly carries an EMA price).
pub(crate) fn parse_latest_price_response(
    body: &str,
    now_secs: i64,
    max_age_secs: u32,
) -> HashMap<u32, pyth::Price> {
    let mut prices = HashMap::new();
    let Ok(response) = near_sdk::serde_json::from_str::<LazerLatestPriceResponse>(body) else {
        tracing::warn!("Unparseable Lazer latest_price response");
        return prices;
    };
    let Some(parsed) = response.parsed else {
        tracing::warn!("Lazer latest_price response has no parsed payload");
        return prices;
    };
    let Some(envelope_secs) = parsed
        .timestamp_us
        .parse::<i64>()
        .ok()
        .map(|us| us / 1_000_000)
    else {
        tracing::warn!(timestamp = %parsed.timestamp_us, "Unparseable Lazer update timestamp");
        return prices;
    };

    for feed in parsed.price_feeds {
        let (Some(ema), Some(expo)) = (feed.ema_price.as_deref(), feed.exponent) else {
            tracing::debug!(
                feed_id = feed.price_feed_id,
                "Lazer feed missing EMA or exponent"
            );
            continue;
        };
        let Ok(mantissa) = ema.parse::<i64>() else {
            tracing::debug!(
                feed_id = feed.price_feed_id,
                "Unparseable Lazer EMA mantissa"
            );
            continue;
        };
        // Confidence is requested, so a feed answering without a usable one
        // is malformed — skipped, not zero-filled (zero would read as
        // maximally confident to any consumer that inspects it).
        let Some(conf) = feed
            .ema_confidence
            .as_ref()
            .and_then(value_as_i64)
            .and_then(|c| u64::try_from(c).ok())
        else {
            tracing::debug!(
                feed_id = feed.price_feed_id,
                "Lazer feed missing or malformed EMA confidence"
            );
            continue;
        };
        // Freshness is per feed: `feedUpdateTimestamp` says when this feed
        // last updated, while the envelope timestamp only says when the
        // response was assembled — a dead feed must not ride a fresh
        // envelope. Absent and malformed are different cases: absent falls
        // back to the envelope at warn level (the property is explicitly
        // requested, so absence means upstream changed shape, and the
        // envelope time is fresh by construction — silently degrading would
        // reopen the dead-feed hole); present-but-unparseable means the
        // feed's own age is unknowable, so it fails closed and is skipped.
        let publish_time_secs = if let MaybeTimestamp::Present(value) = &feed.feed_update_timestamp
        {
            // Present: `null` or unparseable means this feed's own age is
            // unknowable — fail closed and skip rather than letting the
            // fresh-by-construction envelope vouch for it.
            let Some(us) = value.as_ref().and_then(value_as_i64) else {
                tracing::warn!(
                    feed_id = feed.price_feed_id,
                    "Lazer feed carries a null or malformed feedUpdateTimestamp; skipping the feed"
                );
                continue;
            };
            us / 1_000_000
        } else {
            tracing::warn!(
                feed_id = feed.price_feed_id,
                "Lazer feed carries no feedUpdateTimestamp despite it being requested; falling back to the envelope assembly time"
            );
            envelope_secs
        };
        if !crate::oracle::publish_time_is_fresh(publish_time_secs, now_secs, max_age_secs) {
            tracing::debug!(
                feed_id = feed.price_feed_id,
                publish_time = publish_time_secs,
                "Lazer feed is stale or future-dated, skipping"
            );
            continue;
        }
        prices.insert(
            feed.price_feed_id,
            pyth::Price {
                price: near_sdk::json_types::I64(mantissa),
                conf: near_sdk::json_types::U64(conf),
                expo: i32::from(expo),
                publish_time: pyth::PythTimestamp::from_secs(publish_time_secs),
            },
        );
    }
    prices
}

/// Lazer endpoint plus its access token, validated as a pair: construction
/// enforces HTTPS, so no later hop — including a library caller bypassing
/// `Args::build_config` — can pair the bearer token with a cleartext
/// endpoint or transpose the two values.
#[derive(Clone)]
pub struct LazerApiConfig {
    url: Url,
    token: String,
}

impl std::fmt::Debug for LazerApiConfig {
    /// Redacts the access token and renders only the URL's origin — a URL
    /// can carry credentials in its userinfo or query components, and this
    /// impl is what `ServiceConfig`'s `Debug` delegates to.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazerApiConfig")
            .field("url", &self.url.origin().ascii_serialization())
            .field("token", &"<redacted>")
            .finish()
    }
}

impl LazerApiConfig {
    /// # Errors
    ///
    /// Rejects a non-`https` endpoint — the bearer token would travel in
    /// cleartext. The message names the scheme only, never the URL: a URL
    /// can carry credentials in its userinfo component.
    pub fn new(url: Url, token: String) -> Result<Self, String> {
        if url.scheme() != "https" {
            return Err(format!(
                "LAZER_API_URL must be https when LAZER_API_TOKEN is set — the access token would otherwise travel in cleartext (got scheme '{}'; value withheld)",
                url.scheme()
            ));
        }
        Ok(Self { url, token })
    }
}

/// Appends `v1/latest_price` to the configured URL's path — appends, never
/// replaces its last segment, so a gateway path prefix survives. `None` for
/// a cannot-be-a-base URL.
fn latest_price_endpoint(base: &Url) -> Option<Url> {
    let mut url = base.clone();
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(["v1", "latest_price"]);
    Some(url)
}

/// Channels tried in order. `fixed_rate@200ms` first: it is the
/// widely-carried cadence (still 300× inside a 60s market bound), while
/// `real_time` is not carried by every feed — and the live API rejects a
/// whole batch over one feed that lacks the requested channel. `real_time`
/// remains the per-feed retry for any feed that turns out not to carry the
/// fixed rate.
pub(crate) const PRICE_CHANNELS: [&str; 2] = ["fixed_rate@200ms", "real_time"];

/// Why one `latest_price` request priced nothing: the API rejected it (with
/// the status that says how — 400 is a channel the feeds don't carry, 403 an
/// unknown feed id), or the transport failed before any status existed.
#[derive(Debug, Clone, Copy)]
enum FetchFailure {
    Http(reqwest::StatusCode),
    Transport,
}

impl FetchFailure {
    /// True for the two rejections the API issues over individual feeds in
    /// a batch — 400 (a feed lacks the requested channel) and 403 (an
    /// unknown feed id) — where splitting the batch isolates the bad feed.
    /// Everything else (auth, rate limiting, server errors, transport)
    /// would fail identically per feed, so splitting only multiplies dead
    /// requests against a struggling endpoint.
    fn is_feed_level_rejection(self) -> bool {
        match self {
            Self::Http(status) => {
                status == reqwest::StatusCode::BAD_REQUEST
                    || status == reqwest::StatusCode::FORBIDDEN
            }
            Self::Transport => false,
        }
    }
}

impl std::fmt::Display for FetchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(status) => write!(f, "http {status}"),
            Self::Transport => write!(f, "transport error"),
        }
    }
}

/// One `latest_price` request body: the given feeds on the given channel,
/// parsed EMA projection only (no signed payload formats).
fn latest_price_request_body(feed_ids: &[u32], channel: &str) -> near_sdk::serde_json::Value {
    near_sdk::serde_json::json!({
        "priceFeedIds": feed_ids,
        "properties": ["emaPrice", "emaConfidence", "exponent", "feedUpdateTimestamp"],
        "formats": [],
        "parsed": true,
        "channel": channel,
    })
}

/// Client for the Lazer price service. Constructed only when an access token
/// is configured — Lazer has no anonymous tier, so without one the Lazer
/// composition leg reads the on-chain adapter instead.
pub(crate) struct LazerApiClient {
    http: reqwest::Client,
    base_url: Url,
    token: String,
}

impl std::fmt::Debug for LazerApiClient {
    /// Redacts the access token and renders only the URL's origin, same as
    /// [`LazerApiConfig`]'s `Debug` — a URL can carry credentials in its
    /// userinfo or query components.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazerApiClient")
            .field("http", &self.http)
            .field("base_url", &self.base_url.origin().ascii_serialization())
            .field("token", &"<redacted>")
            .finish()
    }
}

impl LazerApiClient {
    /// Builds with a dedicated client that follows no redirects: the request
    /// carries a bearer token, and following a redirect with it would hand
    /// routing decisions to the server (reqwest strips `Authorization` when
    /// host, port, or scheme change, but a same-origin redirect keeps it) —
    /// a redirect response instead surfaces as a non-success status and
    /// prices nothing.
    ///
    /// # Errors
    ///
    /// Fails closed if the no-redirect client cannot be built — falling back
    /// to a redirect-following client would silently void the contract; the
    /// caller disables the API leg instead and the adapter read takes over.
    pub(crate) fn new(config: LazerApiConfig) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            base_url: config.url,
            token: config.token,
        })
    }

    /// Fetches the latest EMA prices for `feed_ids`: one batched request on
    /// the widely-carried channel first; if the API rejects the batch (it
    /// rejects a whole batch over ONE feed lacking the channel — 400 — or
    /// one unknown feed id — 403), each feed retries individually across
    /// [`PRICE_CHANNELS`] so one bad feed cannot zero its siblings. Returns
    /// only the feeds it could price fresh; anything unpriced falls back to
    /// the on-chain adapter read in the caller. No error-body parsing
    /// anywhere — feed-level fault isolation comes from splitting the
    /// batch, not from reading upstream message text.
    pub(crate) async fn get_ema_prices(
        &self,
        feed_ids: &[u32],
        max_age_secs: u32,
    ) -> HashMap<u32, pyth::Price> {
        if feed_ids.is_empty() {
            return HashMap::new();
        }
        let Some(url) = latest_price_endpoint(&self.base_url) else {
            tracing::warn!("Invalid Lazer API URL (cannot be a base)");
            return HashMap::new();
        };

        match self
            .fetch_channel_prices(&url, feed_ids, PRICE_CHANNELS[0], max_age_secs)
            .await
        {
            Ok(prices) => prices,
            // Feed-level rejections split the batch so one bad feed cannot
            // zero its siblings. Anything else — auth, rate limiting, 5xx,
            // transport — would fail identically per feed: it costs exactly
            // one request, warns with the reason, and defers to the adapter
            // reads (splitting here would fire 2×N guaranteed-dead requests
            // at a struggling endpoint, each with a 10s timeout, inside the
            // scan round that /healthz readiness keys on).
            Err(failure) if failure.is_feed_level_rejection() => {
                tracing::debug!(
                    %failure,
                    feeds = feed_ids.len(),
                    "Lazer batch rejected; retrying feeds individually across channels"
                );
                let mut prices = HashMap::new();
                'split: for &feed_id in feed_ids {
                    for channel in PRICE_CHANNELS {
                        match self
                            .fetch_channel_prices(&url, &[feed_id], channel, max_age_secs)
                            .await
                        {
                            Ok(single) if !single.is_empty() => {
                                prices.extend(single);
                                break;
                            }
                            Ok(_) => break, // priced nothing (stale/malformed) — channel was fine
                            // Expected mid-split: this feed lacks this
                            // channel (or is unknown) — try its next channel.
                            Err(failure) if failure.is_feed_level_rejection() => {
                                tracing::debug!(feed_id, channel, %failure, "Lazer feed rejected on channel");
                            }
                            // Terminal mid-split (auth, rate limit, 5xx,
                            // transport): every further request would fail
                            // the same way, each with up to a 10s timeout.
                            // Keep what already priced, abort the rest.
                            Err(failure) => {
                                tracing::warn!(
                                    %failure,
                                    feed_id,
                                    "Lazer split pass hit a terminal failure; keeping recovered prices, deferring the rest to adapter reads"
                                );
                                break 'split;
                            }
                        }
                    }
                }
                if prices.is_empty() {
                    tracing::warn!(
                        %failure,
                        feeds = feed_ids.len(),
                        "Lazer API priced no feeds on any channel; deferring to adapter reads"
                    );
                }
                prices
            }
            Err(failure) => {
                tracing::warn!(
                    %failure,
                    feeds = feed_ids.len(),
                    "Lazer latest_price failed; deferring to adapter reads"
                );
                HashMap::new()
            }
        }
    }

    /// One `latest_price` POST for `feed_ids` on `channel`. `Err` says why
    /// the request priced nothing — an HTTP rejection with its status, or a
    /// transport-level failure with no status to report (no sentinel
    /// numbers).
    async fn fetch_channel_prices(
        &self,
        url: &Url,
        feed_ids: &[u32],
        channel: &str,
        max_age_secs: u32,
    ) -> Result<HashMap<u32, pyth::Price>, FetchFailure> {
        let response = match self
            .http
            .post(url.clone())
            .bearer_auth(&self.token)
            .json(&latest_price_request_body(feed_ids, channel))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                // `reqwest::Error`'s Display embeds the request URL, whose
                // userinfo/query components this module treats as sensitive.
                let error = error.without_url();
                tracing::warn!(%error, "Lazer latest_price request failed");
                return Err(FetchFailure::Transport);
            }
        };
        if !response.status().is_success() {
            // Status only, never the body: an upstream error page can
            // reflect the request — bearer token included — and no
            // truncation redacts that.
            return Err(FetchFailure::Http(response.status()));
        }
        match response.text().await {
            // Clock sampled after the round-trip, so in-flight HTTP latency
            // counts against the update's age — same rule as every other leg.
            Ok(text) => Ok(parse_latest_price_response(
                &text,
                crate::oracle::unix_now_secs(),
                max_age_secs,
            )),
            Err(error) => {
                let error = error.without_url();
                tracing::warn!(%error, "Failed to read Lazer latest_price response");
                Err(FetchFailure::Transport)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000;

    /// The true wire shape: `emaPrice` is serialized through the SDK's
    /// string helper, while `emaConfidence` is a bare `Price(NonZeroI64)` —
    /// a JSON **number**. The parser must accept that asymmetry.
    fn response(timestamp_us: i64) -> String {
        format!(
            r#"{{"parsed":{{"timestampUs":"{timestamp_us}","priceFeeds":[
                {{"priceFeedId":7,"emaPrice":"6435000000000","emaConfidence":1500000000,"exponent":-8}},
                {{"priceFeedId":8,"exponent":-8}}
            ]}}}}"#
        )
    }

    /// A fresh update prices the feeds that carry an EMA mantissa and
    /// exponent; a feed missing either is skipped, not zero-filled.
    #[test]
    fn fresh_update_prices_ema_and_skips_incomplete_feeds() {
        let prices = parse_latest_price_response(&response((NOW - 10) * 1_000_000), NOW, 60);
        assert_eq!(prices.len(), 1);
        let price = &prices[&7];
        assert_eq!(price.price.0, 6_435_000_000_000);
        assert_eq!(price.conf.0, 1_500_000_000);
        assert_eq!(price.expo, -8);
        assert_eq!(price.publish_time.as_secs(), NOW - 10);
    }

    /// Stale and future-dated updates price nothing — the caller falls back
    /// to the on-chain adapter read, which enforces the same bound.
    #[test]
    fn stale_or_future_updates_price_nothing() {
        assert!(
            parse_latest_price_response(&response((NOW - 2000) * 1_000_000), NOW, 60).is_empty()
        );
        assert!(
            parse_latest_price_response(&response((NOW + 300) * 1_000_000), NOW, 60).is_empty()
        );
    }

    /// The envelope timestamp says when the response was assembled; a feed
    /// whose own `feedUpdateTimestamp` is old must not ride a fresh
    /// envelope past the freshness bound. A feed without the per-feed field
    /// falls back to the envelope time.
    #[test]
    fn per_feed_timestamp_overrides_the_envelope() {
        let body = format!(
            r#"{{"parsed":{{"timestampUs":"{env_us}","priceFeeds":[
                {{"priceFeedId":7,"emaPrice":"100","emaConfidence":5,"exponent":-8,"feedUpdateTimestamp":{stale_us}}},
                {{"priceFeedId":9,"emaPrice":"200","emaConfidence":5,"exponent":-8,"feedUpdateTimestamp":{fresh_us}}}
            ]}}}}"#,
            env_us = (NOW - 1) * 1_000_000,
            stale_us = (NOW - 2000) * 1_000_000,
            fresh_us = (NOW - 30) * 1_000_000,
        );
        let prices = parse_latest_price_response(&body, NOW, 60);
        assert!(
            !prices.contains_key(&7),
            "stale per-feed timestamp must not ride a fresh envelope"
        );
        let fresh = &prices[&9];
        assert_eq!(fresh.publish_time.as_secs(), NOW - 30);
    }

    /// A present-but-malformed `feedUpdateTimestamp` is not the same as an
    /// absent one: absent falls back to the envelope (with a warning), but
    /// an unparseable value means the feed's own age is unknowable — fail
    /// closed and skip it rather than letting a fresh envelope vouch for it.
    #[test]
    fn malformed_per_feed_timestamp_skips_the_feed() {
        let body = format!(
            r#"{{"parsed":{{"timestampUs":"{env_us}","priceFeeds":[
                {{"priceFeedId":7,"emaPrice":"100","emaConfidence":5,"exponent":-8,"feedUpdateTimestamp":true}},
                {{"priceFeedId":9,"emaPrice":"200","emaConfidence":5,"exponent":-8}}
            ]}}}}"#,
            env_us = (NOW - 1) * 1_000_000,
        );
        let prices = parse_latest_price_response(&body, NOW, 60);
        assert!(
            !prices.contains_key(&7),
            "malformed per-feed timestamp must fail closed, not fall back to the envelope"
        );
        assert!(
            prices.contains_key(&9),
            "absent field still falls back to the envelope"
        );
    }

    /// An explicit JSON `null` is not the same as an absent field: absent
    /// means upstream dropped the property (envelope fallback, warned), but
    /// `null` is upstream saying "no timestamp for this feed" — its age is
    /// unknowable, so it fails closed like a malformed value.
    #[test]
    fn null_per_feed_timestamp_skips_the_feed() {
        let body = format!(
            r#"{{"parsed":{{"timestampUs":"{env_us}","priceFeeds":[
                {{"priceFeedId":7,"emaPrice":"100","emaConfidence":5,"exponent":-8,"feedUpdateTimestamp":null}}
            ]}}}}"#,
            env_us = (NOW - 1) * 1_000_000,
        );
        let prices = parse_latest_price_response(&body, NOW, 60);
        assert!(
            !prices.contains_key(&7),
            "explicit null timestamp must fail closed, not ride the envelope"
        );
    }

    /// The endpoint must append to the configured URL's path, not replace
    /// its last segment — an operator routing through a gateway prefix
    /// (`https://gateway.example/lazer`) keeps that prefix.
    #[test]
    fn endpoint_preserves_the_configured_path_prefix() {
        let base: Url = "https://gateway.example/lazer".parse().unwrap();
        assert_eq!(
            latest_price_endpoint(&base).unwrap().as_str(),
            "https://gateway.example/lazer/v1/latest_price"
        );
        let rootless: Url = "https://pyth-lazer.dourolabs.app".parse().unwrap();
        assert_eq!(
            latest_price_endpoint(&rootless).unwrap().as_str(),
            "https://pyth-lazer.dourolabs.app/v1/latest_price"
        );
        let slash: Url = "https://gateway.example/lazer/".parse().unwrap();
        assert_eq!(
            latest_price_endpoint(&slash).unwrap().as_str(),
            "https://gateway.example/lazer/v1/latest_price"
        );
    }

    /// The config type carries the HTTPS invariant, so no constructor —
    /// including library callers bypassing `Args::build_config` — can pair
    /// the bearer token with a cleartext endpoint.
    #[test]
    fn lazer_config_refuses_cleartext_transport() {
        assert!(LazerApiConfig::new(
            "http://pyth-lazer.example.com".parse().unwrap(),
            "token".to_string(),
        )
        .is_err());
        assert!(LazerApiConfig::new(
            "https://pyth-lazer.dourolabs.app".parse().unwrap(),
            "token".to_string(),
        )
        .is_ok());
    }

    /// The batch requests `fixed_rate@200ms`, not `real_time`: not every
    /// Lazer feed carries the real-time channel (the live API 400s the whole
    /// batch over one such feed), while 200ms is the widely-carried cadence
    /// and sits far inside any market freshness bound.
    #[test]
    fn request_body_uses_the_widely_carried_channel() {
        let body = latest_price_request_body(&[1, 11], PRICE_CHANNELS[0]);
        assert_eq!(body["channel"], "fixed_rate@200ms");
        assert_eq!(body["priceFeedIds"], near_sdk::serde_json::json!([1, 11]));
        assert_eq!(body["parsed"], true);
        let retry = latest_price_request_body(&[1], PRICE_CHANNELS[1]);
        assert_eq!(retry["channel"], "real_time");
    }

    /// Only the two feed-level rejections split the batch: 400 (a feed
    /// lacks the requested channel) and 403 (an unknown feed id). Auth
    /// failures, rate limits, server errors, and transport failures would
    /// fail identically per feed — splitting would multiply dead requests
    /// against a struggling or unusable endpoint.
    #[test]
    fn only_feed_level_rejections_split_the_batch() {
        assert!(FetchFailure::Http(reqwest::StatusCode::BAD_REQUEST).is_feed_level_rejection());
        assert!(FetchFailure::Http(reqwest::StatusCode::FORBIDDEN).is_feed_level_rejection());
        assert!(!FetchFailure::Http(reqwest::StatusCode::UNAUTHORIZED).is_feed_level_rejection());
        assert!(
            !FetchFailure::Http(reqwest::StatusCode::TOO_MANY_REQUESTS).is_feed_level_rejection()
        );
        assert!(
            !FetchFailure::Http(reqwest::StatusCode::INTERNAL_SERVER_ERROR)
                .is_feed_level_rejection()
        );
        assert!(!FetchFailure::Transport.is_feed_level_rejection());
    }

    use crate::rpc::test_support::scripted_server;

    fn test_client(base_url: Url) -> LazerApiClient {
        LazerApiClient {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            base_url,
            token: "test-token".to_string(),
        }
    }

    /// The symbol map is the Pyth Core → Pyth Pro bridge: only stable feeds
    /// with a `hermes_id` (the Pyth Core price id) enter it; a null id or a
    /// non-stable feed is skipped, a malformed id or an entry that does not
    /// parse never poisons the map, and a `0x` prefix is accepted.
    #[tokio::test]
    async fn symbol_map_keeps_only_stable_feeds_with_a_core_id() {
        let body = r#"[
          {"pyth_lazer_id":1,"symbol":"Crypto.BTC/USD","state":"stable",
           "hermes_id":"e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43"},
          {"pyth_lazer_id":2,"symbol":"Crypto.ETH/USD","state":"stable","hermes_id":null},
          {"pyth_lazer_id":3,"symbol":"Crypto.ZZZ/USD","state":"coming_soon",
           "hermes_id":"ff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace"},
          {"pyth_lazer_id":4,"symbol":"Crypto.BAD/USD","state":"stable","hermes_id":"nothex"},
          {"pyth_lazer_id":"not-a-number","symbol":"Crypto.ODD/USD","state":"stable",
           "hermes_id":"ff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace"},
          {"pyth_lazer_id":5,"symbol":"Crypto.ETH/USD","state":"stable",
           "hermes_id":"0xff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace"}
        ]"#;
        let (url, requests) = scripted_server(vec![(200, body.to_string())]).await;
        let map = fetch_pyth_core_to_pro_map(&url)
            .await
            .expect("symbol map fetches");
        assert_eq!(
            map.len(),
            2,
            "only the stable feeds with a valid core id map"
        );
        let eth = PriceIdentifier(
            hex::decode("ff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace")
                .unwrap()
                .try_into()
                .unwrap(),
        );
        assert_eq!(map.get(&eth), Some(&5), "0x prefix accepted");
        let btc = PriceIdentifier(
            hex::decode("e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43")
                .unwrap()
                .try_into()
                .unwrap(),
        );
        assert_eq!(map.get(&btc), Some(&1));
        assert_eq!(requests.lock().unwrap().len(), 1, "one GET, no retries");
    }

    /// A redirect is a failed fetch, not a followed one: the map decides
    /// what gets priced, and following an `https → http` hop would fetch it
    /// in cleartext despite the startup scheme check.
    #[tokio::test]
    async fn symbol_map_fetch_refuses_redirects() {
        let (url, requests) = scripted_server(vec![(302, String::new()); 2]).await;
        let err = fetch_pyth_core_to_pro_map(&url)
            .await
            .expect_err("a redirect must not be followed");
        assert!(err.contains("302"), "names the status: {err}");
        assert_eq!(requests.lock().unwrap().len(), 1, "no follow-up request");
    }

    /// A successful response with no usable feed is a failed fetch, so the
    /// caller keeps a populated bridge instead of replacing it with nothing.
    #[tokio::test]
    async fn symbol_map_with_no_usable_feed_is_a_failed_fetch() {
        let (url, _requests) = scripted_server(vec![(200, "[]".to_string())]).await;
        let err = fetch_pyth_core_to_pro_map(&url)
            .await
            .expect_err("an empty map must not replace a populated one");
        assert!(err.contains("no usable"), "{err}");
    }

    fn live_feed_body(feed_id: u32) -> String {
        let now_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros();
        format!(
            r#"{{"parsed":{{"timestampUs":"{now_us}","priceFeeds":[
                {{"priceFeedId":{feed_id},"emaPrice":"100","emaConfidence":5,"exponent":-8,"feedUpdateTimestamp":{now_us}}}
            ]}}}}"#
        )
    }

    fn request_channel_and_feeds(raw: &str) -> (String, Vec<u32>) {
        let v: near_sdk::serde_json::Value = near_sdk::serde_json::from_str(raw).unwrap();
        let feeds = v["priceFeedIds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| u32::try_from(f.as_u64().unwrap()).unwrap())
            .collect();
        (v["channel"].as_str().unwrap().to_string(), feeds)
    }

    /// The core routing behavior: a 400 batch splits per feed in channel
    /// order, a sibling that prices is preserved, and a feed rejected on
    /// every channel is simply absent.
    #[tokio::test]
    async fn rejected_batch_splits_per_feed_and_preserves_siblings() {
        let (url, requests) = scripted_server(vec![
            (400, String::new()),     // batch [7, 11] on fixed_rate@200ms
            (200, live_feed_body(7)), // feed 7 on fixed_rate@200ms
            (403, String::new()),     // feed 11 on fixed_rate@200ms
            (403, String::new()),     // feed 11 on real_time
        ])
        .await;
        let prices = test_client(url).get_ema_prices(&[7, 11], 60).await;

        assert!(prices.contains_key(&7), "priced sibling must be preserved");
        assert!(
            !prices.contains_key(&11),
            "rejected-everywhere feed is absent"
        );

        let raw = requests.lock().unwrap().clone();
        let parsed: Vec<_> = raw.iter().map(|r| request_channel_and_feeds(r)).collect();
        assert_eq!(parsed[0], ("fixed_rate@200ms".to_string(), vec![7, 11]));
        assert_eq!(parsed[1], ("fixed_rate@200ms".to_string(), vec![7]));
        assert_eq!(parsed[2], ("fixed_rate@200ms".to_string(), vec![11]));
        assert_eq!(parsed[3], ("real_time".to_string(), vec![11]));
        assert_eq!(parsed.len(), 4);
    }

    /// A terminal failure mid-split aborts the pass: no further requests
    /// are issued for the remaining feeds.
    #[tokio::test]
    async fn terminal_mid_split_failure_aborts_the_pass() {
        let (url, requests) = scripted_server(vec![
            (400, String::new()), // batch [7, 11]
            (429, String::new()), // feed 7 on fixed_rate@200ms — terminal
            // Never served while the abort holds. Without this sentinel the
            // server would stop listening once the script ran out, every
            // overflow request would be connection-refused and unlogged, and
            // the count assertion below could not fail. An unaborted pass
            // consumes it (feed 7 on real_time), moving both assertions.
            (200, live_feed_body(7)),
        ])
        .await;
        let prices = test_client(url).get_ema_prices(&[7, 11], 60).await;

        assert!(prices.is_empty());
        assert_eq!(
            requests.lock().unwrap().len(),
            2,
            "terminal failure must stop the split, not walk remaining feeds/channels"
        );
    }

    /// A terminal batch failure never splits at all — one request total.
    #[tokio::test]
    async fn terminal_batch_failure_never_splits() {
        let (url, requests) = scripted_server(vec![
            (401, String::new()),
            // Sentinel with the same purpose as above: a 401 that wrongly
            // split would consume it and flip both assertions.
            (200, live_feed_body(7)),
        ])
        .await;
        let prices = test_client(url).get_ema_prices(&[7, 11], 60).await;
        assert!(prices.is_empty());
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn garbage_prices_nothing() {
        assert!(parse_latest_price_response("not json", NOW, 60).is_empty());
        assert!(parse_latest_price_response(r#"{"parsed":null}"#, NOW, 60).is_empty());
        assert!(parse_latest_price_response(
            r#"{"parsed":{"timestampUs":"garbage","priceFeeds":[]}}"#,
            NOW,
            60
        )
        .is_empty());
    }
}
