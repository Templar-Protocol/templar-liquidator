//! Swap error classification and retry logic.
//!
//! Provides:
//! - `SwapErrorKind`, phase-split at the deposit transfer: `PreDeposit`
//!   kinds (idempotent phases; transient ones retry) and `PostDeposit`
//!   kinds (`Indeterminate` / `Definitive`; never retried — structurally,
//!   by phase match, not by variant list)
//! - `SwapError` wrapper with context
//! - `SwapRetryConfig` for configurable retry behavior
//! - `swap_with_retry` for automatic retry of transient failures

use std::time::Duration;

use tokio::time::sleep;

use crate::rpc::AppError;

/// Failures from phases **before the swap's deposit transfer is submitted**
/// (quote, deposit-address validation, deposit-account funding, storage
/// registration). These phases are idempotent — re-running them cannot
/// double-spend inventory, even though account funding and storage
/// registration bond small fixed amounts of NEAR — so whether to retry is a
/// question of *worth* (`is_retryable`), never of safety.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PreDepositError {
    /// Amount below bridge/swap minimum (not retryable, batchable)
    #[error("Amount too low: {message}")]
    AmountTooLow { message: String },

    /// Quote failure — treated as a permanent "no route for this asset
    /// pair" condition, not retried. See `is_retryable()`.
    #[error("Quote failed: {message}")]
    QuoteFailed { message: String },

    /// Network/connection error (retryable)
    #[error("Network error: {message}")]
    NetworkError { message: String },

    /// Server error 5xx (retryable)
    #[error("Server error ({status}): {message}")]
    ServerError { status: u16, message: String },

    /// Rate limited 429 (retryable)
    #[error("Rate limited")]
    RateLimited,

    /// Client validation error 400 (not retryable)
    #[error("Validation error: {message}")]
    ValidationError { message: String },

    /// Timed out (retryable — pre-deposit phases are idempotent). There is
    /// deliberately no timeout variant on [`PostDepositError`]: a timeout at
    /// or after the deposit transfer is an unknown outcome and must be
    /// `Indeterminate` — retrying a swap whose deposit already landed
    /// double-spends.
    #[error("Timeout: {message}")]
    Timeout { message: String },

    /// Unknown / uncategorized error (not retryable)
    #[error("Unknown error: {message}")]
    Unknown { message: String },
}

impl PreDepositError {
    /// Returns true if this error is worth retrying.
    ///
    /// `QuoteFailed` is not retried — "Failed to get quote" from the 1-Click
    /// API means no swap route exists for the asset pair, which is a
    /// permanent condition, not transient. Transient API failures are
    /// captured by `NetworkError` and `ServerError` instead.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::NetworkError { .. }
                | Self::ServerError { .. }
                | Self::RateLimited
                | Self::Timeout { .. }
        )
    }
}

/// Failures **at or after the deposit transfer** — inventory has left, or
/// may have left, the account. Never retryable: re-running the swap would
/// deposit again. Reconciliation is the next inventory refresh: balances are
/// re-read from chain, so a settlement or refund is reflected before
/// anything sizes a new swap.
#[derive(Debug, Clone, thiserror::Error)]
pub enum PostDepositError {
    /// The outcome is unknown and funds may already have moved (deposit RPC
    /// error, notify failure, status polling timed out). The case worth
    /// waking an operator for: nothing yet accounts for the deposit.
    #[error("Swap outcome unknown (deposit address {deposit_address}): {message}")]
    Indeterminate {
        message: String,
        /// Where the funds were sent — the 1-Click deposit address, or a
        /// fork provider's equivalent reconciliation handle. This is the
        /// datum an operator (or a future reconciliation job) keys on, so
        /// it is a field, not prose inside `message`.
        deposit_address: String,
    },

    /// The outcome is known and final: the venue reported a terminal
    /// non-success — the deposit transfer reverted on-chain (funds never
    /// left), the deposit was refunded, or the swap ended in a terminal
    /// failed status. Nothing is left in flight to reconcile; the message
    /// states what happened to the funds, and the next inventory refresh
    /// reflects the final balances.
    #[error("Swap failed definitively (deposit address {deposit_address}): {message}")]
    Definitive {
        message: String,
        /// Same reconciliation handle as
        /// [`Indeterminate`](Self::Indeterminate) — kept even though the
        /// outcome is known, so an operator auditing the venue's side has
        /// the address without parsing prose.
        deposit_address: String,
    },
}

