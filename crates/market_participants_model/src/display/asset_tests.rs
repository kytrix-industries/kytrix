//! Unit tests of the asset display codes.

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

/// a currency is its ISO 4217 code, uppercased like every other code.
#[test]
fn currency_keeps_the_iso_code() {
    assert_eq!(currency_code("USD").as_str(), "USD");
    assert_eq!(currency_code("usd").as_str(), "USD");
    assert_eq!(
        display(AssetDisplayTerms::Currency(&currency_terms("JPY"))).as_str(),
        "JPY"
    );
}

/// a token is its verified native code, uppercased.
#[test]
fn token_code_is_the_verified_native_code() {
    assert_eq!(token_code("BTC").as_str(), "BTC");
    assert_eq!(token_code("btc").as_str(), "BTC");
    assert_eq!(
        display(AssetDisplayTerms::CryptoToken(&venue_token_terms("usdt"))).as_str(),
        "USDT"
    );
}

/// a contract token states no code of its own — its chain and its address
/// are not a name, so its code comes from the staging of its route.
#[test]
fn a_contract_token_states_no_code() {
    let terms = contract_token_terms("0x0000000000000000000000000000000000000001");
    assert_eq!(
        AssetDisplayCode::from_terms(AssetDisplayTerms::CryptoToken(&terms))
            .expect("the terms are renderable"),
        None
    );
}

/// the ticker of a security is staged by its route and normalized like
/// every other code.
#[test]
fn a_staged_ticker_is_the_security_code() {
    assert_eq!(staged_code("AAPL").as_str(), "AAPL");
    assert_eq!(staged_code("  aapl  ").as_str(), "AAPL");
    assert_eq!(staged_code("688888").as_str(), "688888");
    // A `.` of a ticker is a literal character, and so is every other structure
    // character: nothing is escaped and nothing is marked.
    assert_eq!(staged_code("brk.b").as_str(), "BRK.B");
    assert_eq!(staged_code("btc/usdt").as_str(), "BTC/USDT");
}

/// a staged code is normalized like every other component, so a code the
/// display layer cannot render is refused where it is staged.
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

/// a commodity is named by its own code, whatever origin published it.
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

/// an index is `namespace/code`, because its bare code is a number.
#[test]
fn an_index_code_carries_its_namespace() {
    assert_eq!(index_code("sp", "500").as_str(), "SP/500");
    assert_eq!(
        display(AssetDisplayTerms::Index(&index_terms("sp", "500"))).as_str(),
        "SP/500"
    );
}

/// an external reference is named by its reference value, and its issuer is
/// not part of a name.
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

/// no canonical type is renamed to tell it apart from another one, so
/// a currency, a token and a staged ticker of one spelling publish one code and
/// all three stay readable.
#[test]
fn two_assets_may_share_one_code() {
    let currency = currency_code("USD");
    let token = token_code("USD");
    let staged = staged_code("usd");
    assert_eq!(currency, token);
    assert_eq!(token, staged);
    assert_eq!(currency.as_str(), "USD");
}

/// a code is a pure function of its own terms, so the same terms
/// state the same code and the identity behind them never reaches it.
#[test]
fn a_code_is_a_pure_function_of_its_terms() {
    assert_eq!(token_code("BTC"), token_code("BTC"));
    // Two tokens of one native code, issued by different venues, publish the same
    // text: the origin is an identity and not a display decision.
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

/// Returns the one code the given terms state.
fn display(terms: AssetDisplayTerms<'_>) -> AssetDisplayCode {
    AssetDisplayCode::from_terms(terms)
        .expect("the terms are renderable")
        .expect("the canonical type states a code of its own")
}
