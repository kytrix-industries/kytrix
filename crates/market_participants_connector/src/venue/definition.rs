//! The static definition table of every venue this build serves.
//!
//! One venue is defined exactly once, here: its canonical identity, the capability
//! names its loader can produce and the loader that produces its scopes. Nothing
//! on a request path reads any of
//! it — what may be addressed now is answered by the loaded venues alone — and
//! the only two public accessors are the venue list and the venues that declare
//! one capability.
//!
//! A loader answers the scope set its venue publishes, together with the route
//! metadata that scope set is read against. Binance and OKX publish their API
//! products, so their loaders are constants over the static table in
//! `crate::venue::specs`. Hyperliquid's loader additionally carries the deployed
//! `perp-dex/*` scopes of the venue's own `perpDexs` discovery: that discovery is
//! the one conversation a load has to have before it knows what the venue
//! publishes, and it is dialed through the venue's connection configuration
//! ([`VenueConfig`]), which is what makes a first load need no published route.
//! This table is what that transaction dispatches on.

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

/// One capability of one scope, and whether it needs the venue's credential.
///
/// A capability does not declare a transport: whether one request travels over
/// HTTP or a WebSocket is the request type's property, not the capability's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapabilitySupport {
    /// The capability this scope supports.
    pub capability: CapabilityId,
    /// Whether a request for it has to carry the venue's credential.
    pub needs_auth: bool,
}

/// One scope a venue's loader produces.
///
/// The entry carries no origin and no credential: which host a request dials and
/// which keys it signs with are the venue's own settings, and whether it
/// authenticates is [`CapabilitySupport::needs_auth`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeEntry {
    /// The scope this entry describes.
    pub scope: market_participants_model::identifier::VenueScopeId,
    /// The catalog source this scope's directory is bound to, absent for a scope
    /// that publishes no catalog.
    pub catalog_source: Option<crate::catalog::source::CatalogSourceId>,
    /// The identifier normalization rules of this scope.
    pub normalization: Vec<IdentifierNormalizationRule>,
    /// The capabilities this scope supports.
    pub capabilities: Vec<CapabilitySupport>,
}

/// Everything one successful load answers with.
///
/// The scope set and the metadata it is read against are one reading of one
/// venue: a commit publishes both or neither, so a loaded scope always has the
/// metadata its requests are resolved with. The catalog batches are fetched by
/// the transaction after this answer — they are staged against the routes this
/// same answer registers — so they are not part of it.
pub struct VenueLoadData {
    /// The scopes this load publishes.
    pub scopes: Vec<ScopeEntry>,
    /// The route metadata of this venue.
    metadata: Arc<VenueMetadata>,
}

impl VenueLoadData {
    /// Returns the route metadata this load read.
    ///
    /// Crate-visible because the value it carries is: the transaction that commits a
    /// load is this crate's, and no caller outside it ever names a venue's metadata.
    pub(crate) fn metadata(&self) -> &Arc<VenueMetadata> {
        &self.metadata
    }
}

/// Produces the scope set and the metadata of one venue.
///
/// The load transaction hands the venue's connection configuration and its
/// cancellation to the loader and commits what the loader answered. The loader
/// reaches its venue through that configuration alone: while a load is staging,
/// the venue is not loaded yet, so a conversation has to be dialed through the
/// venue's own configuration rather than through a registered route.
///
/// The answer is a boxed future rather than an `async fn` because the loaders are
/// stored as `&'static dyn VenueLoader` in the definition table, and a trait
/// object cannot carry an `impl Future` return type.
pub trait VenueLoader: Send + Sync {
    /// Returns the scopes and the metadata this venue publishes.
    fn load<'a>(
        &'a self,
        config: &'a VenueConfig,
        cancel: &'a LoadCancellation,
    ) -> BoxFuture<'a, Result<VenueLoadData, VenueLoadError>>;
}

