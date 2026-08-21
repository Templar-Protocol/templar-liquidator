//! Pyth Lazer (Pyth Pro) price API client (scan-side, off-chain).
//!
//! Fetches the latest EMA prices for Lazer feed ids from the Lazer price
//! service (`POST /v1/latest_price`, Bearer-token authenticated — unlike
//! Hermes and the RedStone public API, Lazer is a subscription service).
//! Used only to compose proxy-oracle prices for scan-time position
//! evaluation; execution-time pricing still goes through the on-chain oracle
//! push in [`crate::oracle::OracleFetcher::update_onchain_prices`], so
//! nothing here can bypass on-chain freshness or circuit-breaker state where
//! funds move.
//!
//! EMA (not spot) is requested deliberately: it is the projection the
//! on-chain proxy's `Lazer` source consumes, and what the Hermes leg feeds —
//! every scan-side pricing path speaks the same value.

use std::collections::HashMap;

use templar_common::oracle::pyth;
use url::Url;

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

/// Char-boundary-safe cap on upstream text bound for the logs — an error
/// page must not inflate every scan's log output or panic mid-truncation.
fn truncate_for_log(text: String) -> String {
    if text.chars().count() <= 512 {
        text
    } else {
        text.chars().take(512).collect()
    }
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
    /// Redacts the access token; the URL alone identifies the deployment.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazerApiClient")
            .field("http", &self.http)
            .field("base_url", &self.base_url.as_str())
            .field("token", &"<redacted>")
            .finish()
    }
}

impl LazerApiClient {
    /// Builds with a dedicated client that follows no redirects: the request
    /// carries a bearer token, and an https→http redirect (even same-host)
    /// would downgrade it to cleartext, bypassing the HTTPS invariant
    /// [`LazerApiConfig::new`] enforces on the configured URL. A redirect
    /// response therefore surfaces as a non-success status and prices
    /// nothing. Falls back to the shared client only if the builder fails
    /// (it has no failing configuration here, but `unwrap` is denied).
    pub(crate) fn new(http: reqwest::Client, config: LazerApiConfig) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or(http);
        Self {
            http,
            base_url: config.url,
            token: config.token,
        }
    }

    /// Fetches the latest EMA prices for `feed_ids` in one request. Returns
    /// only the feeds it could price fresh; any error prices nothing (the
    /// caller falls back to the on-chain adapter read).
    pub(crate) async fn get_ema_prices(
        &self,
        feed_ids: &[u32],
        now_secs: i64,
        max_age_secs: u32,
    ) -> HashMap<u32, pyth::Price> {
        if feed_ids.is_empty() {
            return HashMap::new();
        }
        let Some(url) = latest_price_endpoint(&self.base_url) else {
            tracing::warn!("Invalid Lazer API URL (cannot be a base)");
            return HashMap::new();
        };
        let body = near_sdk::serde_json::json!({
            "priceFeedIds": feed_ids,
            "properties": ["emaPrice", "emaConfidence", "exponent", "feedUpdateTimestamp"],
            "formats": [],
            "parsed": true,
            "channel": "real_time",
        });
        let response = match self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .json(&body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(%error, "Lazer latest_price request failed");
                return HashMap::new();
            }
        };
        if !response.status().is_success() {
            let status = response.status();
            let text = truncate_for_log(response.text().await.unwrap_or_default());
            tracing::warn!(%status, response = %text, "Lazer latest_price returned an error");
            return HashMap::new();
        }
        match response.text().await {
            Ok(text) => parse_latest_price_response(&text, now_secs, max_age_secs),
            Err(error) => {
                tracing::warn!(%error, "Failed to read Lazer latest_price response");
                HashMap::new()
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

    /// Log truncation must be char-boundary-safe: a multi-byte character
    /// straddling the limit must not panic.
    #[test]
    fn log_truncation_is_char_safe() {
        let s = "é".repeat(400);
        let t = truncate_for_log(s);
        assert!(t.chars().count() <= 512);
        let short = truncate_for_log("ok".to_string());
        assert_eq!(short, "ok");
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
