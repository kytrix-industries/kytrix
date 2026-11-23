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

pub(super) const MIDNIGHT_20251219: UnixMillis = 1_766_102_400_000;

pub(super) const MIDNIGHT_20251226: UnixMillis = 1_766_707_200_000;

pub(super) const MIDNIGHT_20251231: UnixMillis = 1_767_139_200_000;

pub(super) const AT_20251219T080000Z: UnixMillis = 1_766_131_200_000;

pub(super) const AT_20251219T153000Z: UnixMillis = 1_766_158_200_000;

pub(super) const AT_20251219T000000_250Z: UnixMillis = 1_766_102_400_250;

pub(super) fn currency_asset(code: &str) -> AssetId {
    AssetId::Currency(CurrencyId::new(
        Iso4217Code::new(code).expect("a published ISO 4217 code"),
    ))
}

pub(super) fn currency_terms(code: &str) -> CurrencyTerms {
    CurrencyTerms {
        code: Iso4217Code::new(code).expect("a published ISO 4217 code"),
    }
}

pub(super) fn currency_code(code: &str) -> AssetDisplayCode {
    code_of(AssetDisplayTerms::Currency(&currency_terms(code)))
}

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

pub(super) fn token_code(code: &str) -> AssetDisplayCode {
    code_of(AssetDisplayTerms::CryptoToken(&venue_token_terms(code)))
}

pub(super) fn commodity_terms(code: &str) -> CommodityTerms {
    CommodityTerms {
        origin: CommodityOrigin::Iso {
            code: IdentifierValue::new(code).expect("a commodity code"),
        },
    }
}

pub(super) fn commodity_code(code: &str) -> AssetDisplayCode {
    code_of(AssetDisplayTerms::Commodity(&commodity_terms(code)))
}

pub(super) fn index_terms(namespace: &str, code: &str) -> IndexTerms {
    IndexTerms {
        namespace: IdentifierNamespace::new(namespace).expect("an index namespace"),
        code: IdentifierValue::new(code).expect("an index code"),
    }
}

pub(super) fn index_code(namespace: &str, code: &str) -> AssetDisplayCode {
    code_of(AssetDisplayTerms::Index(&index_terms(namespace, code)))
}

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

pub(super) fn external_code(reference: &str) -> AssetDisplayCode {
    code_of(AssetDisplayTerms::External(&external_terms(reference)))
}

pub(super) fn staged_code(code: &str) -> AssetDisplayCode {
    AssetDisplayCode::from_staged_code(code).expect("a verified staged code is renderable")
}

fn code_of(terms: AssetDisplayTerms<'_>) -> AssetDisplayCode {
    AssetDisplayCode::from_terms(terms)
        .expect("the terms are renderable")
        .expect("the canonical type states a code of its own")
}
