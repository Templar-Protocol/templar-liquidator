//! Minimal Prometheus text-format metrics, no external metrics dependencies.
//!
//! Exposed via the optional HTTP surface ([`crate::http`]) when `HTTP_PORT`
//! is set. All counters are process-lifetime (reset on restart), which is the
//! Prometheus counter contract. Fields are private: every update goes through
//! an intent method below so a counter can only move the way its name
//! promises (e.g. nothing can `store()` it backwards).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::RoundSummary;

/// Process-wide operational counters for the liquidator.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Liquidation scan cycles started.
    scans_total: AtomicU64,
    /// Per-market scan failures. Not the same denominator as `scans_total`:
    /// one cycle scans every configured market, so a single cycle can
    /// contribute several of these (e.g. 3 failures in a 5-market cycle) —
    /// dividing this by `scans_total` is not a failure rate.
    market_scan_failures_total: AtomicU64,
    /// Positions that reached profitability evaluation or a submitted
    /// transaction, across all scans. See [`RoundSummary::candidates`] for
    /// exactly what this excludes (insufficient-inventory skips, and
    /// scan/preparation-phase errors before evaluation).
    candidates_found_total: AtomicU64,
    /// Liquidation transactions submitted (or simulated in dry-run).
    liquidations_attempted_total: AtomicU64,
    /// Liquidations that landed successfully.
    liquidations_succeeded_total: AtomicU64,
    /// Liquidations that failed after a transaction was submitted
    /// (`ErrorPhase::Execution` only). Narrower than the `failed` field in
    /// the per-market "Liquidation run completed" log line, which also
    /// counts scan/preparation-phase failures — the two numbers are
    /// expected to disagree; don't reconcile them.
    liquidations_failed_total: AtomicU64,
    /// Unix seconds of the last scan cycle that scanned at least one market
    /// without error (0 = never). A cycle where every market failed, or
    /// where the registry was empty, does not advance this. Alerts computed
    /// as `time() - metric` must guard `metric > 0` first, or a
    /// never-scanned process reads as "last scanned in 1970" instead of
    /// "never scanned".
    last_successful_scan_unix: AtomicU64,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Metrics {
    /// Records the start of a scan cycle.
    pub fn inc_scan(&self) {
        self.scans_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Records one market's scan failing within a cycle. Call once per
    /// failing market, not once per cycle — see the field doc.
    pub fn inc_market_scan_failure(&self) {
        self.market_scan_failures_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Folds a completed round's tally into the cumulative counters.
    pub fn add_round(&self, summary: &RoundSummary) {
        self.candidates_found_total
            .fetch_add(summary.candidates, Ordering::Relaxed);
        self.liquidations_attempted_total
            .fetch_add(summary.attempted, Ordering::Relaxed);
        self.liquidations_succeeded_total
            .fetch_add(summary.succeeded, Ordering::Relaxed);
        self.liquidations_failed_total
            .fetch_add(summary.failed, Ordering::Relaxed);
    }

    /// Records a scan cycle in which at least one market scanned without
    /// error. Callers should gate this on the round's
    /// `markets_scanned_ok` being nonzero — a cycle where every market
    /// failed (or the registry was empty) must not be recorded as a
    /// successful scan.
    pub fn mark_scan_success(&self) {
        self.last_successful_scan_unix
            .store(now_unix(), Ordering::Relaxed);
    }

    /// True when a successful scan happened within `max_age_secs`.
    pub fn healthy(&self, max_age_secs: u64) -> bool {
        let last = self.last_successful_scan_unix.load(Ordering::Relaxed);
        last != 0 && now_unix().saturating_sub(last) <= max_age_secs
    }

    /// Prometheus text exposition (format 0.0.4).
    pub fn render(&self) -> String {
        let c = |n: &str, v: u64| {
            format!("# TYPE templar_liquidator_{n} counter\ntemplar_liquidator_{n} {v}\n")
        };
        let g = |n: &str, v: u64| {
            format!("# TYPE templar_liquidator_{n} gauge\ntemplar_liquidator_{n} {v}\n")
        };
        [
            c("scans_total", self.scans_total.load(Ordering::Relaxed)),
            c(
                "market_scan_failures_total",
                self.market_scan_failures_total.load(Ordering::Relaxed),
            ),
            c(
                "candidates_found_total",
                self.candidates_found_total.load(Ordering::Relaxed),
            ),
            c(
                "liquidations_attempted_total",
                self.liquidations_attempted_total.load(Ordering::Relaxed),
            ),
            c(
                "liquidations_succeeded_total",
                self.liquidations_succeeded_total.load(Ordering::Relaxed),
            ),
            c(
                "liquidations_failed_total",
                self.liquidations_failed_total.load(Ordering::Relaxed),
            ),
            g(
                "last_successful_scan_timestamp_seconds",
                self.last_successful_scan_unix.load(Ordering::Relaxed),
            ),
        ]
        .concat()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_exposes_all_series_in_prometheus_text_format() {
        let m = Metrics::default();
        m.inc_scan();
        m.inc_scan();
        m.inc_scan();
        let out = m.render();
        for name in [
            "templar_liquidator_scans_total",
            "templar_liquidator_market_scan_failures_total",
            "templar_liquidator_candidates_found_total",
            "templar_liquidator_liquidations_attempted_total",
            "templar_liquidator_liquidations_succeeded_total",
            "templar_liquidator_liquidations_failed_total",
            "templar_liquidator_last_successful_scan_timestamp_seconds",
        ] {
            assert!(out.contains(name), "missing {name}");
        }
        assert!(out.contains("templar_liquidator_scans_total 3"));
    }

    #[test]
    fn healthy_requires_a_recent_successful_scan() {
        let m = Metrics::default();
        assert!(!m.healthy(1800), "never-scanned must be unhealthy");
        m.mark_scan_success();
        assert!(m.healthy(1800));
    }

    #[test]
    fn healthy_treats_a_stale_last_scan_as_unhealthy() {
        let m = Metrics::default();
        // A scan that happened, but long enough ago to be outside the window.
        m.last_successful_scan_unix
            .store(now_unix().saturating_sub(10_000), Ordering::Relaxed);
        assert!(!m.healthy(1800));
    }
}
