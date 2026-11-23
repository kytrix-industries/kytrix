use smol_str::SmolStr;

use crate::{
    decimal::{Price, UnixMillis},
    display::asset::AssetDisplayCode,
    settlement::OptionRight,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstrumentDisplayDetails {
    Spot {
        base: AssetDisplayCode,
        quote: AssetDisplayCode,
    },
    Perpetual {
        underlying: AssetDisplayCode,
        price_quote: AssetDisplayCode,
        settlement: AssetDisplayCode,
    },
    Future {
        underlying: AssetDisplayCode,
        price_quote: AssetDisplayCode,
        settlement: Option<AssetDisplayCode>,
        expiration_at: UnixMillis,
    },
    Option {
        underlying: AssetDisplayCode,
        price_quote: AssetDisplayCode,
        settlement: Option<AssetDisplayCode>,
        right: OptionRight,
        strike: Price,
        expiration_at: UnixMillis,
    },
    Outcome {
        question: SmolStr,
        member: SmolStr,
        side: SmolStr,
        settlement: AssetDisplayCode,
        expiration_at: UnixMillis,
    },
}
