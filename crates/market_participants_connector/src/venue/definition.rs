use std::sync::Arc;

use futures::future::BoxFuture;
use market_participants_model::{
    identifier::VenueId,
    routing::{CapabilityId, IdentifierNormalizationRule},
};

use crate::{
    adapter::hyperliquid::catalog::perp_dex_descriptor,
    connector::VenueLoadError,
    venue::{
        config::{LoadCancellation, VenueConfig},
        metadata::{VenueMetadata, VenueReading},
        specs,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapabilitySupport {
    pub capability: CapabilityId,
    pub needs_auth: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeEntry {
    pub scope: market_participants_model::identifier::VenueScopeId,
    pub catalog_source: Option<crate::catalog::source::CatalogSourceId>,
    pub normalization: Vec<IdentifierNormalizationRule>,
    pub capabilities: Vec<CapabilitySupport>,
}

pub struct VenueLoadData {
    pub scopes: Vec<ScopeEntry>,
    metadata: Arc<VenueMetadata>,
}

impl VenueLoadData {
    pub(crate) fn metadata(&self) -> &Arc<VenueMetadata> {
        &self.metadata
    }
}

pub trait VenueLoader: Send + Sync {
    fn load<'a>(
        &'a self,
        config: &'a VenueConfig,
        cancel: &'a LoadCancellation,
    ) -> BoxFuture<'a, Result<VenueLoadData, VenueLoadError>>;
}

#[derive(Clone)]
pub struct VenueDefinition {
    pub venue: VenueId,
    pub capabilities: &'static [CapabilityId],
    pub loader: &'static dyn VenueLoader,
    pub requires_credentials_to_load: bool,
}

pub fn supported_venues() -> Vec<VenueId> {
    definitions()
        .iter()
        .map(|definition| definition.venue)
        .collect()
}

pub fn venues_supporting(capability: CapabilityId) -> Vec<VenueId> {
    definitions()
        .iter()
        .filter(|definition| definition.capabilities.contains(&capability))
        .map(|definition| definition.venue)
        .collect()
}

static SERVED: [VenueDefinition; 5] = [
    VenueDefinition {
        venue: VenueId::Binance,
        capabilities: BINANCE_CAPABILITIES,
        loader: &BINANCE_LOADER,
        requires_credentials_to_load: false,
    },
    VenueDefinition {
        venue: VenueId::Okx,
        capabilities: OKX_CAPABILITIES,
        loader: &OKX_LOADER,
        requires_credentials_to_load: false,
    },
    VenueDefinition {
        venue: VenueId::Hyperliquid,
        capabilities: HYPERLIQUID_CAPABILITIES,
        loader: &HYPERLIQUID_LOADER,
        requires_credentials_to_load: false,
    },
    VenueDefinition {
        venue: VenueId::Longbridge,
        capabilities: LONGBRIDGE_CAPABILITIES,
        loader: &LONGBRIDGE_LOADER,
        requires_credentials_to_load: true,
    },
    VenueDefinition {
        venue: VenueId::Polymarket,
        capabilities: POLYMARKET_CAPABILITIES,
        loader: &POLYMARKET_LOADER,
        requires_credentials_to_load: false,
    },
];

pub(crate) fn definition_of(venue: VenueId) -> &'static VenueDefinition {
    match venue {
        VenueId::Binance => &SERVED[0],
        VenueId::Okx => &SERVED[1],
        VenueId::Hyperliquid => &SERVED[2],
        VenueId::Longbridge => &SERVED[3],
        VenueId::Polymarket => &SERVED[4],
    }
}

fn definitions() -> &'static [VenueDefinition] {
    &SERVED
}

struct BinanceLoader;

impl VenueLoader for BinanceLoader {
    fn load<'a>(
        &'a self,
        config: &'a VenueConfig,
        cancel: &'a LoadCancellation,
    ) -> BoxFuture<'a, Result<VenueLoadData, VenueLoadError>> {
        Box::pin(constant_load(*config.venue(), cancel))
    }
}

struct OkxLoader;

impl VenueLoader for OkxLoader {
    fn load<'a>(
        &'a self,
        config: &'a VenueConfig,
        cancel: &'a LoadCancellation,
    ) -> BoxFuture<'a, Result<VenueLoadData, VenueLoadError>> {
        Box::pin(constant_load(*config.venue(), cancel))
    }
}

struct HyperliquidLoader;

