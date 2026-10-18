//! The five product grammars.
//!
//! Each family has one renderer inside [`format_instrument`], and each states the
//! field order of its own grammar once:
//!
//! | family | name |
//! | --- | --- |
//! | spot | `B/Q` |
//! | perpetual | `U/Q:S` |
//! | future | `U/Q:S-EXPIRY` |
//! | option | `U/Q:S-EXPIRY-STRIKE-CALL\|PUT` |
//! | outcome | `[QUESTION]/MEMBER/SIDE:S-EXPIRY` |
//!
//! An asset is rendered as its resolved display code and nothing else. Every
//! omission is therefore a field the details do not state: a future or an option
//! that delivers physical assets states no settlement asset and renders the
//! `PHYS` value of that position instead, and no other position is ever omitted or
//! added. The two literals of the grammar are `PHYS` and the two option rights;
//! everything else in a name is data.

use smol_str::format_smolstr;

use crate::{
    display::{
        asset::AssetDisplayCode,
        decimal::decimal_display,
        details::InstrumentDisplayDetails,
        lexical::{ComponentText, normalize_component_text},
        timestamp::utc_display,
        values::{DisplayFormatError, InstrumentDisplayString},
    },
    settlement::OptionRight,
};

/// The value of the `:S` position of a contract that hands over real assets.
///
/// this is not a grammar tag but the settlement position's own value
/// for a physical delivery, in the same position and of the same nature as the
/// `USD` or `BTC` a cash settlement writes there.
const PHYSICAL_SETTLEMENT: &str = "PHYS";

/// Renders the instrument display string of one product family.
///
/// This is the entry point of the display grammar: it consumes the typed details
/// of one product family and produces the `InstrumentDisplayString`, and it is the
/// only place an `InstrumentDisplayString` is created. A name is a rendering of the
/// identifying fields it is given, so two instruments that state the same fields
/// render the same name and both keep it.
///
/// A value the display pipeline refuses — an empty or control-bearing question, a
/// label of the same kind — is a [`DisplayFormatError`], which the publication
/// answers by refusing the whole batch rather than by publishing an unnamed
/// instrument.
pub fn format_instrument(
    details: &InstrumentDisplayDetails,
) -> Result<InstrumentDisplayString, DisplayFormatError> {
    let rendered = match details {
        InstrumentDisplayDetails::Spot { base, quote } => format_smolstr!("{base}/{quote}"),
        InstrumentDisplayDetails::Perpetual {
            underlying,
            price_quote,
            settlement,
        } => format_smolstr!("{underlying}/{price_quote}:{settlement}"),
        InstrumentDisplayDetails::Future {
            underlying,
            price_quote,
            settlement,
            expiration_at,
        } => {
            let settlement = settlement_text(settlement.as_ref());
            let expiration = utc_display(*expiration_at)?;
            format_smolstr!("{underlying}/{price_quote}:{settlement}-{expiration}")
        }
        InstrumentDisplayDetails::Option {
            underlying,
            price_quote,
            settlement,
            right,
            strike,
            expiration_at,
        } => {
            let settlement = settlement_text(settlement.as_ref());
            let expiration = utc_display(*expiration_at)?;
            let strike = decimal_display(*strike)?;
            let right = match right {
                OptionRight::Call => "CALL",
                OptionRight::Put => "PUT",
            };
            format_smolstr!("{underlying}/{price_quote}:{settlement}-{expiration}-{strike}-{right}")
        }
        InstrumentDisplayDetails::Outcome {
            question,
            member,
            side,
            settlement,
            expiration_at,
        } => {
            let question = normalize_component_text(ComponentText::FreeText(question))?;
            let member = normalize_component_text(ComponentText::FreeText(member))?;
            let side = normalize_component_text(ComponentText::FreeText(side))?;
            let expiration = utc_display(*expiration_at)?;
            format_smolstr!("[{question}]/{member}/{side}:{settlement}-{expiration}")
        }
    };
    Ok(InstrumentDisplayString::new(rendered))
}

/// Renders the `:S` position of a dated contract.
///
/// A contract that states a settlement asset renders that asset and one that hands
/// over real assets renders the `PHYS` value of the position. The distinction is
/// the only thing a name still says about how a contract settles, because the
/// delivery itself is a listing fact and not an identifying field.
fn settlement_text(settlement: Option<&AssetDisplayCode>) -> &str {
    match settlement {
        Some(code) => code.as_str(),
        None => PHYSICAL_SETTLEMENT,
    }
}
