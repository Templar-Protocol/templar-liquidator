//! Notification system for the liquidator bot.
//!
//! [`Notifier`] is the notification extension seam: it currently sends
//! HTML-formatted Telegram messages and nothing else, with no trait in front of
//! it. A fork that needs a different channel (another chat platform, a paging
//! system, a metrics sink) extends or replaces this type directly — see the
//! crate-level docs for how this fits alongside the other two extension seams.
//!
//! # What triggers each notification
//!
//! - [`notify_liquidation`](Notifier::notify_liquidation) — a liquidation
//!   transaction landed (or, in dry-run, would have), sent from
//!   [`crate::Liquidator::liquidate`].
//! - [`notify_liquidation_failed`](Notifier::notify_liquidation_failed) — a
//!   liquidation reached the execution phase and failed there (not a scan or
//!   pre-submission failure — see [`crate::ErrorPhase`]), sent from
//!   [`crate::Liquidator::run_liquidations`].
//! - [`notify_swap_failed`](Notifier::notify_swap_failed) /
//!   [`notify_swap_unsupported`](Notifier::notify_swap_unsupported) — the
//!   post-liquidation JIT collateral swap errored, or was skipped because
//!   [`crate::swap::SwapProvider::supports_assets`] returned `false`. The
//!   underlying `SwapIssue` is produced inside
//!   [`crate::executor`]'s collateral handling, but — like
//!   [`notify_liquidation`](Notifier::notify_liquidation) — is reported by
//!   [`crate::Liquidator::liquidate`] only after the liquidation itself has
//!   already succeeded, so a swap notification never implies the liquidation
//!   failed too.
//! - [`notify_scan_failures`](Notifier::notify_scan_failures) /
//!   [`notify_scan_recovered`](Notifier::notify_scan_recovered) — see below.
//!
//! # Scan-failure threshold
//!
//! Unlike the other notifications, consecutive-scan-failure tracking is not
//! owned by this module: [`crate::service::LiquidatorService`] counts
//! consecutive scan failures per market and calls
//! [`notify_scan_failures`](Notifier::notify_scan_failures) exactly once, the
//! round the count first *reaches* `SCAN_FAILURE_NOTIFY_THRESHOLD` — not on
//! every failure thereafter, so an outage produces one alert, not a flood. A
//! later successful scan for that market calls
//! [`notify_scan_recovered`](Notifier::notify_scan_recovered) once and resets
//! the count, but only if the threshold had actually been crossed (so a market
//! that fails once and recovers before ever alerting stays silent). Setting the
//! threshold to `0` disables both notifications entirely.
//!
//! # Failure-notification cooldown (dedup)
//!
//! This module *does* own deduplication for
//! [`notify_liquidation_failed`](Notifier::notify_liquidation_failed):
//! repeated failures for the same `(market, borrower, error_kind)` triple are
//! suppressed for `FAILURE_NOTIFICATION_COOLDOWN_HOURS`
//! ([`DEFAULT_FAILURE_NOTIFY_COOLDOWN_HOURS`] hours by default) so a position
//! stuck failing the same way doesn't page every round. A successful
//! liquidation, or the position becoming healthy, clears the dedup state for
//! that borrower immediately via
//! [`clear_failure_dedup_for`](Notifier::clear_failure_dedup_for) — the next
//! failure of any kind fires right away rather than waiting out the cooldown.
//!
//! # Fire-and-forget delivery
//!
//! All `notify_*` methods are truly fire-and-forget: they spawn the HTTP
//! request on a background task and return immediately, so they never
//! block liquidation operations. A bounded semaphore
//! (`MAX_INFLIGHT_NOTIFICATIONS`) limits in-flight notifications to prevent
//! unbounded task growth; once it's full, new notifications are dropped (and
//! logged) rather than queued. Because sends are detached background tasks,
//! nothing keeps them alive if the async runtime shuts down first — that is
//! exactly the case a single-cycle (`--run-mode once`) run hits at the end of
//! `main`, which is why [`drain`](Notifier::drain) exists: call it before
//! exiting to block until every in-flight send has actually completed. A
//! long-running (`loop` mode) process never needs this, since the runtime
//! keeps running past any individual round.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use near_sdk::serde_json::json;
use reqwest::Client;
use tokio::sync::Semaphore;

