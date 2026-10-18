//! The single creation boundary of canonical asset and security identities.
//!
//! Adapters hand verified canonical terms to this registry and get the identity
//! of those terms back. Terms that cannot be proven to describe the same asset
//! keep separate identities: the registry never merges two records because they
//! share a display ticker, and it never allocates an identity per process.

use ahash::AHashMap;
use market_participants_model::asset::{
    AssetId, CommodityId, CommodityTerms, CryptoTokenId, CryptoTokenTerms, CurrencyId,
    CurrencyTerms, SecurityForm, SecurityId, SecurityTerms, ShareClassId, ShareClassTerms,
};

use crate::catalog::error::RegistryError;

/// A resolved asset together with the canonical terms it was derived from.
///
/// The terms are kept next to the identity because a derived identity such as
/// `TokenIssuerId` has to be derived from the same validated origin the asset
/// identity came from, never from a display name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssessedAsset {
    asset: AssetId,
    terms: AssetTerms,
}

impl AssessedAsset {
    /// Returns the canonical asset identity.
    pub fn asset(&self) -> &AssetId {
        &self.asset
    }

    /// Returns the canonical terms of the asset.
    pub fn terms(&self) -> &AssetTerms {
        &self.terms
    }

    /// Returns the currency terms, when this is a fiat currency.
    pub const fn currency(&self) -> Option<&CurrencyTerms> {
        match &self.terms {
            AssetTerms::Currency(terms) => Some(terms),
            _ => None,
        }
    }

    /// Returns the crypto token terms, when this is a token.
    pub const fn crypto_token(&self) -> Option<&CryptoTokenTerms> {
        match &self.terms {
            AssetTerms::CryptoToken(terms) => Some(terms),
            _ => None,
        }
    }

    /// Returns the security terms, when this is an issued security.
    pub const fn security(&self) -> Option<&SecurityTerms> {
        match &self.terms {
            AssetTerms::Security { terms, .. } => Some(terms),
            _ => None,
        }
    }

    /// Returns the commodity terms, when this is a commodity.
    pub const fn commodity(&self) -> Option<&CommodityTerms> {
        match &self.terms {
            AssetTerms::Commodity(terms) => Some(terms),
            _ => None,
        }
    }

    /// Returns the share class of an issued security.
    pub const fn share_class(&self) -> Option<&ShareClassId> {
        match &self.terms {
            AssetTerms::Security { terms, .. } => Some(&terms.share_class),
            _ => None,
        }
    }

    /// Returns the security form of an issued security.
    pub const fn security_form(&self) -> Option<SecurityForm> {
        match &self.terms {
            AssetTerms::Security { terms, .. } => Some(terms.form),
            _ => None,
        }
    }
}

/// The canonical terms of one registered asset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetTerms {
    /// The terms of a fiat currency.
    Currency(CurrencyTerms),
    /// The terms of a crypto token.
    CryptoToken(CryptoTokenTerms),
    /// The terms of an issued security, with the class terms they were derived
    /// from.
    ///
    /// A `SecurityId` is a digest of its issuer, share class and form, so the class
    /// terms travel with the security terms: a reader that has to state what the
    /// identity was derived from has the designation and the form beside it.
    Security {
        /// The canonical terms of the security.
        terms: SecurityTerms,
        /// The share class terms the security identity was derived from.
        share_class: ShareClassTerms,
    },
    /// The terms of a commodity.
    Commodity(CommodityTerms),
}

impl AssetTerms {
    /// Returns the canonical identity of these terms.
    pub fn asset_id(&self) -> AssetId {
        match self {
            Self::Currency(terms) => AssetId::Currency(CurrencyId::of_terms(terms)),
            Self::CryptoToken(terms) => AssetId::CryptoToken(CryptoTokenId::of_terms(terms)),
            Self::Security { terms, .. } => AssetId::Security(SecurityId::of_terms(terms)),
            Self::Commodity(terms) => AssetId::Commodity(CommodityId::of_terms(terms)),
        }
    }
}

/// A copy-on-write view of the asset identities of one catalog generation.
#[derive(Clone, Debug, Default)]
pub struct AssetRegistryView {
    currencies: Option<AHashMap<CurrencyId, CurrencyTerms>>,
    crypto_tokens: Option<AHashMap<CryptoTokenId, CryptoTokenTerms>>,
    securities: Option<AHashMap<SecurityId, SecurityTerms>>,
    share_classes: Option<AHashMap<SecurityId, ShareClassTerms>>,
    commodities: Option<AHashMap<CommodityId, CommodityTerms>>,
}

