use ahash::AHashMap;
use market_participants_model::asset::{
    AssetId, CommodityId, CommodityTerms, CryptoTokenId, CryptoTokenTerms, CurrencyId,
    CurrencyTerms, SecurityForm, SecurityId, SecurityTerms, ShareClassId, ShareClassTerms,
};

use crate::catalog::error::RegistryError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssessedAsset {
    asset: AssetId,
    terms: AssetTerms,
}

impl AssessedAsset {
    pub fn asset(&self) -> &AssetId {
        &self.asset
    }

    pub fn terms(&self) -> &AssetTerms {
        &self.terms
    }

    pub const fn currency(&self) -> Option<&CurrencyTerms> {
        match &self.terms {
            AssetTerms::Currency(terms) => Some(terms),
            _ => None,
        }
    }

    pub const fn crypto_token(&self) -> Option<&CryptoTokenTerms> {
        match &self.terms {
            AssetTerms::CryptoToken(terms) => Some(terms),
            _ => None,
        }
    }

    pub const fn security(&self) -> Option<&SecurityTerms> {
        match &self.terms {
            AssetTerms::Security { terms, .. } => Some(terms),
            _ => None,
        }
    }

    pub const fn commodity(&self) -> Option<&CommodityTerms> {
        match &self.terms {
            AssetTerms::Commodity(terms) => Some(terms),
            _ => None,
        }
    }

    pub const fn share_class(&self) -> Option<&ShareClassId> {
        match &self.terms {
            AssetTerms::Security { terms, .. } => Some(&terms.share_class),
            _ => None,
        }
    }

    pub const fn security_form(&self) -> Option<SecurityForm> {
        match &self.terms {
            AssetTerms::Security { terms, .. } => Some(terms.form),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetTerms {
    Currency(CurrencyTerms),
    CryptoToken(CryptoTokenTerms),
    Security {
        terms: SecurityTerms,
        share_class: ShareClassTerms,
    },
    Commodity(CommodityTerms),
}

impl AssetTerms {
    pub fn asset_id(&self) -> AssetId {
        match self {
            Self::Currency(terms) => AssetId::Currency(CurrencyId::of_terms(terms)),
            Self::CryptoToken(terms) => AssetId::CryptoToken(CryptoTokenId::of_terms(terms)),
            Self::Security { terms, .. } => AssetId::Security(SecurityId::of_terms(terms)),
            Self::Commodity(terms) => AssetId::Commodity(CommodityId::of_terms(terms)),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct AssetRegistryView {
    currencies: Option<AHashMap<CurrencyId, CurrencyTerms>>,
    crypto_tokens: Option<AHashMap<CryptoTokenId, CryptoTokenTerms>>,
    securities: Option<AHashMap<SecurityId, SecurityTerms>>,
    share_classes: Option<AHashMap<SecurityId, ShareClassTerms>>,
    commodities: Option<AHashMap<CommodityId, CommodityTerms>>,
}

impl AssetRegistryView {
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

    pub fn currency(&self, id: &CurrencyId) -> Option<&CurrencyTerms> {
        self.currencies.as_ref()?.get(id)
    }

    pub fn crypto_token(&self, id: &CryptoTokenId) -> Option<&CryptoTokenTerms> {
        self.crypto_tokens.as_ref()?.get(id)
    }

    pub fn security(&self, id: &SecurityId) -> Option<&SecurityTerms> {
        self.securities.as_ref()?.get(id)
    }

    pub fn share_class(&self, id: &SecurityId) -> Option<&ShareClassTerms> {
        self.share_classes.as_ref()?.get(id)
    }

    pub fn commodity(&self, id: &CommodityId) -> Option<&CommodityTerms> {
        self.commodities.as_ref()?.get(id)
    }

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

    pub fn assets(&self) -> Vec<AssetId> {
        let mut assets: Vec<AssetId> = Vec::with_capacity(
            self.currencies.as_ref().map_or(0, |map| map.len())
                + self.crypto_tokens.as_ref().map_or(0, |map| map.len())
                + self.securities.as_ref().map_or(0, |map| map.len())
                + self.commodities.as_ref().map_or(0, |map| map.len()),
        );
        for id in self.currencies.iter().flat_map(|map| map.keys()) {
            assets.push(AssetId::Currency(*id));
        }
        for id in self.crypto_tokens.iter().flat_map(|map| map.keys()) {
            assets.push(AssetId::CryptoToken(*id));
        }
        for id in self.securities.iter().flat_map(|map| map.keys()) {
            assets.push(AssetId::Security(*id));
        }
        for id in self.commodities.iter().flat_map(|map| map.keys()) {
            assets.push(AssetId::Commodity(*id));
        }
        assets
    }
}

#[derive(Clone, Debug, Default)]
pub struct AssetRegistry {
    view: AssetRegistryView,
}

impl AssetRegistry {
    pub fn new() -> Self {
        Self::default()
    }

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

    pub fn currency(&self, id: &CurrencyId) -> Option<&CurrencyTerms> {
        self.view.currency(id)
    }

    pub fn crypto_token(&self, id: &CryptoTokenId) -> Option<&CryptoTokenTerms> {
        self.view.crypto_token(id)
    }

    pub fn security(&self, id: &SecurityId) -> Option<&SecurityTerms> {
        self.view.security(id)
    }

    pub fn share_class(&self, id: &SecurityId) -> Option<&ShareClassTerms> {
        self.view.share_class(id)
    }

    pub fn commodity(&self, id: &CommodityId) -> Option<&CommodityTerms> {
        self.view.commodity(id)
    }

    pub fn terms(&self, asset: &AssetId) -> Option<AssetTerms> {
        self.view.terms(asset)
    }

    pub fn assets(&self) -> Vec<AssetId> {
        self.view.assets()
    }

    pub fn len(&self) -> usize {
        self.view.assets().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Debug, Default)]
pub struct AssetRegistryPending {
    pub currencies: AHashMap<CurrencyId, CurrencyTerms>,
    pub crypto_tokens: AHashMap<CryptoTokenId, CryptoTokenTerms>,
    pub securities: AHashMap<SecurityId, SecurityTerms>,
    pub share_classes: AHashMap<SecurityId, ShareClassTerms>,
    pub commodities: AHashMap<CommodityId, CommodityTerms>,
}

impl AssetRegistryPending {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_currency(&mut self, terms: CurrencyTerms) -> CurrencyId {
        let id = CurrencyId::of_terms(&terms);
        self.currencies.insert(id, terms);
        id
    }
    pub fn register_crypto_token(
        &mut self,
        terms: CryptoTokenTerms,
    ) -> Result<CryptoTokenId, RegistryError> {
        terms.validate().map_err(RegistryError::from)?;
        let id = CryptoTokenId::of_terms(&terms);
        self.crypto_tokens.insert(id, terms);
        Ok(id)
    }

    pub fn register_security(&mut self, terms: SecurityTerms) -> SecurityId {
        let id = SecurityId::of_terms(&terms);
        self.securities.insert(id, terms);
        id
    }

    pub fn is_empty(&self) -> bool {
        self.currencies.is_empty()
            && self.crypto_tokens.is_empty()
            && self.securities.is_empty()
            && self.share_classes.is_empty()
            && self.commodities.is_empty()
    }

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
                self.share_classes.insert(id, share_class);
                self.securities.insert(id, terms);
            }
            AssetTerms::Commodity(terms) => {
                let id = CommodityId::of_terms(&terms);
                self.commodities.insert(id, terms);
            }
        }
    }
}
