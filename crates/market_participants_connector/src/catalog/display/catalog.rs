use std::sync::Arc;

use ahash::AHashMap;
use market_participants_model::{
    asset::{AssetId, IndexId},
    display::{
        asset::AssetDisplayCode, details::InstrumentDisplayDetails, values::InstrumentDisplayString,
    },
    listing::{route::VenueListingKey, rules::ListingPrecision},
    reference::ExternalReferenceId,
    settlement::InstrumentId,
};
use smol_str::SmolStr;

use crate::catalog::{
    display::types::DisplayError,
    generation::{CatalogGeneration, CatalogGenerationId},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayCatalog {
    asset_codes: AHashMap<AssetId, AssetDisplayCode>,
    index_codes: AHashMap<IndexId, AssetDisplayCode>,
    external_reference_codes: AHashMap<ExternalReferenceId, AssetDisplayCode>,
    instrument_names: AHashMap<InstrumentId, InstrumentDisplayString>,
    instrument_details: AHashMap<InstrumentId, InstrumentDisplayDetails>,
    listings: AHashMap<VenueListingKey, InstrumentDisplayString>,
    listing_search_aliases: AHashMap<VenueListingKey, Arc<[SmolStr]>>,
}

impl DisplayCatalog {
    pub(crate) fn from_parts(
        asset_codes: AHashMap<AssetId, AssetDisplayCode>,
        index_codes: AHashMap<IndexId, AssetDisplayCode>,
        external_reference_codes: AHashMap<ExternalReferenceId, AssetDisplayCode>,
        instrument_names: AHashMap<InstrumentId, InstrumentDisplayString>,
        instrument_details: AHashMap<InstrumentId, InstrumentDisplayDetails>,
        listings: AHashMap<VenueListingKey, InstrumentDisplayString>,
        listing_search_aliases: AHashMap<VenueListingKey, Arc<[SmolStr]>>,
    ) -> Self {
        Self {
            asset_codes,
            index_codes,
            external_reference_codes,
            instrument_names,
            instrument_details,
            listings,
            listing_search_aliases,
        }
    }

    pub fn asset_code(&self, asset: &AssetId) -> Option<&AssetDisplayCode> {
        self.asset_codes.get(asset)
    }

    pub fn index_code(&self, index: &IndexId) -> Option<&AssetDisplayCode> {
        self.index_codes.get(index)
    }

    pub fn external_reference_code(
        &self,
        reference: &ExternalReferenceId,
    ) -> Option<&AssetDisplayCode> {
        self.external_reference_codes.get(reference)
    }

    pub fn instrument_details(
        &self,
        instrument: &InstrumentId,
    ) -> Option<&InstrumentDisplayDetails> {
        self.instrument_details.get(instrument)
    }

    pub fn listing_instrument(
        &self,
        listing: &VenueListingKey,
    ) -> Option<&InstrumentDisplayString> {
        self.listings.get(listing)
    }

    pub fn listing_search_aliases(&self, listing: &VenueListingKey) -> Option<&[SmolStr]> {
        self.listing_search_aliases
            .get(listing)
            .map(|aliases| aliases.as_ref())
    }
}

#[derive(Clone, Debug)]
pub struct DisplaySnapshot {
    generation: Arc<CatalogGeneration>,
}

impl DisplaySnapshot {
    pub(crate) const fn new(generation: Arc<CatalogGeneration>) -> Self {
        Self { generation }
    }

    pub fn generation(&self) -> &CatalogGeneration {
        &self.generation
    }

    pub fn generation_id(&self) -> CatalogGenerationId {
        self.generation.id()
    }

    pub fn resolver(&self) -> DisplayResolver<'_> {
        self.generation.display_resolver()
    }

    pub fn listing_instrument(
        &self,
        listing: &VenueListingKey,
    ) -> Result<InstrumentDisplayString, DisplayError> {
        self.resolver().listing_instrument(listing).cloned()
    }

    pub fn listing_precision(&self, listing: &VenueListingKey) -> Option<ListingPrecision> {
        self.generation
            .route_snapshot(&listing.route)?
            .listing(listing)
            .map(|entry| ListingPrecision::of(&entry.snapshot.rules))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DisplayResolver<'a> {
    generation: &'a CatalogGeneration,
}

impl<'a> DisplayResolver<'a> {
    pub const fn new(generation: &'a CatalogGeneration) -> Self {
        Self { generation }
    }

    pub const fn generation(&self) -> &'a CatalogGeneration {
        self.generation
    }

    pub fn generation_id(&self) -> CatalogGenerationId {
        self.generation.id()
    }

    pub fn catalog(&self) -> &'a DisplayCatalog {
        self.generation.display_catalog()
    }

    pub fn listing_instrument(
        &self,
        listing: &VenueListingKey,
    ) -> Result<&'a InstrumentDisplayString, DisplayError> {
        self.catalog()
            .listing_instrument(listing)
            .ok_or_else(|| DisplayError::ListingNotFound {
                listing: Box::new(listing.clone()),
            })
    }

    pub fn listing_instrument_in(
        &self,
        generation: CatalogGenerationId,
        listing: &VenueListingKey,
    ) -> Result<&'a InstrumentDisplayString, DisplayError> {
        if generation != self.generation_id() {
            return Err(DisplayError::GenerationMismatch {
                expected: generation,
                actual: self.generation_id(),
            });
        }
        self.listing_instrument(listing)
    }

    pub fn listing_search_aliases(&self, listing: &VenueListingKey) -> &'a [SmolStr] {
        self.catalog()
            .listing_search_aliases(listing)
            .unwrap_or_default()
    }

    pub fn instrument_details(
        &self,
        instrument: &InstrumentId,
    ) -> Option<&'a InstrumentDisplayDetails> {
        self.catalog().instrument_details(instrument)
    }

    pub fn asset_code(&self, asset: &AssetId) -> Result<&'a AssetDisplayCode, DisplayError> {
        self.catalog()
            .asset_code(asset)
            .ok_or_else(|| DisplayError::AssetDisplayCodeMissing {
                asset: Box::new(asset.clone()),
            })
    }
}
