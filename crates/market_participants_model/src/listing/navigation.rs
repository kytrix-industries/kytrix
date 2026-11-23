use std::cmp::Ordering;

use ahash::{AHashMap, AHashSet};
use smol_str::SmolStr;

use crate::{
    asset::AssetId,
    error::ModelError,
    instrument::{Instrument, InstrumentKind, InstrumentTerms},
    listing::{route::VenueListingKey, venue::PriceBasis},
};

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

pub fn listing_order(left: &VenueListingKey, right: &VenueListingKey) -> Ordering {
    left.route
        .cmp(&right.route)
        .then_with(|| left.listing_id.cmp(&right.listing_id))
}

pub fn sorted_listing_keys(
    keys: impl IntoIterator<Item = VenueListingKey>,
) -> Vec<VenueListingKey> {
    let mut keys: Vec<VenueListingKey> = keys.into_iter().collect();
    keys.sort_by(listing_order);
    keys.dedup();
    keys
}

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

#[derive(Clone, Debug, Default)]
pub struct NativeKeyIndex {
    keys: AHashMap<SmolStr, VenueListingKey>,
}

impl NativeKeyIndex {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            keys: AHashMap::with_capacity(capacity),
        }
    }

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

    pub fn get(&self, key: &str) -> Option<&VenueListingKey> {
        self.keys.get(key)
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}
