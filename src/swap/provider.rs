//! Concrete swap provider enum for dynamic dispatch.
//!
//! Since the `SwapProvider` trait has generic methods, it cannot be made into
//! a trait object. This enum is the dispatch point instead: the crate ships
//! one variant (1-Click), and a fork adding a venue implements
//! [`SwapProvider`] for its own type and adds a variant here — the
//! irrefutable `let` bindings below stop compiling once a second variant
//! exists, so the compiler walks the fork through each method that needs
//! forwarding.

use near_sdk::AccountId;
use templar_common::asset::{AssetClass, FungibleAsset, FungibleAssetAmount};

use crate::rpc::AppResult;

use super::{oneclick::OneClickSwap, SwapProvider};

/// Concrete swap provider implementation that can be used for dynamic dispatch.
///
/// Wraps every shipped swap provider and implements `SwapProvider` by
/// forwarding. Deliberately an enum even while single-variant: third-party
/// venues slot in as new variants without touching the trait.
#[derive(Debug, Clone)]
pub enum SwapProviderImpl {
    /// 1-Click API provider for NEP-245 cross-chain swaps
    OneClick(OneClickSwap),
}

impl SwapProviderImpl {
    /// Creates a 1-Click API provider variant.
    pub fn oneclick(provider: OneClickSwap) -> Self {
        Self::OneClick(provider)
    }

    /// Loads supported tokens for the 1-Click provider (no-op for providers
    /// without a token cache).
    pub async fn load_supported_tokens(&self) {
        let Self::OneClick(provider) = self;
        provider.load_supported_tokens().await;
    }
}

#[async_trait::async_trait]
impl SwapProvider for SwapProviderImpl {
    async fn quote<F: AssetClass, T: AssetClass>(
        &self,
        from_asset: &FungibleAsset<F>,
        to_asset: &FungibleAsset<T>,
        output_amount: FungibleAssetAmount<T>,
    ) -> AppResult<FungibleAssetAmount<F>> {
        let Self::OneClick(provider) = self;
        provider.quote(from_asset, to_asset, output_amount).await
    }

    async fn swap<F: AssetClass, T: AssetClass>(
        &self,
        from_asset: &FungibleAsset<F>,
        to_asset: &FungibleAsset<T>,
        amount: FungibleAssetAmount<F>,
    ) -> AppResult<()> {
        let Self::OneClick(provider) = self;
        provider.swap(from_asset, to_asset, amount).await
    }

    fn provider_name(&self) -> &'static str {
        let Self::OneClick(provider) = self;
        provider.provider_name()
    }

    fn supports_assets<F: AssetClass, T: AssetClass>(
        &self,
        from_asset: &FungibleAsset<F>,
        to_asset: &FungibleAsset<T>,
    ) -> bool {
        let Self::OneClick(provider) = self;
        provider.supports_assets(from_asset, to_asset)
    }

    async fn ensure_storage_registration<F: AssetClass>(
        &self,
        token_contract: &FungibleAsset<F>,
        account_id: &AccountId,
    ) -> AppResult<()> {
        let Self::OneClick(provider) = self;
        provider
            .ensure_storage_registration(token_contract, account_id)
            .await
    }
}
