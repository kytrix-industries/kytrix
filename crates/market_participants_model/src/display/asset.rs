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

#[derive(Clone, Copy, Debug)]
pub enum AssetDisplayTerms<'a> {
    Currency(&'a CurrencyTerms),
    CryptoToken(&'a CryptoTokenTerms),
    Commodity(&'a CommodityTerms),
    Index(&'a IndexTerms),
    External(&'a ExternalReferenceTerms),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AssetDisplayCode {
    text: SmolStr,
}

impl AssetDisplayCode {
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
                    CommodityOrigin::Iso { code } | CommodityOrigin::Venue { code, .. } => {
                        normalize_component_text(ComponentText::Typed(code.as_str()))?
                    }
                },
                AssetDisplayTerms::Index(terms) => format_smolstr!(
                    "{}/{}",
                    normalize_component_text(ComponentText::Typed(terms.namespace.as_str()))?,
                    normalize_component_text(ComponentText::Typed(terms.code.as_str()))?,
                ),
                AssetDisplayTerms::External(terms) => {
                    normalize_component_text(ComponentText::Typed(terms.reference.as_str()))?
                }
            },
        }))
    }

    pub fn from_staged_code(code: &str) -> Result<Self, DisplayFormatError> {
        Ok(Self {
            text: normalize_component_text(ComponentText::FreeText(code))?,
        })
    }

    pub fn as_str(&self) -> &str {
        self.text.as_str()
    }
}

impl fmt::Display for AssetDisplayCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
