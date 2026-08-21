//! Swap error classification and retry logic.
//!
//! Provides:
//! - `SwapErrorKind` for classifying swap failures as retryable or permanent
//! - `SwapError` wrapper with context
//! - `SwapRetryConfig` for configurable retry behavior
//! - `swap_with_retry` for automatic retry of transient failures

use std::time::Duration;

use tokio::time::sleep;

use crate::rpc::AppError;

/// Classification of swap errors for retry decisions.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SwapErrorKind {
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

    /// Timed out in a phase where no funds have moved (retryable). A
    /// timeout *after* funds may have moved must be `Indeterminate` instead
    /// — retrying a whole swap whose deposit already landed double-spends.
    #[error("Timeout: {message}")]
    Timeout { message: String },

    /// The outcome is unknown and funds may already have moved: the failure
    /// happened at or after the deposit transaction was submitted (deposit
    /// RPC error, notify failure, status polling timed out). Never retried —
    /// re-running the operation would deposit again. Reconciliation is the
    /// next inventory refresh: balances are re-read from chain, so a late
    /// settlement or refund is reflected before anything sizes a new swap.
    #[error("Swap outcome unknown: {message}")]
    Indeterminate { message: String },

    /// Unknown / uncategorized error (not retryable)
    #[error("Unknown error: {message}")]
    Unknown { message: String },
}

impl SwapErrorKind {
    /// Returns true if this error type should be retried.
    ///
    /// `QuoteFailed` is not retried — "Failed to get quote" from the 1-Click API means no
    /// swap route exists for the asset pair, which is a permanent condition, not transient.
    /// Transient API failures are captured by `NetworkError` and `ServerError` instead.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::NetworkError { .. }
                | Self::ServerError { .. }
                | Self::RateLimited
                | Self::Timeout { .. }
        )
    }

    /// Returns true if the amount was too small for the swap provider.
    pub fn is_amount_too_low(&self) -> bool {
        matches!(self, Self::AmountTooLow { .. })
    }

    /// Classify an HTTP response from the 1-Click API.
    pub fn from_oneclick_response(status: u16, body: &str) -> Self {
        if body.contains("Amount is too low for bridge") {
            return Self::AmountTooLow {
                message: body.to_string(),
            };
        }

        if body.contains("Failed to get quote") {
            return Self::QuoteFailed {
                message: body.to_string(),
            };
        }

        match status {
            429 => Self::RateLimited,
            400..=499 => Self::ValidationError {
                message: body.to_string(),
            },
            500..=599 => Self::ServerError {
                status,
                message: body.to_string(),
            },
            _ => Self::Unknown {
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

    pub fn is_retryable(&self) -> bool {
        self.kind.is_retryable()
    }

    pub fn is_amount_too_low(&self) -> bool {
        self.kind.is_amount_too_low()
    }

    /// Classifies an [`AppError`] from a phase where **no funds have moved**
    /// (quotes, storage registration). Never produces `Indeterminate` — a
    /// funds-moving phase must classify its own errors instead of using this.
    pub fn from_app_error(context: &str, error: &AppError) -> Self {
        let kind = match error {
            AppError::Rpc(crate::rpc::RpcError::TimeoutError(..)) => SwapErrorKind::Timeout {
                message: error.to_string(),
            },
            AppError::Rpc(_) => SwapErrorKind::NetworkError {
                message: error.to_string(),
            },
            AppError::ValidationError(m) => SwapErrorKind::ValidationError { message: m.clone() },
            AppError::SerializationError(m) => SwapErrorKind::Unknown { message: m.clone() },
        };
        Self::new(kind, context)
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

impl SwapRetryConfig {
    /// Calculate delay for a given attempt (1-indexed).
    fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let multiplier = 1u64 << attempt.saturating_sub(1); // 1, 2, 4, …
        Duration::from_millis(self.base_delay_ms * multiplier)
    }
}

/// Execute an async swap operation with retry logic for transient errors.
///
/// Only errors where `SwapError::is_retryable()` returns true are retried.
/// Non-retryable errors — amount-too-low, validation, and above all
/// [`SwapErrorKind::Indeterminate`] (funds may have moved) — are returned
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
        SwapError::new(
            SwapErrorKind::Unknown {
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
        assert!(!SwapErrorKind::QuoteFailed {
            message: String::new()
        }
        .is_retryable());
        assert!(SwapErrorKind::NetworkError {
            message: String::new()
        }
        .is_retryable());
        assert!(SwapErrorKind::ServerError {
            status: 500,
            message: String::new()
        }
        .is_retryable());
        assert!(SwapErrorKind::RateLimited.is_retryable());
        assert!(SwapErrorKind::Timeout {
            message: String::new()
        }
        .is_retryable());

        // Not retryable
        assert!(!SwapErrorKind::AmountTooLow {
            message: String::new()
        }
        .is_retryable());
        assert!(!SwapErrorKind::ValidationError {
            message: String::new()
        }
        .is_retryable());
        assert!(!SwapErrorKind::Unknown {
            message: String::new()
        }
        .is_retryable());
    }

    #[test]
    fn test_amount_too_low_classification() {
        let kind = SwapErrorKind::from_oneclick_response(
            400,
            r#"{"message":"Amount is too low for bridge, try at least 10000"}"#,
        );
        assert!(kind.is_amount_too_low());
        assert!(!kind.is_retryable());
    }

    #[test]
    fn test_quote_failed_classification() {
        let kind =
            SwapErrorKind::from_oneclick_response(400, r#"{"message":"Failed to get quote"}"#);
        // QuoteFailed is not retryable — "no route" is a permanent condition
        assert!(!kind.is_retryable());
        assert!(!kind.is_amount_too_low());
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
        assert!(matches!(kind, SwapErrorKind::RateLimited));
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
                Err(SwapError::new(
                    SwapErrorKind::Indeterminate {
                        message: "poll timed out after deposit".into(),
                    },
                    "1-Click swap",
                ))
            }
        })
        .await;

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let err = result.expect_err("indeterminate must surface as an error");
        assert!(matches!(err.kind, SwapErrorKind::Indeterminate { .. }));
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
                        SwapErrorKind::NetworkError {
                            message: "connection reset".into(),
                        },
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

    #[test]
    fn indeterminate_is_not_retryable() {
        assert!(!SwapErrorKind::Indeterminate {
            message: String::new()
        }
        .is_retryable());
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
