use std::{collections::BTreeMap, sync::Arc};

use ahash::AHashMap;
use market_participants_model::{identifier::VenueId, listing::route::VenueRoute};
use parking_lot::{RwLock, RwLockWriteGuard};

use crate::{
    catalog::generation::CatalogGenerationId,
    venue::{definition::ScopeEntry, metadata::VenueMetadata},
};

pub struct LoadedVenue {
    revision: u64,
    scopes: Arc<[Arc<ScopeEntry>]>,
    generation: CatalogGenerationId,
    metadata: Arc<VenueMetadata>,
}

impl LoadedVenue {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn scopes(&self) -> &[Arc<ScopeEntry>] {
        &self.scopes
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn generation(&self) -> CatalogGenerationId {
        self.generation
    }

    pub(crate) fn metadata(&self) -> &Arc<VenueMetadata> {
        &self.metadata
    }
}

impl std::fmt::Debug for LoadedVenue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoadedVenue")
            .field("revision", &self.revision)
            .field("scopes", &self.scopes.len())
            .field("generation", &self.generation)
            .field("perp_dexes", &self.metadata.perp_dexes().len())
            .finish()
    }
}

pub struct LoadedVenues {
    venues: RwLock<BTreeMap<VenueId, Arc<LoadedVenue>>>,
    revisions: RwLock<AHashMap<VenueId, u64>>,
}

impl LoadedVenues {
    pub(crate) fn new() -> Self {
        Self {
            venues: RwLock::new(BTreeMap::new()),
            revisions: RwLock::new(AHashMap::new()),
        }
    }

    pub fn loaded(&self, venue: &VenueId) -> Option<Arc<LoadedVenue>> {
        self.venues.read().get(venue).cloned()
    }

    pub fn scopes(&self) -> Arc<[VenueRoute]> {
        self.venues
            .read()
            .iter()
            .flat_map(|(venue, loaded)| {
                loaded
                    .scopes
                    .iter()
                    .map(|entry| VenueRoute::new(*venue, entry.scope.clone()))
            })
            .collect()
    }

    pub fn entries(&self) -> Arc<[(VenueRoute, Arc<ScopeEntry>)]> {
        self.venues
            .read()
            .iter()
            .flat_map(|(venue, loaded)| {
                loaded.scopes.iter().map(|entry| {
                    (
                        VenueRoute::new(*venue, entry.scope.clone()),
                        Arc::clone(entry),
                    )
                })
            })
            .collect()
    }

    pub fn scope(&self, route: &VenueRoute) -> Option<Arc<ScopeEntry>> {
        self.venues
            .read()
            .get(&route.venue)?
            .scopes
            .iter()
            .find(|entry| entry.scope == route.scope)
            .cloned()
    }

    pub(crate) fn lock_for_commit(&self) -> VenueCommitGuard<'_> {
        VenueCommitGuard {
            venues: self.venues.write(),
            revisions: self.revisions.write(),
        }
    }
}

pub(crate) struct VenueCommitGuard<'a> {
    venues: RwLockWriteGuard<'a, BTreeMap<VenueId, Arc<LoadedVenue>>>,
    revisions: RwLockWriteGuard<'a, AHashMap<VenueId, u64>>,
}

impl VenueCommitGuard<'_> {
    pub(crate) fn replace(
        &mut self,
        venue: VenueId,
        scopes: Vec<Arc<ScopeEntry>>,
        generation: CatalogGenerationId,
        metadata: Arc<VenueMetadata>,
    ) -> u64 {
        let revision = self.revisions.get(&venue).map_or(1, |last| last + 1);
        self.revisions.insert(venue, revision);
        if scopes.is_empty() {
            self.venues.remove(&venue);
            return revision;
        }
        self.venues.insert(
            venue,
            Arc::new(LoadedVenue {
                revision,
                scopes: scopes.into(),
                generation,
                metadata,
            }),
        );
        revision
    }
}