/// The static definition of one venue.
#[derive(Clone)]
pub struct VenueDefinition {
    /// The canonical venue identity.
    pub venue: VenueId,
    /// Every capability this venue's loader can produce.
    pub capabilities: &'static [CapabilityId],
    /// The scope producer of this venue.
    pub loader: &'static dyn VenueLoader,
    /// Whether loading this venue without stored credentials leaves a market
    /// nothing can read.
    ///
    /// A venue whose every read signs is one a user has to configure before a load
    /// is worth offering: the load itself would succeed — its directory is public —
    /// and every pane that then read the market would fail on the credential. A
    /// venue whose reads are public answers `false` and is offered unconditionally.
    pub requires_credentials_to_load: bool,
}

/// Returns every venue this build serves, in the served table's product order.
///
/// This is the static venue list: the market menu draws one row per entry and
/// the settings page one group per entry, whether or not anything is loaded. It
/// is not an answer to what may be addressed now — that is the loaded venues' —
/// and it is not a subset of the identity either: every venue [`VenueId`] carries
/// is served here, because each one's loader and capability names have landed. A
/// venue the identity gained before its loader did would be the one carried
/// without being listed, and this build has none.
pub fn supported_venues() -> Vec<VenueId> {
    definitions()
        .iter()
        .map(|definition| definition.venue)
        .collect()
}

/// Returns every venue whose loader can produce one capability.
///
/// The answer is the capability's name set, never its scopes, its origins or its
/// credentials: it exists for the neutral "nothing here is loaded yet" hint,
/// which may say which venues could answer a capability but not what they would
/// answer it with. A venue without a definition serves nothing and so declares
/// nothing, which is why the table is walked directly rather than through a
/// lookup that could not answer it.
pub fn venues_supporting(capability: CapabilityId) -> Vec<VenueId> {
    definitions()
        .iter()
        .filter(|definition| definition.capabilities.contains(&capability))
        .map(|definition| definition.venue)
        .collect()
}

/// The definition table: one entry per venue this build serves, in product order.
///
/// The order is the interface's rather than the identity's. This is the order a
/// reader meets the venues in — the market menu's rows, the settings rows, and the
/// order a notice that lists markets names them in — so it is a product order,
/// stated here rather than inherited from the alphabetical order of [`VenueId::ALL`].
///
/// An entry is served only while an arm of [`definition_of`] answers it, and nothing
/// else reads this table, so the entry and the arm are the two halves of one fact and
/// are changed together. Every venue this build carries an identity for is served
/// here — the four entries and the four arms are the same four — and a venue the
/// identity gains before its loader lands is the one that would be carried without an
/// entry, which is the state this table and [`definition_of`] would have to separate.
static SERVED: [VenueDefinition; 4] = [
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
    // The venue's directory is public, but every read of the market behind it signs:
    // a load that finished without credentials would leave a market nothing can read.
    VenueDefinition {
        venue: VenueId::Longbridge,
        capabilities: LONGBRIDGE_CAPABILITIES,
        loader: &LONGBRIDGE_LOADER,
        requires_credentials_to_load: true,
    },
];

/// Returns the definition of one venue.
///
/// This is the only dispatch on a venue identity, and it answers every venue the
/// identity carries: each one's loader and capability names have landed, so the match
/// below is total over [`VenueId`] and has an arm for every variant. The three sets —
/// the identity, [`SERVED`] and the arms below — are one set today, which is why the
/// answer is a definition rather than an option. A venue the identity gains before its
/// loader lands is the one that would separate them again, and it would separate this
/// signature with them: an arm answering nothing is what "this build serves no market
/// for it" would look like, and a load of it would be refused rather than panicked on.
///
/// Serving a venue is stated twice and changed twice: its entry in [`SERVED`] and the
/// arm below that answers it. The match is total over [`VenueId`], so a venue added to
/// the identity is a compile error here until its arm is stated, and the set test in
/// `definition_tests` keeps the arms and the table from drifting apart.
pub(crate) fn definition_of(venue: VenueId) -> &'static VenueDefinition {
    match venue {
        VenueId::Binance => &SERVED[0],
        VenueId::Okx => &SERVED[1],
        VenueId::Hyperliquid => &SERVED[2],
        VenueId::Longbridge => &SERVED[3],
    }
}

/// Returns the served definition table.
///
/// The table is a static rather than a value built on first use: the entries above are
/// the whole of it, and nothing about the process can move them.
fn definitions() -> &'static [VenueDefinition] {
    &SERVED
}

