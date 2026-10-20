//! The adapter-backed catalog source of the shared catalogue.
//!
//! A source owns no identity of its own: it asks the route's adapter for the
//! venue's own catalog answer, turns each fully decoded entry into a canonical
//! `Instrument` together with its `VenueListing` and `ListingSnapshot`, and hands
//! the whole batch back. Validation and publication belong to the catalogue, and
//! the venue's wire identity belongs to the adapter — nothing here invents a
//! symbol, normalizes a market name or keeps a second directory.
//!
//! The one thing this source is asked for is a whole-route catalog: nothing here
//! reads a single venue record on demand, so no read path of the directory can
//! reach the wire. The factory that binds this source to a registered catalog
//! route is [`market_participant_catalog_source`].

use std::sync::Arc;

use ahash::AHashSet;
use futures::future::BoxFuture;
use market_participants_model::{
    asset::AssetId,
    error::ModelError,
    listing::listing_impl::{ListingSnapshot, VenueListing},
    reference::{PriceReferenceId, PriceReferenceTerms},
};

use crate::{
    adapter::{NativeCatalogEntry, ProtocolRequest, ProtocolResponse},
    catalog::{
        batch::{CatalogBatch, CatalogEntry},
        display::publication::StagedAssetCode,
        error::{SourceError, SourceProtocolRejection},
        registry::asset::AssetTerms,
        source::{CatalogFetchContext, CatalogSource, CatalogSourceId},
        store::Catalog,
    },
    client::MarketParticipantGateway,
    market_participants::model::MarketCatalogQuery,
    protocol::{RequestIdentity, classify},
    venue::metadata::VenueMetadata,
};

/// Adapter-owned protocol boundary for the shared catalogue.
#[derive(Clone)]
pub struct MarketParticipantCatalogSource {
    gateway: MarketParticipantGateway,
    catalog: Arc<Catalog>,
    /// The route view this source's fetch is validated against.
    ///
    /// A load fetches its catalogs before the venue it is loading exists in the
    /// registry, so the fetch is validated against the candidate the same load is
    /// about to commit: the routes are the venue's own, built from its scope
    /// set, and the registry is written from that same candidate inside the
    /// commit. A source bound to an already registered route leaves this absent and
    /// validates against the registry as it stands.
    routes: Option<Arc<crate::routing::RegisteredRoutes>>,
    /// The venue metadata the catalog is derived from.
    ///
    /// A catalog is a projection of what the load read, so the fetch carries that
    /// reading rather than asking the venue again. A source bound to an
    /// already registered route leaves this absent: its fetch is admitted by the
    /// loaded venues, which hand the commit's own metadata to the execution.
    metadata: Option<Arc<VenueMetadata>>,
}

impl MarketParticipantCatalogSource {
    pub(crate) fn new(gateway: MarketParticipantGateway, catalog: Arc<Catalog>) -> Self {
        Self {
            gateway,
            catalog,
            routes: None,
            metadata: None,
        }
    }

    /// Builds the source of one load, validated against the load's own candidate
    /// and resolved against the metadata the same load read.
    pub(crate) fn with_routes(
        gateway: MarketParticipantGateway,
        catalog: Arc<Catalog>,
        routes: Arc<crate::routing::RegisteredRoutes>,
        metadata: Arc<VenueMetadata>,
    ) -> Self {
        Self {
            gateway,
            catalog,
            routes: Some(routes),
            metadata: Some(metadata),
        }
    }
}

impl CatalogSource for MarketParticipantCatalogSource {
    fn fetch_catalog(
        &self,
        context: CatalogFetchContext,
    ) -> BoxFuture<'static, Result<CatalogBatch, SourceError>> {
        let gateway = self.gateway.clone();
        let catalog = Arc::clone(&self.catalog);
        let routes = self.routes.clone();
        let metadata = self.metadata.clone();
        Box::pin(async move {
            let route = context.venue_route().clone();
            let request = ProtocolRequest::FetchMarkets(MarketCatalogQuery {
                route: route.clone(),
            });
            // The capability a request needs is the classifier's answer, so a
            // fetch never restates it.
            // A fetch is staged: the venue it reads is exactly the one no commit has
            // published yet, so it is validated against the view the load built for
            // it rather than against the loaded venues (which do not hold it).
            let view = routes.unwrap_or_else(|| gateway.registered());
            let request_context =
                RequestIdentity::new(route.clone(), classify(&request).capability());
            let response = gateway
                .execute_with_routes(&view, metadata, request, request_context, &catalog)
                .await
                .map_err(source_error)?;
            match response {
                ProtocolResponse::Markets(entries) => markets_batch(entries),
                _ => Err(SourceError::Protocol {
                    operation: "fetch instrument catalog".into(),
                    reason: SourceProtocolRejection::UnexpectedResponse,
                }),
            }
        })
    }
}

