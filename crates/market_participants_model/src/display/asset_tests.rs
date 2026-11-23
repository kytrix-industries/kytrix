use crate::{
    asset::{CommodityOrigin, CommodityTerms, CryptoTokenOrigin, CryptoTokenTerms, TokenReference},
    display::{
        asset::{AssetDisplayCode, AssetDisplayTerms},
        test_support::{
            commodity_terms, contract_token_terms, currency_code, currency_terms, external_code,
            external_terms, index_code, index_terms, staged_code, token_code, venue_token_terms,
        },
        values::{DisplayFormatError, InvalidTextReason},
    },
    identifier::{IdentifierNamespace, IdentifierValue, TokenIssuerId},
};

#[test]
fn currency_keeps_the_iso_code() {
    assert_eq!(currency_code("USD").as_str(), "USD");
    assert_eq!(currency_code("usd").as_str(), "USD");
    assert_eq!(
        display(AssetDisplayTerms::Currency(&currency_terms("JPY"))).as_str(),
        "JPY"
    );
}

#[test]
fn token_code_is_the_verified_native_code() {
    assert_eq!(token_code("BTC").as_str(), "BTC");
    assert_eq!(token_code("btc").as_str(), "BTC");
    assert_eq!(
        display(AssetDisplayTerms::CryptoToken(&venue_token_terms("usdt"))).as_str(),
        "USDT"
    );
}

#[test]
fn a_contract_token_states_no_code() {
    let terms = contract_token_terms("0x0000000000000000000000000000000000000001");
    assert_eq!(
        AssetDisplayCode::from_terms(AssetDisplayTerms::CryptoToken(&terms))
            .expect("the terms are renderable"),
        None
    );
}

#[test]
fn a_staged_ticker_is_the_security_code() {
    assert_eq!(staged_code("AAPL").as_str(), "AAPL");
    assert_eq!(staged_code("  aapl  ").as_str(), "AAPL");
    assert_eq!(staged_code("688888").as_str(), "688888");
    assert_eq!(staged_code("brk.b").as_str(), "BRK.B");
    assert_eq!(staged_code("btc/usdt").as_str(), "BTC/USDT");
}

#[test]
fn a_staged_code_refuses_unrenderable_text() {
    for value in ["", "   "] {
        assert_eq!(
            AssetDisplayCode::from_staged_code(value).expect_err("empty text fails"),
            DisplayFormatError::InvalidText {
                value: smol_str::SmolStr::new(value),
                reason: InvalidTextReason::Empty,
            }
        );
    }
    assert_eq!(
        AssetDisplayCode::from_staged_code("a\nb").expect_err("a control character fails"),
        DisplayFormatError::InvalidText {
            value: smol_str::SmolStr::new("a\nb"),
            reason: InvalidTextReason::ControlCharacter,
        }
    );
}

#[test]
fn a_commodity_is_its_code() {
    assert_eq!(
        display(AssetDisplayTerms::Commodity(&commodity_terms("XAU"))).as_str(),
        "XAU"
    );
    let venue = CommodityTerms {
        origin: CommodityOrigin::Venue {
            namespace: IdentifierNamespace::new("lme").expect("lme is a namespace"),
            code: IdentifierValue::new("XAU").expect("a commodity code"),
        },
    };
    assert_eq!(
        display(AssetDisplayTerms::Commodity(&venue)).as_str(),
        "XAU"
    );
}

#[test]
fn an_index_code_carries_its_namespace() {
    assert_eq!(index_code("sp", "500").as_str(), "SP/500");
    assert_eq!(
        display(AssetDisplayTerms::Index(&index_terms("sp", "500"))).as_str(),
        "SP/500"
    );
}

#[test]
fn an_external_reference_code_is_its_reference() {
    assert_eq!(external_code("btc-usd").as_str(), "BTC-USD");
    let terms = external_terms("oracle-1");
    assert_eq!(
        display(AssetDisplayTerms::External(&terms)).as_str(),
        "ORACLE-1"
    );
    assert!(!external_code("btc-usd").as_str().contains("HYPERLIQUID"));
}

#[test]
fn two_assets_may_share_one_code() {
    let currency = currency_code("USD");
    let token = token_code("USD");
    let staged = staged_code("usd");
    assert_eq!(currency, token);
    assert_eq!(token, staged);
    assert_eq!(currency.as_str(), "USD");
}

#[test]
fn a_code_is_a_pure_function_of_its_terms() {
    assert_eq!(token_code("BTC"), token_code("BTC"));
    let other_venue = CryptoTokenTerms {
        origin: CryptoTokenOrigin::VenueIssued {
            issuer: TokenIssuerId::new("kraken").expect("kraken is an issuer"),
            namespace: IdentifierNamespace::new("kraken-spot").expect("a namespace"),
            token: TokenReference::Native {
                code: IdentifierValue::new("BTC").expect("a token code"),
            },
        },
    };
    assert_eq!(
        display(AssetDisplayTerms::CryptoToken(&other_venue)),
        token_code("BTC")
    );
}

fn display(terms: AssetDisplayTerms<'_>) -> AssetDisplayCode {
    AssetDisplayCode::from_terms(terms)
        .expect("the terms are renderable")
        .expect("the canonical type states a code of its own")
}