/// Classification of swap errors, split by the one boundary that decides
/// retry safety: the deposit transfer. The phase is part of the type so a
/// retryable-looking kind cannot exist on the post-deposit side at all —
/// the invariant "never retry after funds may have moved" is structural,
/// not a doc obligation on each construction site.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SwapErrorKind {
    /// Before the deposit transfer: idempotent phases, retry is safe.
    #[error(transparent)]
    PreDeposit(#[from] PreDepositError),

    /// At or after the deposit transfer: never retried.
    #[error(transparent)]
    PostDeposit(#[from] PostDepositError),
}

impl SwapErrorKind {
    /// Returns true if this error should be retried. Post-deposit failures
    /// are non-retryable by phase, not by variant list — adding a variant to
    /// [`PostDepositError`] cannot make it retryable.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::PreDeposit(pre) => pre.is_retryable(),
            Self::PostDeposit(_) => false,
        }
    }

    /// Returns true if the amount was too small for the swap provider.
    pub fn is_amount_too_low(&self) -> bool {
        matches!(self, Self::PreDeposit(PreDepositError::AmountTooLow { .. }))
    }

    /// Classify an HTTP response from a **pre-deposit** 1-Click API call
    /// (quoting). Returns [`PreDepositError`] by type: an HTTP failure from
    /// a post-deposit call (deposit notification) has already moved funds
    /// and must classify as [`PostDepositError`] at its own site.
    pub fn from_oneclick_response(status: u16, body: &str) -> PreDepositError {
        if body.contains("Amount is too low for bridge") {
            return PreDepositError::AmountTooLow {
                message: body.to_string(),
            };
        }

        if body.contains("Failed to get quote") {
            return PreDepositError::QuoteFailed {
                message: body.to_string(),
            };
        }

        match status {
            429 => PreDepositError::RateLimited,
            400..=499 => PreDepositError::ValidationError {
                message: body.to_string(),
            },
            500..=599 => PreDepositError::ServerError {
                status,
                message: body.to_string(),
            },
            _ => PreDepositError::Unknown {
                message: body.to_string(),
            },
        }
    }
}

/// Swap error with classification and context.
#[derive(Debug, thiserror::Error)]
#[error("{context}: {kind}")]
pub struct SwapError {
    /// Error classification
    pub kind: SwapErrorKind,
    /// Human-readable context (e.g. "Quote request", "Deposit")
    pub context: String,
}

impl SwapError {
    pub fn new(kind: SwapErrorKind, context: impl Into<String>) -> Self {
        Self {
            kind,
            context: context.into(),
        }
    }

    /// A failure from an idempotent phase before the deposit transfer.
    pub fn pre(kind: PreDepositError, context: impl Into<String>) -> Self {
        Self::new(SwapErrorKind::PreDeposit(kind), context)
    }

    /// A failure at or after the deposit transfer — never retried.
    pub fn post(kind: PostDepositError, context: impl Into<String>) -> Self {
        Self::new(SwapErrorKind::PostDeposit(kind), context)
    }

    pub fn is_retryable(&self) -> bool {
        self.kind.is_retryable()
    }

    pub fn is_amount_too_low(&self) -> bool {
        self.kind.is_amount_too_low()
    }

    /// Classifies an [`AppError`] from a phase **before the swap's deposit
    /// transfer is submitted** (quotes, storage registration — idempotent
    /// operations whose retry cannot double-spend inventory). The phase is
    /// in the return path's type: this can only build [`PreDepositError`]
    /// kinds, so it structurally cannot classify a post-deposit failure as
    /// retryable.
    pub(crate) fn from_pre_deposit_app_error(context: &str, error: &AppError) -> Self {
        let kind = match error {
            AppError::Rpc(crate::rpc::RpcError::TimeoutError(_)) => PreDepositError::Timeout {
                message: error.to_string(),
            },
            AppError::Rpc(_) => PreDepositError::NetworkError {
                message: error.to_string(),
            },
            AppError::ValidationError(m) => PreDepositError::ValidationError { message: m.clone() },
            AppError::SerializationError(m) => PreDepositError::Unknown { message: m.clone() },
        };
        Self::pre(kind, context)
    }
}

