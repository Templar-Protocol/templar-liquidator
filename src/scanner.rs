//! Market position scanner module.
//!
//! Handles scanning markets for borrow positions and checking liquidation status.

use near_sdk::AccountId;
use std::collections::HashMap;
use templar_common::{
    borrow::{BorrowPosition, BorrowStatus},
    oracle::pyth::OracleResponse,
};
use templar_gateway_client::SigningClient;
use templar_gateway_methods_spec::market;
use templar_gateway_types::common::Pagination;

use crate::{rpc::RpcError, LiquidatorError, LiquidatorResult};

/// A market contract's NEP-330 version, provably parsed: the only
/// constructors are [`MarketVersion::parse`] (the one semver parser every
/// version consumer shares, so policies can't drift) and the explicit
/// [`MarketVersion::new`]. Ordering is semver-lexicographic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct MarketVersion {
    major: u32,
    minor: u32,
    patch: u32,
}

impl MarketVersion {
    /// Minimum supported contract version; older markets are skipped at
    /// registration.
    pub const MIN_SUPPORTED: Self = Self::new(1, 0, 0);

    /// Minimum version that supports partial liquidation; older markets
    /// require liquidating the full position.
    pub const MIN_PARTIAL_LIQUIDATION: Self = Self::new(1, 1, 0);

    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parses a `major.minor.patch` NEP-330 version string. Exactly three
    /// numeric parts; anything else is `None`.
    #[must_use]
    pub fn parse(version: &str) -> Option<Self> {
        let mut parts = version.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self::new(major, minor, patch))
    }

    /// Whether this version supports partial liquidation
    /// (>= [`Self::MIN_PARTIAL_LIQUIDATION`]).
    #[must_use]
    pub fn supports_partial_liquidation(self) -> bool {
        self >= Self::MIN_PARTIAL_LIQUIDATION
    }
}

impl std::fmt::Display for MarketVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Whether a market's contract version supports partial liquidation. `None`
/// — an unknown version — is treated as the oldest supported, i.e. full
/// liquidation required: assuming partial support would submit requests a
/// pre-1.1 market rejects on-chain.
pub fn supports_partial_liquidation(version: Option<MarketVersion>) -> bool {
    version.is_some_and(MarketVersion::supports_partial_liquidation)
}

/// Type alias for borrow positions map
pub type BorrowPositions = HashMap<AccountId, BorrowPosition>;

/// Market position scanner.
///
/// Responsible for:
/// - Fetching all borrow positions from a market
/// - Checking liquidation status of positions
/// - Pagination handling for large markets
pub struct MarketScanner {
    client: SigningClient,
    market: AccountId,
}

impl MarketScanner {
    /// Creates a new market scanner.
    pub fn new(client: SigningClient, market: AccountId) -> Self {
        Self { client, market }
    }

    /// Fetches borrow status for an account.
    #[tracing::instrument(skip(self, oracle_response), level = "debug")]
    pub async fn get_borrow_status(
        &self,
        account_id: &AccountId,
        oracle_response: &OracleResponse,
    ) -> Result<Option<BorrowStatus>, RpcError> {
        let result = self
            .client
            .read(market::GetBorrowStatus::new(
                self.market.clone(),
                account_id.clone(),
                oracle_response.clone(),
            ))
            .await
            .map_err(RpcError::from)?;
        Ok(result.status)
    }

    /// Fetches a single borrow position from the market.
    #[tracing::instrument(skip(self), level = "debug")]
    pub async fn get_borrow_position(
        &self,
        account_id: &AccountId,
    ) -> Result<Option<BorrowPosition>, RpcError> {
        let result = self
            .client
            .read(market::GetBorrowPosition::new(
                self.market.clone(),
                account_id.clone(),
            ))
            .await
            .map_err(RpcError::from)?;
        Ok(result.position)
    }