/// Binance's scope producer: the API products it declares.
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

/// OKX's scope producer: the API products it declares.
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

/// Hyperliquid's scope producer.
///
/// The scopes answered here are the venue's core ones plus one `perp-dex/<name>`
/// scope per deployment the venue's own `perpDexs` reading verified. That reading
/// is the one conversation the load has to have, and it goes through the venue's
/// connection configuration, so a first load needs no published route. It answers
/// the metadata of every scope beside the names it deploys, so the scopes this
/// loader registers and the metadata the commit publishes come from one reading.
///
/// A reading that could not verify every deployment the venue named, or that
/// stopped at this build's own bound, fails the whole load with nothing committed
/// the venue publishes those deployments, so a partial scope set would
/// be answering something other than what the venue said. Neither is truncated nor
/// skipped.
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
            // A reading that could not verify every deployment the venue named — or
            // that stopped at this build's own bound — fails the whole load.
            // Nothing is truncated and nothing is skipped: the load reports
            // the deployment, or the bound, and commits nothing.
            let (deployed, metadata) = match reading {
                VenueReading::Verified {
                    deployments,
                    metadata,
                } => (deployments, metadata),
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
            Ok(VenueLoadData { scopes, metadata })
        })
    }
}

/// Longbridge's scope producer: the one scope the venue publishes.
///
/// The venue's scope set is static — one US-equity route — so there is nothing to
/// discover: what it publishes is the static table's answer, and the market data
/// behind those scopes is read through the venue's own authenticated socket rather
/// than through a second discovery conversation.
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

/// Answers a load from the static table alone.
///
/// A venue whose scopes are its API products has nothing to discover: its answer
/// is the same value however often it is asked, and the only thing the load can
/// observe is cancellation.
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

/// The one Binance loader instance.
static BINANCE_LOADER: BinanceLoader = BinanceLoader;
/// The one OKX loader instance.
static OKX_LOADER: OkxLoader = OkxLoader;
/// The one Hyperliquid loader instance.
static HYPERLIQUID_LOADER: HyperliquidLoader = HyperliquidLoader;
/// The one Longbridge loader instance.
static LONGBRIDGE_LOADER: LongbridgeLoader = LongbridgeLoader;

/// Every capability Binance's loader can produce.
const BINANCE_CAPABILITIES: &[CapabilityId] = &[
    CapabilityId::MarketCatalog,
    CapabilityId::Candlesticks,
    CapabilityId::MarketTrades,
    CapabilityId::Tickers,
    CapabilityId::OrderBook,
    CapabilityId::ForceOrders,
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

/// Every capability OKX's loader can produce.
const OKX_CAPABILITIES: &[CapabilityId] = &[
    CapabilityId::MarketCatalog,
    CapabilityId::Candlesticks,
    CapabilityId::MarketTrades,
    CapabilityId::Tickers,
    CapabilityId::OrderBook,
    CapabilityId::ForceOrders,
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

/// Every capability Hyperliquid's loader can produce.
///
/// The venue publishes no liquidation stream and none of the derivatives
/// statistics, so it declares neither.
const HYPERLIQUID_CAPABILITIES: &[CapabilityId] = &[
    CapabilityId::MarketCatalog,
    CapabilityId::Candlesticks,
    CapabilityId::MarketTrades,
    CapabilityId::Tickers,
    CapabilityId::OrderBook,
    CapabilityId::WalletMonitor,
    CapabilityId::AuthenticatedTrading,
    CapabilityId::AccountOrders,
    CapabilityId::AccountPositions,
    CapabilityId::AccountBalances,
    CapabilityId::AccountTrades,
];

/// Every capability Longbridge's loader can produce.
///
/// The venue publishes a symbol directory and, over one authenticated session, the
/// candles and the real-time quotes of the markets in it — and nothing else this build
/// serves: its order book, its trades and every account read belong to surfaces this
/// build does not declare for it.
const LONGBRIDGE_CAPABILITIES: &[CapabilityId] = &[
    CapabilityId::MarketCatalog,
    CapabilityId::Candlesticks,
    CapabilityId::Tickers,
];
