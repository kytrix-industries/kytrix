use std::{fmt, str::FromStr};

use bytes::BytesMut;
use serde::{Deserialize, Deserializer, Serialize, de};
use sha2::{Digest as _, Sha256};
use smol_str::{SmolStr, format_smolstr};

use crate::error::ModelError;

pub const IDENTITY_VERSION: &str = "v1";

pub const VENUE_LISTING_IDENTITY_LABEL: &str = "listing-id/v1";

pub const IDENTITY_DIGEST_LENGTH: usize = 64;

fn is_lowercase_hex_digit(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

pub trait CanonicalTerms {
    fn encode(&self, encoder: &mut CanonicalEncoder);
}

pub trait IdentityFamily {
    fn label() -> &'static str;

    fn namespace() -> &'static str;
}

pub trait IdentityKind: IdentityFamily + Sized {
    fn from_identity(value: CanonicalId) -> Self;

    fn identity(&self) -> &CanonicalId;
}

pub trait CanonicalIdentityTerms: CanonicalTerms + IdentityFamily {}

const IDENTITY_LABEL_MAX: usize = VENUE_LISTING_IDENTITY_LABEL.len();

const IDENTITY_TEXT_CAPACITY: usize = IDENTITY_LABEL_MAX + 1 + IDENTITY_DIGEST_LENGTH;

pub struct IdentityText {
    buffer: [u8; IDENTITY_TEXT_CAPACITY],
    length: usize,
}

impl IdentityText {
    #[doc(hidden)]
    pub fn render(label: &str, digest: Digest) -> Self {
        let mut buffer = [0_u8; IDENTITY_TEXT_CAPACITY];
        let label_bytes = label.as_bytes();
        debug_assert!(label_bytes.len() <= IDENTITY_LABEL_MAX);
        buffer[..label_bytes.len()].copy_from_slice(label_bytes);
        let mut at = label_bytes.len();
        buffer[at] = b'/';
        at += 1;
        let mut hex = [0_u8; IDENTITY_DIGEST_LENGTH];
        digest.write_hex(&mut hex);
        buffer[at..at + IDENTITY_DIGEST_LENGTH].copy_from_slice(&hex);
        at += IDENTITY_DIGEST_LENGTH;
        Self { buffer, length: at }
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buffer[..self.length]).expect("an identity is ASCII")
    }
}

impl std::ops::Deref for IdentityText {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for IdentityText {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for IdentityText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl fmt::Debug for IdentityText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), formatter)
    }
}

impl PartialEq for IdentityText {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for IdentityText {}

impl PartialEq<str> for IdentityText {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for IdentityText {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<IdentityText> for str {
    fn eq(&self, other: &IdentityText) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<IdentityText> for &str {
    fn eq(&self, other: &IdentityText) -> bool {
        *self == other.as_str()
    }
}

impl PartialOrd for IdentityText {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for IdentityText {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl std::hash::Hash for IdentityText {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl serde::Serialize for IdentityText {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Debug, Default)]
pub struct CanonicalEncoder {
    bytes: BytesMut,
}

impl CanonicalEncoder {
    pub fn new() -> Self {
        Self {
            bytes: BytesMut::with_capacity(256),
        }
    }

    pub fn write_label(&mut self, label: &str) {
        self.write_bytes(label.as_bytes());
    }

    pub fn write_bytes(&mut self, value: &[u8]) {
        self.bytes
            .extend_from_slice(&(value.len() as u64).to_be_bytes());
        self.bytes.extend_from_slice(value);
    }

    pub fn write_bool(&mut self, value: bool) {
        self.bytes.extend_from_slice(&[u8::from(value)]);
    }

    pub fn write_u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub fn write_i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub fn write_u128(&mut self, value: u128) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub fn write_i128(&mut self, value: i128) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    pub fn write_optional<T>(&mut self, value: Option<&T>, write: impl FnOnce(&mut Self, &T)) {
        match value {
            Some(value) => {
                self.write_bool(true);
                write(self, value);
            }
            None => self.write_bool(false),
        }
    }

    pub fn write_terms<T: CanonicalTerms + ?Sized>(&mut self, value: &T) {
        value.encode(self);
    }

