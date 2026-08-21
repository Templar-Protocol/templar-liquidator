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
}

/// The `parsed` object of a `latest_price` response: one update timestamp
/// (microseconds since the epoch, as a decimal string) covering every feed.
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

/// Parses a `latest_price` response into per-feed EMA prices, enforcing the
/// market's freshness bound: an update older than `max_age_secs`, or further
/// ahead of `now_secs` than clock skew explains
/// ([`crate::redstone::MAX_FUTURE_SKEW_MS`]), prices nothing — absent beats
/// a wrong number. Feeds missing their EMA mantissa or exponent are skipped
/// individually.
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
    let Some(publish_time_secs) = parsed
        .timestamp_us
        .parse::<i64>()
        .ok()
        .map(|us| us / 1_000_000)
    else {
        tracing::warn!(timestamp = %parsed.timestamp_us, "Unparseable Lazer update timestamp");
        return prices;
    };
    let Some(age_secs) = now_secs.checked_sub(publish_time_secs) else {
        return prices;
    };
    if age_secs > i64::from(max_age_secs)
        || age_secs < -(crate::redstone::MAX_FUTURE_SKEW_MS / 1000)
    {
        tracing::debug!(
            publish_time = publish_time_secs,
            age_secs,
            "Lazer update is stale or future-dated, pricing nothing"
        );
        return prices;
    }

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
        let conf = feed
            .ema_confidence
            .as_ref()
            .and_then(|c| match c {
                near_sdk::serde_json::Value::Number(n) => n.as_u64(),
                near_sdk::serde_json::Value::String(s) => s.parse::<u64>().ok(),
                _ => None,
            })
            .unwrap_or(0);
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
    pub(crate) fn new(http: reqwest::Client, base_url: Url, token: String) -> Self {
        Self {
            http,
            base_url,
            token,
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
        let url = match self.base_url.join("v1/latest_price") {
            Ok(url) => url,
            Err(error) => {
                tracing::warn!(%error, "Invalid Lazer API URL");
                return HashMap::new();
            }
        };
        let body = near_sdk::serde_json::json!({
            "priceFeedIds": feed_ids,
            "properties": ["emaPrice", "emaConfidence", "exponent"],
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
            let text = response.text().await.unwrap_or_default();
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
