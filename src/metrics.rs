//! Minimal Prometheus text-format metrics, no external metrics dependencies.
//!
//! Exposed via the optional HTTP surface ([`crate::http`]) when `HTTP_PORT`
//! is set. All counters are process-lifetime (reset on restart), which is the
//! Prometheus counter contract.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Process-wide operational counters for the liquidator.
#[derive(Debug, Default)]
pub struct Metrics {
    /// Liquidation scan cycles started.
    pub scans_total: AtomicU64,
    /// Scan cycles that failed before completing.
    pub scan_failures_total: AtomicU64,
    /// Underwater positions identified across all scans.
    pub candidates_found_total: AtomicU64,
    /// Liquidation transactions submitted (or simulated in dry-run).
    pub liquidations_attempted_total: AtomicU64,
    /// Liquidations that landed successfully.
    pub liquidations_succeeded_total: AtomicU64,
    /// Liquidations that failed.
    pub liquidations_failed_total: AtomicU64,
    /// Unix seconds of the last fully successful scan cycle (0 = never).
    pub last_successful_scan_unix: AtomicU64,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Metrics {
    /// Records a completed successful scan cycle.
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
                "scan_failures_total",
                self.scan_failures_total.load(Ordering::Relaxed),
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
        m.scans_total.fetch_add(3, Ordering::Relaxed);
        let out = m.render();
        for name in [
            "templar_liquidator_scans_total",
            "templar_liquidator_scan_failures_total",
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
}
