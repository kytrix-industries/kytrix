use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, de};
use smol_str::SmolStr;

use crate::{
    decimal::Quantity,
    error::ModelError,
    identifier::{
        CanonicalEncoder, CanonicalTerms, ChainId, CommodityIdentity, CryptoTokenIdentity,
        CurrencyIdentity, IdentifierNamespace, IdentifierValue, IndexIdentity, SecurityIdentity,
        ShareClassIdentity, TokenIssuerId, is_base58_address, is_evm_address,
    },
    reference::ExternalReferenceId,
};

const ISO_4217_CODES: [&str; 177] = [
    "AED", "AFN", "ALL", "AMD", "ANG", "AOA", "ARS", "AUD", "AWG", "AZN", "BAM", "BBD", "BDT",
    "BGN", "BHD", "BIF", "BMD", "BND", "BOB", "BOV", "BRL", "BSD", "BTN", "BWP", "BYN", "BZD",
    "CAD", "CDF", "CHE", "CHF", "CHW", "CLF", "CLP", "CNY", "COP", "COU", "CRC", "CUC", "CUP",
    "CVE", "CZK", "DJF", "DKK", "DOP", "DZD", "EGP", "ERN", "ETB", "EUR", "FJD", "FKP", "GBP",
    "GEL", "GHS", "GIP", "GMD", "GNF", "GTQ", "GYD", "HKD", "HNL", "HTG", "HUF", "IDR", "ILS",
    "INR", "IQD", "IRR", "ISK", "JMD", "JOD", "JPY", "KES", "KGS", "KHR", "KMF", "KPW", "KRW",
    "KWD", "KYD", "KZT", "LAK", "LBP", "LKR", "LRD", "LSL", "LYD", "MAD", "MDL", "MGA", "MKD",
    "MMK", "MNT", "MOP", "MRU", "MUR", "MVR", "MWK", "MXN", "MXV", "MYR", "MZN", "NAD", "NGN",
    "NIO", "NOK", "NPR", "NZD", "OMR", "PAB", "PEN", "PGK", "PHP", "PKR", "PLN", "PYG", "QAR",
    "RON", "RSD", "RUB", "RWF", "SAR", "SBD", "SCR", "SDG", "SEK", "SGD", "SHP", "SLE", "SOS",
    "SRD", "SSP", "STN", "SVC", "SYP", "SZL", "THB", "TJS", "TMT", "TND", "TOP", "TRY", "TTD",
    "TWD", "TZS", "UAH", "UGX", "USD", "USN", "UYI", "UYU", "UYW", "UZS", "VED", "VES", "VND",
    "VUV", "WST", "XAF", "XAG", "XAU", "XBA", "XBB", "XBC", "XBD", "XCD", "XDR", "XOF", "XPD",
    "XPF", "XPT", "XSU", "XUA", "YER", "ZAR", "ZMW", "ZWG",
];

const NON_TRADABLE_CURRENCY_CODES: [&str; 2] = ["XTS", "XXX"];

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Iso4217Code(SmolStr);

