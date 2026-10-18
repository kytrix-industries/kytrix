//! The route metadata of one venue, as one committed load holds it.
//!
//! Metadata is a **venue-level** value, not a route's: a reader that resolves one
//! route's listings against the venue's own tables — the deployment table that
//! gives a builder-deployed DEX its position, or the metadata of scopes other than
//! the one the request addresses, which a venue-wide wallet read decodes against —
//! has to reach every route of the venue, so one venue holds one copy.
//!
//! It is read **once**, at load time, and committed with the venue; it
//! reaches an adapter through the execution context and is never fetched on a
//! request path and never answered by [`crate::venue::loaded::LoadedVenues`].
//! A venue whose request path reads no metadata — Binance and OKX — holds an
//! empty value, which is its correct answer rather than a missing one.
//!
//! Every value here describes one moment of the venue. The deployment table and
//! the metadata of every perpetual scope come from one reading, because a DEX's
//! position in that table is what the venue's own asset ID for its perpetuals is
//! built from: a table read at another moment would move the identity of every
//! listing the reading published.
//!
//! A venue-level spot token table is deliberately **not** part of this value,
//! The reading needs it — a builder-deployed DEX's
//! collateral is an index into it and verifying the deployment resolves that index
//! — but it is a local of the reading: the table is already carried by the spot
//! scope's own metadata, because the first element of `spotMetaAndAssetCtxs` *is*
//! the token table, and that is the copy the spot catalog resolves its pairs
//! against. A second venue-level copy would be a field nothing reads.

use std::{collections::BTreeMap, sync::Arc};

use market_participants_model::listing::route::VenueRoute;
use smol_str::SmolStr;

use crate::adapter::hyperliquid::{
    HyperliquidActiveContextValues, HyperliquidMetaResponse, HyperliquidPerpDex,
    HyperliquidSpotMetaResponse,
};

/// One scope's asset contexts, keyed by the coin the venue named.
pub(crate) type RouteContexts = BTreeMap<SmolStr, HyperliquidActiveContextValues>;

/// The venue metadata of one scope.
#[derive(Debug)]
pub(crate) enum ScopeMetadata {
    /// A spot scope: the pairs it publishes and their asset contexts.
    Spot(SpotRouteMeta),
    /// A perpetual scope: its assets, its contexts, its position in the venue's
    /// deployment table and the token it settles in.
    Perp(PerpRouteMeta),
}

impl ScopeMetadata {
    /// Returns the asset contexts of this scope, keyed by the venue's own coin.
    pub(crate) fn contexts(&self) -> &RouteContexts {
        match self {
            Self::Spot(meta) => &meta.contexts,
            Self::Perp(meta) => &meta.contexts,
        }
    }
}

/// One spot scope's venue metadata.
#[derive(Debug)]
pub(crate) struct SpotRouteMeta {
    pub meta: HyperliquidSpotMetaResponse,
    pub contexts: RouteContexts,
}

/// One perpetual scope's venue metadata.
#[derive(Debug)]
pub(crate) struct PerpRouteMeta {
    /// The position of the DEX in the venue's own `perpDexs` answer.
    pub perp_dex_index: u32,
    pub meta: HyperliquidMetaResponse,
    pub contexts: RouteContexts,
    /// The token name the DEX settles in.
    pub collateral: SmolStr,
}

/// One venue's route metadata, committed with the venue that published it.
///
/// The value never leaves this crate: a loaded venue hands it to the adapter through
/// the execution context, and the crate's own accessors are crate-private, so its
/// visibility is the crate's too.
#[derive(Debug)]
pub(crate) struct VenueMetadata {
    per_scope: BTreeMap<VenueRoute, ScopeMetadata>,
    perp_dexes: Arc<Vec<Option<HyperliquidPerpDex>>>,
}

impl VenueMetadata {
    /// Returns the metadata of a venue whose request path reads none.
    ///
    /// An empty value is a venue's own answer, not a gap: Binance and OKX resolve
    /// every request from their catalog alone, so their scope metadata and their
    /// deployment table are empty by construction.
    pub(crate) fn empty() -> Self {
        Self {
            per_scope: BTreeMap::new(),
            perp_dexes: Arc::new(Vec::new()),
        }
    }

    /// Builds the metadata one reading of a venue answered.
    ///
    /// The transaction is the one caller: it hands the value to the commit that
    /// publishes it, and the same value is what every request of the loaded venue
    /// is resolved against.
    pub(crate) fn new(
        per_scope: BTreeMap<VenueRoute, ScopeMetadata>,
        perp_dexes: Vec<Option<HyperliquidPerpDex>>,
    ) -> Self {
        Self {
            per_scope,
            perp_dexes: Arc::new(perp_dexes),
        }
    }

    /// Returns the metadata of one scope, or nothing for a scope this venue's
    /// reading did not cover.
    pub(crate) fn scope(&self, route: &VenueRoute) -> Option<&ScopeMetadata> {
        self.per_scope.get(route)
    }

    /// Returns the venue's builder-deployed DEX table, as the reading named it.
    pub(crate) fn perp_dexes(&self) -> &[Option<HyperliquidPerpDex>] {
        &self.perp_dexes
    }
}

impl Default for VenueMetadata {
    fn default() -> Self {
        Self::empty()
    }
}

/// What one reading of a venue's deployments came to.
///
/// A reading either verifies every deployment the venue named and answers the
/// scope set to register, or reports the one thing that stopped it. Both failures
/// are the whole load's answer: the venue decides what it publishes, so a
/// scope set that leaves a deployment out — or that stops at a bound of this
/// build's own — would be answering something other than what the venue said.
/// Which of the two is a failure and how it is reported is the loader's decision;
/// this value only states what the reading found.
#[derive(Debug)]
pub(crate) enum VenueReading {
    /// Every deployment the venue named verified.
    Verified {
        /// The deployed scopes, in the venue's own order.
        deployments: Vec<SmolStr>,
        /// The route metadata of every scope the reading covered.
        metadata: Arc<VenueMetadata>,
    },
    /// The venue named a deployment whose metadata this build cannot serve.
    Unverifiable { dex: SmolStr },
    /// The venue named more deployments than one reading verifies.
    LimitExceeded { named: usize, limit: usize },
}
