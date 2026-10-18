//! Unit tests of the product grammars.
//!
//! Every family is asserted verbatim: the expected string is the specification's
//! own example, so a change of the field order, of a rendered position or of an
//! omission rule is a failing expectation rather than a silently different name.

use smol_str::SmolStr;

use crate::{
    decimal::{Price, UnixMillis},
    display::{
        asset::AssetDisplayCode,
        details::InstrumentDisplayDetails,
        grammar::format_instrument,
        test_support::{
            AT_20251219T000000_250Z, AT_20251219T080000Z, AT_20251219T153000Z, MIDNIGHT_20251219,
            MIDNIGHT_20251226, MIDNIGHT_20251231, commodity_code, currency_code, external_code,
            index_code, staged_code, token_code,
        },
        values::{DisplayFormatError, InstrumentDisplayString, InvalidTextReason},
    },
    settlement::OptionRight,
};

/// Renders one fixture, which every test in this module builds from validated
/// terms and resolved display codes.
fn render(details: &InstrumentDisplayDetails) -> InstrumentDisplayString {
    format_instrument(details).expect("a fixture of validated terms renders")
}

/// A spot instrument of the two given assets.
fn spot(base: AssetDisplayCode, quote: AssetDisplayCode) -> InstrumentDisplayDetails {
    InstrumentDisplayDetails::Spot { base, quote }
}

/// A perpetual contract of one underlying, quote and settlement asset.
fn perpetual(
    underlying: AssetDisplayCode,
    price_quote: AssetDisplayCode,
    settlement: AssetDisplayCode,
) -> InstrumentDisplayDetails {
    InstrumentDisplayDetails::Perpetual {
        underlying,
        price_quote,
        settlement,
    }
}

/// A dated contract of one underlying, quote, settlement and expiry.
fn future(
    underlying: AssetDisplayCode,
    price_quote: AssetDisplayCode,
    settlement: Option<AssetDisplayCode>,
    expiration_at: UnixMillis,
) -> InstrumentDisplayDetails {
    InstrumentDisplayDetails::Future {
        underlying,
        price_quote,
        settlement,
        expiration_at,
    }
}

/// An option of `right` and `strike` on one underlying, quote and settlement.
fn option(
    underlying: AssetDisplayCode,
    price_quote: AssetDisplayCode,
    settlement: Option<AssetDisplayCode>,
    right: OptionRight,
    strike: Price,
    expiration_at: UnixMillis,
) -> InstrumentDisplayDetails {
    InstrumentDisplayDetails::Option {
        underlying,
        price_quote,
        settlement,
        right,
        strike,
        expiration_at,
    }
}

/// A quote-settled option of the standard fixture, expiring 2025-12-26.
fn quote_settled_option(right: OptionRight, strike: Price) -> InstrumentDisplayDetails {
    option(
        token_code("BTC"),
        token_code("USDT"),
        Some(token_code("USDT")),
        right,
        strike,
        MIDNIGHT_20251226,
    )
}

/// One member and side of an outcome group.
fn outcome(
    question: &str,
    member: &str,
    side: &str,
    settlement: AssetDisplayCode,
) -> InstrumentDisplayDetails {
    InstrumentDisplayDetails::Outcome {
        question: question.into(),
        member: member.into(),
        side: side.into(),
        settlement,
        expiration_at: MIDNIGHT_20251231,
    }
}

// ---------------------------------------------------------------------------
// Spot
// ---------------------------------------------------------------------------

/// `BTC/USDT`.
#[test]
fn spot_btc_usdt_renders_verbatim() {
    assert_eq!(
        render(&spot(token_code("BTC"), token_code("USDT"))).as_str(),
        "BTC/USDT"
    );
}

/// `USDC/EUR`, a token quoted in a fiat currency.
#[test]
fn spot_usdc_eur_renders_verbatim() {
    assert_eq!(
        render(&spot(token_code("USDC"), currency_code("EUR"))).as_str(),
        "USDC/EUR"
    );
}