/// Turns one adapter's market answer into the catalogue's own batch.
///
/// The terms the adapter built while it translated the venue's answer travel with the
/// listings that name their identities: one canonical asset or reference is named by many
/// listings of one route, so each identity is carried once, in the order the venue's own
/// answer first stated it. The codes the adapter verified for the assets whose own terms
/// state none — a security, whose ticker is a listing fact — travel the same way.
///
/// This is the production path from an adapter's answer to a published generation, so a
/// fixture that already holds an adapter's entries goes through it rather than around it.
pub(crate) fn markets_batch(
    entries: Vec<crate::adapter::NativeCatalogEntry>,
) -> Result<CatalogBatch, SourceError> {
    // The terms the adapter built while it translated the venue's answer travel with the
    // listings that name their identities: one canonical asset or reference is named by
    // many listings of one route, so each identity is carried once, in the order the
    // venue's own answer first stated it.
    let mut asset_terms: Vec<AssetTerms> = Vec::with_capacity(entries.len() * 2);
    let mut asset_ids: AHashSet<AssetId> = AHashSet::with_capacity(entries.len() * 2);
    let mut reference_terms: Vec<PriceReferenceTerms> = Vec::with_capacity(entries.len());
    let mut reference_ids: AHashSet<PriceReferenceId> = AHashSet::with_capacity(entries.len());
    // The codes the adapter verified for the assets whose own terms state none — a
    // security, whose ticker is a listing fact — travel the same way, and one asset is
    // stated once whatever listing of the route names it.
    let mut asset_codes: Vec<StagedAssetCode> = Vec::with_capacity(entries.len());
    let mut coded_assets: AHashSet<AssetId> = AHashSet::with_capacity(entries.len());
    for entry in &entries {
        for terms in &entry.asset_terms {
            if asset_ids.insert(terms.asset_id()) {
                asset_terms.push(terms.clone());
            }
        }
        for staged in &entry.asset_codes {
            if coded_assets.insert(staged.asset.clone()) {
                asset_codes.push(staged.clone());
            }
        }
        for terms in &entry.reference_terms {
            let id = terms
                .reference_id()
                .map_err(|error| SourceError::Protocol {
                    operation: "build instrument catalog".into(),
                    reason: SourceProtocolRejection::Model {
                        error: Box::new(error),
                    },
                })?;
            if reference_ids.insert(id) {
                reference_terms.push(terms.clone());
            }
        }
    }
    let mut listings: Vec<CatalogEntry> = Vec::with_capacity(entries.len());
    for entry in entries {
        listings.push(catalog_entry(entry).map_err(|error| SourceError::Protocol {
            operation: "build instrument catalog".into(),
            reason: SourceProtocolRejection::Model {
                error: Box::new(error),
            },
        })?);
    }
    Ok(CatalogBatch {
        listings,
        asset_terms,
        reference_terms,
        asset_codes,
        // The remaining fields are the outcome dimensions, which a listing read
        // publishes none of.
        ..CatalogBatch::default()
    })
}

/// Turns one fully decoded venue entry into its canonical catalog entry.
///
/// The adapter has already decided the venue's own typed identifiers and its trading
/// rules, so what is left here is pairing the entry with the instrument it names: a
/// disagreement between the identifiers and the instrument is refused by the
/// catalogue's publication gate rather than papered over.
fn catalog_entry(entry: NativeCatalogEntry) -> Result<CatalogEntry, ModelError> {
    let NativeCatalogEntry {
        listing,
        native_symbol,
        aliases,
        identifiers,
        rules,
        status,
        available_from,
        available_until,
        instrument,
        ..
    } = entry;
    let instrument_id = *instrument.id();
    let snapshot_key = listing.clone();
    let listing = VenueListing::new(
        listing,
        instrument_id,
        Some(native_symbol),
        aliases,
        identifiers,
    )?;
    let snapshot = ListingSnapshot {
        key: snapshot_key,
        rules,
        status,
        available_from,
        available_until,
    };
    Ok(CatalogEntry {
        instrument,
        listing,
        snapshot,
    })
}

/// Classifies one market-participant failure for the catalogue's fetch boundary.
fn source_error(error: crate::error::MarketParticipantError) -> SourceError {
    use crate::error::{DisconnectReason, MarketParticipantError};

    match error {
        MarketParticipantError::ConnectionFailed { .. }
        | MarketParticipantError::Disconnected(
            DisconnectReason::Network(_) | DisconnectReason::HeartbeatTimeout,
        ) => SourceError::Network {
            reason: smol_str::format_smolstr!("{}", error),
        },
        MarketParticipantError::AuthenticationFailed { .. } => SourceError::Authentication {
            reason: smol_str::format_smolstr!("{}", error),
        },
        MarketParticipantError::RateLimited { retry_after_ms, .. } => {
            SourceError::RateLimited { retry_after_ms }
        }
        MarketParticipantError::Rejected {
            code,
            message,
            retryable,
            ..
        } => SourceError::Rejected {
            code,
            message,
            retryable,
        },
        MarketParticipantError::DecodeFailed { operation, .. } => SourceError::Decode { operation },
        MarketParticipantError::ProtocolError {
            operation, reason, ..
        } => SourceError::Protocol {
            operation,
            reason: SourceProtocolRejection::Venue { reason },
        },
        MarketParticipantError::Configuration(reason) => SourceError::Configuration { reason },
        // The one failure of a fetch that is about a market other than the route's
        // own venue: a symbol directory belongs to the market participant that publishes it,
        // so the venue travels as a typed identity rather than as an English
        // sentence interpolated into a localized one.
        MarketParticipantError::SymbolDirectoryFetchFailed { venue, reason } => {
            SourceError::SymbolDirectory { venue, reason }
        }
        other => SourceError::Other {
            reason: smol_str::format_smolstr!("{}", other),
        },
    }
}

/// Binds the adapter-backed source to every catalog route a registry update
/// names.
///
/// The registry constructs one source instance per catalog source id and binds
/// it there; a source never sees a bare route string, and it carries the shared
/// catalogue so its own fetch resolves listings through the same gateway
/// validation as every other protocol call.
pub(crate) fn market_participant_catalog_source<'a>(
    gateway: &'a MarketParticipantGateway,
    catalog: &'a Arc<Catalog>,
) -> impl Fn(&CatalogSourceId) -> Arc<dyn CatalogSource> + 'a {
    let gateway = gateway.clone();
    let catalog = Arc::clone(catalog);
    move |_| {
        Arc::new(MarketParticipantCatalogSource::new(
            gateway.clone(),
            Arc::clone(&catalog),
        ))
    }
}
