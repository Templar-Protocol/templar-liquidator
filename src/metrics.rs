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
    /// Liquidatable positions skipped because inventory or sizing did not
    /// permit an attempt — the "money left on the table" counter; alert on
    /// it growing. See `LiquidationOutcome::SkippedUnfunded`.
    liquidations_skipped_unfunded_total: AtomicU64,
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
    /// Raw units currently reserved for in-flight liquidations, per asset —
    /// the labelled family `templar_liquidator_inventory_reserved_raw{asset=…}`. An asset stays
    /// in the map after settling back to zero: a gauge reading 0 is signal,
    /// a vanished series is a scrape gap. Raw token units (u128, rendered as
    /// an integer): scrapers parse Prometheus values as f64, so very large
    /// 24-decimal amounts lose low-order precision at the scraper — fine
    /// for the alerting this exists for ("reservations stuck nonzero").
    reserved_by_asset: std::sync::Mutex<std::collections::BTreeMap<String, u128>>,
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
        self.liquidations_skipped_unfunded_total
            .fetch_add(summary.skipped_unfunded, Ordering::Relaxed);
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

    /// Sets the reserved-inventory gauge for one asset (raw token units).
    /// Call with the asset's current total whenever a reservation is issued
    /// or settled; zero keeps the series present rather than removing it.
    pub fn set_reserved_raw(&self, asset: &str, amount: u128) {
        if let Ok(mut map) = self.reserved_by_asset.lock() {
            map.insert(asset.to_string(), amount);
        }
    }

    /// True when a successful scan happened within `max_age_secs`.
    pub fn healthy(&self, max_age_secs: u64) -> bool {
        let last = self.last_successful_scan_unix.load(Ordering::Relaxed);
        last != 0 && now_unix().saturating_sub(last) <= max_age_secs
    }

    /// Prometheus text exposition (format 0.0.4).
    pub fn render(&self) -> String {
        let c = |n: &str, help: &str, v: u64| {
            format!(
                "# HELP templar_liquidator_{n} {help}\n# TYPE templar_liquidator_{n} counter\ntemplar_liquidator_{n} {v}\n"
            )
        };
        let g = |n: &str, help: &str, v: u64| {
            format!(
                "# HELP templar_liquidator_{n} {help}\n# TYPE templar_liquidator_{n} gauge\ntemplar_liquidator_{n} {v}\n"
            )
        };
        let reserved = {
            let mut out = String::from(
                "# HELP templar_liquidator_inventory_reserved_raw Raw token units currently reserved for in-flight liquidations, per asset.\n# TYPE templar_liquidator_inventory_reserved_raw gauge\n",
            );
            if let Ok(map) = self.reserved_by_asset.lock() {
                use std::fmt::Write as _;
                for (asset, amount) in map.iter() {
                    // Infallible for String; ignore the fmt::Result.
                    let _ = writeln!(
                        out,
                        "templar_liquidator_inventory_reserved_raw{{asset=\"{}\"}} {amount}",
                        escape_label_value(asset)
                    );
                }
            }
            out
        };
        [
            c(
                "scans_total",
                "Liquidation scan cycles started.",
                self.scans_total.load(Ordering::Relaxed),
            ),
            c(
                "market_scan_failures_total",
                "Individual market scan failures; one cycle can contribute several.",
                self.market_scan_failures_total.load(Ordering::Relaxed),
            ),
            c(
                "candidates_found_total",
                "Positions that reached profitability evaluation or a submitted transaction.",
                self.candidates_found_total.load(Ordering::Relaxed),
            ),
            c(
                "liquidations_attempted_total",
                "Liquidation transactions submitted (or simulated in dry-run).",
                self.liquidations_attempted_total.load(Ordering::Relaxed),
            ),
            c(
                "liquidations_succeeded_total",
                "Liquidations that landed successfully.",
                self.liquidations_succeeded_total.load(Ordering::Relaxed),
            ),
            c(
                "liquidations_failed_total",
                "Liquidations that failed after a transaction was submitted.",
                self.liquidations_failed_total.load(Ordering::Relaxed),
            ),
            c(
                "liquidations_skipped_unfunded_total",
                "Liquidatable positions skipped because inventory or sizing did not permit an attempt.",
                self.liquidations_skipped_unfunded_total.load(Ordering::Relaxed),
            ),
            g(
                "last_successful_scan_timestamp_seconds",
                "Unix time of the last cycle with at least one clean market scan; 0 = never.",
                self.last_successful_scan_unix.load(Ordering::Relaxed),
            ),
            reserved,
        ]
        .concat()
    }
}

/// Escapes a label value per the Prometheus text format: backslash, double
/// quote, and newline are the three characters the format requires escaping.
fn escape_label_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
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
            "templar_liquidator_liquidations_skipped_unfunded_total",
            "templar_liquidator_last_successful_scan_timestamp_seconds",
        ] {
            assert!(out.contains(name), "missing {name}");
        }
        assert!(out.contains("templar_liquidator_scans_total 3"));
    }

    #[test]
    fn every_series_has_help_and_type_lines() {
        let m = Metrics::default();
        m.set_reserved_raw("usdc.near", 5);
        let out = m.render();
        for name in [
            "templar_liquidator_scans_total",
            "templar_liquidator_market_scan_failures_total",
            "templar_liquidator_candidates_found_total",
            "templar_liquidator_liquidations_attempted_total",
            "templar_liquidator_liquidations_succeeded_total",
            "templar_liquidator_liquidations_failed_total",
            "templar_liquidator_liquidations_skipped_unfunded_total",
            "templar_liquidator_last_successful_scan_timestamp_seconds",
            "templar_liquidator_inventory_reserved_raw",
        ] {
            assert!(
                out.contains(&format!("# HELP {name} ")),
                "no HELP for {name}"
            );
            assert!(
                out.contains(&format!("# TYPE {name} ")),
                "no TYPE for {name}"
            );
        }
    }

    #[test]
    fn reserved_gauge_renders_one_labelled_line_per_asset() {
        let m = Metrics::default();
        m.set_reserved_raw("usdc.near", 1_500_000);
        m.set_reserved_raw("wbtc.near", 42);
        let out = m.render();
        assert!(
            out.contains(r#"templar_liquidator_inventory_reserved_raw{asset="usdc.near"} 1500000"#)
        );
        assert!(out.contains(r#"templar_liquidator_inventory_reserved_raw{asset="wbtc.near"} 42"#));
        // Settling back to zero keeps the series (a gauge going 5 -> 0 is
        // signal; a vanished series is a scrape gap).
        m.set_reserved_raw("usdc.near", 0);
        let out = m.render();
        assert!(out.contains(r#"templar_liquidator_inventory_reserved_raw{asset="usdc.near"} 0"#));
    }

    /// NEAR account ids cannot contain quotes or backslashes, but the
    /// renderer must not rely on that — escaping is the renderer's job.
    #[test]
    fn label_values_are_escaped_per_prometheus_text_format() {
        let m = Metrics::default();
        m.set_reserved_raw("we\\ird\"asset\nname", 7);
        let out = m.render();
        assert!(out.contains("{asset=\"we\\\\ird\\\"asset\\nname\"} 7"));
    }

    /// With no reservations ever seen, the family still emits HELP/TYPE so
    /// scrapers learn the series exists (an empty family, not an absent one).
    #[test]
    fn reserved_family_header_is_present_without_data() {
        let out = Metrics::default().render();
        assert!(out.contains("# TYPE templar_liquidator_inventory_reserved_raw gauge"));
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