/// `AAPL/USD`, a security quoted in a fiat currency.
///
/// the code a route staged for the security is its ticker, so no MIC
/// and no local code reaches the name — either of them could only appear in one
/// dotted onto the ticker.
#[test]
fn spot_aapl_usd_renders_verbatim() {
    let details = spot(staged_code("AAPL"), currency_code("USD"));
    assert_eq!(render(&details).as_str(), "AAPL/USD");
    assert!(!render(&details).as_str().contains('.'));
}

/// `688888/CNY`, a numeric ticker.
#[test]
fn spot_688888_cny_renders_verbatim() {
    assert_eq!(
        render(&spot(staged_code("688888"), currency_code("CNY"))).as_str(),
        "688888/CNY"
    );
}

/// a name states exactly the fields it is given, so one code on both
/// sides of a pair is stated twice and a name never drops one of its fields.
#[test]
fn a_name_states_only_the_fields_it_is_given() {
    assert_eq!(
        render(&spot(token_code("BTC"), token_code("USDT"))).as_str(),
        "BTC/USDT"
    );
    assert_eq!(
        render(&spot(token_code("BTC"), token_code("BTC"))).as_str(),
        "BTC/BTC"
    );
}

/// two different assets may resolve to one code, and both render it. A name
/// says which fields an instrument identifies itself by, not which asset is which.
#[test]
fn two_assets_of_one_code_render_the_same_text() {
    let currency = spot(currency_code("USD"), token_code("BTC"));
    let staged = spot(staged_code("usd"), token_code("BTC"));
    assert_eq!(render(&currency).as_str(), "USD/BTC");
    assert_eq!(render(&staged).as_str(), "USD/BTC");
    assert_eq!(render(&currency), render(&staged));
}

/// a commodity is named by its own code, without a type prefix.
#[test]
fn a_commodity_code_carries_no_type_prefix() {
    let details = spot(commodity_code("XAU"), currency_code("USD"));
    assert_eq!(render(&details).as_str(), "XAU/USD");
}

// ---------------------------------------------------------------------------
// Perpetual
// ---------------------------------------------------------------------------

/// `BTC/USDT:USDT`, the quote-settled default.
#[test]
fn perpetual_quote_settled_renders_verbatim() {
    let details = perpetual(token_code("BTC"), token_code("USDT"), token_code("USDT"));
    assert_eq!(render(&details).as_str(), "BTC/USDT:USDT");
}

/// `BTC/USD:BTC`, an inverse contract.
#[test]
fn perpetual_inverse_renders_verbatim() {
    let details = perpetual(token_code("BTC"), currency_code("USD"), token_code("BTC"));
    assert_eq!(render(&details).as_str(), "BTC/USD:BTC");
}

/// `ABC/USD:USD`, a quanto contract. The pnl model is not part of
/// the name: what a name states is the asset the contract settles in.
#[test]
fn perpetual_quanto_renders_verbatim() {
    let details = perpetual(
        token_code("ABC"),
        currency_code("USD"),
        currency_code("USD"),
    );
    assert_eq!(render(&details).as_str(), "ABC/USD:USD");
}

/// `SP/500/USD:USD`, an index underlying. The code of an index is
/// two halves, so a name nests one more separator.
#[test]
fn perpetual_index_underlying_renders_verbatim() {
    let details = perpetual(
        index_code("sp", "500"),
        currency_code("USD"),
        currency_code("USD"),
    );
    assert_eq!(render(&details).as_str(), "SP/500/USD:USD");
}

/// an external reference is named by its reference value alone, so its
/// issuer never reaches a name.
#[test]
fn perpetual_external_underlying_renders_its_reference() {
    let details = perpetual(
        external_code("btc-usd-oracle"),
        currency_code("USD"),
        currency_code("USD"),
    );
    assert_eq!(render(&details).as_str(), "BTC-USD-ORACLE/USD:USD");
}

/// a perpetual always states the asset it settles in — that position is the
/// one thing that tells it apart from the spot pair of the same two assets.
#[test]
fn a_perpetual_states_its_settlement_asset() {
    let details = perpetual(token_code("BTC"), token_code("USDT"), token_code("USDT"));
    assert_eq!(render(&details).as_str(), "BTC/USDT:USDT");
    assert_ne!(
        render(&details).as_str(),
        render(&spot(token_code("BTC"), token_code("USDT"))).as_str()
    );
}

