use market_participants_model::{
    asset::{AssetId, IndexId},
    display::values::DisplayFormatError,
    error::ModelError,
    listing::route::VenueListingKey,
    outcome::OutcomeGroupId,
    reference::{ExternalReferenceId, PriceReferenceId},
    routing::ListingIdentifierKind,
    settlement::InstrumentId,
};
use smol_str::SmolStr;

use crate::catalog::generation::CatalogGenerationId;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DisplayError {
    #[error("the display formatter refused the value: {error}")]
    Format {
        #[from]
        error: DisplayFormatError,
    },
    #[error("listing {listing:?} is absent from the published display catalog")]
    ListingNotFound { listing: Box<VenueListingKey> },
    #[error("the canonical asset {asset} has no registered terms")]
    MissingAssetTerms { asset: Box<AssetId> },
    #[error("the index {index:?} has no registered terms")]
    MissingIndexTerms { index: Box<IndexId> },
    #[error("the external reference {reference} has no registered terms")]
    MissingExternalReferenceTerms { reference: Box<ExternalReferenceId> },
    #[error("the price reference {reference} has no registered terms")]
    MissingReferenceTerms { reference: PriceReferenceId },
    #[error("outcome group {group} has no registered terms")]
    MissingOutcomeTerms { group: OutcomeGroupId },
    #[error("the canonical asset {asset} has no display code")]
    AssetDisplayCodeMissing { asset: Box<AssetId> },
    #[error("the canonical asset {asset} is staged with two different display codes")]
    StagedCodeMismatch { asset: Box<AssetId> },
    #[error("the published name of instrument {instrument} is not the rendering of its details")]
    InstrumentNameMismatch { instrument: Box<InstrumentId> },
    #[error("the price reference {reference} is not the {field} its contract requires")]
    ReferenceDirectionMismatch {
        reference: PriceReferenceId,
        field: &'static str,
    },
    #[error("listing {listing:?} declares identifiers that {kind} does not echo")]
    ListingIdentifierEcho {
        listing: Box<VenueListingKey>,
        kind: ListingIdentifierKind,
        error: Box<ModelError>,
    },
    #[error(
        "listing {listing:?} declares {value} as {kind}, which its native symbols do not carry"
    )]
    ListingIdentifierMismatch {
        listing: Box<VenueListingKey>,
        kind: ListingIdentifierKind,
        value: SmolStr,
    },
    #[error("the operation was made for generation {expected} but read generation {actual}")]
    GenerationMismatch {
        expected: CatalogGenerationId,
        actual: CatalogGenerationId,
    },
}