impl VenueLoader for HyperliquidLoader {
    fn load<'a>(
        &'a self,
        config: &'a VenueConfig,
        cancel: &'a LoadCancellation,
    ) -> BoxFuture<'a, Result<VenueLoadData, VenueLoadError>> {
        Box::pin(async move {
            let venue = *config.venue();
            if cancel.is_cancelled() {
                return Err(VenueLoadError::Interrupted { venue });
            }
            let reading =
                config
                    .read_venue()
                    .await
                    .map_err(|source| VenueLoadError::Discovery {
                        venue,
                        source: Box::new(source),
                    })?;
            if cancel.is_cancelled() {
                return Err(VenueLoadError::Interrupted { venue });
            }
            let (deployed, outcome_scopes, metadata) = match reading {
                VenueReading::Verified {
                    deployments,
                    outcome_scopes,
                    metadata,
                } => (deployments, outcome_scopes, metadata),
                VenueReading::Unverifiable { dex } => {
                    return Err(VenueLoadError::DiscoveryUnverifiable { venue, dex });
                }
                VenueReading::LimitExceeded { named, limit } => {
                    return Err(VenueLoadError::DiscoveryLimitExceeded {
                        venue,
                        named,
                        limit,
                    });
                }
            };
            let mut scopes = specs::scope_entries(venue);
            for name in &deployed {
                let descriptor = perp_dex_descriptor(name).map_err(|_| {
                    VenueLoadError::DiscoveryUnverifiable {
                        venue,
                        dex: name.clone(),
                    }
                })?;
                scopes.push(specs::entry_of_descriptor(&descriptor));
            }
            for scope in outcome_scopes {
                let descriptor = crate::adapter::hyperliquid::catalog::outcome_venue_descriptor(
                    scope,
                )
                .map_err(|source| VenueLoadError::Discovery {
                    venue,
                    source: Box::new(source),
                })?;
                scopes.push(specs::entry_of_descriptor(&descriptor));
            }
            Ok(VenueLoadData { scopes, metadata })
        })
    }
}

struct PolymarketLoader;

impl VenueLoader for PolymarketLoader {
    fn load<'a>(
        &'a self,
        config: &'a VenueConfig,
        cancel: &'a LoadCancellation,
    ) -> BoxFuture<'a, Result<VenueLoadData, VenueLoadError>> {
        Box::pin(constant_load(*config.venue(), cancel))
    }
}

struct LongbridgeLoader;

impl VenueLoader for LongbridgeLoader {
    fn load<'a>(
        &'a self,
        config: &'a VenueConfig,
        cancel: &'a LoadCancellation,
    ) -> BoxFuture<'a, Result<VenueLoadData, VenueLoadError>> {
        Box::pin(constant_load(*config.venue(), cancel))
    }
}

async fn constant_load(
    venue: VenueId,
    cancel: &LoadCancellation,
) -> Result<VenueLoadData, VenueLoadError> {
    if cancel.is_cancelled() {
        return Err(VenueLoadError::Interrupted { venue });
    }
    Ok(VenueLoadData {
        scopes: specs::scope_entries(venue),
        metadata: Arc::new(VenueMetadata::empty()),
    })
}

static BINANCE_LOADER: BinanceLoader = BinanceLoader;
static OKX_LOADER: OkxLoader = OkxLoader;
static HYPERLIQUID_LOADER: HyperliquidLoader = HyperliquidLoader;
static LONGBRIDGE_LOADER: LongbridgeLoader = LongbridgeLoader;
static POLYMARKET_LOADER: PolymarketLoader = PolymarketLoader;

const BINANCE_CAPABILITIES: &[CapabilityId] = &[
    CapabilityId::MarketCatalog,
    CapabilityId::Candlesticks,
    CapabilityId::MarketTrade,
    CapabilityId::Tickers,
    CapabilityId::AllTickers,
    CapabilityId::OrderBook,
    CapabilityId::ForceOrder,
    CapabilityId::OpenInterest,
    CapabilityId::LongShortAccountRatio,
    CapabilityId::TopTraderAccountRatio,
    CapabilityId::TopTraderPositionRatio,
    CapabilityId::TakerVolume,
    CapabilityId::AuthenticatedTrading,
    CapabilityId::AccountOrders,
    CapabilityId::AccountPositions,
    CapabilityId::AccountBalances,
    CapabilityId::AccountTrades,
    CapabilityId::AccountHistory,
];

const OKX_CAPABILITIES: &[CapabilityId] = &[
    CapabilityId::MarketCatalog,
    CapabilityId::Candlesticks,
    CapabilityId::MarketTrade,
    CapabilityId::Tickers,
    CapabilityId::AllTickers,
    CapabilityId::OrderBook,
    CapabilityId::ForceOrder,
    CapabilityId::OpenInterest,
    CapabilityId::LongShortAccountRatio,
    CapabilityId::TopTraderAccountRatio,
    CapabilityId::TopTraderPositionRatio,
    CapabilityId::TakerVolume,
    CapabilityId::AuthenticatedTrading,
    CapabilityId::AccountOrders,
    CapabilityId::AccountPositions,
    CapabilityId::AccountBalances,
    CapabilityId::AccountTrades,
    CapabilityId::AccountHistory,
];

const HYPERLIQUID_CAPABILITIES: &[CapabilityId] = &[
    CapabilityId::MarketCatalog,
    CapabilityId::Candlesticks,
    CapabilityId::MarketTrade,
    CapabilityId::Tickers,
    CapabilityId::AllTickers,
    CapabilityId::OrderBook,
    CapabilityId::PredictionEvents,
    CapabilityId::PredictionSeries,
    CapabilityId::Wallet,
    CapabilityId::AuthenticatedTrading,
    CapabilityId::AccountOrders,
    CapabilityId::AccountPositions,
    CapabilityId::AccountBalances,
    CapabilityId::AccountTrades,
];

const LONGBRIDGE_CAPABILITIES: &[CapabilityId] = &[
    CapabilityId::MarketCatalog,
    CapabilityId::Candlesticks,
    CapabilityId::Tickers,
];

const POLYMARKET_CAPABILITIES: &[CapabilityId] = &[
    CapabilityId::PredictionEvents,
    CapabilityId::PredictionSeries,
    CapabilityId::PredictionOrderBook,
];