/// A string wrapper that redacts its value in Debug output.
#[derive(Clone)]
pub struct SecretString(String);

impl SecretString {
    /// Access the inner value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for SecretString {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Telegram notification configuration.
#[derive(Debug, Clone)]
pub struct TelegramConfig {
    pub bot_token: SecretString,
    pub chat_id: String,
    pub thread_id: Option<i64>,
}

/// Shared notifier handle.
pub type SharedNotifier = Arc<Notifier>;

/// Maximum number of in-flight Telegram notifications.
const MAX_INFLIGHT_NOTIFICATIONS: usize = 10;

/// Default cooldown (in hours) for repeated identical failure notifications.
/// Shared source of truth used by both the runtime default and the CLI/env default.
pub const DEFAULT_FAILURE_NOTIFY_COOLDOWN_HOURS: u64 = 24;

/// Default cooldown for repeated identical failure notifications.
///
/// Same (market, borrower, `error_kind`) within this window is suppressed.
pub const DEFAULT_FAILURE_NOTIFY_COOLDOWN: Duration =
    Duration::from_secs(DEFAULT_FAILURE_NOTIFY_COOLDOWN_HOURS * 60 * 60);

/// Dedup key for failure notifications. Stable across rounds for the same
/// (market, borrower, error class).
type DedupKey = (String, String, crate::NotificationKind);

/// Liquidator event notifier.
///
/// When Telegram is configured, sends HTML-formatted messages via
/// background tasks. When unconfigured, all methods are silent no-ops.
/// A semaphore bounds the number of concurrent in-flight notifications.
#[derive(Debug)]
pub struct Notifier {
    telegram: Option<TelegramConfig>,
    client: Client,
    semaphore: Arc<Semaphore>,
    /// Last-sent time for each (market, borrower, `error_kind`) — suppresses
    /// repeat alerts within `failure_cooldown`. Cleared per-borrower when a
    /// liquidation succeeds.
    failure_dedup: Mutex<HashMap<DedupKey, Instant>>,
    failure_cooldown: Duration,
}

/// Escape HTML special characters in dynamic values so they don't break
/// Telegram's HTML parse mode or get rejected.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ── Message formatting (pure functions, easily testable) ────────────────

/// Format a successful liquidation message.
#[allow(clippy::too_many_arguments)]
fn format_liquidation_message(
    market: &str,
    borrower: &str,
    send_amount: &str,
    receive_amount: &str,
    profit: &str,
    tx_hash: Option<&str>,
    dry_run: bool,
) -> String {
    let prefix = if dry_run { "🧪 DRY RUN " } else { "" };
    let tx_line = tx_hash.map_or(String::new(), |h| {
        format!("\nTx: <code>{}</code>", html_escape(h))
    });

    format!(
        "{prefix}✅ <b>Liquidation Executed</b>\n\
         \n\
         Market: <code>{}</code>\n\
         Borrower: <code>{}</code>\n\
         Sent: {}\n\
         Received: {}\n\
         Profit: {}{tx_line}",
        html_escape(market),
        html_escape(borrower),
        html_escape(send_amount),
        html_escape(receive_amount),
        html_escape(profit),
    )
}

/// Format a failed liquidation message.
fn format_liquidation_failed_message(market: &str, borrower: &str, error: &str) -> String {
    format!(
        "❌ <b>Liquidation Failed</b>\n\
         \n\
         Market: <code>{}</code>\n\
         Borrower: <code>{}</code>\n\
         Error: {}",
        html_escape(market),
        html_escape(borrower),
        html_escape(error),
    )
}

/// Format a swap failure message.
fn format_swap_failed_message(
    market: &str,
    from_asset: &str,
    to_asset: &str,
    amount: &str,
    error: &str,
) -> String {
    format!(
        "⚠️ <b>Swap Failed</b>\n\
         \n\
         Market: <code>{}</code>\n\
         From: <code>{}</code>\n\
         To: <code>{}</code>\n\
         Amount: {}\n\
         Error: {}",
        html_escape(market),
        html_escape(from_asset),
        html_escape(to_asset),
        html_escape(amount),
        html_escape(error),
    )
}

/// Format a swap-unsupported message.
fn format_swap_unsupported_message(
    market: &str,
    from_asset: &str,
    to_asset: &str,
    amount: &str,
) -> String {
    format!(
        "🚫 <b>Swap Unsupported</b>\n\
         \n\
         Market: <code>{}</code>\n\
         From: <code>{}</code>\n\
         To: <code>{}</code>\n\
         Amount: {}\n\
         \n\
         Asset pair not supported by swap provider.",
        html_escape(market),
        html_escape(from_asset),
        html_escape(to_asset),
        html_escape(amount),
    )
}

/// Format a repeated scan failure message.
fn format_scan_failures_message(market: &str, count: u32, last_error: &str) -> String {
    format!(
        "🔴 <b>Market Scan Failing</b>\n\
         \n\
         Market: <code>{}</code>\n\
         Consecutive failures: {count}\n\
         Last error: {}",
        html_escape(market),
        html_escape(last_error),
    )
}

/// Format a market recovery message.
fn format_scan_recovered_message(market: &str, prev_failures: u32) -> String {
    format!(
        "🟢 <b>Market Recovered</b>\n\
         \n\
         Market: <code>{}</code>\n\
         After {prev_failures} consecutive failures.",
        html_escape(market),
    )
}

impl Notifier {
    /// Creates a new notifier. Pass `None` to disable notifications.
    pub fn new(telegram: Option<TelegramConfig>) -> Self {
        Self::with_cooldown(telegram, DEFAULT_FAILURE_NOTIFY_COOLDOWN)
    }