impl AssetRegistryView {
    /// Creates a view that carries exactly the given terms.
    pub fn new(
        currencies: AHashMap<CurrencyId, CurrencyTerms>,
        crypto_tokens: AHashMap<CryptoTokenId, CryptoTokenTerms>,
        securities: AHashMap<SecurityId, SecurityTerms>,
        share_classes: AHashMap<SecurityId, ShareClassTerms>,
        commodities: AHashMap<CommodityId, CommodityTerms>,
    ) -> Self {
        Self {
            currencies: Some(currencies),
            crypto_tokens: Some(crypto_tokens),
            securities: Some(securities),
            share_classes: Some(share_classes),
            commodities: Some(commodities),
        }
    }

    /// Returns the registered currency terms.
    pub fn currency(&self, id: &CurrencyId) -> Option<&CurrencyTerms> {
        self.currencies.as_ref()?.get(id)
    }

    /// Returns the registered crypto token terms.
    pub fn crypto_token(&self, id: &CryptoTokenId) -> Option<&CryptoTokenTerms> {
        self.crypto_tokens.as_ref()?.get(id)
    }

    /// Returns the registered security terms.
    pub fn security(&self, id: &SecurityId) -> Option<&SecurityTerms> {
        self.securities.as_ref()?.get(id)
    }

    /// Returns the registered share class terms of one security.
    ///
    /// A `SecurityId` is a digest that cannot be taken apart again, so the issuer,
    /// the class designation and the form are kept next to the identity derived
    /// from them; a reader that has to name a security reads the code a route
    /// staged for it instead.
    pub fn share_class(&self, id: &SecurityId) -> Option<&ShareClassTerms> {
        self.share_classes.as_ref()?.get(id)
    }

    /// Returns the registered commodity terms.
    pub fn commodity(&self, id: &CommodityId) -> Option<&CommodityTerms> {
        self.commodities.as_ref()?.get(id)
    }

    /// Returns the canonical terms of any registered asset.
    ///
    /// A security's terms are only complete with the class terms they were derived
    /// from, so a view that carries the security without them answers nothing for
    /// it rather than an incomplete term set.
    pub fn terms(&self, asset: &AssetId) -> Option<AssetTerms> {
        match asset {
            AssetId::Currency(id) => self.currency(id).cloned().map(AssetTerms::Currency),
            AssetId::CryptoToken(id) => self.crypto_token(id).cloned().map(AssetTerms::CryptoToken),
            AssetId::Security(id) => {
                let terms = self.security(id)?.clone();
                let share_class = self.share_class(id)?.clone();
                Some(AssetTerms::Security { terms, share_class })
            }
            AssetId::Commodity(id) => self.commodity(id).cloned().map(AssetTerms::Commodity),
        }
    }

    /// Returns the assets this view knows, in registration order per family.
    pub fn assets(&self) -> Vec<AssetId> {
        let mut assets: Vec<AssetId> = Vec::with_capacity(
            self.currencies.as_ref().map_or(0, |map| map.len())
                + self.crypto_tokens.as_ref().map_or(0, |map| map.len())
                + self.securities.as_ref().map_or(0, |map| map.len())
                + self.commodities.as_ref().map_or(0, |map| map.len()),
        );
        for id in self.currencies.iter().flat_map(|map| map.keys()) {
            assets.push(AssetId::Currency(id.clone()));
        }
        for id in self.crypto_tokens.iter().flat_map(|map| map.keys()) {
            assets.push(AssetId::CryptoToken(id.clone()));
        }
        for id in self.securities.iter().flat_map(|map| map.keys()) {
            assets.push(AssetId::Security(id.clone()));
        }
        for id in self.commodities.iter().flat_map(|map| map.keys()) {
            assets.push(AssetId::Commodity(id.clone()));
        }
        assets
    }
}

/// The asset identities of one catalog generation.
#[derive(Clone, Debug, Default)]
pub struct AssetRegistry {
    view: AssetRegistryView,
}