// ---------------------------------------------------------------------------
// Future
// ---------------------------------------------------------------------------

/// `BTC/USD:USD-20251219`, a cash-settled future.
#[test]
fn cash_future_renders_verbatim() {
    let details = future(
        token_code("BTC"),
        currency_code("USD"),
        Some(currency_code("USD")),
        MIDNIGHT_20251219,
    );
    assert_eq!(render(&details).as_str(), "BTC/USD:USD-20251219");
}

/// `BTC/USD:PHYS-20251219`, a physically delivered future.
#[test]
fn physical_future_renders_verbatim() {
    let details = future(
        token_code("BTC"),
        currency_code("USD"),
        None,
        MIDNIGHT_20251219,
    );
    assert_eq!(render(&details).as_str(), "BTC/USD:PHYS-20251219");
}

/// a physical delivery and a cash settlement are different names,
/// and the `:S` position states which one it is.
#[test]
fn a_physical_delivery_is_not_a_cash_settlement() {
    let cash = future(
        token_code("BTC"),
        currency_code("USD"),
        Some(currency_code("USD")),
        MIDNIGHT_20251219,
    );
    let physical = future(
        token_code("BTC"),
        currency_code("USD"),
        None,
        MIDNIGHT_20251219,
    );
    assert_ne!(render(&cash), render(&physical));
    assert!(render(&physical).as_str().contains(":PHYS-"));
}

/// `BTC/USD:BTC-20251219`, an inverse dated contract.
#[test]
fn inverse_cash_future_renders_verbatim() {
    let details = future(
        token_code("BTC"),
        currency_code("USD"),
        Some(token_code("BTC")),
        MIDNIGHT_20251219,
    );
    assert_eq!(render(&details).as_str(), "BTC/USD:BTC-20251219");
}

/// the main date is the expiry, and a non-midnight instant keeps its
/// time, so two contracts of one day never share a name.
#[test]
fn every_expiry_instant_keeps_its_own_name() {
    let midnight = future(
        token_code("BTC"),
        currency_code("USD"),
        Some(currency_code("USD")),
        MIDNIGHT_20251219,
    );
    let eight = future(
        token_code("BTC"),
        currency_code("USD"),
        Some(currency_code("USD")),
        AT_20251219T080000Z,
    );
    let half_past_three = future(
        token_code("BTC"),
        currency_code("USD"),
        Some(currency_code("USD")),
        AT_20251219T153000Z,
    );
    let millis = future(
        token_code("BTC"),
        currency_code("USD"),
        Some(currency_code("USD")),
        AT_20251219T000000_250Z,
    );
    assert_eq!(render(&midnight).as_str(), "BTC/USD:USD-20251219");
    assert_eq!(render(&eight).as_str(), "BTC/USD:USD-20251219T080000Z");
    assert_eq!(
        render(&half_past_three).as_str(),
        "BTC/USD:USD-20251219T153000Z"
    );
    assert_eq!(render(&millis).as_str(), "BTC/USD:USD-20251219T000000.250Z");
}

// ---------------------------------------------------------------------------
// Option
// ---------------------------------------------------------------------------

/// `BTC/USDT:USDT-20251226-60000-PUT`.
#[test]
fn quote_settled_option_renders_verbatim() {
    let details = quote_settled_option(OptionRight::Put, Price::from(60_000_u32));
    assert_eq!(
        render(&details).as_str(),
        "BTC/USDT:USDT-20251226-60000-PUT"
    );
}

/// `AAPL/USD:USD-20251219-150-CALL`. The exercise style, the
/// contract size and the references are not part of the name.
#[test]
fn american_equity_option_renders_verbatim() {
    let details = option(
        staged_code("AAPL"),
        currency_code("USD"),
        Some(currency_code("USD")),
        OptionRight::Call,
        Price::from(150_u16),
        MIDNIGHT_20251219,
    );
    assert_eq!(render(&details).as_str(), "AAPL/USD:USD-20251219-150-CALL");
}