    /// Creates a notifier with a custom failure-notification cooldown.
    pub fn with_cooldown(telegram: Option<TelegramConfig>, failure_cooldown: Duration) -> Self {
        Self {
            telegram,
            client: Client::new(),
            semaphore: Arc::new(Semaphore::new(MAX_INFLIGHT_NOTIFICATIONS)),
            failure_dedup: Mutex::new(HashMap::new()),
            failure_cooldown,
        }
    }

    /// Returns `true` if Telegram notifications are enabled.
    pub fn is_enabled(&self) -> bool {
        self.telegram.is_some()
    }

    /// Waits for all in-flight notifications to finish sending, up to `timeout`.
    ///
    /// A no-op when Telegram is unconfigured — nothing was ever spawned. Works
    /// by acquiring every semaphore permit: that only succeeds once every
    /// `spawn_send` task has released its permit, i.e. once the background
    /// sends have all completed. Single-cycle runs must call this before
    /// returning, since dropping the runtime otherwise cancels any Telegram
    /// POST still in flight; long-running modes never need this because the
    /// process keeps running past any individual cycle.
    pub async fn drain(&self, timeout: Duration) {
        if self.telegram.is_none() {
            return;
        }
        let all_permits = u32::try_from(MAX_INFLIGHT_NOTIFICATIONS).unwrap_or(u32::MAX);
        if tokio::time::timeout(timeout, self.semaphore.acquire_many(all_permits))
            .await
            .is_err()
        {
            tracing::warn!("Timed out draining in-flight notifications");
        }
    }

    /// Notify about a successful liquidation.
    #[allow(clippy::too_many_arguments)]
    pub fn notify_liquidation(
        self: &Arc<Self>,
        market: &str,
        borrower: &str,
        send_amount: &str,
        receive_amount: &str,
        profit: &str,
        tx_hash: Option<&str>,
        dry_run: bool,
    ) {
        self.spawn_send(format_liquidation_message(
            market,
            borrower,
            send_amount,
            receive_amount,
            profit,
            tx_hash,
            dry_run,
        ));
    }

