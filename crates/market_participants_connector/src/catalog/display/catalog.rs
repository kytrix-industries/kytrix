//! The immutable display catalog of one published generation.
//!
//! The catalog is the single place the generation's display decisions live: the
//! code of every canonical asset, the name of every instrument and the name of
//! every published listing. A reader reads these values and never re-formats them,
//! so a name can never be rendered twice with two different answers inside one
//! generation.
//!
//! [`super::publication`] builds the catalog in one pass over the surviving route
//! snapshots and registries; nothing here mutates a published value.

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

/// The display values of one catalog generation.
///
/// Every map is keyed by a canonical identity and holds values derived from that
/// identity's own terms or from the staging of the route that published it, so two
/// canonical assets can never share one display entry and one asset can never
/// answer with two codes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DisplayCatalog {
    /// The published display code of every canonical asset the catalog names.
    ///
    /// A security is read here too: its ticker is the code a route staged, and the
    /// catalog publishes it under the same `AssetId` every other asset uses.
    asset_codes: AHashMap<AssetId, AssetDisplayCode>,
    /// The published display code of every index.
    index_codes: AHashMap<IndexId, AssetDisplayCode>,
    /// The published display code of every external reference.
    external_reference_codes: AHashMap<ExternalReferenceId, AssetDisplayCode>,
    /// The published economic name of every instrument.
    instrument_names: AHashMap<InstrumentId, InstrumentDisplayString>,
    /// The details every economic name was rendered from.
    instrument_details: AHashMap<InstrumentId, InstrumentDisplayDetails>,
    /// The published name of every listing, which is the name of the instrument it
    /// implements.
    listings: AHashMap<VenueListingKey, InstrumentDisplayString>,
    /// The normalized search aliases of every listing: its primary native symbol
    /// and its declared aliases.
    listing_search_aliases: AHashMap<VenueListingKey, Arc<[SmolStr]>>,
}

impl DisplayCatalog {
    /// Builds a catalog from its published maps.
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

    /// Returns the display code of one canonical asset.
    ///
    /// A missing entry is an asset the catalog never named: either its terms state
    /// no code and no route staged one, or the identity is not part of this
    /// generation at all.
    pub fn asset_code(&self, asset: &AssetId) -> Option<&AssetDisplayCode> {
        self.asset_codes.get(asset)
    }

    /// Returns the display code of one index.
    pub fn index_code(&self, index: &IndexId) -> Option<&AssetDisplayCode> {
        self.index_codes.get(index)
    }

    /// Returns the display code of one external reference.
    pub fn external_reference_code(
        &self,
        reference: &ExternalReferenceId,
    ) -> Option<&AssetDisplayCode> {
        self.external_reference_codes.get(reference)
    }

    /// Returns the details the economic name of one instrument was rendered from.
    pub fn instrument_details(
        &self,
        instrument: &InstrumentId,
    ) -> Option<&InstrumentDisplayDetails> {
        self.instrument_details.get(instrument)
    }

    /// Returns the published name of one listing.
    pub fn listing_instrument(
        &self,
        listing: &VenueListingKey,
    ) -> Option<&InstrumentDisplayString> {
        self.listings.get(listing)
    }

    /// Returns the normalized search aliases of one listing.
    pub fn listing_search_aliases(&self, listing: &VenueListingKey) -> Option<&[SmolStr]> {
        self.listing_search_aliases
            .get(listing)
            .map(|aliases| aliases.as_ref())
    }
}

/// One published generation, captured for a read that has to be answered from a
/// single publication.
///
/// A snapshot owns the pointer the catalogue published at the moment it was
/// taken and holds nothing else: capturing one copies no map — the generation is
/// immutable, so the pointer is all a reader needs — and never fetches, so a
/// route that was never loaded is answered from the generation that is published
/// without it. Every value read through a snapshot belongs to the generation it
/// captured, whatever is published afterwards, which is what makes a list of rows
/// read through one snapshot a list of one publication.
#[derive(Clone, Debug)]
pub struct DisplaySnapshot {
    generation: Arc<CatalogGeneration>,
}