/// `AAPL/USD:PHYS-20251219-150-CALL`, a physical option.
#[test]
fn physical_equity_option_renders_verbatim() {
    let details = option(
        staged_code("AAPL"),
        currency_code("USD"),
        None,
        OptionRight::Call,
        Price::from(150_u16),
        MIDNIGHT_20251219,
    );
    assert_eq!(render(&details).as_str(), "AAPL/USD:PHYS-20251219-150-CALL");
}

/// `SP/500/USD:USD-20251219-5000-CALL`, an index-point option.
#[test]
fn index_point_option_renders_verbatim() {
    let details = option(
        index_code("sp", "500"),
        currency_code("USD"),
        Some(currency_code("USD")),
        OptionRight::Call,
        Price::from(5_000_u16),
        MIDNIGHT_20251219,
    );
    assert_eq!(
        render(&details).as_str(),
        "SP/500/USD:USD-20251219-5000-CALL"
    );
}

/// a strike goes through the one canonical decimal rule, so a
/// fractional strike keeps its digits and never becomes a float.
#[test]
fn a_fractional_strike_keeps_its_digits() {
    let fractional = quote_settled_option(
        OptionRight::Call,
        Price::from_inner(12_500_000_000_000_000_000),
    );
    assert_eq!(
        render(&fractional).as_str(),
        "BTC/USDT:USDT-20251226-12.5-CALL"
    );
}

/// a right is `CALL` or `PUT`, and the two names differ.
#[test]
fn an_option_right_is_call_or_put() {
    let call = quote_settled_option(OptionRight::Call, Price::from(150_u16));
    let put = quote_settled_option(OptionRight::Put, Price::from(150_u16));
    assert!(render(&call).as_str().ends_with("-CALL"));
    assert!(render(&put).as_str().ends_with("-PUT"));
}

/// an option states its underlying, quote, settlement, expiry, strike
/// and right, and changing any one of them changes the name.
#[test]
fn every_option_field_is_stated() {
    let base = quote_settled_option(OptionRight::Call, Price::from(150_u16));
    let other_underlying = option(
        token_code("ETH"),
        token_code("USDT"),
        Some(token_code("USDT")),
        OptionRight::Call,
        Price::from(150_u16),
        MIDNIGHT_20251226,
    );
    let other_quote = option(
        token_code("BTC"),
        currency_code("USD"),
        Some(token_code("USDT")),
        OptionRight::Call,
        Price::from(150_u16),
        MIDNIGHT_20251226,
    );
    let other_settlement = option(
        token_code("BTC"),
        token_code("USDT"),
        Some(currency_code("USD")),
        OptionRight::Call,
        Price::from(150_u16),
        MIDNIGHT_20251226,
    );
    let other_strike = quote_settled_option(OptionRight::Call, Price::from(151_u16));
    let other_expiry = option(
        token_code("BTC"),
        token_code("USDT"),
        Some(token_code("USDT")),
        OptionRight::Call,
        Price::from(150_u16),
        MIDNIGHT_20251219,
    );
    let names = [
        render(&base),
        render(&other_underlying),
        render(&other_quote),
        render(&other_settlement),
        render(&other_strike),
        render(&other_expiry),
    ];
    for (index, left) in names.iter().enumerate() {
        for right in &names[index + 1..] {
            assert_ne!(left, right, "every identifying field reaches the name");
        }
    }
}

