//! Swap provider implementations for liquidation operations.
//!
//! This module provides a flexible, extensible architecture for integrating
//! different swap/exchange protocols (Ref Finance, 1-Click API, etc.) used
//! during liquidation operations.
//!
//! # Architecture
//!
//! The module follows the Strategy pattern to allow runtime selection of swap
//! providers while maintaining a consistent interface. This enables:
//! - Easy addition of new swap providers without modifying existing code
//! - Testability through mock implementations
//! - Type-safe asset handling across different token standards (NEP-141, NEP-245)
//!
//! # Example
//!
//! ```ignore
//! use templar_liquidator::swap::{SwapProvider, RefSwap};
//! use templar_gateway_client::SigningClient;
//!
//! # async fn example(client: SigningClient) -> Result<(), Box<dyn std::error::Error>> {
//! let swap_provider = RefSwap::new(
//!     "v2.ref-finance.near".parse()?,
//!     client,
//! );
//!
//! // Get quote
//! let quote = swap_provider.quote(&from_asset, &to_asset, output_amount).await?;
//!
//! // Execute swap
//! let result = swap_provider.swap(&from_asset, &to_asset, quote).await?;
//! # Ok(())
//! # }
//! ```

pub mod oneclick;
pub mod provider;
pub mod r#ref;
pub mod retry;

// Re-export for convenience
pub use oneclick::OneClickSwap;
pub use provider::SwapProviderImpl;
pub use r#ref::RefSwap;
pub use retry::{SwapError, SwapErrorKind, SwapRetryConfig};

use near_sdk::AccountId;
use templar_common::asset::{AssetClass, FungibleAsset, FungibleAssetAmount};

use crate::rpc::AppResult;

/// Core trait for swap provider implementations.
///
/// This trait defines the interface that all swap providers must implement,
/// enabling polymorphic usage of different DEX protocols. It is the extension
/// seam for adding a venue beyond the two shipped implementations
/// ([`OneClickSwap`], [`RefSwap`]).
///
/// # Type Safety
///
/// The trait uses generic `AssetClass` bounds to ensure compile-time type safety
/// when working with different asset types (collateral vs borrow assets).
///
/// # Object Safety
///
/// Because [`quote`](Self::quote), [`swap`](Self::swap), [`supports_assets`](Self::supports_assets)
/// and [`ensure_storage_registration`](Self::ensure_storage_registration) are all
/// generic over `F`/`T`, this trait itself is **not** object-safe — it cannot be
/// used as `Box<dyn SwapProvider>` or `Arc<dyn SwapProvider>`. Callers that need
/// dynamic dispatch go through the concrete [`SwapProviderImpl`] enum instead,
/// which forwards to whichever provider it wraps.
#[async_trait::async_trait]
pub trait SwapProvider: Send + Sync {
    /// Quotes the `from_asset` amount required to receive `output_amount` of
    /// `to_asset` — an **exact-output** quote.
    ///
    /// This is the single easiest thing to misread in this trait: the caller
    /// supplies the amount it wants to *end up with* (in the target asset `T`),
    /// and gets back how much of the *source* asset `F` that will cost. It is
    /// the inverse of [`swap`](Self::swap), whose `amount` parameter is the
    /// amount to spend, not to receive.
    ///
    /// A conforming implementation must fold in slippage and fees so the
    /// returned amount is directly usable as the `amount` to pass to
    /// [`swap`](Self::swap) — the caller should not need to add its own margin
    /// on top. Neither shipped provider currently implements a working quote:
    /// [`OneClickSwap::quote`] and [`RefSwap::quote`] both return `Err`
    /// unconditionally (1-Click's API only prices exact-input swaps; Ref's
    /// implementation was left as "call `swap()` directly" instead). Nothing in
    /// the liquidation pipeline calls `quote()` today — position sizing goes
    /// through [`crate::liquidation_strategy`], which sizes off oracle prices,
    /// not a swap quote. A new provider that wants this method exercised needs
    /// its own caller as well as its own implementation.
    ///
    /// # Errors
    ///
    /// Returns `AppError` if:
    /// - The asset pair is not supported
    /// - The liquidity is insufficient
    /// - The RPC call fails
    async fn quote<F: AssetClass, T: AssetClass>(
        &self,
        from_asset: &FungibleAsset<F>,
        to_asset: &FungibleAsset<T>,
        output_amount: FungibleAssetAmount<T>,
    ) -> AppResult<FungibleAssetAmount<F>>;

