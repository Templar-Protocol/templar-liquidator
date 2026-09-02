//! RedStone price API client (scan-side, off-chain).
//!
//! Fetches USD prices from the RedStone public API
//! (<https://api.redstone.finance>), keyed by asset symbol. Used only to compose
//! proxy-oracle prices for scan-time position evaluation; execution-time
//! pricing still goes through the on-chain oracle push in
//! [`crate::oracle::OracleFetcher::update_onchain_prices`], so nothing here
//! can bypass on-chain freshness or circuit-breaker state where funds move.
//!
//! Only the multi-symbol form (`/prices?symbols=A,B&provider=<service>`) is
//! used: the single-symbol form serves a frozen quote upstream — never use
//! it. `provider` is not optional — see below.
//!
//! The `provider` is the data service, not the vendor: `redstone-primary-prod`
//! is the live one, and it is the same service id the push leg pulls signed
//! packages from. The bare `provider=redstone` this once sent now answers 500
//! for every symbol, and its `&limit=1` variant — the obvious way to make
//! those 500s stop — answers 200 with quotes a month old. Neither the vendor
//! name nor a 200 is evidence of a live feed; the data service is.

use std::collections::HashMap;

use templar_common::oracle::pyth;
use url::Url;

/// Exponent used for synthesized [`pyth::Price`] values: mantissa × 10⁻⁸.
/// Matches Pyth's own convention for USD feeds, so downstream conversions
/// (`create_price_pair`, profitability) treat both sources identically.
pub(crate) const SYNTH_EXPO: i32 = -8;

/// How far ahead of our clock a quote's publish time may sit before it is
/// rejected as implausible rather than trusted as fresh. Staleness checks
/// compare `now - publish_time` against a bound, and a future-dated quote
/// yields a negative age that passes every bound there is.
pub(crate) const MAX_FUTURE_SKEW_MS: i64 = 30_000;

/// The `/prices` URL for `symbols` on `data_service`, query included.
///
/// The service is a parameter rather than a constant because the push leg
/// already takes it from `REDSTONE_DATA_SERVICE_ID`: pinning scan pricing to
/// the default here would let one knob move the push leg while scan stayed
/// behind, which is the drift the two are meant not to have.
///
/// Pure so the request this module actually sends can be asserted without a
/// network round trip — a wrong service is invisible at the type level and,
/// as the 500s showed, invisible at runtime too until every RedStone-priced
/// market quietly stops being scanned.
pub(crate) fn prices_url(base_url: &Url, symbols: &[String], data_service: &str) -> String {
    format!(
        "{}/prices?symbols={}&provider={data_service}",
        base_url.as_str().trim_end_matches('/'),
        symbols.join(",")
    )
}

/// One symbol's entry in the RedStone `/prices` multi-symbol response.
#[derive(serde::Deserialize)]
struct RedStonePoint {
    value: near_sdk::serde_json::Value,
    /// Epoch milliseconds.
    #[serde(default)]
    timestamp: i64,
}