/// the settlement of a dated contract states the asset it pays in,
/// so a contract that settles in its own underlying is not the one that settles
/// in the quote.
#[test]
fn a_future_states_its_settlement_asset() {
    let quote_settled = future(
        token_code("BTC"),
        currency_code("USD"),
        Some(currency_code("USD")),
        MIDNIGHT_20251219,
    );
    let inverse = future(
        token_code("BTC"),
        currency_code("USD"),
        Some(token_code("BTC")),
        MIDNIGHT_20251219,
    );
    assert_ne!(render(&quote_settled), render(&inverse));
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// `[WILL BTC EXCEED 100K BY 20251231]/MAIN/YES:USDC-20251231`.
#[test]
fn binary_outcome_yes_renders_verbatim() {
    let details = outcome(
        "Will BTC exceed 100k by 20251231",
        "main",
        "yes",
        token_code("USDC"),
    );
    assert_eq!(
        render(&details).as_str(),
        "[WILL BTC EXCEED 100K BY 20251231]/MAIN/YES:USDC-20251231"
    );
}

/// the member and the side are stated, so the two sides of one member are
/// two names.
#[test]
fn an_outcome_states_its_member_and_its_side() {
    let yes = outcome("Will BTC exceed 100k", "main", "yes", token_code("USDC"));
    let no = outcome("Will BTC exceed 100k", "main", "no", token_code("USDC"));
    let other_member = outcome("Will BTC exceed 100k", "alt", "yes", token_code("USDC"));
    assert_eq!(
        render(&yes).as_str(),
        "[WILL BTC EXCEED 100K]/MAIN/YES:USDC-20251231"
    );
    assert_eq!(
        render(&no).as_str(),
        "[WILL BTC EXCEED 100K]/MAIN/NO:USDC-20251231"
    );
    assert_eq!(
        render(&other_member).as_str(),
        "[WILL BTC EXCEED 100K]/ALT/YES:USDC-20251231"
    );
}

/// a question keeps its spaces and a run of
/// whitespace collapses, so a free text is rendered, not rewritten.
#[test]
fn an_outcome_question_keeps_its_spaces() {
    let details = outcome(
        "  will\u{00a0}\u{2003}BTC   exceed 100k  ",
        "main",
        "yes",
        token_code("USDC"),
    );
    assert_eq!(
        render(&details).as_str(),
        "[WILL BTC EXCEED 100K]/MAIN/YES:USDC-20251231"
    );
}

/// only ASCII letters are uppercased, a non-ASCII question is kept
/// as it was published and is never transliterated.
#[test]
fn a_non_ascii_question_is_preserved() {
    let details = outcome("btc 会超过 10 万吗", "main", "yes", token_code("USDC"));
    assert_eq!(
        render(&details).as_str(),
        "[BTC 会超过 10 万吗]/MAIN/YES:USDC-20251231"
    );
}

/// A structure character inside a question is a literal character of
/// the question, so a name can nest the grammar's own separators.
#[test]
fn a_structure_character_inside_a_question_is_literal() {
    let details = outcome(
        "will A/B close above [100k] on 2025-12-31",
        "main",
        "yes",
        token_code("USDC"),
    );
    assert_eq!(
        render(&details).as_str(),
        "[WILL A/B CLOSE ABOVE [100K] ON 2025-12-31]/MAIN/YES:USDC-20251231"
    );
}

/// a question the model accepts but the display layer refuses
/// is an error of the naming step and not a name, so a publication that meets one
/// is refused rather than published under a partial label.
#[test]
fn an_unrenderable_text_is_refused_by_the_naming_step() {
    let blank_question = outcome("   ", "main", "yes", token_code("USDC"));
    assert_eq!(
        format_instrument(&blank_question).expect_err("a blank question has no rendering"),
        DisplayFormatError::InvalidText {
            value: SmolStr::new_static("   "),
            reason: InvalidTextReason::Empty,
        }
    );
    let control_side = outcome("Will BTC exceed 100k", "main", "y\nes", token_code("USDC"));
    assert_eq!(
        format_instrument(&control_side).expect_err("a control character has no rendering"),
        DisplayFormatError::InvalidText {
            value: SmolStr::new_static("y\nes"),
            reason: InvalidTextReason::ControlCharacter,
        }
    );
}

/// an outcome group is named by its own settlement asset, and a group that
/// settles in another asset is another name.
#[test]
fn an_outcome_states_its_settlement_asset() {
    let usdc = outcome("Will BTC exceed 100k", "main", "yes", token_code("USDC"));
    let usdt = outcome("Will BTC exceed 100k", "main", "yes", token_code("USDT"));
    assert_ne!(render(&usdc), render(&usdt));
    assert!(render(&usdc).as_str().ends_with(":USDC-20251231"));
}
