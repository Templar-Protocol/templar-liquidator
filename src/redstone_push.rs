//! RedStone push leg: refreshes RedStone-sourced proxy feeds on-chain before
//! a liquidation, for the feeds the Pyth Pro push cannot reach.
//!
//! Templar's RedStone adapter accepts `write_prices` from anyone, provided the
//! payload carries enough signatures from the adapter's configured signer
//! set (an untrusted writer additionally observes a per-feed minimum
//! interval, enforced inside the contract). Nothing else keeps those
//! adapters fresh, so without this leg a RedStone-only feed is never
//! priceable on-chain. The bot fetches signed packages from RedStone's
//! public gateway, serializes them in the RedStone protocol layout, recovers
//! every signature locally against the adapter's own signer set and
//! timestamp window (both read from the contract), submits only a payload
//! the contract will accept, and skips feeds it pushed within the adapter's
//! minimum interval — gas is not spent discovering a rejection the bot
//! could foresee.
//!
//! This module holds the pure parts (parsing, serialization, signer
//! recovery, payload assembly) and the gateway client; target resolution
//! and the on-chain write live in [`crate::oracle`].

use std::collections::{BTreeSet, HashMap};

use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use near_sdk::base64::prelude::*;
use sha3::{Digest, Keccak256};
use url::Url;

/// Where signed packages come from: the gateway, and the data-service id
/// whose signer set the adapters are configured with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedStonePushConfig {
    pub gateway_url: Url,
    pub data_service_id: String,
}

/// One signed single-feed package as the gateway serves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedPackage {
    pub feed_id: String,
    /// The value as decimal text: a JSON string verbatim, or a JSON number
    /// as `serde_json` re-prints it (shortest round-trip form, exact for
    /// RedStone's 8-decimal values). Scaled at serialization with integer
    /// arithmetic, so no float rounding can drift a byte from what the node
    /// signed.
    pub value_text: String,
    pub timestamp_ms: u64,
    pub signature: [u8; 65],
    pub claimed_signer: [u8; 20],
}

/// What the adapter contract enforces, read from its `get_config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterRules {
    pub signer_threshold: usize,
    pub signers: Vec<[u8; 20]>,
    pub max_delay_ms: u64,
    pub max_ahead_ms: u64,
    /// An untrusted writer's minimum spacing between writes of one feed.
    pub min_interval_ms: u64,
}

impl From<&templar_common::oracle::redstone::Config> for AdapterRules {
    fn from(config: &templar_common::oracle::redstone::Config) -> Self {
        Self {
            signer_threshold: usize::from(config.signer_count_threshold),
            signers: config.signers.clone(),
            max_delay_ms: config.max_timestamp_delay_ms,
            max_ahead_ms: config.max_timestamp_ahead_ms,
            min_interval_ms: config.min_interval_between_updates_ms,
        }
    }
}

/// RedStone protocol constants: values are 32-byte big-endian integers at
/// 8 decimals; a payload ends with the unsigned-metadata size and the marker.
const VALUE_BYTES: usize = 32;
const DECIMALS: u32 = 8;
const REDSTONE_MARKER: [u8; 9] = [0x00, 0x00, 0x02, 0xed, 0x57, 0x01, 0x1e, 0x00, 0x00];

/// Parses a `0x`-prefixed (or bare) 20-byte hex address.
pub(crate) fn parse_address(text: &str) -> Option<[u8; 20]> {
    let bytes = hex::decode(text.trim().strip_prefix("0x").unwrap_or(text.trim())).ok()?;
    bytes.try_into().ok()
}

/// Scales a non-negative decimal text (plain or exponent form) by 10^8,
/// rounding half up — exact integer arithmetic, never a float.
pub(crate) fn scale_value(text: &str) -> Option<u128> {
    let text = text.trim();
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, exponent.parse::<i32>().ok()?),
        None => (text, 0),
    };
    let (int_part, frac_part) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return None;
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let digits_text = format!("{int_part}{frac_part}");
    let digits_text = digits_text.trim_start_matches('0');
    let digits: u128 = if digits_text.is_empty() {
        0
    } else {
        digits_text.parse().ok()?
    };
    let shift = i64::from(DECIMALS) + i64::from(exponent) - i64::try_from(frac_part.len()).ok()?;
    if shift >= 0 {
        digits.checked_mul(10u128.checked_pow(u32::try_from(shift).ok()?)?)
    } else {
        let divisor = 10u128.checked_pow(u32::try_from(-shift).ok()?)?;
        Some(digits.checked_add(divisor / 2)? / divisor)
    }
}