/// Convert `SwapError` into `AppError` so it can flow through existing error paths.
impl From<SwapError> for AppError {
    fn from(err: SwapError) -> Self {
        AppError::ValidationError(err.to_string())
    }
}

/// Configuration for swap retry behaviour.
#[derive(Debug, Clone)]
pub struct SwapRetryConfig {
    /// Maximum number of attempts (including first try)
    pub max_attempts: u32,
    /// Base delay in milliseconds (doubles each attempt: 2s, 4s, 8s …)
    pub base_delay_ms: u64,
}

impl Default for SwapRetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay_ms: 2000,
        }
    }
}

/// Upper bound on a single retry delay. Doubling backoff crosses any
/// realistic threshold within a handful of attempts; without a cap, a large
/// configured base and attempt count saturate to u64::MAX milliseconds —
/// which doesn't panic, it parks the retry loop for centuries.
const MAX_BACKOFF_DELAY: Duration = Duration::from_secs(300);

impl SwapRetryConfig {
    /// Calculate delay for a given attempt (1-indexed): 1×, 2×, 4×, … the
    /// base delay, capped at [`MAX_BACKOFF_DELAY`]. Saturating arithmetic —
    /// the shift is undefined at 64 bits and the multiplication can wrap,
    /// either of which would panic mid-retry under a large configured
    /// attempt count.
    fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let multiplier = 1u64
            .checked_shl(attempt.saturating_sub(1))
            .unwrap_or(u64::MAX);
        Duration::from_millis(self.base_delay_ms.saturating_mul(multiplier)).min(MAX_BACKOFF_DELAY)
    }
}

