//! The typed display details of the five product families.
//!
//! One variant per product family carries the fields of that family, and this
//! module states exactly those fields: the identifying fields a name lets in. Every one of them is either an already validated
//! canonical term of the model or an [`AssetDisplayCode`] the publication batch
//! resolved for one catalog generation. The formatter renders the values it is
//! given and never looks up, guesses or re-derives an economic clause from a
//! native symbol, a route or an identity digest.
//!
//! A field a name keeps out is not part of these details at all — a
//! pnl model, a contract size, a reference, a delivery leg, a topology and a rule
//! live in the canonical terms, the registry and the structured identity, which is
//! where a reader that needs them looks. Carrying one here would state a second
//! time what the contract already states.

use smol_str::SmolStr;

use crate::{
    decimal::{Price, UnixMillis},
    display::asset::AssetDisplayCode,
    settlement::OptionRight,
};

/// Everything one instrument display string is rendered from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstrumentDisplayDetails {
    /// the two assets of a spot instrument.
    Spot {
        /// The base asset.
        base: AssetDisplayCode,
        /// The quote asset.
        quote: AssetDisplayCode,
    },
    /// a perpetual contract.
    Perpetual {
        /// The asset the contract tracks.
        underlying: AssetDisplayCode,
        /// The asset the contract is priced in.
        price_quote: AssetDisplayCode,
        /// The asset the contract settles in. A perpetual always settles in the
        /// asset it states, so this position is never `PHYS`.
        settlement: AssetDisplayCode,
    },
    /// a dated contract.
    Future {
        /// The asset the contract tracks.
        underlying: AssetDisplayCode,
        /// The asset the contract is priced in.
        price_quote: AssetDisplayCode,
        /// The asset the contract settles in, or `None` for a contract that
        /// delivers physical assets and states `PHYS` in its place.
        settlement: Option<AssetDisplayCode>,
        /// The instant the contract expires, rendered by the one UTC rule.
        expiration_at: UnixMillis,
    },
    /// an option contract.
    Option {
        /// The asset the contract tracks.
        underlying: AssetDisplayCode,
        /// The asset the contract is priced in.
        price_quote: AssetDisplayCode,
        /// The asset the contract settles in, or `None` for an option that
        /// delivers physical assets and states `PHYS` in its place.
        settlement: Option<AssetDisplayCode>,
        /// The right of the holder.
        right: OptionRight,
        /// The strike every option states. The model validated it as a non-zero
        /// `StrikePrice`; the publication asserts that invariant when it builds
        /// these details, because the display layer renders the value it is given.
        strike: Price,
        /// The instant the contract expires, rendered by the one UTC rule.
        expiration_at: UnixMillis,
    },
    /// one member and side of an outcome group.
    Outcome {
        /// The display question of the group, already bounded by the model.
        question: SmolStr,
        /// The label of the member, taken from the outcome registry.
        member: SmolStr,
        /// The label of the side, taken from the outcome registry.
        side: SmolStr,
        /// The asset the outcome settles in.
        settlement: AssetDisplayCode,
        /// The instant the group expires, rendered by the one UTC rule.
        expiration_at: UnixMillis,
    },
}