/// Converts a RedStone USD value into a synthesized [`pyth::Price`] at
/// [`SYNTH_EXPO`], carrying the API's publish time. Returns `None` for a
/// non-positive, non-numeric, or mantissa-overflowing value — absent beats a
/// wrong number.
///
/// The f64 hop bounds precision at ~1e-15 relative, orders of magnitude below
/// any bps-level decision this feeds; on-chain amounts never pass through
/// here.
pub(crate) fn to_pyth_price(
    value: &near_sdk::serde_json::Value,
    timestamp_ms: i64,
) -> Option<pyth::Price> {
    // 2^53: the largest f64 integer that is still exactly representable (the
    // first unrepresentable integer is 2^53 + 1), so any mantissa up to and
    // including it survives the round-trip losslessly — the bound below is
    // inclusive. At expo -8 that admits unit prices up to ~$90M.
    const MAX_MANTISSA: f64 = 9_007_199_254_740_992.0;

    let usd = match value {
        near_sdk::serde_json::Value::Number(n) => n.as_f64()?,
        near_sdk::serde_json::Value::String(s) => s.parse::<f64>().ok()?,
        _ => return None,
    };
    if !usd.is_finite() || usd <= 0.0 {
        return None;
    }
    let scaled = (usd * 10f64.powi(-SYNTH_EXPO)).round();
    if !(1.0..=MAX_MANTISSA).contains(&scaled) {
        // Rounded to zero (dust below 10^SYNTH_EXPO) or too large to carry
        // exactly — absent beats a wrong number.
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    let mantissa = scaled as i64;
    Some(pyth::Price {
        price: near_sdk::json_types::I64(mantissa),
        conf: near_sdk::json_types::U64(0),
        expo: SYNTH_EXPO,
        publish_time: pyth::PythTimestamp::from_secs(timestamp_ms / 1000),
    })
}

/// Parses the multi-symbol `/prices` response body into synthesized prices,
/// applying the freshness guards every entry must pass:
///
/// - `timestamp <= 0` (absent field or a units mismatch that would read as
///   1970) — rejected;
/// - older than `max_age_ms` — rejected as stale;
/// - more than [`MAX_FUTURE_SKEW_MS`] ahead of `now_ms` — rejected as
///   implausible;
/// - a value that does not convert — rejected.
///
/// A symbol RedStone does not track is absent from the response and therefore
/// absent from the result — callers treat a missing key as "unpriced", never
/// as zero.
pub(crate) fn parse_prices_response(
    body: &str,
    now_ms: i64,
    max_age_ms: i64,
) -> HashMap<String, pyth::Price> {
    let raw: HashMap<String, RedStonePoint> = match near_sdk::serde_json::from_str(body) {
        Ok(raw) => raw,
        Err(error) => {
            tracing::warn!(%error, "Failed to decode RedStone /prices response");
            return HashMap::new();
        }
    };

    let mut prices = HashMap::with_capacity(raw.len());
    for (symbol, point) in raw {
        if point.timestamp <= 0 {
            tracing::warn!(%symbol, "RedStone entry has missing/invalid timestamp, skipping");
            continue;
        }
        let age_ms = now_ms - point.timestamp;
        if age_ms > max_age_ms {
            tracing::debug!(%symbol, age_ms, max_age_ms, "RedStone quote is stale, skipping");
            continue;
        }
        if age_ms < -MAX_FUTURE_SKEW_MS {
            tracing::warn!(%symbol, lead_ms = -age_ms, "RedStone quote is future-dated, skipping");
            continue;
        }
        let Some(price) = to_pyth_price(&point.value, point.timestamp) else {
            tracing::warn!(%symbol, "RedStone entry has unusable value, skipping");
            continue;
        };
        prices.insert(symbol, price);
    }
    prices
}

/// HTTP client for the RedStone price API.
#[derive(Clone)]
pub struct RedStoneApiClient {
    http_client: reqwest::Client,
    base_url: Url,
    /// The data service every request names, from `REDSTONE_DATA_SERVICE_ID`.
    data_service: String,
}

impl std::fmt::Debug for RedStoneApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedStoneApiClient")
            .field("base_url", &self.base_url.as_str())
            .field("data_service", &self.data_service)
            .finish_non_exhaustive()
    }
}

impl RedStoneApiClient {
    pub fn new(base_url: Url, data_service: String) -> Self {
        Self {
            http_client: reqwest::Client::new(),
            base_url,
            data_service,
        }
    }

    /// Fetches USD prices for `symbols` in one request, returning only the
    /// symbols that passed the parse/freshness guards. Transport failures log
    /// and return an empty map — the caller's fallback (the on-chain proxy
    /// cache read) decides what an unpriced feed means.
    pub async fn get_prices(
        &self,
        symbols: &[String],
        max_age_secs: u32,
    ) -> HashMap<String, pyth::Price> {
        if symbols.is_empty() {
            return HashMap::new();
        }
        let url = prices_url(&self.base_url, symbols, &self.data_service);
        let response = self
            .http_client
            .get(&url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;

        let response = match response {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                tracing::warn!(status = %response.status(), "RedStone API returned error status");
                return HashMap::new();
            }
            Err(error) => {
                tracing::warn!(%error, "RedStone API request failed");
                return HashMap::new();
            }
        };

        let body = match response.text().await {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(%error, "Failed to read RedStone API response");
                return HashMap::new();
            }
        };

