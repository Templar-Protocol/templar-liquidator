//! `RUN_MODE=push-check`: proves the on-chain price-push path without a
//! liquidation. For every admitted market it reads the proxy's cached price
//! ages, pushes through the same path a liquidation uses (live mode only),
//! reads again, and judges the market against its own `price_maximum_age_s`
//! — the bound the market contract applies when it reads its oracle.
//!
//! This module holds the pure parts: the freshness observations, the
//! verdict, and the report. The runner lives in `service.rs`; the one
//! on-chain price read it needs is `OracleFetcher::onchain_publish_times`,
//! a diagnostic that exists for this mode alone — scan pricing stays
//! off-chain.

use near_sdk::AccountId;
use templar_common::oracle::pyth::PriceIdentifier;

/// One feed's cached publish time on the proxy, or `None` when the proxy
/// has nothing cached for it (what the market would read as a missing
/// price).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedFreshness {
    pub price_id: PriceIdentifier,
    pub publish_time_secs: Option<i64>,
}

/// Whether a market's oracle reads fresh enough for a liquidation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushVerdict {
    /// Every feed is within the market's `price_maximum_age_s`.
    Fresh,
    /// At least one feed is missing or over the bound; the reason names each.
    Stale { reason: String },
}

impl std::fmt::Display for PushVerdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fresh => f.write_str("fresh"),
            Self::Stale { reason } => write!(f, "stale ({reason})"),
        }
    }
}

fn short(id: PriceIdentifier) -> String {
    format!("{}…", hex::encode(&id.0[..4]))
}

/// Judges a market from its feeds' cached publish times: fresh only when
/// every feed has a price no older than `max_age_secs` (inclusive, matching
/// the contract's `no_older_than`); a missing price is stale.
#[must_use]
pub fn judge(now_secs: i64, max_age_secs: u32, feeds: &[FeedFreshness]) -> PushVerdict {
    let problems: Vec<String> = feeds
        .iter()
        .filter_map(|feed| match feed.publish_time_secs {
            None => Some(format!("{}: no cached price", short(feed.price_id))),
            Some(published) => {
                let age = now_secs.saturating_sub(published);
                (age > i64::from(max_age_secs)).then(|| {
                    format!(
                        "{}: {age}s old, bound {max_age_secs}s",
                        short(feed.price_id)
                    )
                })
            }
        })
        .collect();
    if problems.is_empty() {
        PushVerdict::Fresh
    } else {
        PushVerdict::Stale {
            reason: problems.join("; "),
        }
    }
}

/// Renders feeds' ages for a log line: `aabbccdd…: 12s, eeff0011…: none`.
#[must_use]
pub fn render_ages(now_secs: i64, feeds: &[FeedFreshness]) -> String {
    feeds
        .iter()
        .map(|feed| match feed.publish_time_secs {
            None => format!("{}: none", short(feed.price_id)),
            Some(published) => format!(
                "{}: {}s",
                short(feed.price_id),
                now_secs.saturating_sub(published)
            ),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// One market's check: what its oracle read before and after the push, and
/// the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketPushReport {
    pub market: AccountId,
    pub oracle: AccountId,
    pub max_age_secs: u32,
    /// Whether any adapter push or proxy re-aggregation was submitted
    /// (always `false` in dry-run).
    pub pushed: bool,
    pub before: Vec<FeedFreshness>,
    pub after: Vec<FeedFreshness>,
    pub verdict: PushVerdict,
}

/// The whole run. `passed()` is the exit-code contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushCheckReport {
    pub dry_run: bool,
    pub markets: Vec<MarketPushReport>,
}

impl PushCheckReport {
    /// A pass means the push path was exercised and every checked market
    /// then read fresh. A dry run pushes nothing, so it reports but never
    /// passes; an empty run proves nothing either.
    #[must_use]
    pub fn passed(&self) -> bool {
        !self.dry_run
            && !self.markets.is_empty()
            && self
                .markets
                .iter()
                .all(|market| market.verdict == PushVerdict::Fresh)
    }

    #[must_use]
    pub fn fresh_count(&self) -> usize {
        self.markets
            .iter()
            .filter(|market| market.verdict == PushVerdict::Fresh)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_common::oracle::pyth::PriceIdentifier;

    fn feed(id: u8, publish_time_secs: Option<i64>) -> FeedFreshness {
        FeedFreshness {
            price_id: PriceIdentifier([id; 32]),
            publish_time_secs,
        }
    }

    /// A market passes only when every feed's cached price is within the
    /// market's own `price_maximum_age_s`; the bound is inclusive, matching
    /// the contract's `no_older_than`.
    #[test]
    fn verdict_is_fresh_only_when_every_feed_is_within_the_bound() {
        let now = 1_000_000;
        assert_eq!(
            judge(now, 60, &[feed(1, Some(now - 60)), feed(2, Some(now))]),
            PushVerdict::Fresh
        );
        let PushVerdict::Stale { reason } =
            judge(now, 60, &[feed(1, Some(now - 61)), feed(2, Some(now))])
        else {
            panic!("one feed over the bound must be stale");
        };
        assert!(reason.contains("61s") && reason.contains("60s"), "{reason}");
    }

    /// A feed with no cached price at all is stale — the market would read
    /// `None` and refuse the liquidation.
    #[test]
    fn missing_price_is_stale() {
        let PushVerdict::Stale { reason } = judge(1_000_000, 60, &[feed(1, None)]) else {
            panic!("a missing price must be stale");
        };
        assert!(reason.contains("no cached price"), "{reason}");
    }

    /// The report is a pass only when every checked market is fresh; a
    /// dry run never counts as a pass because nothing was pushed.
    #[test]
    fn report_passes_only_when_live_and_all_fresh() {
        let market = |verdict| MarketPushReport {
            market: "m.test.near".parse().unwrap(),
            oracle: "o.test.near".parse().unwrap(),
            max_age_secs: 60,
            pushed: true,
            before: vec![],
            after: vec![],
            verdict,
        };
        let live = PushCheckReport {
            dry_run: false,
            markets: vec![market(PushVerdict::Fresh)],
        };
        assert!(live.passed());
        let mixed = PushCheckReport {
            dry_run: false,
            markets: vec![
                market(PushVerdict::Fresh),
                market(PushVerdict::Stale {
                    reason: "x".to_string(),
                }),
            ],
        };
        assert!(!mixed.passed());
        let dry = PushCheckReport {
            dry_run: true,
            markets: vec![market(PushVerdict::Fresh)],
        };
        assert!(!dry.passed());
    }
}
