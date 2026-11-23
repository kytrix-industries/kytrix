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

#[derive(Clone)]
pub struct MarketParticipantCatalogSource {
    gateway: MarketParticipantGateway,
    catalog: Arc<Catalog>,
    routes: Option<Arc<crate::routing::RegisteredRoutes>>,
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

pub(crate) fn markets_batch(
    entries: Vec<crate::adapter::NativeCatalogEntry>,
) -> Result<CatalogBatch, SourceError> {
    let mut asset_terms: Vec<AssetTerms> = Vec::with_capacity(entries.len() * 2);
    let mut asset_ids: AHashSet<AssetId> = AHashSet::with_capacity(entries.len() * 2);
    let mut reference_terms: Vec<PriceReferenceTerms> = Vec::with_capacity(entries.len());
    let mut reference_ids: AHashSet<PriceReferenceId> = AHashSet::with_capacity(entries.len());
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
        ..CatalogBatch::default()
    })
}

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
        MarketParticipantError::SymbolDirectoryFetchFailed { venue, reason } => {
            SourceError::SymbolDirectory { venue, reason }
        }
        other => SourceError::Other {
            reason: smol_str::format_smolstr!("{}", other),
        },
    }
}

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