/// Parses the gateway's `data-packages/latest/{service}` body: a map from
/// feed id to its signed packages. Tolerant per package — one entry that
/// does not parse is skipped, not the whole response — and keyed only by
/// single-feed packages whose data point names the feed they are filed
/// under.
pub(crate) fn parse_gateway_response(
    body: &str,
) -> Result<HashMap<String, Vec<SignedPackage>>, String> {
    let root: near_sdk::serde_json::Value = near_sdk::serde_json::from_str(body)
        .map_err(|error| format!("gateway body did not parse: {error}"))?;
    let map = root
        .as_object()
        .ok_or_else(|| "gateway body is not an object".to_string())?;
    let mut out = HashMap::new();
    for (feed, entries) in map {
        let Some(list) = entries.as_array() else {
            continue;
        };
        let parsed: Vec<SignedPackage> = list
            .iter()
            .filter_map(|entry| parse_package(feed, entry))
            .collect();
        if parsed.len() < list.len() {
            tracing::debug!(
                feed,
                skipped = list.len() - parsed.len(),
                "Skipped gateway packages that did not parse"
            );
        }
        if !parsed.is_empty() {
            out.insert(feed.clone(), parsed);
        }
    }
    Ok(out)
}

fn parse_package(feed: &str, entry: &near_sdk::serde_json::Value) -> Option<SignedPackage> {
    let points = entry.get("dataPoints")?.as_array()?;
    let [point] = points.as_slice() else {
        return None;
    };
    if point.get("dataFeedId")?.as_str()? != feed {
        return None;
    }
    let value_text = match point.get("value")? {
        near_sdk::serde_json::Value::Number(number) => number.to_string(),
        near_sdk::serde_json::Value::String(text) => text.clone(),
        _ => return None,
    };
    let timestamp_ms = entry.get("timestampMilliseconds")?.as_u64()?;
    let signature: [u8; 65] = BASE64_STANDARD
        .decode(entry.get("signature")?.as_str()?)
        .ok()?
        .try_into()
        .ok()?;
    let claimed_signer = parse_address(entry.get("signerAddress")?.as_str()?)?;
    Some(SignedPackage {
        feed_id: feed.to_string(),
        value_text,
        timestamp_ms,
        signature,
        claimed_signer,
    })
}

/// The signed bytes of one package: feed id (32 B, zero-padded), value
/// (32 B), timestamp (6 B, ms), value size (4 B), data-point count (3 B).
pub(crate) fn serialize_package(package: &SignedPackage) -> Option<Vec<u8>> {
    let id = package.feed_id.as_bytes();
    if id.len() > 32 {
        return None;
    }
    let mut out = Vec::with_capacity(32 + VALUE_BYTES + 6 + 4 + 3);
    out.extend_from_slice(id);
    out.resize(32, 0);
    let value = scale_value(&package.value_text)?;
    let mut value_bytes = [0u8; VALUE_BYTES];
    value_bytes[VALUE_BYTES - 16..].copy_from_slice(&value.to_be_bytes());
    out.extend_from_slice(&value_bytes);
    out.extend_from_slice(&package.timestamp_ms.to_be_bytes()[2..]);
    out.extend_from_slice(&u32::try_from(VALUE_BYTES).ok()?.to_be_bytes());
    out.extend_from_slice(&[0, 0, 1]);
    Some(out)
}