impl DisplaySnapshot {
    /// Captures the pointer of one published generation.
    pub(crate) const fn new(generation: Arc<CatalogGeneration>) -> Self {
        Self { generation }
    }

    /// Returns the generation this snapshot captured.
    pub fn generation(&self) -> &CatalogGeneration {
        &self.generation
    }

    /// Returns the identity of the generation this snapshot captured.
    pub fn generation_id(&self) -> CatalogGenerationId {
        self.generation.id()
    }

    /// Binds a resolver to the captured generation.
    pub fn resolver(&self) -> DisplayResolver<'_> {
        self.generation.display_resolver()
    }

    /// Returns the name of one published listing.
    ///
    /// A reader that keeps the value after the read — a row of a list — asks for
    /// it here, so the only thing copied is the name it asked for. A listing the
    /// captured publication never named is a [`DisplayError::ListingNotFound`].
    pub fn listing_instrument(
        &self,
        listing: &VenueListingKey,
    ) -> Result<InstrumentDisplayString, DisplayError> {
        self.resolver().listing_instrument(listing).cloned()
    }

    /// Returns the precision one published listing writes its numbers at.
    ///
    /// The increments come from the listing's own rules in the captured
    /// generation, so every number a reader draws through one snapshot keeps the
    /// precision of the publication it read the listing from. A listing the
    /// captured publication does not hold has no precision: a caller then writes
    /// its number in the canonical decimal form rather than holding back the row.
    pub fn listing_precision(&self, listing: &VenueListingKey) -> Option<ListingPrecision> {
        self.generation
            .route_snapshot(&listing.route)?
            .listing(listing)
            .map(|entry| ListingPrecision::of(&entry.snapshot.rules))
    }
}

/// A reader bound to one published generation.
///
/// The resolver borrows the generation it was created from and answers from the
/// values that generation already published. It never formats, never looks a
/// term up and never reaches another generation, so every value it returns
/// belongs to the same publication.
#[derive(Clone, Copy, Debug)]
pub struct DisplayResolver<'a> {
    generation: &'a CatalogGeneration,
}

impl<'a> DisplayResolver<'a> {
    /// Binds a resolver to one published generation.
    pub const fn new(generation: &'a CatalogGeneration) -> Self {
        Self { generation }
    }

    /// Returns the generation this resolver reads.
    pub const fn generation(&self) -> &'a CatalogGeneration {
        self.generation
    }

    /// Returns the identity of the generation this resolver reads.
    pub fn generation_id(&self) -> CatalogGenerationId {
        self.generation.id()
    }

    /// Returns the display catalog of this generation.
    pub fn catalog(&self) -> &'a DisplayCatalog {
        self.generation.display_catalog()
    }

    /// Returns the name of one published listing.
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

    /// Returns the name of one listing on an explicitly named generation.
    ///
    /// The generation is the caller's own claim about which publication it read
    /// the key from; when it names another generation than the one this resolver
    /// reads, the two values must not be mixed and the read is refused.
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

    /// Returns the search aliases of one listing, empty when it published none.
    pub fn listing_search_aliases(&self, listing: &VenueListingKey) -> &'a [SmolStr] {
        self.catalog()
            .listing_search_aliases(listing)
            .unwrap_or_default()
    }

    /// Returns the details the economic name of one instrument was rendered from.
    pub fn instrument_details(
        &self,
        instrument: &InstrumentId,
    ) -> Option<&'a InstrumentDisplayDetails> {
        self.catalog().instrument_details(instrument)
    }

    /// Returns the display code of one canonical asset.
    pub fn asset_code(&self, asset: &AssetId) -> Result<&'a AssetDisplayCode, DisplayError> {
        self.catalog()
            .asset_code(asset)
            .ok_or_else(|| DisplayError::AssetDisplayCodeMissing {
                asset: Box::new(asset.clone()),
            })
    }
}
