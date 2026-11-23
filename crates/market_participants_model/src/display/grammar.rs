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

const PHYSICAL_SETTLEMENT: &str = "PHYS";

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

fn settlement_text(settlement: Option<&AssetDisplayCode>) -> &str {
    match settlement {
        Some(code) => code.as_str(),
        None => PHYSICAL_SETTLEMENT,
    }
}