        #[allow(clippy::cast_possible_truncation)]
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        parse_prices_response(&body, now_ms, i64::from(max_age_secs) * 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use near_sdk::serde_json::json;

    const NOW_MS: i64 = 1_755_600_000_000;
    const MAX_AGE_MS: i64 = 3_600_000;

    /// The provider is the whole request: `redstone` answers 500 for every
    /// symbol and `redstone&limit=1` answers 200 with month-old quotes, so a
    /// wrong value here is either total silence or, worse, confident staleness.
    #[test]
    fn prices_url_names_the_default_data_service() {
        let base: Url = "https://api.redstone.finance".parse().unwrap();
        let url = prices_url(
            &base,
            &["XLM".to_string(), "LTC".to_string()],
            crate::config::DEFAULT_REDSTONE_DATA_SERVICE_ID,
        );

        assert_eq!(
            url,
            "https://api.redstone.finance/prices?symbols=XLM,LTC&provider=redstone-primary-prod"
        );
        assert!(
            !url.contains("limit="),
            "limit= turns the 500 into a 200 serving quotes a month old: {url}"
        );
    }

    /// `REDSTONE_DATA_SERVICE_ID` reaches scan pricing, not just the push leg.
    /// Pinned because the two drifting is invisible: the push would follow the
    /// override while scan silently stayed on the default, and both legs would
    /// still look healthy while describing different feed sets.
    #[test]
    fn prices_url_follows_a_configured_data_service() {
        let base: Url = "https://api.redstone.finance".parse().unwrap();
        assert_eq!(
            prices_url(&base, &["XLM".to_string()], "redstone-avalanche-prod"),
            "https://api.redstone.finance/prices?symbols=XLM&provider=redstone-avalanche-prod"
        );
    }

    /// A configured base URL may carry a trailing slash or a path prefix;
    /// neither may double a separator or swallow the prefix.
    #[test]
    fn prices_url_tolerates_base_url_shapes() {
        let ds = crate::config::DEFAULT_REDSTONE_DATA_SERVICE_ID;
        let slash: Url = "https://api.redstone.finance/".parse().unwrap();
        assert_eq!(
            prices_url(&slash, &["XLM".to_string()], ds),
            "https://api.redstone.finance/prices?symbols=XLM&provider=redstone-primary-prod"
        );

        let prefixed: Url = "https://proxy.example/redstone/".parse().unwrap();
        assert_eq!(
            prices_url(&prefixed, &["XLM".to_string()], ds),
            "https://proxy.example/redstone/prices?symbols=XLM&provider=redstone-primary-prod"
        );
    }

    #[test]
    fn to_pyth_price_scales_to_expo_minus_8() {
        let price = to_pyth_price(&json!(0.5123), NOW_MS).unwrap();
        assert_eq!(price.price.0, 51_230_000);
        assert_eq!(price.expo, SYNTH_EXPO);
        assert_eq!(price.conf.0, 0);

        let price = to_pyth_price(&json!(64_350.12), NOW_MS).unwrap();
        assert_eq!(price.price.0, 6_435_012_000_000);
    }

    #[test]
    fn to_pyth_price_carries_publish_time_in_seconds() {
        let price = to_pyth_price(&json!(1.0), NOW_MS).unwrap();
        assert_eq!(
            price.publish_time,
            pyth::PythTimestamp::from_secs(NOW_MS / 1000)
        );
    }

    #[test]
    fn to_pyth_price_rejects_unusable_values() {
        assert!(to_pyth_price(&json!(0), NOW_MS).is_none());
        assert!(to_pyth_price(&json!(-1.5), NOW_MS).is_none());
        assert!(to_pyth_price(&json!("not a number"), NOW_MS).is_none());
        assert!(to_pyth_price(&json!(null), NOW_MS).is_none());
        // Below 10^SYNTH_EXPO the mantissa rounds to zero — unusable.
        assert!(to_pyth_price(&json!(1e-12), NOW_MS).is_none());
        // Too large to fit an i64 mantissa at expo -8.
        assert!(to_pyth_price(&json!(1e15), NOW_MS).is_none());
    }

    #[test]
    fn parse_prices_response_keys_by_symbol() {
        let body = format!(
            r#"{{"LTC":{{"value":112.34,"timestamp":{ts}}},"XRP":{{"value":"0.5123","timestamp":{ts}}}}}"#,
            ts = NOW_MS - 40_000
        );
        let prices = parse_prices_response(&body, NOW_MS, MAX_AGE_MS);
        assert_eq!(prices.len(), 2);
        assert_eq!(prices["LTC"].price.0, 11_234_000_000);
        assert_eq!(prices["XRP"].price.0, 51_230_000);
    }

    #[test]
    fn parse_prices_response_rejects_bad_entries_individually() {
        let body = format!(
            r#"{{
                "GOOD": {{"value": 2.0, "timestamp": {fresh}}},
                "NO_TS": {{"value": 2.0, "timestamp": 0}},
                "STALE": {{"value": 2.0, "timestamp": {stale}}},
                "FUTURE": {{"value": 2.0, "timestamp": {future}}},
                "BAD_VALUE": {{"value": "junk", "timestamp": {fresh}}}
            }}"#,
            fresh = NOW_MS - 1000,
            stale = NOW_MS - MAX_AGE_MS - 1000,
            future = NOW_MS + MAX_FUTURE_SKEW_MS + 1000,
        );
        let prices = parse_prices_response(&body, NOW_MS, MAX_AGE_MS);
        assert_eq!(prices.len(), 1, "only the good entry survives: {prices:?}");
        assert!(prices.contains_key("GOOD"));
    }

    #[test]
    fn parse_prices_response_tolerates_garbage_bodies() {
        assert!(parse_prices_response("not json", NOW_MS, MAX_AGE_MS).is_empty());
        assert!(parse_prices_response("[]", NOW_MS, MAX_AGE_MS).is_empty());
    }
}