/// Recovers the signer of a package from its signature over the keccak256
/// of the signed bytes (no EIP-191 prefix — RedStone signs the raw digest).
pub(crate) fn recover_signer(package: &SignedPackage) -> Option<[u8; 20]> {
    let body = serialize_package(package)?;
    let digest = Keccak256::digest(&body);
    let signature = Signature::from_slice(&package.signature[..64]).ok()?;
    let v = package.signature[64];
    let recovery = RecoveryId::from_byte(v.checked_sub(27).unwrap_or(v))?;
    let key = VerifyingKey::recover_from_prehash(&digest, &signature, recovery).ok()?;
    let point = key.to_encoded_point(false);
    let hash = Keccak256::digest(&point.as_bytes()[1..]);
    let mut address = [0u8; 20];
    address.copy_from_slice(&hash[12..]);
    Some(address)
}

/// Assembles the payload for `feed_ids`, in that order, from packages that
/// pass the adapter's rules: within its timestamp window, signature
/// recovering to the claimed signer, that signer in the adapter's set, one
/// package per signer — and exactly the threshold's worth of them, since
/// every extra package is signature-verification work and bytes on-chain
/// for nothing. A feed with fewer usable signers than the threshold fails
/// the whole build, naming the feed and the shortfall.
pub(crate) fn build_payload(
    packages: &HashMap<String, Vec<SignedPackage>>,
    feed_ids: &[String],
    rules: &AdapterRules,
    now_ms: u64,
) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut count: u16 = 0;
    for feed in feed_ids {
        let candidates = packages
            .get(feed)
            .ok_or_else(|| format!("{feed}: no packages from the gateway"))?;
        let mut seen = BTreeSet::new();
        let mut accepted = Vec::new();
        for package in candidates {
            let fresh = package.timestamp_ms.saturating_add(rules.max_delay_ms) >= now_ms
                && package.timestamp_ms <= now_ms.saturating_add(rules.max_ahead_ms);
            if !fresh {
                continue;
            }
            let Some(signer) = recover_signer(package) else {
                continue;
            };
            if signer != package.claimed_signer
                || !rules.signers.contains(&signer)
                || !seen.insert(signer)
            {
                continue;
            }
            accepted.push(package);
            if accepted.len() >= rules.signer_threshold {
                break;
            }
        }
        if accepted.len() < rules.signer_threshold {
            return Err(format!(
                "{feed}: {} usable signer(s), adapter threshold {}",
                accepted.len(),
                rules.signer_threshold
            ));
        }
        for package in accepted {
            out.extend(
                serialize_package(package)
                    .ok_or_else(|| format!("{feed}: package could not be serialized"))?,
            );
            out.extend_from_slice(&package.signature);
            count = count
                .checked_add(1)
                .ok_or_else(|| "too many packages for one payload".to_string())?;
        }
    }
    out.extend_from_slice(&count.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&REDSTONE_MARKER);
    Ok(out)
}

/// The feeds of one adapter that are due for a write: those with no
/// recorded push, or one older than the adapter's minimum interval. The
/// contract rejects a too-soon write per feed and the transaction still
/// costs gas, so the bot keeps its own memo instead of finding out on-chain.
pub(crate) fn due_feeds<F: Clone + Eq + std::hash::Hash>(
    last_push: &HashMap<(near_sdk::AccountId, F), std::time::Instant>,
    adapter: &near_sdk::AccountId,
    feeds: &[F],
    min_interval: std::time::Duration,
    now: std::time::Instant,
) -> Vec<F> {
    feeds
        .iter()
        .filter(|feed| {
            last_push
                .get(&(adapter.clone(), (*feed).clone()))
                .is_none_or(|last| now.saturating_duration_since(*last) >= min_interval)
        })
        .cloned()
        .collect()
}

/// The data-packages URL for a gateway config. Built by concatenation, not
/// `Url::join`, so a gateway configured with a path prefix keeps it.
pub(crate) fn packages_url(config: &RedStonePushConfig) -> Result<Url, String> {
    format!(
        "{}/data-packages/latest/{}",
        config.gateway_url.as_str().trim_end_matches('/'),
        config.data_service_id
    )
    .parse()
    .map_err(|error| format!("bad RedStone gateway URL: {error}"))
}

/// Fetches signed packages from the RedStone gateway. Follows no redirects:
/// the packages are signed, but the URL decides whose view of the feed set
/// the bot submits, so a redirect is a failed fetch rather than a followed
/// one.
pub struct RedStonePushClient {
    http: reqwest::Client,
    config: RedStonePushConfig,
}

