//! Shared fixtures of the display unit tests.
//!
//! Every fixture builds one canonical model value or one resolved display code,
//! so a test states a single economic contract and then asserts the exact string
//! the formatter publishes for it. Nothing here is part of the production path.

use crate::{
    asset::{
        AssetId, CommodityOrigin, CommodityTerms, ContractAddress, CryptoTokenOrigin,
        CryptoTokenTerms, CurrencyId, CurrencyTerms, IndexTerms, Iso4217Code, TokenReference,
    },
    decimal::UnixMillis,
    display::asset::{AssetDisplayCode, AssetDisplayTerms},
    identifier::{
        ChainId, IdentifierNamespace, IdentifierValue, ReferenceSourceId, ReferenceTermsVersion,
        TokenIssuerId,
    },
    listing::venue::PriceBasis,
    reference::{
        ExternalReferenceTerms, ReferenceAggregation, ReferenceObservation,
        ReferenceObservationKind, ReferenceSessionConvention, ReferenceSettlementConvention,
    },
};

/// 2025-12-19T00:00:00Z.
pub(super) const MIDNIGHT_20251219: UnixMillis = 1_766_102_400_000;

/// 2025-12-26T00:00:00Z.
pub(super) const MIDNIGHT_20251226: UnixMillis = 1_766_707_200_000;

/// 2025-12-31T00:00:00Z.
pub(super) const MIDNIGHT_20251231: UnixMillis = 1_767_139_200_000;

/// 2025-12-19T08:00:00Z.
pub(super) const AT_20251219T080000Z: UnixMillis = 1_766_131_200_000;

/// 2025-12-19T15:30:00Z.
pub(super) const AT_20251219T153000Z: UnixMillis = 1_766_158_200_000;

/// 2025-12-19T00:00:00.250Z.
pub(super) const AT_20251219T000000_250Z: UnixMillis = 1_766_102_400_250;

/// The canonical identity of the fiat currency `code`.
pub(super) fn currency_asset(code: &str) -> AssetId {
    AssetId::Currency(CurrencyId::new(
        Iso4217Code::new(code).expect("a published ISO 4217 code"),
    ))
}

/// The canonical terms of the fiat currency `code`.
pub(super) fn currency_terms(code: &str) -> CurrencyTerms {
    CurrencyTerms {
        code: Iso4217Code::new(code).expect("a published ISO 4217 code"),
    }
}

/// The code of the fiat currency `code`.
pub(super) fn currency_code(code: &str) -> AssetDisplayCode {
    code_of(AssetDisplayTerms::Currency(&currency_terms(code)))
}

/// The canonical terms of a venue-issued token with `code` as its native code.
///
/// The issuer and the namespace are deliberately different, so a code that
/// carried either of them instead of the native code would still compile.
pub(super) fn venue_token_terms(code: &str) -> CryptoTokenTerms {
    CryptoTokenTerms {
        origin: CryptoTokenOrigin::VenueIssued {
            issuer: TokenIssuerId::new("okx").expect("okx is an issuer"),
            namespace: IdentifierNamespace::new("okx-swap").expect("okx-swap is a namespace"),
            token: TokenReference::Native {
                code: IdentifierValue::new(code).expect("a token code"),
            },
        },
    }
}

/// The canonical terms of a chain token identified by its contract address.
pub(super) fn contract_token_terms(address: &str) -> CryptoTokenTerms {
    CryptoTokenTerms {
        origin: CryptoTokenOrigin::Chain {
            chain_id: ChainId::new("1").expect("1 is a chain"),
            token: TokenReference::Contract {
                address: ContractAddress::new(address).expect("an EVM contract address"),
            },
        },
    }
}

/// The code of the venue-issued token `code`.
pub(super) fn token_code(code: &str) -> AssetDisplayCode {
    code_of(AssetDisplayTerms::CryptoToken(&venue_token_terms(code)))
}

/// The canonical terms of the commodity `code`.
pub(super) fn commodity_terms(code: &str) -> CommodityTerms {
    CommodityTerms {
        origin: CommodityOrigin::Iso {
            code: IdentifierValue::new(code).expect("a commodity code"),
        },
    }
}

/// The code of the commodity `code`.
pub(super) fn commodity_code(code: &str) -> AssetDisplayCode {
    code_of(AssetDisplayTerms::Commodity(&commodity_terms(code)))
}

/// The canonical terms of the index `namespace/code`.
pub(super) fn index_terms(namespace: &str, code: &str) -> IndexTerms {
    IndexTerms {
        namespace: IdentifierNamespace::new(namespace).expect("an index namespace"),
        code: IdentifierValue::new(code).expect("an index code"),
    }
}

/// The code of the index `namespace/code`.
pub(super) fn index_code(namespace: &str, code: &str) -> AssetDisplayCode {
    code_of(AssetDisplayTerms::Index(&index_terms(namespace, code)))
}

/// The validated external reference terms of `reference`.
///
/// The remaining fields are the ones an external reference always states; they do
/// not take part in the display code, which is the reference value alone.
pub(super) fn external_terms(reference: &str) -> ExternalReferenceTerms {
    ExternalReferenceTerms {
        issuer: IdentifierNamespace::new("hyperliquid").expect("an external reference issuer"),
        reference: IdentifierValue::new(reference).expect("an external reference value"),
        quote_asset: currency_asset("USD"),
        basis: PriceBasis::PerAssetUnit,
        observation: ReferenceObservation {
            source: ReferenceSourceId::new("hyperliquid")
                .expect("a reference source is publishable"),
            kind: ReferenceObservationKind::Index,
        },
        aggregation: ReferenceAggregation::Direct,
        session: ReferenceSessionConvention::Continuous,
        settlement: ReferenceSettlementConvention::OfficialFixing,
        schema_version: ReferenceTermsVersion::new("v1").expect("a reference schema version"),
    }
}

/// The code of the external reference `reference`.
pub(super) fn external_code(reference: &str) -> AssetDisplayCode {
    code_of(AssetDisplayTerms::External(&external_terms(reference)))
}

/// The code a route staged for one asset, such as a security ticker.
pub(super) fn staged_code(code: &str) -> AssetDisplayCode {
    AssetDisplayCode::from_staged_code(code).expect("a verified staged code is renderable")
}

/// Returns the one code the given terms state.
fn code_of(terms: AssetDisplayTerms<'_>) -> AssetDisplayCode {
    AssetDisplayCode::from_terms(terms)
        .expect("the terms are renderable")
        .expect("the canonical type states a code of its own")
}