    /// Fetches all borrow positions from the market with pagination.
    #[tracing::instrument(skip(self), level = "debug")]
    pub async fn get_all_borrows(&self) -> LiquidatorResult<BorrowPositions> {
        let mut all_positions: BorrowPositions = HashMap::new();
        let page_size: u32 = 500;
        let mut current_offset: u32 = 0;

        loop {
            let page = self
                .client
                .read(
                    market::ListBorrowPositions::new(self.market.clone()).with_args(Pagination {
                        offset: Some(current_offset),
                        limit: Some(page_size),
                    }),
                )
                .await
                .map_err(|e| LiquidatorError::ListBorrowPositionsError(e.into()))?
                .positions;

            let fetched = u32::try_from(page.len()).unwrap_or(u32::MAX);
            if fetched == 0 {
                break;
            }

            tracing::debug!(
                market = %self.market,
                offset = current_offset,
                fetched = fetched,
                "Fetched borrow positions page"
            );

            all_positions.extend(page);
            current_offset += fetched;

            if fetched < page_size {
                break;
            }
        }

        tracing::info!(
            market = %self.market,
            total_positions = all_positions.len(),
            "Fetched borrow positions"
        );

        Ok(all_positions)
    }

    /// Checks if a position is liquidatable.
    ///
    /// Returns `Some(reason)` if the position is liquidatable with the liquidation reason,
    /// or `None` if the position is not liquidatable.
    ///
    /// # Errors
    ///
    /// Returns an error if the borrow status cannot be fetched
    pub async fn is_liquidatable(
        &self,
        account_id: &AccountId,
        oracle_response: &OracleResponse,
    ) -> LiquidatorResult<Option<String>> {
        let status = self
            .get_borrow_status(account_id, oracle_response)
            .await
            .map_err(LiquidatorError::FetchBorrowStatus)?;

        match status {
            Some(BorrowStatus::Liquidation(reason)) => Ok(Some(format!("{reason:?}"))),
            Some(_) | None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_only_three_numeric_parts() {
        assert_eq!(
            MarketVersion::parse("1.2.3"),
            Some(MarketVersion::new(1, 2, 3))
        );
        assert_eq!(
            MarketVersion::parse("0.1.0"),
            Some(MarketVersion::new(0, 1, 0))
        );
        assert_eq!(MarketVersion::parse("1.2"), None);
        assert_eq!(MarketVersion::parse("1.2.3.4"), None);
        assert_eq!(MarketVersion::parse("1.x.3"), None);
        assert_eq!(MarketVersion::parse(""), None);
    }

    /// Ordering is semver-lexicographic on (major, minor, patch) — what the
    /// registry gate and the partial-liquidation gate both compare with.
    #[test]
    fn version_ordering_is_semver() {
        assert!(MarketVersion::new(1, 0, 5) < MarketVersion::new(1, 1, 0));
        assert!(MarketVersion::new(2, 0, 0) > MarketVersion::new(1, 9, 9));
        assert!(MarketVersion::new(1, 0, 0) >= MarketVersion::MIN_SUPPORTED);
        assert!(MarketVersion::new(0, 9, 9) < MarketVersion::MIN_SUPPORTED);
    }

    /// Partial liquidation requires >= MIN_PARTIAL_LIQUIDATION. An
    /// equality check against 1.0.0 would silently route any *other*
    /// pre-partial version — 1.0.5, or an unknown version — down the
    /// partial path a market that old rejects on-chain.
    #[test]
    fn partial_support_gates_on_the_minimum_version_not_equality() {
        assert!(!supports_partial_liquidation(None));
        assert!(!supports_partial_liquidation(Some(MarketVersion::new(
            1, 0, 0
        ))));
        assert!(!supports_partial_liquidation(Some(MarketVersion::new(
            1, 0, 5
        ))));
        assert!(supports_partial_liquidation(Some(MarketVersion::new(
            1, 1, 0
        ))));
        assert!(supports_partial_liquidation(Some(MarketVersion::new(
            2, 0, 0
        ))));
    }
}