    /// Swaps exactly `amount` of `from_asset` for `to_asset`, blocking until the
    /// outcome is known.
    ///
    /// Unlike [`quote`](Self::quote), `amount` here is the amount to *spend*
    /// (an exact-input swap): the caller does not know or control how much
    /// `to_asset` it will receive.
    ///
    /// A conforming implementation must not return `Ok(())` on the strength of
    /// a NEAR transaction merely reaching the chain — on NEAR a top-level
    /// transaction can report success while an inner receipt (the actual swap
    /// logic, or a callback it depends on) fails and the tokens are refunded.
    /// Both shipped providers guard against this at the cost of `swap()`
    /// blocking for the full settlement window rather than returning as soon as
    /// a transaction hash exists:
    /// - [`RefSwap::swap`] inspects the transaction's receipts after the
    ///   `ft_transfer_call` lands and errors if any of them failed.
    /// - [`OneClickSwap::swap`] deposits, then polls the 1-Click status
    ///   endpoint until a terminal state; only [`oneclick::SwapStatus::Success`]
    ///   maps to `Ok(())`. `Failed`, `Refunded`, and `IncompleteDeposit` (1-Click
    ///   received less than the full deposit — a genuine partial fill) are all
    ///   surfaced as `Err`, never as a partial `Ok(())`.
    ///
    /// A new implementation must uphold the same guarantee: callers treat
    /// `Ok(())` as "the entire requested `amount` was swapped and settled,"
    /// full stop — there is no partial-success return value in this API, so a
    /// partial fill must be reported as an error, not folded into a smaller
    /// `Ok`.
    ///
    /// # Errors
    ///
    /// Returns `AppError` if:
    /// - The transaction fails to execute
    /// - The slippage exceeds acceptable limits
    /// - The deadline is exceeded
    /// - The swap settles as anything other than a full fill (refund, partial
    ///   deposit, or on-chain failure)
    async fn swap<F: AssetClass, T: AssetClass>(
        &self,
        from_asset: &FungibleAsset<F>,
        to_asset: &FungibleAsset<T>,
        amount: FungibleAssetAmount<F>,
    ) -> AppResult<()>;

    /// Returns the name of the swap provider for logging and debugging.
    fn provider_name(&self) -> &'static str;

    /// Returns a debug representation of the provider.
    fn debug_name(&self) -> String {
        self.provider_name().to_string()
    }

    /// Reports whether this provider can route a swap between `from_asset` and
    /// `to_asset` at all — a cheap, synchronous capability check, not a
    /// liquidity or pricing check.
    ///
    /// Must return `false` for any pair the provider structurally cannot
    /// service: [`RefSwap`] requires both legs to be NEP-141 tokens;
    /// [`OneClickSwap`] requires at least one leg to be NEP-245
    /// (`intents.near`-wrapped) and, once its supported-token cache is
    /// populated, both asset ids to appear in it.
    ///
    /// This is called before a swap is attempted, so returning `true` for a
    /// pair the provider cannot actually route doesn't fail closed — it defers
    /// the failure to [`swap`](Self::swap), which in the executor's JIT-swap
    /// path runs only *after* the liquidation transaction has already landed
    /// and the collateral has already been received. At that point the only
    /// remaining options are holding the collateral or retrying the swap; the
    /// clean "unsupported, hold collateral" outcome this check exists to
    /// provide is no longer available. Prefer a false negative (declining a
    /// pair the provider could actually handle) over a false positive.
    ///
    /// # Default Implementation
    ///
    /// The default implementation returns `true` for all pairs. Providers
    /// should override this if they have specific asset restrictions.
    fn supports_assets<F: AssetClass, T: AssetClass>(
        &self,
        _from_asset: &FungibleAsset<F>,
        _to_asset: &FungibleAsset<T>,
    ) -> bool {
        true
    }

    /// Ensures `account_id` is registered with `token_contract`'s NEP-145
    /// storage, registering it if necessary.
    ///
    /// NEP-141 fungible-token contracts bill each holder's balance entry to
    /// that holder's own on-contract storage deposit (NEP-145) rather than
    /// paying for it out of contract funds. An account with no storage deposit
    /// has nowhere on the contract to record a balance, so `ft_transfer` /
    /// `ft_transfer_call` into an unregistered account panics in the receiver
    /// and the transfer is refunded — silently, from the sender's point of
    /// view, unless the caller specifically checks for it. Any swap path that
    /// ends with tokens landing in a new account (a deposit address, an
    /// intermediate hop, the caller's own account for a token it has never
    /// held before) must call this first.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if registration succeeds or the account is already registered.
    ///
    /// # Errors
    ///
    /// Returns `AppError` if the registration transaction fails.
    async fn ensure_storage_registration<F: AssetClass>(
        &self,
        token_contract: &FungibleAsset<F>,
        account_id: &AccountId,
    ) -> AppResult<()>;
}