impl Iso4217Code {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ModelError> {
        let value = value.as_ref();
        let normalized = value.to_ascii_uppercase();
        if ISO_4217_CODES.binary_search(&normalized.as_str()).is_err()
            || NON_TRADABLE_CURRENCY_CODES.contains(&normalized.as_str())
        {
            return Err(ModelError::InvalidIso4217Code {
                kind: SmolStr::new(value),
            });
        }
        Ok(Self(SmolStr::new(normalized)))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for Iso4217Code {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for Iso4217Code {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Iso4217Code {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl From<Iso4217Code> for SmolStr {
    fn from(value: Iso4217Code) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for Iso4217Code {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = SmolStr::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct CurrencyTerms {
    #[serde(rename = "code")]
    pub code: Iso4217Code,
}

impl CanonicalTerms for CurrencyTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label("currency");
        encoder.write_label(self.code.as_str());
    }
}

crate::identifier::identity_type!(CurrencyId, CurrencyIdentity);

impl CurrencyId {
    pub fn new(code: Iso4217Code) -> Self {
        Self::of_terms(&CurrencyTerms { code })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ContractAddress(SmolStr);

impl ContractAddress {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ModelError> {
        let value = value.as_ref();
        if is_evm_address(value) {
            Ok(Self(SmolStr::new(value)))
        } else {
            Err(ModelError::InvalidContractAddress {
                value: SmolStr::new(value),
            })
        }
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for ContractAddress {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for ContractAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ContractAddress {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for ContractAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = SmolStr::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "reference")]
pub enum TokenReference {
    #[serde(rename = "native")]
    Native {
        #[serde(rename = "code")]
        code: IdentifierValue,
    },
    #[serde(rename = "contract")]
    Contract {
        #[serde(rename = "address")]
        address: ContractAddress,
    },
}

impl TokenReference {
    pub fn contract_address(&self) -> Option<&ContractAddress> {
        match self {
            Self::Contract { address } => Some(address),
            Self::Native { .. } => None,
        }
    }
}

impl CanonicalTerms for TokenReference {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        match self {
            Self::Native { code } => {
                encoder.write_label("native");
                encoder.write_label(code.as_str());
            }
            Self::Contract { address } => {
                encoder.write_label("contract");
                encoder.write_label(address.as_str());
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "origin")]
pub enum CryptoTokenOrigin {
    #[serde(rename = "chain")]
    Chain {
        #[serde(rename = "chain_id")]
        chain_id: ChainId,
        #[serde(rename = "token")]
        token: TokenReference,
    },
    #[serde(rename = "venue_issued")]
    VenueIssued {
        #[serde(rename = "issuer")]
        issuer: TokenIssuerId,
        #[serde(rename = "namespace")]
        namespace: IdentifierNamespace,
        #[serde(rename = "token")]
        token: TokenReference,
    },
}

impl CryptoTokenOrigin {
    pub fn chain(&self) -> Option<&ChainId> {
        match self {
            Self::Chain { chain_id, .. } => Some(chain_id),
            Self::VenueIssued { .. } => None,
        }
    }

    pub fn token(&self) -> &TokenReference {
        match self {
            Self::Chain { token, .. } | Self::VenueIssued { token, .. } => token,
        }
    }
}

impl CanonicalTerms for CryptoTokenOrigin {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        match self {
            Self::Chain { chain_id, token } => {
                encoder.write_label("chain");
                encoder.write_label(chain_id.as_str());
                encoder.write_terms(token);
            }
            Self::VenueIssued {
                issuer,
                namespace,
                token,
            } => {
                encoder.write_label("venue-issued");
                encoder.write_label(issuer.as_str());
                encoder.write_label(namespace.as_str());
                encoder.write_terms(token);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct CryptoTokenTerms {
    #[serde(rename = "origin")]
    pub origin: CryptoTokenOrigin,
}

impl CryptoTokenTerms {
    pub fn validate(&self) -> Result<(), ModelError> {
        let Some(address) = self.origin.token().contract_address() else {
            return Ok(());
        };
        let solana_chain = self
            .origin
            .chain()
            .is_some_and(|chain| chain.as_str() == "solana");
        let readable = if solana_chain {
            is_base58_address(address.as_str())
        } else {
            is_evm_address(address.as_str())
        };
        if readable {
            return Ok(());
        }
        Err(ModelError::UnverifiedTokenOrigin {
            chain: self
                .origin
                .chain()
                .cloned()
                .unwrap_or_else(|| ChainId::new("venue-issued").expect("static chain is valid")),
            token: SmolStr::new(address.as_str()),
        })
    }
}

impl CanonicalTerms for CryptoTokenTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label("crypto-token");
        encoder.write_terms(&self.origin);
    }
}

crate::identifier::identity_type!(CryptoTokenId, CryptoTokenIdentity);

impl CryptoTokenId {
    pub fn new(origin: CryptoTokenOrigin) -> Result<Self, ModelError> {
        let terms = CryptoTokenTerms { origin };
        terms.validate()?;
        Ok(Self::of_terms(&terms))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SecurityForm {
    #[serde(rename = "common_stock")]
    CommonStock,
    #[serde(rename = "preferred_stock")]
    PreferredStock,
    #[serde(rename = "adr")]
    Adr,
    #[serde(rename = "etf_share")]
    EtfShare,
}

impl SecurityForm {
    pub const fn label(self) -> &'static str {
        match self {
            Self::CommonStock => "common-stock",
            Self::PreferredStock => "preferred-stock",
            Self::Adr => "adr",
            Self::EtfShare => "etf-share",
        }
    }
}

impl CanonicalTerms for SecurityForm {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label(self.label());
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct SecurityIssuerId(SmolStr);

impl SecurityIssuerId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ModelError> {
        let value = value.as_ref();
        let valid = value.split_once('/').is_some_and(|(namespace, code)| {
            !namespace.is_empty()
                && namespace.len() <= 16
                && namespace
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                && !code.is_empty()
                && code.len() <= 64
                && code.bytes().all(|byte| byte.is_ascii_alphanumeric())
        });
        if valid {
            Ok(Self(SmolStr::new(value)))
        } else {
            Err(ModelError::InvalidSecurityIssuerId {
                value: SmolStr::new(value),
            })
        }
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for SecurityIssuerId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for SecurityIssuerId {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl From<SecurityIssuerId> for SmolStr {
    fn from(value: SecurityIssuerId) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for SecurityIssuerId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = SmolStr::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct ShareClassTerms {
    #[serde(rename = "issuer")]
    pub issuer: SecurityIssuerId,
    #[serde(rename = "designation")]
    pub designation: SmolStr,
    #[serde(rename = "form")]
    pub form: SecurityForm,
    #[serde(rename = "par_value")]
    pub par_value: Option<Quantity>,
    #[serde(rename = "adr_ratio")]
    pub adr_ratio: Option<Quantity>,
}

impl ShareClassTerms {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.designation.is_empty() {
            return Err(ModelError::InvalidShareClassDesignation {
                value: self.designation.clone(),
            });
        }
        if let Some(ratio) = self.adr_ratio
            && ratio.is_zero()
        {
            return Err(ModelError::NonPositiveQuantity {
                field: "share_class.adr_ratio",
            });
        }
        if self.form == SecurityForm::Adr && self.adr_ratio.is_none() {
            return Err(ModelError::MissingAdrRatio {
                value: self.designation.clone(),
            });
        }
        Ok(())
    }
}

impl CanonicalTerms for ShareClassTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label("share-class");
        encoder.write_label(self.issuer.as_str());
        encoder.write_label(&self.designation);
        encoder.write_terms(&self.form);
        encoder.write_optional(self.par_value.as_ref(), |encoder, value| {
            encoder.write_terms(value);
        });
        encoder.write_optional(self.adr_ratio.as_ref(), |encoder, value| {
            encoder.write_terms(value);
        });
    }
}

crate::identifier::identity_type!(ShareClassId, ShareClassIdentity);

impl ShareClassId {
    pub fn new(terms: &ShareClassTerms) -> Result<Self, ModelError> {
        terms.validate()?;
        Ok(Self::of_terms(terms))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct SecurityTerms {
    #[serde(rename = "issuer")]
    pub issuer: SecurityIssuerId,
    #[serde(rename = "share_class")]
    pub share_class: ShareClassId,
    #[serde(rename = "form")]
    pub form: SecurityForm,
}

impl CanonicalTerms for SecurityTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label("security");
        encoder.write_label(self.issuer.as_str());
        encoder.write_terms(&self.share_class);
        encoder.write_terms(&self.form);
    }
}

crate::identifier::identity_type!(SecurityId, SecurityIdentity);

impl SecurityId {
    pub fn new(issuer: SecurityIssuerId, share_class: ShareClassId, form: SecurityForm) -> Self {
        Self::of_terms(&SecurityTerms {
            issuer,
            share_class,
            form,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "origin")]
pub enum CommodityOrigin {
    #[serde(rename = "iso")]
    Iso {
        #[serde(rename = "code")]
        code: IdentifierValue,
    },
    #[serde(rename = "venue")]
    Venue {
        #[serde(rename = "namespace")]
        namespace: IdentifierNamespace,
        #[serde(rename = "code")]
        code: IdentifierValue,
    },
}

impl CanonicalTerms for CommodityOrigin {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        match self {
            Self::Iso { code } => {
                encoder.write_label("iso");
                encoder.write_label(code.as_str());
            }
            Self::Venue { namespace, code } => {
                encoder.write_label("venue");
                encoder.write_label(namespace.as_str());
                encoder.write_label(code.as_str());
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct CommodityTerms {
    #[serde(rename = "origin")]
    pub origin: CommodityOrigin,
}

impl CommodityTerms {
    pub fn validate(&self) -> Result<(), ModelError> {
        if let CommodityOrigin::Iso { code } = &self.origin {
            let code = code.as_str();
            let iso_currency = ISO_4217_CODES.binary_search(&code).is_ok();
            if !iso_currency && !is_iso3166_alpha2(code) {
                return Err(ModelError::InvalidCommodityCode {
                    value: SmolStr::new(code),
                });
            }
        }
        Ok(())
    }
}

impl CanonicalTerms for CommodityTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label("commodity");
        encoder.write_terms(&self.origin);
    }
}

fn is_iso3166_alpha2(value: &str) -> bool {
    value.len() == 2 && value.bytes().all(|byte| byte.is_ascii_uppercase())
}

crate::identifier::identity_type!(CommodityId, CommodityIdentity);

impl CommodityId {
    pub fn new(origin: CommodityOrigin) -> Result<Self, ModelError> {
        let terms = CommodityTerms { origin };
        terms.validate()?;
        Ok(Self::of_terms(&terms))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct IndexTerms {
    #[serde(rename = "namespace")]
    pub namespace: IdentifierNamespace,
    #[serde(rename = "code")]
    pub code: IdentifierValue,
}

impl CanonicalTerms for IndexTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_label("index");
        encoder.write_label(self.namespace.as_str());
        encoder.write_label(self.code.as_str());
    }
}

crate::identifier::identity_type!(IndexId, IndexIdentity);

impl IndexId {
    pub fn new(namespace: IdentifierNamespace, code: IdentifierValue) -> Self {
        Self::of_terms(&IndexTerms { namespace, code })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "spec")]
pub enum AssetId {
    #[serde(rename = "currency")]
    Currency(CurrencyId),
    #[serde(rename = "crypto_token")]
    CryptoToken(CryptoTokenId),
    #[serde(rename = "security")]
    Security(SecurityId),
    #[serde(rename = "commodity")]
    Commodity(CommodityId),
}

impl AssetId {
    pub const fn is_settlement_asset(&self) -> bool {
        matches!(self, Self::Currency(_) | Self::CryptoToken(_))
    }
}

impl CanonicalTerms for AssetId {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        match self {
            Self::Currency(currency) => {
                encoder.write_label("currency");
                encoder.write_terms(currency);
            }
            Self::CryptoToken(token) => {
                encoder.write_label("crypto-token");
                encoder.write_terms(token);
            }
            Self::Security(security) => {
                encoder.write_label("security");
                encoder.write_terms(security);
            }
            Self::Commodity(commodity) => {
                encoder.write_label("commodity");
                encoder.write_terms(commodity);
            }
        }
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Currency(currency) => write!(formatter, "currency:{currency}"),
            Self::CryptoToken(token) => write!(formatter, "crypto-token:{token}"),
            Self::Security(security) => write!(formatter, "security:{security}"),
            Self::Commodity(commodity) => write!(formatter, "commodity:{commodity}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "reference")]
pub enum UnderlyingRef {
    #[serde(rename = "asset")]
    Asset(AssetId),
    #[serde(rename = "index")]
    Index(IndexId),
    #[serde(rename = "external")]
    External(ExternalReferenceId),
}

impl UnderlyingRef {
    pub const fn asset(&self) -> Option<&AssetId> {
        match self {
            Self::Asset(asset) => Some(asset),
            Self::Index(_) | Self::External(_) => None,
        }
    }

    pub const fn index(&self) -> Option<&IndexId> {
        match self {
            Self::Index(index) => Some(index),
            Self::Asset(_) | Self::External(_) => None,
        }
    }
}

impl CanonicalTerms for UnderlyingRef {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        match self {
            Self::Asset(asset) => {
                encoder.write_label("asset");
                encoder.write_terms(asset);
            }
            Self::Index(index) => {
                encoder.write_label("index");
                encoder.write_terms(index);
            }
            Self::External(external) => {
                encoder.write_label("external");
                encoder.write_terms(external);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DerivativeUnderlying {
    #[serde(rename = "reference")]
    pub reference: UnderlyingRef,
    #[serde(rename = "price_reference")]
    pub price_reference: crate::reference::PriceReferenceId,
}

impl DerivativeUnderlying {
    pub fn new(
        reference: UnderlyingRef,
        price_reference: crate::reference::PriceReferenceId,
    ) -> Self {
        Self {
            reference,
            price_reference,
        }
    }

    pub fn reference(&self) -> &UnderlyingRef {
        &self.reference
    }

    pub fn price_reference(&self) -> &crate::reference::PriceReferenceId {
        &self.price_reference
    }
}

impl CanonicalTerms for DerivativeUnderlying {
    fn encode(&self, encoder: &mut CanonicalEncoder) {
        encoder.write_terms(&self.reference);
        encoder.write_terms(&self.price_reference);
    }
}

pub fn require_chain_readable_address(
    chain: Option<&ChainId>,
    address: &ContractAddress,
) -> Result<(), ModelError> {
    let readable = match chain {
        Some(chain) if chain.as_str() == "solana" => is_base58_address(address.as_str()),
        _ => is_evm_address(address.as_str()),
    };
    if readable {
        Ok(())
    } else {
        Err(ModelError::UnverifiedTokenOrigin {
            chain: chain
                .cloned()
                .unwrap_or_else(|| ChainId::new("venue-issued").expect("static chain is valid")),
            token: SmolStr::new(address.as_str()),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        asset::{
            AssetId, CommodityId, CommodityOrigin, ContractAddress, CryptoTokenId,
            CryptoTokenOrigin, CurrencyId, Iso4217Code, SecurityForm, SecurityId, SecurityIssuerId,
            SecurityTerms, ShareClassId, ShareClassTerms, TokenReference,
        },
        decimal::Quantity,
        error::ModelError,
        identifier::{Digest, IdentifierNamespace, IdentifierValue, TokenIssuerId},
    };

    fn issuer() -> SecurityIssuerId {
        SecurityIssuerId::new("cik/320193").expect("issuer should be valid")
    }

    fn share_class(designation: &str, form: SecurityForm) -> ShareClassId {
        ShareClassId::new(&ShareClassTerms {
            issuer: issuer(),
            designation: designation.into(),
            form,
            par_value: None,
            adr_ratio: None,
        })
        .expect("share class should be valid")
    }

    fn security(designation: &str, form: SecurityForm) -> SecurityId {
        SecurityId::new(issuer(), share_class(designation, form), form)
    }

    fn venue_token(issuer_text: &str, code: &str) -> CryptoTokenId {
        CryptoTokenId::new(CryptoTokenOrigin::VenueIssued {
            issuer: TokenIssuerId::new(issuer_text).expect("issuer should be valid"),
            namespace: IdentifierNamespace::new(issuer_text).expect("namespace should be valid"),
            token: TokenReference::Native {
                code: IdentifierValue::new(code).expect("code should be valid"),
            },
        })
        .expect("token should be valid")
    }

    #[test]
    fn iso4217_should_reject_stablecoins_and_chain_assets() {
        for code in ["USDT", "USDC", "BTC", "DAI", "ETH"] {
            assert_eq!(
                Iso4217Code::new(code),
                Err(ModelError::InvalidIso4217Code { kind: code.into() }),
                "{code} must not become a currency"
            );
        }
    }

    #[test]
    fn iso4217_should_reject_the_reserved_and_non_currency_codes() {
        for code in ["XTS", "XXX"] {
            assert_eq!(
                Iso4217Code::new(code),
                Err(ModelError::InvalidIso4217Code { kind: code.into() }),
                "{code} must not denominate a tradable instrument"
            );
        }
    }

    #[test]
    fn iso4217_should_normalize_the_spelling_and_reject_unknown_codes() {
        assert_eq!(
            Iso4217Code::new("USD")
                .expect("USD should be valid")
                .as_str(),
            "USD"
        );
        assert_eq!(
            Iso4217Code::new("usd")
                .expect("a lower-case code must normalize to uppercase")
                .as_str(),
            "USD"
        );
        assert_eq!(
            Iso4217Code::new("cNy")
                .expect("a mixed-case code must normalize to uppercase")
                .as_str(),
            "CNY"
        );
        assert_eq!(
            Iso4217Code::new("USDT"),
            Err(ModelError::InvalidIso4217Code {
                kind: "USDT".into()
            })
        );
        assert_eq!(
            Iso4217Code::new("BTC"),
            Err(ModelError::InvalidIso4217Code { kind: "BTC".into() })
        );
        assert_eq!(
            Iso4217Code::new(" usd "),
            Err(ModelError::InvalidIso4217Code {
                kind: " usd ".into()
            }),
            "normalization must not trim surrounding whitespace"
        );
        assert_eq!(
            Iso4217Code::new("XYZ"),
            Err(ModelError::InvalidIso4217Code { kind: "XYZ".into() })
        );
        assert_eq!(
            Iso4217Code::new("US"),
            Err(ModelError::InvalidIso4217Code { kind: "US".into() })
        );
    }

    #[test]
    fn currency_should_round_trip_through_serde() {
        let currency = CurrencyId::new(Iso4217Code::new("USD").expect("USD should be valid"));
        let encoded = serde_json::to_string(&currency).expect("currency should serialize");
        let decoded = serde_json::from_str::<CurrencyId>(&encoded).expect("currency should decode");

        assert_eq!(decoded, currency);
    }

    #[test]
    fn currency_should_reject_an_identity_that_was_never_published() {
        assert!(CurrencyId::from_identity_value("USDT").is_err());
        assert!(CurrencyId::from_identity_value("v1/short").is_err());
    }

    #[test]
    fn security_id_should_distinguish_display_identical_securities() {
        let common = security("common", SecurityForm::CommonStock);
        let other_issuer = SecurityId::new(
            SecurityIssuerId::new("cik/999999").expect("issuer should be valid"),
            share_class("common", SecurityForm::CommonStock),
            SecurityForm::CommonStock,
        );

        assert_ne!(common, other_issuer);
        assert_ne!(common, security("class-a", SecurityForm::CommonStock));
        assert_eq!(common, security("common", SecurityForm::CommonStock));
    }

    #[test]
    fn security_id_should_not_depend_on_a_display_ticker() {
        let terms = SecurityTerms {
            issuer: issuer(),
            share_class: share_class("common", SecurityForm::CommonStock),
            form: SecurityForm::CommonStock,
        };
        let encoded = serde_json::to_string(&security("common", SecurityForm::CommonStock))
            .expect("security should serialize");

        assert!(encoded.starts_with("\"v1/"));
        assert!(!encoded.contains("AAPL"));
        assert_eq!(
            Digest::of_terms(&terms).to_hex(),
            Digest::of_terms(&terms).to_hex()
        );
    }

    #[test]
    fn adr_share_class_should_require_a_ratio() {
        let terms = ShareClassTerms {
            issuer: issuer(),
            designation: "adr".into(),
            form: SecurityForm::Adr,
            par_value: None,
            adr_ratio: None,
        };

        assert!(matches!(
            ShareClassId::new(&terms),
            Err(ModelError::MissingAdrRatio { .. })
        ));
        assert!(
            ShareClassId::new(&ShareClassTerms {
                adr_ratio: Some(Quantity::from(2_u8)),
                ..terms
            })
            .is_ok()
        );
    }

    #[test]
    fn security_forms_should_stay_a_closed_set() {
        assert!(serde_json::from_str::<SecurityForm>("\"unit\"").is_err());
        assert!(serde_json::from_str::<SecurityForm>("\"warrant\"").is_err());
        assert_eq!(
            serde_json::from_str::<SecurityForm>("\"common_stock\"").expect("form should decode"),
            SecurityForm::CommonStock
        );
    }

    #[test]
    fn contract_address_should_require_evm_shape() {
        assert!(ContractAddress::new("0x0000000000000000000000000000000000000001").is_ok());
        assert!(ContractAddress::new("0000000000000000000000000000000000000001").is_err());
        assert!(ContractAddress::new("0xZZ").is_err());
    }

    #[test]
    fn venue_issued_token_should_keep_its_issuance_domain() {
        assert_ne!(
            venue_token("binance", "USDT").as_str(),
            venue_token("okx", "USDT").as_str()
        );
        assert_eq!(
            venue_token("binance", "USDT"),
            venue_token("binance", "USDT")
        );
    }

    #[test]
    fn wrapped_bridged_and_venue_issued_tokens_should_stay_distinct() {
        let native = CryptoTokenId::new(CryptoTokenOrigin::Chain {
            chain_id: crate::identifier::ChainId::new("1").expect("chain should be valid"),
            token: TokenReference::Native {
                code: IdentifierValue::new("ETH").expect("code should be valid"),
            },
        })
        .expect("token should be valid");
        let bridged = CryptoTokenId::new(CryptoTokenOrigin::Chain {
            chain_id: crate::identifier::ChainId::new("42161").expect("chain should be valid"),
            token: TokenReference::Contract {
                address: ContractAddress::new("0x0000000000000000000000000000000000000002")
                    .expect("address should be valid"),
            },
        })
        .expect("token should be valid");

        assert_ne!(native, bridged);
        assert_ne!(native, venue_token("binance", "ETH"));
    }

    #[test]
    fn a_contract_reference_should_match_its_chain_encoding() {
        let solana = CryptoTokenId::new(CryptoTokenOrigin::Chain {
            chain_id: crate::identifier::ChainId::new("solana").expect("chain should be valid"),
            token: TokenReference::Contract {
                address: ContractAddress::new("0x0000000000000000000000000000000000000001")
                    .expect("address should be valid"),
            },
        });

        assert!(
            matches!(solana, Err(ModelError::UnverifiedTokenOrigin { .. })),
            "an EVM-shaped address cannot identify a Solana token"
        );
    }

    #[test]
    fn commodity_should_reject_an_unknown_iso_code() {
        assert!(matches!(
            CommodityId::new(CommodityOrigin::Iso {
                code: IdentifierValue::new("ZZZ").expect("code should be valid"),
            }),
            Err(ModelError::InvalidCommodityCode { .. })
        ));
        assert!(
            CommodityId::new(CommodityOrigin::Iso {
                code: IdentifierValue::new("XAU").expect("code should be valid"),
            })
            .is_ok()
        );
        assert!(
            CommodityId::new(CommodityOrigin::Iso {
                code: IdentifierValue::new("AU").expect("code should be valid"),
            })
            .is_ok()
        );
        assert!(
            CommodityId::new(CommodityOrigin::Venue {
                namespace: IdentifierNamespace::new("cme").expect("namespace should be valid"),
                code: IdentifierValue::new("CL").expect("code should be valid"),
            })
            .is_ok()
        );
    }

    #[test]
    fn asset_id_should_not_be_constructible_from_a_bare_ticker() {
        assert!(serde_json::from_str::<AssetId>("\"BTC\"").is_err());
        assert!(
            serde_json::from_str::<AssetId>(r#"{"kind":"crypto_token","spec":"v1/short"}"#)
                .is_err()
        );
    }

    #[test]
    fn asset_id_should_round_trip_through_serde() {
        let asset = AssetId::Currency(CurrencyId::new(
            Iso4217Code::new("USD").expect("USD should be valid"),
        ));
        let encoded = serde_json::to_string(&asset).expect("asset should serialize");
        let decoded = serde_json::from_str::<AssetId>(&encoded).expect("asset should decode");

        assert_eq!(decoded, asset);
    }

    #[test]
    fn share_class_terms_should_reject_an_empty_designation() {
        let terms = ShareClassTerms {
            issuer: issuer(),
            designation: String::new().into(),
            form: SecurityForm::CommonStock,
            par_value: Some(Quantity::ZERO),
            adr_ratio: None,
        };

        assert!(matches!(
            ShareClassId::new(&terms),
            Err(ModelError::InvalidShareClassDesignation { .. })
        ));
    }

    #[test]
    fn iso_table_should_stay_sorted_and_unique() {
        let mut sorted = super::ISO_4217_CODES;
        sorted.sort_unstable();
        assert_eq!(sorted, super::ISO_4217_CODES);
        assert!(
            super::ISO_4217_CODES
                .windows(2)
                .all(|pair| pair[0] != pair[1])
        );
    }
}