    pub fn write_terms_slice<T: CanonicalTerms>(&mut self, values: &[T]) {
        self.write_u64(values.len() as u64);
        for value in values {
            self.write_terms(value);
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    pub fn of_terms<T: CanonicalTerms + ?Sized>(terms: &T) -> Self {
        let mut encoder = CanonicalEncoder::new();
        terms.encode(&mut encoder);
        Self::of_bytes(encoder.as_bytes())
    }

    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> SmolStr {
        format_smolstr!("{}", const_hex::encode(self.0))
    }

    fn write_hex(self, out: &mut [u8; IDENTITY_DIGEST_LENGTH]) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for (index, byte) in self.0.iter().enumerate() {
            out[index * 2] = HEX[(byte >> 4) as usize];
            out[index * 2 + 1] = HEX[(byte & 0x0f) as usize];
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalId(Digest);

impl CanonicalId {
    pub const fn of_digest(digest: Digest) -> Self {
        Self(digest)
    }

    pub fn identify(label: &'static str, value: &str) -> Result<Self, ModelError> {
        let invalid = || ModelError::InvalidIdentityValue {
            version: label,
            value: SmolStr::new(value),
        };
        let encoded = value
            .strip_prefix(label)
            .and_then(|rest| rest.strip_prefix('/'))
            .ok_or_else(invalid)?;
        if encoded.len() != IDENTITY_DIGEST_LENGTH || !encoded.bytes().all(is_lowercase_hex_digit) {
            return Err(invalid());
        }
        let mut digest = [0_u8; 32];
        const_hex::decode_to_slice(encoded, &mut digest).map_err(|_| invalid())?;
        Ok(Self(Digest(digest)))
    }

    pub const fn digest(&self) -> &Digest {
        &self.0
    }
}

impl FromStr for CanonicalId {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::identify(IDENTITY_VERSION, value)
    }
}

impl<'de> Deserialize<'de> for CanonicalId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = SmolStr::deserialize(deserializer)?;
        Self::identify(IDENTITY_VERSION, &value).map_err(de::Error::custom)
    }
}

pub fn identity_from_encoder<F: IdentityFamily>(encoder: &CanonicalEncoder) -> CanonicalId {
    let mut prelude = CanonicalEncoder::new();
    prelude.write_label(F::namespace());
    prelude.write_bytes(encoder.as_bytes());
    CanonicalId::of_digest(Digest::of_bytes(prelude.as_bytes()))
}

pub fn identity_from_terms<F: IdentityFamily, T: CanonicalTerms + ?Sized>(
    terms: &T,
) -> CanonicalId {
    let mut encoder = CanonicalEncoder::new();
    terms.encode(&mut encoder);
    identity_from_encoder::<F>(&encoder)
}

macro_rules! identity_family {
    ($family:ident, $label:expr, $namespace:literal) => {
        #[doc = concat!("The `", $namespace, "` canonical identity family.")]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $family {}

        impl IdentityFamily for $family {
            fn label() -> &'static str {
                $label
            }

            fn namespace() -> &'static str {
                $namespace
            }
        }
    };
}

identity_family!(CurrencyIdentity, IDENTITY_VERSION, "currency");
identity_family!(CryptoTokenIdentity, IDENTITY_VERSION, "crypto-token");
identity_family!(SecurityIdentity, IDENTITY_VERSION, "security");
identity_family!(ShareClassIdentity, IDENTITY_VERSION, "share-class");
identity_family!(CommodityIdentity, IDENTITY_VERSION, "commodity");
identity_family!(IndexIdentity, IDENTITY_VERSION, "index");
identity_family!(PriceReferenceIdentity, IDENTITY_VERSION, "price-reference");
identity_family!(
    ExternalReferenceIdentity,
    IDENTITY_VERSION,
    "external-reference"
);
identity_family!(InstrumentIdentity, IDENTITY_VERSION, "instrument");
identity_family!(OutcomeEventIdentity, IDENTITY_VERSION, "outcome-event");
identity_family!(OutcomeGroupIdentity, IDENTITY_VERSION, "outcome-group");
identity_family!(OutcomeMemberIdentity, IDENTITY_VERSION, "outcome-member");
identity_family!(OutcomeSideIdentity, IDENTITY_VERSION, "outcome-side");
identity_family!(
    OutcomeRiskPoolIdentity,
    IDENTITY_VERSION,
    "outcome-risk-pool"
);
identity_family!(
    VenueListingIdentity,
    VENUE_LISTING_IDENTITY_LABEL,
    "venue-listing"
);

#[macro_export]
macro_rules! identity_type {
    ($name:ident, $family:ident) => {
        #[doc = concat!("A canonical `", stringify!($family), "` identity.")]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name($crate::identifier::CanonicalId);

        impl $name {
            pub fn of_terms<T: $crate::identifier::CanonicalTerms + ?Sized>(terms: &T) -> Self {
                Self($crate::identifier::identity_from_terms::<$family, T>(terms))
            }

            pub fn from_identity_value(value: &str) -> Result<Self, $crate::error::ModelError> {
                Ok(Self($crate::identifier::CanonicalId::identify(
                    <$family as $crate::identifier::IdentityFamily>::label(),
                    value,
                )?))
            }

            pub fn as_str(&self) -> $crate::identifier::IdentityText {
                $crate::identifier::IdentityText::render(
                    <$family as $crate::identifier::IdentityFamily>::label(),
                    *self.0.digest(),
                )
            }
        }

        impl $crate::identifier::IdentityFamily for $name {
            fn label() -> &'static str {
                <$family as $crate::identifier::IdentityFamily>::label()
            }

            fn namespace() -> &'static str {
                <$family as $crate::identifier::IdentityFamily>::namespace()
            }
        }