/// Execute an async swap operation with retry logic for transient errors.
///
/// Only errors where `SwapError::is_retryable()` returns true are retried.
/// Non-retryable errors — amount-too-low, validation, and above all
/// [`PostDepositError`] (funds may have moved, or moved and failed) — are returned
/// immediately.
///
/// # Errors
///
/// Returns the last `SwapError` if all retries are exhausted or a
/// non-retryable error is encountered.
pub async fn swap_with_retry<F, Fut>(
    config: &SwapRetryConfig,
    swap_name: &str,
    mut operation: F,
) -> Result<(), SwapError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), SwapError>>,
{
    let mut last_error: Option<SwapError> = None;

    for attempt in 1..=config.max_attempts {
        match operation().await {
            Ok(()) => return Ok(()),
            Err(e) if e.is_retryable() && attempt < config.max_attempts => {
                let delay = config.delay_for_attempt(attempt);
                tracing::debug!(
                    swap = %swap_name,
                    attempt,
                    max_attempts = config.max_attempts,
                    delay_ms = delay.as_millis(),
                    error = %e,
                    "Swap failed with retryable error, retrying"
                );
                sleep(delay).await;
                last_error = Some(e);
            }
            Err(e) => return Err(e),
        }
    }

    // Should not normally reach here, but be safe
    Err(last_error.unwrap_or_else(|| {
        SwapError::pre(
            PreDepositError::Unknown {
                message: "Retry loop exhausted".into(),
            },
            swap_name.to_string(),
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retryable_classification() {
        // QuoteFailed is not retryable — permanent "no route" condition
        assert!(!PreDepositError::QuoteFailed {
            message: String::new()
        }
        .is_retryable());
        assert!(PreDepositError::NetworkError {
            message: String::new()
        }
        .is_retryable());
        assert!(PreDepositError::ServerError {
            status: 500,
            message: String::new()
        }
        .is_retryable());
        assert!(PreDepositError::RateLimited.is_retryable());
        assert!(PreDepositError::Timeout {
            message: String::new()
        }
        .is_retryable());

        // Not retryable
        assert!(!PreDepositError::AmountTooLow {
            message: String::new()
        }
        .is_retryable());
        assert!(!PreDepositError::ValidationError {
            message: String::new()
        }
        .is_retryable());
        assert!(!PreDepositError::Unknown {
            message: String::new()
        }
        .is_retryable());
        // The wrapper delegates: a retryable pre-deposit kind stays
        // retryable through SwapErrorKind.
        assert!(SwapErrorKind::PreDeposit(PreDepositError::RateLimited).is_retryable());
    }

    #[test]
    fn test_amount_too_low_classification() {
        let kind = SwapErrorKind::from_oneclick_response(
            400,
            r#"{"message":"Amount is too low for bridge, try at least 10000"}"#,
        );
        assert!(matches!(kind, PreDepositError::AmountTooLow { .. }));
        assert!(SwapErrorKind::from(kind.clone()).is_amount_too_low());
        assert!(!kind.is_retryable());
    }

    #[test]
    fn test_quote_failed_classification() {
        let kind =
            SwapErrorKind::from_oneclick_response(400, r#"{"message":"Failed to get quote"}"#);
        // QuoteFailed is not retryable — "no route" is a permanent condition
        assert!(!kind.is_retryable());
        assert!(matches!(kind, PreDepositError::QuoteFailed { .. }));
    }

    #[test]
    fn test_server_error_classification() {
        let kind = SwapErrorKind::from_oneclick_response(500, "Internal Server Error");
        assert!(kind.is_retryable());
    }

    #[test]
    fn test_rate_limit_classification() {
        let kind = SwapErrorKind::from_oneclick_response(429, "Too Many Requests");
        assert!(kind.is_retryable());
        assert!(matches!(kind, PreDepositError::RateLimited));
    }

    /// An indeterminate error means funds may already have moved (the deposit
    /// was submitted before the failure). Re-running the operation would
    /// deposit again — a double-spend — so the retry wrapper must return it
    /// without a second attempt, no matter how many attempts remain.
    #[tokio::test]
    async fn indeterminate_outcome_is_never_retried() {
        let config = SwapRetryConfig {
            max_attempts: 3,
            base_delay_ms: 1,
        };
        let calls = std::sync::atomic::AtomicU32::new(0);

        let result = swap_with_retry(&config, "test", || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {
                Err(SwapError::post(
                    PostDepositError::Indeterminate {
                        message: "poll timed out after deposit".into(),
                        deposit_address: "deposit.near".into(),
                    },
                    "1-Click swap",
                ))
            }
        })
        .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let err = result.expect_err("indeterminate must surface as an error");
        assert!(matches!(
            err.kind,
            SwapErrorKind::PostDeposit(PostDepositError::Indeterminate { .. })
        ));
    }

    /// Transient errors before any funds move stay retryable: the wrapper
    /// re-runs the operation and returns the eventual success.
    #[tokio::test]
    async fn transient_error_is_retried_to_success() {
        let config = SwapRetryConfig {
            max_attempts: 3,
            base_delay_ms: 1,
        };
        let calls = std::sync::atomic::AtomicU32::new(0);

        let result = swap_with_retry(&config, "test", || {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(SwapError::new(
                        SwapErrorKind::PreDeposit(PreDepositError::NetworkError {
                            message: "connection reset".into(),
                        }),
                        "Quote request",
                    ))
                } else {
                    Ok(())
                }
            }
        })
        .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(result.is_ok());
    }

    /// Post-deposit failures are structurally non-retryable: the check is a
    /// phase match, not a per-variant list, so no variant added to
    /// [`PostDepositError`] can ever become retryable by omission.
    #[test]
    fn post_deposit_errors_are_never_retryable() {
        let post = [
            PostDepositError::Indeterminate {
                message: String::new(),
                deposit_address: String::new(),
            },
            PostDepositError::Definitive {
                message: String::new(),
                deposit_address: String::new(),
            },
        ];
        for kind in post {
            assert!(
                !SwapErrorKind::PostDeposit(kind).is_retryable(),
                "post-deposit failures must never be retryable"
            );
        }
    }

    /// A definitive failure (refund landed, on-chain revert, terminal failed
    /// status) is final: the retry wrapper must return it without a second
    /// attempt, exactly like an indeterminate one.
    #[tokio::test]
    async fn definitive_outcome_is_never_retried() {
        let config = SwapRetryConfig {
            max_attempts: 3,
            base_delay_ms: 1,
        };
        let calls = std::sync::atomic::AtomicU32::new(0);

        let result = swap_with_retry(&config, "test", || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {
                Err(SwapError::post(
                    PostDepositError::Definitive {
                        message: "deposit was refunded by 1-Click".into(),
                        deposit_address: "deposit.near".into(),
                    },
                    "Deposit",
                ))
            }
        })
        .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let err = result.expect_err("definitive must surface as an error");
        assert!(matches!(
            err.kind,
            SwapErrorKind::PostDeposit(PostDepositError::Definitive { .. })
        ));
    }

    /// The reconciliation handle must survive into the rendered message for
    /// both post-deposit variants — it is what an operator keys on.
    #[test]
    fn post_deposit_display_names_the_deposit_address() {
        let indeterminate = SwapErrorKind::PostDeposit(PostDepositError::Indeterminate {
            message: "poll timed out".into(),
            deposit_address: "abc123.near".into(),
        });
        assert!(indeterminate.to_string().contains("abc123.near"));
        let definitive = SwapErrorKind::PostDeposit(PostDepositError::Definitive {
            message: "refunded".into(),
            deposit_address: "abc123.near".into(),
        });
        assert!(definitive.to_string().contains("abc123.near"));
    }

    /// A persistently retryable error is attempted exactly `max_attempts`
    /// times, then the last error surfaces.
    #[tokio::test]
    async fn retryable_error_stops_at_max_attempts() {
        let config = SwapRetryConfig {
            max_attempts: 3,
            base_delay_ms: 1,
        };
        let calls = std::sync::atomic::AtomicU32::new(0);

        let result = swap_with_retry(&config, "test", || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async {
                Err(SwapError::pre(
                    PreDepositError::NetworkError {
                        message: "connection reset".into(),
                    },
                    "Quote request",
                ))
            }
        })
        .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        let err = result.expect_err("exhausted retries must surface an error");
        assert!(matches!(
            err.kind,
            SwapErrorKind::PreDeposit(PreDepositError::NetworkError { .. })
        ));
    }

    /// The pre-deposit classifier is phase-typed: it can only produce
    /// `PreDeposit` kinds (post-deposit variants aren't reachable from its
    /// implementation), so the old "never produces Indeterminate" test is a
    /// compile-time fact. What remains testable is the mapping itself.
    #[test]
    fn pre_deposit_classifier_maps_app_errors_by_transience() {
        let classified = SwapError::from_pre_deposit_app_error(
            "Storage deposit",
            &AppError::Rpc(crate::rpc::RpcError::TimeoutError(
                "timed out after 30s".into(),
            )),
        );
        assert!(matches!(
            classified.kind,
            SwapErrorKind::PreDeposit(PreDepositError::Timeout { .. })
        ));
        let classified = SwapError::from_pre_deposit_app_error(
            "Storage deposit",
            &AppError::Rpc(crate::rpc::RpcError::WrongResponseKind("x".into())),
        );
        assert!(matches!(
            classified.kind,
            SwapErrorKind::PreDeposit(PreDepositError::NetworkError { .. })
        ));
        let classified = SwapError::from_pre_deposit_app_error(
            "Storage deposit",
            &AppError::SerializationError("x".into()),
        );
        assert!(matches!(
            classified.kind,
            SwapErrorKind::PreDeposit(PreDepositError::Unknown { .. })
        ));
    }

    /// A large configured attempt count must not overflow the shift or the
    /// multiplication, and the resulting delay must be capped — a saturated
    /// u64::MAX milliseconds would park the retry loop for centuries, which
    /// is a hang with extra steps.
    #[test]
    fn backoff_delay_saturates_and_is_capped() {
        let config = SwapRetryConfig {
            max_attempts: 200,
            base_delay_ms: u64::MAX / 2,
        };
        // Shift alone overflows at attempt 65; the multiplication overflows
        // far earlier with a large base. No panic, and never above the cap.
        assert!(config.delay_for_attempt(200) <= MAX_BACKOFF_DELAY);
        assert!(config.delay_for_attempt(1) <= MAX_BACKOFF_DELAY);
        // Sane configs are unaffected by the cap.
        let sane = SwapRetryConfig {
            max_attempts: 3,
            base_delay_ms: 2000,
        };
        assert_eq!(sane.delay_for_attempt(3), Duration::from_millis(8000));
    }

    #[test]
    fn test_retry_config_delay() {
        let config = SwapRetryConfig {
            max_attempts: 3,
            base_delay_ms: 2000,
        };
        assert_eq!(config.delay_for_attempt(1), Duration::from_millis(2000));
        assert_eq!(config.delay_for_attempt(2), Duration::from_millis(4000));
        assert_eq!(config.delay_for_attempt(3), Duration::from_millis(8000));
    }
}
