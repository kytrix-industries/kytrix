use std::{collections::BTreeMap, sync::Arc};

use market_participants_model::listing::route::VenueRoute;
use smol_str::SmolStr;

use crate::adapter::hyperliquid::{
    HyperliquidActiveContextValues, HyperliquidMetaResponse, HyperliquidOutcomeQuestion,
    HyperliquidOutcomeSpec, HyperliquidOutcomeTemplate, HyperliquidPerpDex,
    HyperliquidSpotMetaResponse,
};

pub(crate) type RouteContexts = BTreeMap<SmolStr, HyperliquidActiveContextValues>;

#[derive(Debug)]
pub(crate) enum ScopeMetadata {
    Spot(SpotRouteMeta),
    Perp(PerpRouteMeta),
    Outcome(OutcomeRouteMeta),
}

impl ScopeMetadata {
    pub(crate) fn contexts(&self) -> &RouteContexts {
        match self {
            Self::Spot(meta) => &meta.contexts,
            Self::Perp(meta) => &meta.contexts,
            Self::Outcome(_) => empty_contexts(),
        }
    }
}

fn empty_contexts() -> &'static RouteContexts {
    static EMPTY: std::sync::OnceLock<RouteContexts> = std::sync::OnceLock::new();
    EMPTY.get_or_init(RouteContexts::new)
}

#[derive(Debug)]
pub(crate) struct SpotRouteMeta {
    pub meta: HyperliquidSpotMetaResponse,
    pub contexts: RouteContexts,
}

#[derive(Debug)]
pub(crate) struct PerpRouteMeta {
    pub perp_dex_index: u32,
    pub meta: HyperliquidMetaResponse,
    pub contexts: RouteContexts,
    pub collateral: SmolStr,
}

#[derive(Debug)]
pub(crate) struct OutcomeRouteMeta {
    pub deployer: Option<SmolStr>,
    pub outcomes: Vec<HyperliquidOutcomeSpec>,
    pub questions: Vec<HyperliquidOutcomeQuestion>,
    pub templates: Vec<HyperliquidOutcomeTemplate>,
}

#[derive(Debug)]
pub(crate) struct VenueMetadata {
    per_scope: BTreeMap<VenueRoute, ScopeMetadata>,
    perp_dexes: Arc<Vec<Option<HyperliquidPerpDex>>>,
}

impl VenueMetadata {
    pub(crate) fn empty() -> Self {
        Self {
            per_scope: BTreeMap::new(),
            perp_dexes: Arc::new(Vec::new()),
        }
    }

    pub(crate) fn new(
        per_scope: BTreeMap<VenueRoute, ScopeMetadata>,
        perp_dexes: Vec<Option<HyperliquidPerpDex>>,
    ) -> Self {
        Self {
            per_scope,
            perp_dexes: Arc::new(perp_dexes),
        }
    }

    pub(crate) fn scope(&self, route: &VenueRoute) -> Option<&ScopeMetadata> {
        self.per_scope.get(route)
    }

    pub(crate) fn perp_dexes(&self) -> &[Option<HyperliquidPerpDex>] {
        &self.perp_dexes
    }
}

impl Default for VenueMetadata {
    fn default() -> Self {
        Self::empty()
    }
}

#[derive(Debug)]
pub(crate) enum VenueReading {
    Verified {
        deployments: Vec<SmolStr>,
        outcome_scopes: Vec<market_participants_model::identifier::VenueScopeId>,
        metadata: Arc<VenueMetadata>,
    },
    Unverifiable {
        dex: SmolStr,
    },
    LimitExceeded {
        named: usize,
        limit: usize,
    },
}
