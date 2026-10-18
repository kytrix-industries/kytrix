//! Canonical asset terms and the display code generated from them.
//!
//! A display code is the one short label an asset is rendered as. It is generated
//! from the validated canonical terms of the asset — never from a native symbol, a
//! route or an identity digest — and every canonical asset has exactly one code,
//! without a type prefix and without a second form:
//!
//! | canonical type | code | source |
//! | --- | --- | --- |
//! | currency | ISO 4217 code | its own terms |
//! | crypto token, venue-issued or chain native | the verified native code | its own terms |
//! | crypto token, contract | the staged code | the route that publishes it |
//! | security | the verified ticker | the route that publishes it |
//! | commodity | its code | its own terms |
//! | index | `namespace/code` | its own terms |
//! | external reference | its reference value | its own terms |
//!
//! A contract token is known by its chain and its address and a security by its
//! issuer, designation and form, so neither states a code of its own: their code
//! comes from the staging of the route that publishes them, through
//! [`AssetDisplayCode::from_staged_code`].
//!
//! Nothing here decides whether two assets may share a code. A code is a pure
//! function of one asset's terms or of one staged value, so two assets can render
//! as the same text and both keep it; the display layer never renames an asset to
//! tell it apart from another one.

use std::fmt;

use smol_str::{SmolStr, format_smolstr};

use crate::{
    asset::{
        CommodityOrigin, CommodityTerms, CryptoTokenTerms, CurrencyTerms, IndexTerms,
        TokenReference,
    },
    display::{
        lexical::{ComponentText, normalize_component_text},
        values::DisplayFormatError,
    },
    reference::ExternalReferenceTerms,
};

/// The canonical terms one asset display code is generated from.
///
/// The variant is the canonical type of the asset, so the one code that type has
/// is generated from the one statement that states it.
#[derive(Clone, Copy, Debug)]
pub enum AssetDisplayTerms<'a> {
    /// ISO 4217 currency terms.
    Currency(&'a CurrencyTerms),
    /// Crypto token terms; a contract token states no code here.
    CryptoToken(&'a CryptoTokenTerms),
    /// Commodity terms.
    Commodity(&'a CommodityTerms),
    /// Index terms.
    Index(&'a IndexTerms),
    /// External reference terms, whose reference value is the code of a reference
    /// such as a venue-published custom oracle.
    External(&'a ExternalReferenceTerms),
}

/// The display code of one canonical asset.
///
/// The text is the normalized code itself: a structure character inside it is a
/// literal character of the code, so the type is a thin validated wrapper and not
/// an encoder. The code is private, so every code of the catalog was normalized
/// exactly once by [`AssetDisplayCode::from_terms`] or
/// [`AssetDisplayCode::from_staged_code`].
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AssetDisplayCode {
    text: SmolStr,
}

impl AssetDisplayCode {
    /// Builds the code the canonical terms of one asset state on their own.
    ///
    /// `Ok(None)` means the requested terms state no code at all, which is the
    /// contract token: it keeps its chain and its address, so it is named by the
    /// code a route staged for it. A security has no variant here at all — its
    /// issuer, designation and form state no code either — so the staged
    /// ticker is the only way a security is named, and only the canonical types
    /// whose own terms state a code are variants of this enum.
    pub fn from_terms(terms: AssetDisplayTerms<'_>) -> Result<Option<Self>, DisplayFormatError> {
        Ok(Some(Self {
            text: match terms {
                AssetDisplayTerms::Currency(terms) => {
                    normalize_component_text(ComponentText::Typed(terms.code.as_str()))?
                }
                AssetDisplayTerms::CryptoToken(terms) => match terms.origin.token() {
                    TokenReference::Contract { .. } => return Ok(None),
                    TokenReference::Native { code } => {
                        normalize_component_text(ComponentText::Typed(code.as_str()))?
                    }
                },
                AssetDisplayTerms::Commodity(terms) => match &terms.origin {
                    // The code of a commodity is its own code, whichever
                    // origin published it, and its origin is not part of a name.
                    CommodityOrigin::Iso { code } | CommodityOrigin::Venue { code, .. } => {
                        normalize_component_text(ComponentText::Typed(code.as_str()))?
                    }
                },
                // An index is the one asset whose code is two halves: the bare
                // code of an index is a number, so its namespace states which
                // index it is. The `/` between them is part of the code.
                AssetDisplayTerms::Index(terms) => format_smolstr!(
                    "{}/{}",
                    normalize_component_text(ComponentText::Typed(terms.namespace.as_str()))?,
                    normalize_component_text(ComponentText::Typed(terms.code.as_str()))?,
                ),
                // The issuer of an external reference is not part of a name: the
                // reference value is the code.
                AssetDisplayTerms::External(terms) => {
                    normalize_component_text(ComponentText::Typed(terms.reference.as_str()))?
                }
            },
        }))
    }

    /// Builds the code one route staged for one canonical asset.
    ///
    /// A staged code is free text: it is the value a venue published for an asset
    /// whose own terms state no code — the verified ticker of a security or the
    /// code of a contract token — so it runs through the whole display pipeline and
    /// is stored normalized. Two entries of one asset that disagree after that
    /// pipeline are a publication error, not two codes.
    pub fn from_staged_code(code: &str) -> Result<Self, DisplayFormatError> {
        Ok(Self {
            text: normalize_component_text(ComponentText::FreeText(code))?,
        })
    }

    /// Returns the display code.
    pub fn as_str(&self) -> &str {
        self.text.as_str()
    }
}

impl fmt::Display for AssetDisplayCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