    /// Notify about a failed liquidation attempt.
    ///
    /// Repeated alerts for the same `(market, borrower, error_kind)` are
    /// suppressed within the configured cooldown window.
    ///
    /// The dedup entry is recorded only when the send is actually accepted
    /// by the in-flight semaphore; if the message is dropped due to overload,
    /// the entry is rolled back so the next call can retry.
    pub fn notify_liquidation_failed(
        self: &Arc<Self>,
        market: &str,
        borrower: &str,
        error_kind: crate::NotificationKind,
        error: &str,
    ) {
        if !self.should_send_failure(market, borrower, error_kind) {
            tracing::debug!(
                market,
                borrower,
                error_kind = %error_kind,
                "Liquidation failure notification suppressed by dedup"
            );
            return;
        }
        let queued = self.spawn_send(format_liquidation_failed_message(market, borrower, error));
        if !queued {
            self.rollback_failure_dedup(market, borrower, error_kind);
        }
    }

    /// Removes a specific dedup entry. Used to roll back when `spawn_send`
    /// could not queue the message.
    fn rollback_failure_dedup(
        &self,
        market: &str,
        borrower: &str,
        error_kind: crate::NotificationKind,
    ) {
        if let Ok(mut dedup) = self.failure_dedup.lock() {
            dedup.remove(&(market.to_string(), borrower.to_string(), error_kind));
        }
    }

    /// Clears suppression state for a borrower so the next failure (of any
    /// kind) fires a fresh notification. Call this on successful liquidation
    /// or when the position becomes healthy.
    pub fn clear_failure_dedup_for(&self, market: &str, borrower: &str) {
        if let Ok(mut dedup) = self.failure_dedup.lock() {
            dedup.retain(|(m, b, _), _| !(m == market && b == borrower));
        }
    }

    /// Returns `true` if the (market, borrower, kind) tuple is outside the
    /// cooldown window, and records the send time. Garbage-collects stale
    /// entries opportunistically.
    fn should_send_failure(
        &self,
        market: &str,
        borrower: &str,
        kind: crate::NotificationKind,
    ) -> bool {
        let Ok(mut dedup) = self.failure_dedup.lock() else {
            // Poisoned mutex — fall through and send rather than block alerts.
            return true;
        };
        let now = Instant::now();
        dedup.retain(|_, last| now.duration_since(*last) < self.failure_cooldown);
        let key = (market.to_string(), borrower.to_string(), kind);
        match dedup.get(&key) {
            Some(last) if now.duration_since(*last) < self.failure_cooldown => false,
            _ => {
                dedup.insert(key, now);
                true
            }
        }
    }

    /// Notify about a swap failure after liquidation.
    pub fn notify_swap_failed(
        self: &Arc<Self>,
        market: &str,
        from_asset: &str,
        to_asset: &str,
        amount: &str,
        error: &str,
    ) {
        self.spawn_send(format_swap_failed_message(
            market, from_asset, to_asset, amount, error,
        ));
    }

    /// Notify about repeated scan failures for a market.
    pub fn notify_scan_failures(self: &Arc<Self>, market: &str, count: u32, last_error: &str) {
        self.spawn_send(format_scan_failures_message(market, count, last_error));
    }

    /// Notify that a market recovered after consecutive scan failures.
    pub fn notify_scan_recovered(self: &Arc<Self>, market: &str, prev_failures: u32) {
        self.spawn_send(format_scan_recovered_message(market, prev_failures));
    }

    /// Notify when a swap is skipped because the asset pair is unsupported.
    pub fn notify_swap_unsupported(
        self: &Arc<Self>,
        market: &str,
        from_asset: &str,
        to_asset: &str,
        amount: &str,
    ) {
        self.spawn_send(format_swap_unsupported_message(
            market, from_asset, to_asset, amount,
        ));
    }

    // ── Internal ────────────────────────────────────────────────────────────