impl std::fmt::Debug for RedStonePushClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedStonePushClient")
            .field(
                "gateway",
                &self.config.gateway_url.origin().ascii_serialization(),
            )
            .field("data_service_id", &self.config.data_service_id)
            .finish_non_exhaustive()
    }
}

impl RedStonePushClient {
    /// # Errors
    ///
    /// The no-redirect client failing to build — the caller disables the
    /// leg rather than fall back to a redirect-following client.
    pub fn new(config: RedStonePushConfig) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|error| {
                format!(
                    "RedStone gateway client could not be built: {}",
                    error.without_url()
                )
            })?;
        Ok(Self { http, config })
    }

    #[must_use]
    pub fn config(&self) -> &RedStonePushConfig {
        &self.config
    }

    /// One GET of the whole data-service dump (the gateway ignores feed
    /// filters), parsed per package.
    ///
    /// # Errors
    ///
    /// Transport failure, a non-success status (a redirect included), or a
    /// body that is not the expected object.
    pub async fn fetch_packages(&self) -> Result<HashMap<String, Vec<SignedPackage>>, String> {
        let url = packages_url(&self.config)?;
        let response =
            self.http.get(url).send().await.map_err(|error| {
                format!("RedStone gateway request failed: {}", error.without_url())
            })?;
        if !response.status().is_success() {
            return Err(format!("RedStone gateway returned {}", response.status()));
        }
        let body = response.text().await.map_err(|error| {
            format!(
                "RedStone gateway body could not be read: {}",
                error.without_url()
            )
        })?;
        parse_gateway_response(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three live packages per feed captured from RedStone's gateway
    /// (`redstone-primary-prod`, 2026-08-26), signatures included. Any drift
    /// in the serialization fails signer recovery below.
    const FIXTURE: &str = r#"{"USDC":[{"timestampMilliseconds":1787785650000,"signature":"ABcQP5mJtTnMD4jL/7eiW1ARh+Z4Gptnc/nBoCZ9uUwACiEWnSwCmfcR3aKvHXvwRei5enwN/TTAyLQEBG8XxBw=","dataPoints":[{"dataFeedId":"USDC","value":0.99994}],"signerAddress":"0x9c5AE89C4Af6aA32cE58588DBaF90d18a855B6de","dataServiceId":"redstone-primary-prod","dataPackageId":"USDC"},{"timestampMilliseconds":1787785650000,"signature":"/Hp8oxkfepb0EvgLNkOel18NXg3O2c3xGoZOicwoYow7PHLZ03UGMyRy39xKXWPqre18xWXTX56y33iyTG76Vxw=","dataPoints":[{"dataFeedId":"USDC","value":0.999915}],"signerAddress":"0xDD682daEC5A90dD295d14DA4b0bec9281017b5bE","dataServiceId":"redstone-primary-prod","dataPackageId":"USDC"},{"timestampMilliseconds":1787785650000,"signature":"mQ42qftcdRKraXg41DnY2LgQAQ6hG/QZqpwvWqVC6A4Vf5YHLI7iIj/2AXShlq6iHvI8QZ8/zluBlEZPTnnSUBw=","dataPoints":[{"dataFeedId":"USDC","value":0.99994}],"signerAddress":"0xdEB22f54738d54976C4c0fe5ce6d408E40d88499","dataServiceId":"redstone-primary-prod","dataPackageId":"USDC"}],"CETES":[{"timestampMilliseconds":1787785650000,"signature":"1NQturtmLidJ0ICFtXxDd+VOYb0rZVf8aA6IofVU41E7SCJ+o/g+Lq8jiXMDupGR7EuauIdNpwBFXEq6l/T0chw=","dataPoints":[{"dataFeedId":"CETES","value":0.069524}],"signerAddress":"0xdEB22f54738d54976C4c0fe5ce6d408E40d88499","dataServiceId":"redstone-primary-prod","dataPackageId":"CETES"},{"timestampMilliseconds":1787785650000,"signature":"V0cdw0OoDsj8Nk39x8m9iRQN0g2su5JhYe4a7W7dJJVdqWFsToIMtAbyXAnNKt8rMgI/p4cIe3upn5Ke9WTs3Rs=","dataPoints":[{"dataFeedId":"CETES","value":0.069524}],"signerAddress":"0x51Ce04Be4b3E32572C4Ec9135221d0691Ba7d202","dataServiceId":"redstone-primary-prod","dataPackageId":"CETES"},{"timestampMilliseconds":1787785650000,"signature":"ERWoIG0jq1ApSN9cmIXOGemdLUo+V6rqT1EMz0zYs+tXyrU1MCiEYuCrhRC+5577mfw+7CFeg2jGJB8sq3W1cxw=","dataPoints":[{"dataFeedId":"CETES","value":0.069524}],"signerAddress":"0xDD682daEC5A90dD295d14DA4b0bec9281017b5bE","dataServiceId":"redstone-primary-prod","dataPackageId":"CETES"}],"BTC":[{"timestampMilliseconds":1787785650000,"signature":"ykv9wwxjgcAu90OwW8eCPoIBghKhN9h7SLGrw+k/mIZZPD7Gp6ZtSibE5ZOMMOYwRqlwRQlfKgk2IA9hI7JM0hw=","dataPoints":[{"dataFeedId":"BTC","value":78676.8627996}],"signerAddress":"0x9c5AE89C4Af6aA32cE58588DBaF90d18a855B6de","dataServiceId":"redstone-primary-prod","dataPackageId":"BTC"},{"timestampMilliseconds":1787785650000,"signature":"xH0Ho5qiyFrgx8AFyOfE7FJL2tY1yX4IZbBVHVgJUl8pvuqJou2PZ1E1rNTq+8Uu20eX3qSaTjI01ngQBucWLBs=","dataPoints":[{"dataFeedId":"BTC","value":78676.8627996}],"signerAddress":"0xDD682daEC5A90dD295d14DA4b0bec9281017b5bE","dataServiceId":"redstone-primary-prod","dataPackageId":"BTC"},{"timestampMilliseconds":1787785650000,"signature":"xC37xCs/WdMKATMYRKOH+s8QTHAp7QOBUGuJxfY9Tb914ZyOy5ytcvnEjclG/lMyvxPrl6eQYlwKaTRirVPGLhs=","dataPoints":[{"dataFeedId":"BTC","value":78676.8627996}],"signerAddress":"0xdEB22f54738d54976C4c0fe5ce6d408E40d88499","dataServiceId":"redstone-primary-prod","dataPackageId":"BTC"},{"timestampMilliseconds":1787785650000,"signature":"vStS0UR8LW+2dlBAYXL0WqG6ZeyRGohvMIoR8QV6z4p+8qo1XD2s9HaXXJdD1AXZ7Z71HgBnc3y/YOLX7js2NRs=","dataPoints":[{"dataFeedId":"BTC","value":78676.8577998}],"signerAddress":"0x51Ce04Be4b3E32572C4Ec9135221d0691Ba7d202","dataServiceId":"redstone-primary-prod","dataPackageId":"BTC"},{"timestampMilliseconds":1787785650000,"signature":"uETOJEc1hKZQIZmaAdP/wnHSVetyTFH49vSaMdQgkvhOLhaBvLCmmfG5NQv8KYNVhHs+z6f+d4K5t7cMWocICBs=","dataPoints":[{"dataFeedId":"BTC","value":78676.8627996}],"signerAddress":"0x8BB8F32Df04c8b654987DAaeD53D6B6091e3B774","dataServiceId":"redstone-primary-prod","dataPackageId":"BTC"}]}"#;
    const FIXTURE_TS_MS: u64 = 1_787_785_650_000;

    fn rules() -> AdapterRules {
        AdapterRules {
            signer_threshold: 3,
            signers: FIXTURE_SIGNERS
                .iter()
                .map(|s| parse_address(s).unwrap())
                .collect(),
            max_delay_ms: 180_000,
            max_ahead_ms: 180_000,
            min_interval_ms: 40_000,
        }
    }
    const FIXTURE_SIGNERS: [&str; 5] = [
        "0x8BB8F32Df04c8b654987DAaeD53D6B6091e3B774",
        "0xdEB22f54738d54976C4c0fe5ce6d408E40d88499",
        "0x51Ce04Be4b3E32572C4Ec9135221d0691Ba7d202",
        "0xDD682daEC5A90dD295d14DA4b0bec9281017b5bE",
        "0x9c5AE89C4Af6aA32cE58588DBaF90d18a855B6de",
    ];

    /// The serialization is byte-exact with what RedStone's nodes signed:
    /// every captured package recovers to its claimed signer, and the
    /// recovered signer is one of the adapter's configured five.
    #[test]
    fn every_captured_package_recovers_to_its_signer() {
        let packages = parse_gateway_response(FIXTURE).unwrap();
        let rules = rules();
        let mut checked = 0;
        for (feed, list) in &packages {
            for package in list {
                let signer = recover_signer(package)
                    .unwrap_or_else(|| panic!("{feed}: no signer recovered"));
                assert_eq!(
                    signer, package.claimed_signer,
                    "{feed}: recovered signer differs from the claimed one"
                );
                assert!(
                    rules.signers.contains(&signer),
                    "{feed}: signer not in the adapter set"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 11);
    }

    /// Values are scaled by 10^8 from the gateway's decimal text, not from a
    /// float — a rounding slip of one unit would break the signature.
    #[test]
    fn values_scale_from_decimal_text() {
        assert_eq!(scale_value("0.999955").unwrap(), 99_995_500);
        assert_eq!(scale_value("78676.8627996").unwrap(), 7_867_686_279_960);
        assert_eq!(scale_value("1").unwrap(), 100_000_000);
        assert_eq!(scale_value("0.000000001").unwrap(), 0);
        assert_eq!(scale_value("1e-7").unwrap(), 10);
        assert!(scale_value("abc").is_none());
        // 2^128 - 1 with 39 decimals: the rounding add must not overflow
        // (release builds abort on overflow) — a hostile value is dropped.
        assert!(scale_value("0.340282366920938463463374607431768211455").is_none());
        assert!(scale_value("340282366920938463463374607431768211455").is_none());
    }

    /// The payload follows the RedStone protocol layout: per package the
    /// data point (32 + 32 B), a 6 B timestamp, a 4 B value size (32), a 3 B
    /// count (1) and the 65 B signature; then a 2 B package count, empty
    /// unsigned metadata with its 3 B size, and the 9 B RedStone marker.
    #[test]
    fn payload_layout_matches_the_protocol() {
        const PACKAGE: usize = 32 + 32 + 6 + 4 + 3 + 65;
        let packages = parse_gateway_response(FIXTURE).unwrap();
        let payload = build_payload(
            &packages,
            &["USDC".into(), "CETES".into()],
            &rules(),
            FIXTURE_TS_MS,
        )
        .unwrap();
        assert_eq!(payload.len(), 6 * PACKAGE + 2 + 3 + 9);
        assert_eq!(
            &payload[payload.len() - 9..],
            &[0, 0, 0x02, 0xed, 0x57, 0x01, 0x1e, 0, 0]
        );
        assert_eq!(&payload[payload.len() - 12..payload.len() - 9], &[0, 0, 0]);
        assert_eq!(&payload[payload.len() - 14..payload.len() - 12], &[0, 6]);
        assert_eq!(&payload[..4], b"USDC");

        // Exactly the threshold's worth of packages per feed, no more.
        let mut two = rules();
        two.signer_threshold = 2;
        let trimmed = build_payload(&packages, &["USDC".into()], &two, FIXTURE_TS_MS).unwrap();
        assert_eq!(trimmed.len(), 2 * PACKAGE + 2 + 3 + 9);
        assert_eq!(&trimmed[trimmed.len() - 14..trimmed.len() - 12], &[0, 2]);
    }

    /// A feed pushed within the adapter's minimum interval is not due; one
    /// never pushed, or pushed long enough ago, is.
    #[test]
    fn due_feeds_honour_the_minimum_interval() {
        use std::time::{Duration, Instant};
        let adapter: near_sdk::AccountId = "redstone.test.near".parse().unwrap();
        let now = Instant::now();
        let mut last = HashMap::new();
        last.insert(
            (adapter.clone(), "USDC".to_string()),
            now.checked_sub(Duration::from_secs(10)).unwrap(),
        );
        last.insert(
            (adapter.clone(), "BTC".to_string()),
            now.checked_sub(Duration::from_secs(41)).unwrap(),
        );
        let feeds = ["USDC".to_string(), "BTC".to_string(), "CETES".to_string()];
        assert_eq!(
            due_feeds(&last, &adapter, &feeds, Duration::from_secs(40), now),
            vec!["BTC".to_string(), "CETES".to_string()]
        );
    }

    /// A gateway configured with a path prefix keeps it — `Url::join` would
    /// have dropped it and turned the leg into a 404 per liquidation.
    #[test]
    fn packages_url_keeps_a_path_prefix() {
        let url = packages_url(&RedStonePushConfig {
            gateway_url: "https://mirror.example/redstone".parse().unwrap(),
            data_service_id: "redstone-primary-prod".to_string(),
        })
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://mirror.example/redstone/data-packages/latest/redstone-primary-prod"
        );
        let default = packages_url(&RedStonePushConfig {
            gateway_url: "https://oracle-gateway-1.a.redstone.finance"
                .parse()
                .unwrap(),
            data_service_id: "redstone-primary-prod".to_string(),
        })
        .unwrap();
        assert_eq!(default.as_str(), "https://oracle-gateway-1.a.redstone.finance/data-packages/latest/redstone-primary-prod");
    }

    /// A feed with fewer distinct in-set signers than the adapter's
    /// threshold is refused, naming the feed and the shortfall — no gas is
    /// spent on a payload the contract would reject.
    #[test]
    fn build_refuses_a_feed_below_the_signer_threshold() {
        let mut packages = parse_gateway_response(FIXTURE).unwrap();
        let mut rules = rules();
        rules.signer_threshold = 4;
        let err = build_payload(&packages, &["USDC".into()], &rules, FIXTURE_TS_MS).unwrap_err();
        assert!(
            err.contains("USDC") && err.contains('3') && err.contains('4'),
            "{err}"
        );
        packages.remove("CETES");
        let err = build_payload(&packages, &["CETES".into()], &rules, FIXTURE_TS_MS).unwrap_err();
        assert!(err.contains("CETES"), "{err}");
    }

    /// Packages outside the adapter's timestamp window, or whose signature
    /// does not recover to a configured signer, are dropped before counting.
    #[test]
    fn stale_or_foreign_packages_are_dropped() {
        let packages = parse_gateway_response(FIXTURE).unwrap();
        let rules = rules();
        let too_late = FIXTURE_TS_MS + 180_001;
        assert!(build_payload(&packages, &["USDC".into()], &rules, too_late).is_err());
        let mut foreign = rules.clone();
        foreign.signers.truncate(2);
        assert!(build_payload(&packages, &["USDC".into()], &foreign, FIXTURE_TS_MS).is_err());
        let mut tampered = packages.clone();
        tampered.get_mut("USDC").unwrap()[0].value_text = "0.5".to_string();
        let err = build_payload(&tampered, &["USDC".into()], &rules, FIXTURE_TS_MS).unwrap_err();
        assert!(
            err.contains("USDC") && err.contains('2') && err.contains('3'),
            "{err}"
        );
    }

    /// The fetch is one GET of the whole service dump (the gateway ignores
    /// feed filters), over a no-redirect client; an HTTP error is a failed
    /// fetch, not an empty map.
    #[tokio::test]
    async fn fetch_reads_the_service_dump_once() {
        let (url, requests) = crate::rpc::test_support::scripted_server(vec![
            (200, FIXTURE.to_string()),
            (502, String::new()),
        ])
        .await;
        let client = RedStonePushClient::new(RedStonePushConfig {
            gateway_url: url.clone(),
            data_service_id: "redstone-primary-prod".to_string(),
        })
        .unwrap();
        let packages = client.fetch_packages().await.unwrap();
        assert_eq!(packages["BTC"].len(), 5);
        assert!(client.fetch_packages().await.unwrap_err().contains("502"));
        let sent = requests.lock().unwrap();
        assert_eq!(sent.len(), 2);
    }
}
