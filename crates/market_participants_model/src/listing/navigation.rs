//! Deterministic listing ordering and normalized native-key indexing.

use std::cmp::Ordering;

use ahash::{AHashMap, AHashSet};
use smol_str::SmolStr;

use crate::{
    asset::AssetId,
    error::ModelError,
    instrument::{Instrument, InstrumentKind, InstrumentTerms},
    listing::{route::VenueListingKey, venue::PriceBasis},
};

/// Returns the price quote asset and price basis a listing of `instrument` uses.
pub fn expected_price_terms(instrument: &Instrument) -> Result<(AssetId, PriceBasis), ModelError> {
    let terms = instrument.terms();
    match &terms {
        InstrumentTerms::Spot(spot) => Ok((spot.quote().clone(), PriceBasis::PerAssetUnit)),
        InstrumentTerms::Perpetual(perpetual) => Ok((
            perpetual.price_quote().clone(),
            terms.price_basis().unwrap_or(PriceBasis::PerContract),
        )),
        InstrumentTerms::Future(future) => Ok((
            future.price_quote().clone(),
            terms.price_basis().unwrap_or(PriceBasis::PerContract),
        )),
        InstrumentTerms::Option(option) => Ok((
            option.price_quote().clone(),
            terms.price_basis().unwrap_or(PriceBasis::PerContract),
        )),
        InstrumentTerms::Outcome(_) => Err(ModelError::UnsupportedInstrumentKind {
            kind: InstrumentKind::Outcome,
        }),
    }
}

/// Returns the canonical order of two listing keys: route, then listing id.
pub fn listing_order(left: &VenueListingKey, right: &VenueListingKey) -> Ordering {
    left.route
        .cmp(&right.route)
        .then_with(|| left.listing_id.cmp(&right.listing_id))
}

/// Sorts listing keys by their full key and drops exact duplicates.
pub fn sorted_listing_keys(
    keys: impl IntoIterator<Item = VenueListingKey>,
) -> Vec<VenueListingKey> {
    let mut keys: Vec<VenueListingKey> = keys.into_iter().collect();
    keys.sort_by(listing_order);
    keys.dedup();
    keys
}

/// Requires that every normalized native key of one route is unique.
pub fn require_unique_native_keys(
    keys: impl IntoIterator<Item = SmolStr>,
) -> Result<(), ModelError> {
    let mut seen = AHashSet::new();
    for key in keys {
        if !seen.insert(key) {
            return Err(ModelError::DuplicateCollectionEntry {
                field: "native_keys",
            });
        }
    }
    Ok(())
}

/// The normalized native key index of one route.
#[derive(Clone, Debug, Default)]
pub struct NativeKeyIndex {
    keys: AHashMap<SmolStr, VenueListingKey>,
}

impl NativeKeyIndex {
    /// Creates an index with room for `capacity` keys.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            keys: AHashMap::with_capacity(capacity),
        }
    }

    /// Inserts a normalized native key, rejecting a duplicate.
    pub fn insert(&mut self, key: SmolStr, listing: VenueListingKey) -> Result<(), ModelError> {
        if self.keys.contains_key(&key) {
            return Err(ModelError::AmbiguousListingContinuity {
                venue: listing.route.venue,
                scope: listing.route.scope,
            });
        }
        self.keys.insert(key, listing);
        Ok(())
    }

    /// Resolves a normalized native key.
    pub fn get(&self, key: &str) -> Option<&VenueListingKey> {
        self.keys.get(key)
    }

    /// Returns the number of indexed keys.
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Returns `true` when no key has been indexed.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}