    /// Spawns the send on a background task so callers never block.
    ///
    /// Returns `true` if the message was queued (or notifications are
    /// disabled, which is a configured no-op), and `false` only when the
    /// semaphore was full and the message was dropped due to overload.
    /// Callers that own dedup state can use the `false` return to roll back.
    fn spawn_send(self: &Arc<Self>, message: String) -> bool {
        if self.telegram.is_none() {
            return true;
        }
        let Ok(permit) = Arc::clone(&self.semaphore).try_acquire_owned() else {
            tracing::warn!("Notification dropped — too many in-flight messages");
            return false;
        };
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.send(&message).await;
            drop(permit);
        });
        true
    }

    /// Sends an HTML message to the configured Telegram chat.
    /// Failures are logged and swallowed — never propagated.
    async fn send(&self, text: &str) {
        let Some(config) = &self.telegram else {
            return;
        };

        let url = format!(
            "https://api.telegram.org/bot{}/sendMessage",
            config.bot_token.as_str()
        );

        let mut payload = json!({
            "chat_id": config.chat_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        });

        if let Some(tid) = config.thread_id {
            payload["message_thread_id"] = json!(tid);
        }

        match self
            .client
            .post(&url)
            .json(&payload)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(response) if response.status() == 429 => {
                tracing::warn!("Telegram rate limit hit, skipping notification");
            }
            Ok(response) if !response.status().is_success() => {
                let status = response.status();
                let body = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "unknown".to_string());
                tracing::warn!(
                    status = %status,
                    body = %body,
                    "Telegram notification failed"
                );
            }
            Ok(_) => {
                tracing::debug!("Telegram notification sent");
            }
            Err(e) => {
                let safe_error = e.without_url();
                tracing::warn!(error = %safe_error, "Failed to send Telegram notification");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_notifier_disabled_by_default() {
        let notifier = Notifier::new(None);
        assert!(!notifier.is_enabled());
    }

    #[test]
    fn test_notifier_enabled_with_config() {
        let config = TelegramConfig {
            bot_token: "123:ABC".to_string().into(),
            chat_id: "-100123".to_string(),
            thread_id: None,
        };
        let notifier = Notifier::new(Some(config));
        assert!(notifier.is_enabled());
    }

    #[test]
    fn test_notifier_with_thread_id() {
        let config = TelegramConfig {
            bot_token: "123:ABC".to_string().into(),
            chat_id: "-100123".to_string(),
            thread_id: Some(42),
        };
        let notifier = Notifier::new(Some(config.clone()));
        assert!(notifier.is_enabled());
        assert_eq!(config.thread_id, Some(42));
    }

    #[test]
    fn test_secret_string_redacts_debug() {
        let secret = SecretString::from("my-secret-token".to_string());
        assert_eq!(format!("{secret:?}"), "<redacted>");
        assert_eq!(secret.as_str(), "my-secret-token");
    }

    #[test]
    fn test_telegram_config_debug_redacts_token() {
        let config = TelegramConfig {
            bot_token: "super-secret".to_string().into(),
            chat_id: "-100123".to_string(),
            thread_id: None,
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("super-secret"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn test_html_escape() {
        assert_eq!(html_escape("a < b & c > d"), "a &lt; b &amp; c &gt; d");
        assert_eq!(html_escape("no special chars"), "no special chars");
        assert_eq!(
            html_escape("<script>alert(1)</script>"),
            "&lt;script&gt;alert(1)&lt;/script&gt;"
        );
    }

    #[test]
    fn test_format_liquidation_message() {
        let msg = format_liquidation_message(
            "market.near",
            "borrower.near",
            "100.00 USDC",
            "0.005 BTC",
            "+1.50 USDC (+1.5%)",
            None,
            false,
        );
        assert!(msg.contains("✅ <b>Liquidation Executed</b>"));
        assert!(msg.contains("<code>market.near</code>"));
        assert!(msg.contains("<code>borrower.near</code>"));
        assert!(msg.contains("100.00 USDC"));
        assert!(msg.contains("0.005 BTC"));
        assert!(msg.contains("+1.50 USDC (+1.5%)"));
        assert!(!msg.contains("Tx:"));
    }

    #[test]
    fn test_format_liquidation_message_dry_run() {
        let msg = format_liquidation_message(
            "market.near",
            "borrower.near",
            "100.00 USDC",
            "0.005 BTC",
            "+1.50 USDC",
            None,
            true,
        );
        assert!(msg.starts_with("🧪 DRY RUN ✅"));
    }

    #[test]
    fn test_format_liquidation_message_with_tx_hash() {
        let msg = format_liquidation_message(
            "market.near",
            "borrower.near",
            "100.00 USDC",
            "0.005 BTC",
            "+1.50 USDC",
            Some("abc123"),
            false,
        );
        assert!(msg.contains("Tx: <code>abc123</code>"));
    }

    #[test]
    fn test_format_liquidation_message_escapes_html() {
        let msg = format_liquidation_message(
            "a<b>c", "x&y", "10 USDC", "0.1 BTC", "+1 USDC", None, false,
        );
        assert!(msg.contains("a&lt;b&gt;c"));
        assert!(msg.contains("x&amp;y"));
    }

    #[test]
    fn test_format_liquidation_failed_message() {
        let msg = format_liquidation_failed_message(
            "market.near",
            "borrower.near",
            "Transaction timed out",
        );
        assert!(msg.contains("❌ <b>Liquidation Failed</b>"));
        assert!(msg.contains("<code>market.near</code>"));
        assert!(msg.contains("<code>borrower.near</code>"));
        assert!(msg.contains("Transaction timed out"));
    }

    #[test]
    fn test_format_liquidation_failed_escapes_error() {
        let msg = format_liquidation_failed_message("m", "b", "error <contains> html & stuff");
        assert!(msg.contains("error &lt;contains&gt; html &amp; stuff"));
    }

    #[test]
    fn test_format_swap_failed_message() {
        let msg = format_swap_failed_message("market.near", "BTC", "USDC", "0.005 BTC", "No route");
        assert!(msg.contains("⚠️ <b>Swap Failed</b>"));
        assert!(msg.contains("<code>BTC</code>"));
        assert!(msg.contains("<code>USDC</code>"));
        assert!(msg.contains("0.005 BTC"));
        assert!(msg.contains("No route"));
    }

    #[test]
    fn test_format_swap_unsupported_message() {
        let msg = format_swap_unsupported_message("market.near", "stNEAR", "USDC", "100 stNEAR");
        assert!(msg.contains("🚫 <b>Swap Unsupported</b>"));
        assert!(msg.contains("<code>stNEAR</code>"));
        assert!(msg.contains("<code>USDC</code>"));
        assert!(msg.contains("100 stNEAR"));
        assert!(msg.contains("not supported by swap provider"));
    }

    #[test]
    fn test_format_scan_failures_message() {
        let msg = format_scan_failures_message("market.near", 3, "Timeout exceeded after 30s");
        assert!(msg.contains("🔴 <b>Market Scan Failing</b>"));
        assert!(msg.contains("<code>market.near</code>"));
        assert!(msg.contains("Consecutive failures: 3"));
        assert!(msg.contains("Timeout exceeded after 30s"));
    }

    #[test]
    fn test_format_scan_failures_escapes_html() {
        let msg = format_scan_failures_message("m<arket", 2, "err <&> stuff");
        assert!(msg.contains("m&lt;arket"));
        assert!(msg.contains("err &lt;&amp;&gt; stuff"));
    }

    #[test]
    fn test_format_scan_recovered_message() {
        let msg = format_scan_recovered_message("market.near", 5);
        assert!(msg.contains("🟢 <b>Market Recovered</b>"));
        assert!(msg.contains("<code>market.near</code>"));
        assert!(msg.contains("After 5 consecutive failures"));
    }

    use crate::NotificationKind;

    const K1: NotificationKind = NotificationKind::ExcessiveLiquidation;
    const K2: NotificationKind = NotificationKind::OfferTooLow;

    #[test]
    fn test_failure_dedup_suppresses_repeats_same_kind() {
        let notifier = Notifier::with_cooldown(None, Duration::from_secs(60));
        assert!(notifier.should_send_failure("m", "b", K1));
        assert!(!notifier.should_send_failure("m", "b", K1));
        assert!(!notifier.should_send_failure("m", "b", K1));
    }

    #[test]
    fn test_failure_dedup_allows_different_kinds() {
        let notifier = Notifier::with_cooldown(None, Duration::from_secs(60));
        assert!(notifier.should_send_failure("m", "b", K1));
        assert!(notifier.should_send_failure("m", "b", K2));
    }

    #[test]
    fn test_failure_dedup_separates_borrowers() {
        let notifier = Notifier::with_cooldown(None, Duration::from_secs(60));
        assert!(notifier.should_send_failure("m", "b1", K1));
        assert!(notifier.should_send_failure("m", "b2", K1));
    }

    #[test]
    fn test_failure_dedup_separates_markets() {
        let notifier = Notifier::with_cooldown(None, Duration::from_secs(60));
        assert!(notifier.should_send_failure("m1", "b", K1));
        assert!(notifier.should_send_failure("m2", "b", K1));
    }

    #[test]
    fn test_failure_dedup_resets_after_cooldown() {
        let notifier = Notifier::with_cooldown(None, Duration::from_millis(10));
        assert!(notifier.should_send_failure("m", "b", K1));
        std::thread::sleep(Duration::from_millis(20));
        assert!(notifier.should_send_failure("m", "b", K1));
    }

    #[test]
    fn test_clear_failure_dedup_for_releases_borrower() {
        let notifier = Notifier::with_cooldown(None, Duration::from_secs(60));
        assert!(notifier.should_send_failure("m", "b", K1));
        assert!(notifier.should_send_failure("m", "b", K2));
        assert!(notifier.should_send_failure("m", "b2", K1));
        notifier.clear_failure_dedup_for("m", "b");
        // b can fire again for k1 and k2
        assert!(notifier.should_send_failure("m", "b", K1));
        assert!(notifier.should_send_failure("m", "b", K2));
        // b2 is unaffected
        assert!(!notifier.should_send_failure("m", "b2", K1));
    }

    #[test]
    fn test_rollback_failure_dedup_removes_entry() {
        let notifier = Notifier::with_cooldown(None, Duration::from_secs(60));
        // Record an entry, then roll it back; the next call should send again.
        assert!(notifier.should_send_failure("m", "b", K1));
        notifier.rollback_failure_dedup("m", "b", K1);
        assert!(notifier.should_send_failure("m", "b", K1));
    }

    #[test]
    fn test_spawn_send_noop_when_disabled() {
        let notifier = Arc::new(Notifier::new(None));
        // Should not panic or spawn anything
        notifier.notify_liquidation("m", "b", "1", "2", "3", None, false);
        notifier.notify_liquidation_failed("m", "b", K1, "err");
        notifier.notify_swap_failed("m", "a", "b", "1", "err");
        notifier.notify_swap_unsupported("m", "a", "b", "1");
        notifier.notify_scan_failures("m", 2, "err");
        notifier.notify_scan_recovered("m", 3);
    }

    #[tokio::test]
    async fn drain_is_noop_when_disabled() {
        let notifier = Notifier::new(None);
        // No Telegram config means nothing was ever spawned; must return
        // immediately even with a near-zero budget.
        notifier.drain(Duration::from_millis(1)).await;
    }

    #[tokio::test]
    async fn drain_completes_when_no_inflight_sends() {
        let config = TelegramConfig {
            bot_token: "123:ABC".to_string().into(),
            chat_id: "-100123".to_string(),
            thread_id: None,
        };
        let notifier = Notifier::new(Some(config));
        // All permits are free — draining must not block or time out.
        notifier.drain(Duration::from_secs(1)).await;
    }
}