impl AssetRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a registry that holds exactly the given canonical terms.
    pub fn from_terms(
        currencies: AHashMap<CurrencyId, CurrencyTerms>,
        crypto_tokens: AHashMap<CryptoTokenId, CryptoTokenTerms>,
        securities: AHashMap<SecurityId, SecurityTerms>,
        share_classes: AHashMap<SecurityId, ShareClassTerms>,
        commodities: AHashMap<CommodityId, CommodityTerms>,
    ) -> Self {
        Self {
            view: AssetRegistryView::new(
                currencies,
                crypto_tokens,
                securities,
                share_classes,
                commodities,
            ),
        }
    }

    /// Returns the registered currency terms.
    pub fn currency(&self, id: &CurrencyId) -> Option<&CurrencyTerms> {
        self.view.currency(id)
    }

    /// Returns the registered crypto token terms.
    pub fn crypto_token(&self, id: &CryptoTokenId) -> Option<&CryptoTokenTerms> {
        self.view.crypto_token(id)
    }

    /// Returns the registered security terms.
    pub fn security(&self, id: &SecurityId) -> Option<&SecurityTerms> {
        self.view.security(id)
    }

    /// Returns the registered share class terms of one security.
    pub fn share_class(&self, id: &SecurityId) -> Option<&ShareClassTerms> {
        self.view.share_class(id)
    }

    /// Returns the registered commodity terms.
    pub fn commodity(&self, id: &CommodityId) -> Option<&CommodityTerms> {
        self.view.commodity(id)
    }

    /// Returns the canonical terms of any registered asset.
    pub fn terms(&self, asset: &AssetId) -> Option<AssetTerms> {
        self.view.terms(asset)
    }

    /// Returns the assets this registry knows.
    pub fn assets(&self) -> Vec<AssetId> {
        self.view.assets()
    }

    /// Returns the number of registered assets.
    pub fn len(&self) -> usize {
        self.view.assets().len()
    }

    /// Returns `true` when no asset is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The asset terms staged by one in-flight catalog batch.
#[derive(Clone, Debug, Default)]
pub struct AssetRegistryPending {
    /// The staged currency terms.
    pub currencies: AHashMap<CurrencyId, CurrencyTerms>,
    /// The staged crypto token terms.
    pub crypto_tokens: AHashMap<CryptoTokenId, CryptoTokenTerms>,
    /// The staged security terms.
    pub securities: AHashMap<SecurityId, SecurityTerms>,
    /// The staged share class terms of every staged security.
    pub share_classes: AHashMap<SecurityId, ShareClassTerms>,
    /// The staged commodity terms.
    pub commodities: AHashMap<CommodityId, CommodityTerms>,
}

impl AssetRegistryPending {
    /// Creates an empty pending view.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stages a currency identity.
    pub fn register_currency(&mut self, terms: CurrencyTerms) -> CurrencyId {
        let id = CurrencyId::of_terms(&terms);
        self.currencies.insert(id.clone(), terms);
        id
    }
    /// Stages a crypto token identity.
    pub fn register_crypto_token(
        &mut self,
        terms: CryptoTokenTerms,
    ) -> Result<CryptoTokenId, RegistryError> {
        terms.validate().map_err(RegistryError::from)?;
        let id = CryptoTokenId::of_terms(&terms);
        self.crypto_tokens.insert(id.clone(), terms);
        Ok(id)
    }

    /// Stages a security identity.
    pub fn register_security(&mut self, terms: SecurityTerms) -> SecurityId {
        let id = SecurityId::of_terms(&terms);
        self.securities.insert(id.clone(), terms);
        id
    }

    /// Returns `true` when nothing was staged.
    pub fn is_empty(&self) -> bool {
        self.currencies.is_empty()
            && self.crypto_tokens.is_empty()
            && self.securities.is_empty()
            && self.share_classes.is_empty()
            && self.commodities.is_empty()
    }

    /// Stages one canonical asset from its own terms.
    ///
    /// A canonical identity is a digest of its terms, so a source that carries the
    /// terms of the assets its listings are denominated in stages them here: the
    /// identity is derived from the terms again and the `AssetId` a listing names
    /// therefore always finds its terms in the registry.
    pub fn stage(&mut self, terms: AssetTerms) {
        match terms {
            AssetTerms::Currency(terms) => {
                let id = CurrencyId::of_terms(&terms);
                self.currencies.insert(id, terms);
            }
            AssetTerms::CryptoToken(terms) => {
                let id = CryptoTokenId::of_terms(&terms);
                self.crypto_tokens.insert(id, terms);
            }
            AssetTerms::Security { terms, share_class } => {
                let id = SecurityId::of_terms(&terms);
                self.share_classes.insert(id.clone(), share_class);
                self.securities.insert(id, terms);
            }
            AssetTerms::Commodity(terms) => {
                let id = CommodityId::of_terms(&terms);
                self.commodities.insert(id, terms);
            }
        }
    }
}
