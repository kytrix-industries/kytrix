//! The typed display values one published catalog generation exposes.
//!
//! A display string is derived from the canonical terms of one generation and is
//! never an identity encoder: no type here turns a string back into an
//! `InstrumentId` or a `VenueListingKey`. The listing side keeps exactly one
//! value — the name of the listed instrument — because every other fact about a
//! listing, such as its route, its native symbol and its typed identifiers, is
//! read from the [`VenueListing`](market_participants_model::listing::listing_impl::VenueListing)
//! and the key the caller already holds.

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

/// A failure at the display read or publication boundary.
///
/// The set is closed and covers both sides of the contract. The formatter's own
/// failures arrive as [`DisplayError::Format`]; every other variant is a
/// publication or read failure the pure formatter cannot see, such as a missing
/// registry entry, an asset no code was resolved for, a staged code two entries
/// disagree on, a name that is not the rendering of its own details, a listing
/// whose identifiers contradict their declared symbols, or a generation mismatch.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DisplayError {
    /// The pure formatter refused one display step.
    #[error("the display formatter refused the value: {error}")]
    Format {
        /// The formatter failure.
        #[from]
        error: DisplayFormatError,
    },
    /// The listing has no published display name.
    #[error("listing {listing:?} is absent from the published display catalog")]
    ListingNotFound {
        /// The listing the read named.
        listing: Box<VenueListingKey>,
    },
    /// A canonical asset the display needs has no registered terms.
    #[error("the canonical asset {asset} has no registered terms")]
    MissingAssetTerms {
        /// The asset whose terms are missing.
        asset: Box<AssetId>,
    },
    /// An index the display needs has no registered terms.
    #[error("the index {index:?} has no registered terms")]
    MissingIndexTerms {
        /// The index whose terms are missing.
        index: Box<IndexId>,
    },
    /// An external reference the display needs has no registered terms.
    #[error("the external reference {reference} has no registered terms")]
    MissingExternalReferenceTerms {
        /// The reference whose terms are missing.
        reference: Box<ExternalReferenceId>,
    },
    /// A price reference the display needs has no registered terms.
    #[error("the price reference {reference} has no registered terms")]
    MissingReferenceTerms {
        /// The reference whose terms are missing.
        reference: PriceReferenceId,
    },
    /// The outcome registry of this generation carries no terms for the group an
    /// outcome listing names.
    #[error("outcome group {group} has no registered terms")]
    MissingOutcomeTerms {
        /// The group whose terms are missing.
        group: OutcomeGroupId,
    },
    /// A canonical asset a published name renders has no display code.
    ///
    /// A code comes from the terms of the asset or from the staging of a route,
    /// and the assets whose own terms state none — a contract token, a security —
    /// need the staged one. An asset nothing displays may stay without a code; one
    /// a name renders may not, so the batch that would publish that name is
    /// refused rather than published under a code the catalog cannot answer.
    #[error("the canonical asset {asset} has no display code")]
    AssetDisplayCodeMissing {
        /// The asset that has no code.
        asset: Box<AssetId>,
    },
    /// Two staged codes of one canonical asset disagree.
    ///
    /// One asset has exactly one code, whatever route staged it and whatever its
    /// own terms state, so two statements that differ refuse the whole batch
    /// instead of being resolved by order.
    #[error("the canonical asset {asset} is staged with two different display codes")]
    StagedCodeMismatch {
        /// The asset whose staged codes disagree.
        asset: Box<AssetId>,
    },
    /// A published name is not the rendering of the details it was published
    /// with, or a published listing names an instrument the publication did not
    /// name at all.
    ///
    /// Both are the same failure: one generation would carry two answers about
    /// one instrument's name.
    #[error("the published name of instrument {instrument} is not the rendering of its details")]
    InstrumentNameMismatch {
        /// The instrument whose name and details disagree.
        instrument: Box<InstrumentId>,
    },
    /// A price reference's registered terms do not state the direction and basis
    /// the instrument requires.
    ///
    /// The direction is fixed: the reference tracks the settlement asset as its
    /// underlying, is quoted in the price quote and is expressed per asset unit.
    /// An identity is a one-way digest, so only the registry terms can answer
    /// this, and a mismatch refuses the publication.
    #[error("the price reference {reference} is not the {field} its contract requires")]
    ReferenceDirectionMismatch {
        /// The reference whose direction disagreed.
        reference: PriceReferenceId,
        /// The role the reference was published for.
        field: &'static str,
    },
    /// A listing's typed identifiers contradict its declared native symbols, or
    /// its own normalization refused a value.
    ///
    /// The echo check is the model's low-level comparison of the typed
    /// identifiers against the declared symbols, and a failure of normalization is
    /// reported the same way: the comparison could not be made at all, and
    /// publishing an unverified listing is exactly what the gate exists to
    /// prevent.
    #[error("listing {listing:?} declares identifiers that {kind} does not echo")]
    ListingIdentifierEcho {
        /// The listing whose echo disagreed.
        listing: Box<VenueListingKey>,
        /// The primary identifier family of the listing.
        kind: ListingIdentifierKind,
        /// The normalization or comparison that refused the listing.
        error: Box<ModelError>,
    },
    /// A typed identifier of a listing names a value its declared native symbols
    /// do not carry.
    ///
    /// The listing has a typed identity and a declared native spelling of it, and
    /// the two disagree, so the publication is refused rather than published under
    /// a name one of the two contradicts.
    #[error(
        "listing {listing:?} declares {value} as {kind}, which its native symbols do not carry"
    )]
    ListingIdentifierMismatch {
        /// The listing whose echo disagreed.
        listing: Box<VenueListingKey>,
        /// The identifier family whose echo disagreed.
        kind: ListingIdentifierKind,
        /// The typed value that was not echoed.
        value: SmolStr,
    },
    /// A read was handed a generation that is not the one it read from.
    #[error("the operation was made for generation {expected} but read generation {actual}")]
    GenerationMismatch {
        /// The generation the caller named.
        expected: CatalogGenerationId,
        /// The generation that was actually read.
        actual: CatalogGenerationId,
    },
}