        impl $crate::identifier::IdentityKind for $name {
            fn from_identity(value: $crate::identifier::CanonicalId) -> Self {
                Self(value)
            }

            fn identity(&self) -> &$crate::identifier::CanonicalId {
                &self.0
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                formatter.write_str(self.as_str().as_str())
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: ::serde::Serializer,
            {
                serializer.serialize_str(self.as_str().as_str())
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::error::ModelError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::from_identity_value(value)
            }
        }

        impl $crate::identifier::CanonicalTerms for $name {
            fn encode(&self, encoder: &mut $crate::identifier::CanonicalEncoder) {
                encoder.write_label(self.as_str().as_str());
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: ::serde::Deserializer<'de>,
            {
                let value = <smol_str::SmolStr as ::serde::Deserialize>::deserialize(deserializer)?;
                Self::from_identity_value(&value).map_err(::serde::de::Error::custom)
            }
        }
    };
}

pub(crate) use identity_type;

fn is_namespace_text(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
}

fn is_graphic_text(value: &str) -> bool {
    !value.is_empty()
        && value.chars().count() <= 128
        && !value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

fn is_uppercase_alphanumeric(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

pub fn is_evm_address(value: &str) -> bool {
    value
        .strip_prefix("0x")
        .is_some_and(|hex| hex.len() == 40 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

pub fn is_base58_address(value: &str) -> bool {
    (32..=44).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() && !matches!(byte, b'0' | b'O' | b'I' | b'l'))
}

pub fn is_chain_address(value: &str) -> bool {
    is_evm_address(value) || is_base58_address(value)
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct IdentifierNamespace(SmolStr);

impl IdentifierNamespace {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ModelError> {
        let value = value.as_ref();
        if !is_namespace_text(value) {
            return Err(ModelError::InvalidIdentifierNamespace {
                value: SmolStr::new(value),
            });
        }
        Ok(Self(SmolStr::new(value)))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for IdentifierNamespace {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for IdentifierNamespace {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<SmolStr> for IdentifierNamespace {
    type Error = ModelError;

    fn try_from(value: SmolStr) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<IdentifierNamespace> for SmolStr {
    fn from(value: IdentifierNamespace) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for IdentifierNamespace {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = SmolStr::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct IdentifierValue(SmolStr);

impl IdentifierValue {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ModelError> {
        let value = value.as_ref();
        if value.is_empty() {
            return Err(ModelError::EmptyIdentifier { field: "value" });
        }
        if !is_graphic_text(value) {
            return Err(ModelError::InvalidIdentifierValue {
                value: SmolStr::new(value),
            });
        }
        Ok(Self(SmolStr::new(value)))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for IdentifierValue {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for IdentifierValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for IdentifierValue {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<SmolStr> for IdentifierValue {
    type Error = ModelError;

    fn try_from(value: SmolStr) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<IdentifierValue> for SmolStr {
    fn from(value: IdentifierValue) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for IdentifierValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = SmolStr::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct NativeSymbol(SmolStr);

impl NativeSymbol {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ModelError> {
        let value = value.as_ref();
        if value.is_empty() {
            return Err(ModelError::EmptyIdentifier {
                field: "native_symbol",
            });
        }
        if !is_graphic_text(value) {
            return Err(ModelError::InvalidNativeIdentifier {
                symbol: SmolStr::new(value),
            });
        }
        Ok(Self(SmolStr::new(value)))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for NativeSymbol {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<NativeSymbol> for SmolStr {
    fn from(value: NativeSymbol) -> Self {
        value.0
    }
}

impl From<&NativeSymbol> for SmolStr {
    fn from(value: &NativeSymbol) -> Self {
        value.0.clone()
    }
}

impl fmt::Display for NativeSymbol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for NativeSymbol {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<SmolStr> for NativeSymbol {
    type Error = ModelError;

    fn try_from(value: SmolStr) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for NativeSymbol {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = SmolStr::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum VenueId {
    #[serde(rename = "binance")]
    Binance,
    #[serde(rename = "hyperliquid")]
    Hyperliquid,
    #[serde(rename = "longbridge")]
    Longbridge,
    #[serde(rename = "okx")]
    Okx,
    #[serde(rename = "polymarket")]
    Polymarket,
}

impl VenueId {
    pub const ALL: [Self; 5] = [
        Self::Binance,
        Self::Hyperliquid,
        Self::Longbridge,
        Self::Okx,
        Self::Polymarket,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Binance => "binance",
            Self::Hyperliquid => "hyperliquid",
            Self::Longbridge => "longbridge",
            Self::Okx => "okx",
            Self::Polymarket => "polymarket",
        }
    }

    pub fn from_name(value: &str) -> Result<Self, ModelError> {
        Self::ALL
            .into_iter()
            .find(|venue| venue.as_str() == value)
            .ok_or_else(|| ModelError::UnknownVenue {
                value: SmolStr::new(value),
            })
    }
}

impl fmt::Display for VenueId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct VenueScopeId(SmolStr);

impl VenueScopeId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ModelError> {
        let value = value.as_ref();
        let valid = !value.is_empty()
            && value.len() <= 64
            && value
                .split('/')
                .all(|segment| is_namespace_text(segment) && segment.len() <= 32);
        if !valid {
            return Err(ModelError::InvalidVenueScopeId {
                value: SmolStr::new(value),
            });
        }
        Ok(Self(SmolStr::new(value)))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for VenueScopeId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for VenueScopeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for VenueScopeId {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl From<VenueScopeId> for SmolStr {
    fn from(value: VenueScopeId) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for VenueScopeId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = SmolStr::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ReferenceTermsVersion(SmolStr);

impl ReferenceTermsVersion {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ModelError> {
        let value = value.as_ref();
        if value.is_empty() {
            return Err(ModelError::EmptyIdentifier {
                field: "reference_terms_version",
            });
        }
        if value.len() > 16 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(ModelError::InvalidReferenceTermsVersion {
                value: SmolStr::new(value),
            });
        }
        Ok(Self(SmolStr::new(value)))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for ReferenceTermsVersion {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for ReferenceTermsVersion {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl From<ReferenceTermsVersion> for SmolStr {
    fn from(value: ReferenceTermsVersion) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for ReferenceTermsVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = SmolStr::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

macro_rules! text_identifier {
    ($name:ident, $kind:literal) => {
        #[doc = concat!("A validated ", $kind, ".")]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(SmolStr);

        impl $name {
            #[doc = concat!("Validates a ", $kind, " exactly as published.")]
            pub fn new(value: impl AsRef<str>) -> Result<Self, ModelError> {
                let value = value.as_ref();
                if value.is_empty() {
                    return Err(ModelError::EmptyIdentifier { field: $kind });
                }
                if !is_graphic_text(value) {
                    return Err(ModelError::InvalidIdentifierValue {
                        value: SmolStr::new(value),
                    });
                }
                Ok(Self(SmolStr::new(value)))
            }

            #[doc = concat!("Returns the canonical ", $kind, ".")]
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl FromStr for $name {
            type Err = ModelError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<SmolStr> for $name {
            type Error = ModelError;

            fn try_from(value: SmolStr) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = ModelError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for SmolStr {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = SmolStr::deserialize(deserializer)?;
                Self::new(value).map_err(de::Error::custom)
            }
        }
    };
}

macro_rules! normalized_identifier {
    ($name:ident, $kind:literal, $normalize:expr, $invalid:expr) => {
        #[doc = concat!("A validated ", $kind, ".")]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(SmolStr);

        impl $name {
            #[doc = concat!("Validates a ", $kind, " and normalizes its case.")]
            pub fn new(value: impl AsRef<str>) -> Result<Self, ModelError> {
                let value = value.as_ref();
                let normalize: fn(&str) -> Option<SmolStr> = $normalize;
                let Some(normalized) = normalize(value) else {
                    return Err($invalid(SmolStr::new(value)));
                };
                Ok(Self(normalized))
            }

            #[doc = concat!("Returns the canonical ", $kind, ".")]
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl FromStr for $name {
            type Err = ModelError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<SmolStr> for $name {
            type Error = ModelError;

            fn try_from(value: SmolStr) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for SmolStr {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = SmolStr::deserialize(deserializer)?;
                Self::new(value).map_err(de::Error::custom)
            }
        }
    };
}

fn normalize_mic(value: &str) -> Option<SmolStr> {
    let normalized = value.to_ascii_uppercase();
    (normalized.len() == 4 && is_uppercase_alphanumeric(&normalized))
        .then(|| SmolStr::new(normalized))
}

fn normalize_hex_32(value: &str) -> Option<SmolStr> {
    let hex = value.strip_prefix("0x").unwrap_or(value);
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(format_smolstr!("0x{}", hex.to_ascii_lowercase()))
}

fn normalize_uint256(value: &str) -> Option<SmolStr> {
    if value.starts_with("0x") {
        return normalize_hex_32(value);
    }
    let decimal =
        !value.is_empty() && value.len() <= 78 && value.bytes().all(|byte| byte.is_ascii_digit());
    if !decimal {
        return None;
    }
    let trimmed = value.trim_start_matches('0');
    Some(if trimmed.is_empty() {
        SmolStr::new_static("0")
    } else {
        SmolStr::new(trimmed)
    })
}

text_identifier!(ReferenceSourceId, "reference source");
text_identifier!(ResolutionSourceId, "resolution source");
text_identifier!(TradingCalendarId, "trading calendar");

impl TradingCalendarId {
    pub const CONTINUOUS_24X7: &'static str = "continuous-24x7";

    pub fn continuous_24x7() -> Self {
        Self(SmolStr::new_static(Self::CONTINUOUS_24X7))
    }
}

text_identifier!(BoardId, "board");
text_identifier!(ContractCode, "contract code");
text_identifier!(MarketParticipantSeriesId, "market participant series");
text_identifier!(LocalSymbol, "local symbol");
text_identifier!(EquityLocalCode, "equity local code");
text_identifier!(OccSymbol, "OCC option symbol");
text_identifier!(PolymarketEventId, "Polymarket event id");
text_identifier!(PolymarketMarketId, "Polymarket market id");
text_identifier!(TokenIssuerId, "token issuer");
text_identifier!(ShareClassId, "share class");
text_identifier!(MarketParticipantAccountId, "market participant account");
text_identifier!(HyperliquidQuestionId, "Hyperliquid question id");

normalized_identifier!(
    Mic,
    "ISO 10383 market identifier code",
    normalize_mic,
    |value| ModelError::InvalidMic { value }
);
normalized_identifier!(
    CtfConditionId,
    "CTF condition id",
    normalize_hex_32,
    |value| ModelError::InvalidCtfConditionId { value }
);
normalized_identifier!(CtfTokenId, "CTF token id", normalize_uint256, |value| {
    ModelError::InvalidCtfTokenId { value }
});

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ChainId(SmolStr);

impl ChainId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ModelError> {
        let value = value.as_ref();
        let numeric = !value.is_empty()
            && value.len() <= 20
            && value.bytes().all(|byte| byte.is_ascii_digit())
            && value.parse::<u64>().is_ok();
        let named = is_namespace_text(value) && !value.bytes().any(|byte| byte.is_ascii_digit());
        if numeric || named {
            return Ok(Self(SmolStr::new(value)));
        }
        Err(ModelError::InvalidChainId {
            value: SmolStr::new(value),
        })
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for ChainId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for ChainId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ChainId {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl From<ChainId> for SmolStr {
    fn from(value: ChainId) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for ChainId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = SmolStr::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct WalletAddress(SmolStr);

impl WalletAddress {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ModelError> {
        let value = value.as_ref();
        if is_chain_address(value) {
            return Ok(Self(SmolStr::new(value)));
        }
        Err(ModelError::InvalidWalletAddress {
            value: SmolStr::new(value),
        })
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl AsRef<str> for WalletAddress {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for WalletAddress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for WalletAddress {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl From<WalletAddress> for SmolStr {
    fn from(value: WalletAddress) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for WalletAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = SmolStr::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct TimeZoneId(SmolStr);

const RESERVED_TIME_ZONE_PREFIXES: [&str; 4] = ["Etc/", "Factory", "localtime", "posix"];

impl TimeZoneId {
    pub fn new(value: impl AsRef<str>) -> Result<Self, ModelError> {
        let value = value.as_ref();
        let invalid = || ModelError::InvalidTimeZoneId {
            value: SmolStr::new(value),
        };
        let segments: Vec<&str> = value.split('/').collect();
        if segments.len() > 3 || segments.iter().any(|segment| !is_iana_segment(segment)) {
            return Err(invalid());
        }
        if RESERVED_TIME_ZONE_PREFIXES
            .iter()
            .any(|reserved| value.starts_with(reserved))
        {
            return Err(invalid());
        }
        Ok(Self(SmolStr::new(value)))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

fn is_iana_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 24
        && segment.starts_with(|character: char| character.is_ascii_uppercase())
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphabetic() || matches!(byte, b'_' | b'-' | b'.' | b'+'))
}

impl AsRef<str> for TimeZoneId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for TimeZoneId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for TimeZoneId {
    type Err = ModelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl From<TimeZoneId> for SmolStr {
    fn from(value: TimeZoneId) -> Self {
        value.0
    }
}

impl<'de> Deserialize<'de> for TimeZoneId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = SmolStr::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

pub fn is_lowercase_hex_digest(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(is_lowercase_hex_digit)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, str::FromStr};

    use crate::{
        decimal::Quantity,
        error::ModelError,
        identifier::{
            CanonicalEncoder, CanonicalId, CanonicalTerms, ChainId, CtfConditionId, CtfTokenId,
            Digest, IDENTITY_DIGEST_LENGTH, IDENTITY_VERSION, IdentifierNamespace, IdentifierValue,
            InstrumentIdentity, Mic, NativeSymbol, TimeZoneId, TokenIssuerId, VenueId,
            VenueListingIdentity, VenueScopeId, WalletAddress,
        },
    };

    identity_type!(TestInstrumentId, InstrumentIdentity);
    identity_type!(TestVenueListingId, VenueListingIdentity);

    const PINNED_ENCODER_DIGEST: &str =
        "v1/58ff05273e77d54f198ec94ff331c99e7bda1e0732aec5ac984630ec1103dbe4";

    #[derive(PartialEq, Eq, Hash)]
    struct Terms {
        quantity: Quantity,
        nested: Vec<Quantity>,
    }

    impl CanonicalTerms for Terms {
        fn encode(&self, encoder: &mut CanonicalEncoder) {
            encoder.write_label("terms");
            encoder.write_u128(self.quantity.inner());
            encoder.write_u64(self.nested.len() as u64);
            for value in &self.nested {
                encoder.write_u128(value.inner());
            }
        }
    }

    #[test]
    fn canonical_encoder_should_be_deterministic_for_the_same_terms() {
        let terms = Terms {
            quantity: Quantity::from(5_u8),
            nested: vec![Quantity::from(1_u8), Quantity::from(2_u8)],
        };
        let same = Terms {
            quantity: Quantity::from(5_u8),
            nested: vec![Quantity::from(1_u8), Quantity::from(2_u8)],
        };
        let reordered = Terms {
            quantity: Quantity::from(5_u8),
            nested: vec![Quantity::from(2_u8), Quantity::from(1_u8)],
        };

        assert_eq!(
            TestInstrumentId::of_terms(&terms).as_str(),
            PINNED_ENCODER_DIGEST
        );
        assert_eq!(
            Digest::of_terms(&terms).to_hex(),
            Digest::of_terms(&same).to_hex()
        );
        assert_ne!(
            Digest::of_terms(&terms).to_hex(),
            Digest::of_terms(&reordered).to_hex(),
            "the order of a collection is part of the encoded body"
        );
        assert_eq!(Digest::of_terms(&terms).as_bytes().len(), 32);
    }

    #[test]
    fn identity_families_should_separate_otherwise_identical_terms() {
        let terms = Terms {
            quantity: Quantity::from(5_u8),
            nested: Vec::new(),
        };

        assert_ne!(
            TestInstrumentId::of_terms(&terms).as_str(),
            TestVenueListingId::of_terms(&terms).as_str()
        );
        assert!(
            TestInstrumentId::of_terms(&terms)
                .as_str()
                .starts_with("v1/")
        );
        assert!(
            TestVenueListingId::of_terms(&terms)
                .as_str()
                .starts_with("listing-id/v1/")
        );
    }

    #[test]
    fn instrument_identity_should_render_a_versioned_full_digest() {
        let id = TestInstrumentId::of_terms(&Terms {
            quantity: Quantity::from(5_u8),
            nested: Vec::new(),
        });
        let value = id.as_str();

        assert!(value.starts_with("v1/"));
        assert_eq!(
            value.len(),
            IDENTITY_VERSION.len() + 1 + IDENTITY_DIGEST_LENGTH
        );
        assert!(
            value[3..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
    }

    #[test]
    fn venue_listing_identity_should_use_the_listing_label() {
        let id = TestVenueListingId::of_terms(&Terms {
            quantity: Quantity::from(5_u8),
            nested: Vec::new(),
        });

        assert!(id.as_str().starts_with("listing-id/v1/"));
        assert_eq!(
            id.as_str().len(),
            "listing-id/v1".len() + 1 + IDENTITY_DIGEST_LENGTH
        );
    }

    #[test]
    fn identities_should_reject_truncated_or_uppercase_digests() {
        let uppercase = format!("v1/{}", "A".repeat(IDENTITY_DIGEST_LENGTH));
        let truncated = format!("v1/{}", "a".repeat(IDENTITY_DIGEST_LENGTH - 1));

        assert!(TestInstrumentId::from_identity_value(&uppercase).is_err());
        assert!(TestInstrumentId::from_identity_value(&truncated).is_err());
        assert!(TestInstrumentId::from_identity_value("v2/0000").is_err());
        assert!(
            TestVenueListingId::from_identity_value(&format!(
                "v1/{}",
                "a".repeat(IDENTITY_DIGEST_LENGTH)
            ))
            .is_err(),
            "a listing identity must carry the listing-id label"
        );
    }

    #[test]
    fn identities_should_round_trip_through_serde() {
        let id = TestInstrumentId::of_terms(&Terms {
            quantity: Quantity::from(5_u8),
            nested: Vec::new(),
        });
        let encoded = serde_json::to_string(&id).expect("identity should serialize");
        let decoded =
            serde_json::from_str::<TestInstrumentId>(&encoded).expect("identity should decode");

        assert_eq!(decoded, id);
        assert_eq!(
            CanonicalId::from_str(id.as_str().as_str()).expect("identity should parse"),
            CanonicalId::from_str(encoded.trim_matches('"')).expect("the wire text parses"),
        );
    }

    #[test]
    fn namespace_should_reject_uppercase_and_unicode_text() {
        assert!(IdentifierNamespace::new("binance").is_ok());
        assert_eq!(
            IdentifierNamespace::new("Binance"),
            Err(ModelError::InvalidIdentifierNamespace {
                value: "Binance".into()
            })
        );
        assert!(IdentifierNamespace::new("binance spot").is_err());
        assert!(IdentifierNamespace::new("").is_err());
    }

    #[test]
    fn identifier_value_should_reject_whitespace_only_text() {
        assert_eq!(
            IdentifierValue::new(""),
            Err(ModelError::EmptyIdentifier { field: "value" })
        );
        assert!(matches!(
            IdentifierValue::new(" "),
            Err(ModelError::InvalidIdentifierValue { .. })
        ));
        assert!(IdentifierValue::new("trade\nid").is_err());
    }

    #[test]
    fn native_symbol_should_reject_empty_and_whitespace_values() {
        assert_eq!(
            NativeSymbol::new(""),
            Err(ModelError::EmptyIdentifier {
                field: "native_symbol"
            })
        );
        assert!(NativeSymbol::new(" BTCUSDT ").is_err());
        assert!(NativeSymbol::new("BTC\nUSDT").is_err());
    }

    #[test]
    fn native_symbol_should_round_trip_without_normalization() {
        let symbol = NativeSymbol::new("@1").expect("protocol symbol should be valid");
        let encoded = serde_json::to_string(&symbol).expect("symbol should serialize");
        let decoded = serde_json::from_str::<NativeSymbol>(&encoded).expect("symbol should decode");

        assert_eq!(decoded, symbol);
        assert_eq!(decoded.as_str(), "@1");
    }

    #[test]
    fn venue_scope_should_accept_separated_segments_only() {
        assert!(VenueScopeId::new("perp-dex/xyz").is_ok());
        assert!(VenueScopeId::new("core-spot").is_ok());
        assert!(VenueScopeId::new("perp-dex//xyz").is_err());
        assert!(VenueScopeId::new("/perp-dex").is_err());
        assert!(VenueScopeId::new("perp-dex/XYZ").is_err());
    }

    #[test]
    fn venue_should_accept_only_the_venues_this_build_speaks() {
        assert_eq!(
            VenueId::from_name("binance").expect("binance is a venue of this build"),
            VenueId::Binance
        );
        assert_eq!(
            VenueId::from_name("longbridge").expect("longbridge is a venue of this build"),
            VenueId::Longbridge
        );
        assert_eq!(
            VenueId::from_name("Binance"),
            Err(ModelError::UnknownVenue {
                value: "Binance".into()
            }),
            "the canonical text is lowercase, so a display form is not an identity"
        );
        assert!(VenueId::from_name("kraken").is_err());
        assert_eq!(
            VenueId::ALL.map(VenueId::as_str),
            ["binance", "hyperliquid", "longbridge", "okx", "polymarket"],
            "the canonical order is what every venue-keyed map is ordered by"
        );
        assert_eq!(
            VenueId::ALL.len(),
            5,
            "the venue set is the five venues this build speaks"
        );
    }

    #[test]
    fn a_venue_reads_as_the_canonical_text_it_is_written_as() {
        for venue in VenueId::ALL {
            assert_eq!(venue.as_str(), venue.to_string());
        }
        assert_eq!(VenueId::Longbridge.as_str(), "longbridge");
        assert_eq!(VenueId::Longbridge.to_string(), "longbridge");
    }

    #[test]
    fn mic_should_accept_real_alphanumeric_codes() {
        assert_eq!(
            Mic::new("xnas").expect("MIC should normalize").as_str(),
            "XNAS"
        );
        assert_eq!(
            Mic::new("360T")
                .expect("an alphanumeric MIC is valid")
                .as_str(),
            "360T"
        );
        assert!(Mic::new("XNASX").is_err());
        assert!(Mic::new("XN-S").is_err());
    }

    #[test]
    fn ctf_condition_id_should_accept_a_real_hex_value() {
        let id = CtfConditionId::new(
            "0xABCDEF00112233445566778899aAbBcCdDeEfF00112233445566778899001122",
        )
        .expect("a hex condition id should be valid");

        assert!(id.as_str().starts_with("0x"));
        assert_eq!(id.as_str().len(), 66);
        assert!(CtfConditionId::new("not-hex").is_err());
    }

    #[test]
    fn ctf_token_id_should_accept_decimal_and_hex_values() {
        assert_eq!(
            CtfTokenId::new("000123")
                .expect("a decimal token id is valid")
                .as_str(),
            "123"
        );
        assert_eq!(
            CtfTokenId::new("0")
                .expect("zero is a valid token id")
                .as_str(),
            "0"
        );
        assert!(
            CtfTokenId::new("0xabcdef00112233445566778899aabbccddeeff00112233445566778899001122")
                .is_ok()
        );
        assert!(CtfTokenId::new("12a").is_err());
    }

    #[test]
    fn token_issuer_should_be_stable_across_adapters() {
        let first = TokenIssuerId::new("binance").expect("issuer should be valid");
        let second = TokenIssuerId::new("binance").expect("issuer should be valid");

        assert_eq!(first, second);
        assert_eq!(
            BTreeSet::from([first, second]).len(),
            1,
            "the same issuance domain must collapse to one issuer id"
        );
    }

    #[test]
    fn chain_id_should_accept_numeric_and_named_chains() {
        assert!(ChainId::new("1").is_ok());
        assert!(ChainId::new("solana").is_ok());
        assert!(ChainId::new("Solana").is_err());
        assert!(ChainId::new("0x1").is_err());
    }

    #[test]
    fn wallet_address_should_accept_evm_and_base58_encodings() {
        assert!(WalletAddress::new("0x0000000000000000000000000000000000000001").is_ok());
        assert!(WalletAddress::new("11111111111111111111111111111111").is_ok());
        assert!(WalletAddress::new("0xnothex").is_err());
        assert!(WalletAddress::new("short").is_err());
        assert!(WalletAddress::new("").is_err());
    }

    #[test]
    fn time_zone_should_reject_invalid_and_reserved_identifiers() {
        assert!(TimeZoneId::new("America/New_York").is_ok());
        assert!(TimeZoneId::new("UTC").is_ok());
        assert!(TimeZoneId::new("Asia/Shanghai").is_ok());
        assert!(TimeZoneId::new("America/Argentina/Buenos_Aires").is_ok());
        assert!(TimeZoneId::new("america/new_york").is_err());
        assert!(TimeZoneId::new("America//New_York").is_err());
        assert!(TimeZoneId::new("Europe/London/Extra/More").is_err());
        assert!(TimeZoneId::new("Etc/GMT+8").is_err());
        assert!(TimeZoneId::new("Factory").is_err());
        assert!(TimeZoneId::new("").is_err());
    }

    #[test]
    fn hex_digest_helper_should_only_accept_the_persisted_shape() {
        assert!(crate::identifier::is_lowercase_hex_digest(
            &"a".repeat(IDENTITY_DIGEST_LENGTH),
            IDENTITY_DIGEST_LENGTH
        ));
        assert!(!crate::identifier::is_lowercase_hex_digest(
            &"A".repeat(IDENTITY_DIGEST_LENGTH),
            IDENTITY_DIGEST_LENGTH
        ));
    }
}
