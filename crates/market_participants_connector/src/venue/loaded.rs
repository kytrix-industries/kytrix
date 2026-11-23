//! The loaded venues: the one routing view of what a load has committed.
//!
//! A venue appears here exactly when its last load committed everything the
//! venue published, so "in the map" and "fully usable" are the same statement.
//! The view answers four questions about a route: whether its venue is loaded,
//! which scopes are in play, what a scope supports, and what a scope is. Catalog
//! *content* is deliberately not among them: it is answered by the published
//! generation of the catalogue, which this view never carries to a
//! reader and never reads.
//!
//! A commit pins the generation it published beside the scope set it published,
//! so the entry a reader captures names one indivisible commit of both the route
//! view and the directory. Neither the pinned generation nor the venue metadata is
//! reachable through this view's query face: they exist for the commit's own
//! correctness and for the pruning of the next one.
//!
//! A commit reaches the application through the catalogue's published generation: the
//! composition root mirrors `CatalogGenerationId` into the engine signal widgets follow,
//! and this view answers the routing questions of the same commit. There is no event
//! boundary to attach and no subscriber to announce to.

use std::{collections::BTreeMap, sync::Arc};

use ahash::AHashMap;
use market_participants_model::{identifier::VenueId, listing::route::VenueRoute};
use parking_lot::{RwLock, RwLockWriteGuard};

use crate::{
    catalog::generation::CatalogGenerationId,
    venue::{definition::ScopeEntry, metadata::VenueMetadata},
};

/// One venue's last successful commit.
pub struct LoadedVenue {
    revision: u64,
    scopes: Arc<[Arc<ScopeEntry>]>,
    /// The identity of the directory generation this commit published.
    ///
    /// The generation itself is deliberately **not** held: a `CatalogGeneration`
    /// owns every route snapshot's index maps and the whole display catalog, so a
    /// venue that pinned one would retain a complete copy of the directory from its
    /// own commit — and every commit made after it would leave one more copy alive
    /// for as long as any venue still pinned it. The id is what this entry has to
    /// state about its commit, and the catalogue's published generation is the one
    /// place the content is read from.
    generation: CatalogGenerationId,
    metadata: Arc<VenueMetadata>,
}

impl LoadedVenue {
    /// Returns the revision this commit published.
    ///
    /// The first commit of a venue is revision one, and every later successful
    /// commit of that venue is one higher; a refreshed venue keeps its revision
    /// unchanged when the load failed.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns this venue's scope entries, in the order the commit published them.
    pub fn scopes(&self) -> &[Arc<ScopeEntry>] {
        &self.scopes
    }

    /// Returns the identity of the directory generation this commit published.
    ///
    /// The id is the commit's own record of which publication it made; the content
    /// of that publication is read from the catalogue, which holds exactly one. No
    /// production read names it — a caller that wants content asks the catalogue —
    /// so the accessor exists for the tests that assert a commit published a new
    /// generation, which is the same seam [`LoadedVenue`] itself is inspected
    /// through.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn generation(&self) -> CatalogGenerationId {
        self.generation
    }

    /// Returns the route metadata this commit published.
    ///
    /// Like the generation id this is the commit's own record rather than a
    /// read face: the gateway hands it to the adapter of an admitted request
    /// through its execution context, and nothing outside this crate can reach it.
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

/// The routing view of every venue a load has committed.
///
/// The venues are ordered by identity, so the scope list a selector draws has one
/// order for the life of the process and cannot reshuffle when a second venue
/// loads. A scope is askable exactly when its venue is present: there is no
/// intermediate state in which a route is known but its content is not.
pub struct LoadedVenues {
    venues: RwLock<BTreeMap<VenueId, Arc<LoadedVenue>>>,
    /// The revision of each venue's last successful commit, removed ones included.
    ///
    /// A venue that publishes no scope leaves the table, but its commit history does
    /// not: the counter is what makes every successful commit of a venue — a first
    /// load, a refresh, a whole-venue removal — exactly one revision higher than the
    /// commit before it. Without it a removal would either report the previous
    /// commit's revision again or lose the trail, and a reader that compares
    /// revisions would drop the removal.
    revisions: RwLock<AHashMap<VenueId, u64>>,
}

impl LoadedVenues {
    /// Creates the empty view: no venue is loaded.
    pub(crate) fn new() -> Self {
        Self {
            venues: RwLock::new(BTreeMap::new()),
            revisions: RwLock::new(AHashMap::new()),
        }
    }

    /// Returns one venue's last commit, or nothing when it is not loaded.
    pub fn loaded(&self, venue: &VenueId) -> Option<Arc<LoadedVenue>> {
        self.venues.read().get(venue).cloned()
    }

    /// Returns every loaded scope, ordered by venue and then by commit order.
    ///
    /// This is the one list of what may be addressed now: the selector's
    /// candidates, a pane's gating and a persisted target's validation all read
    /// it, and none of them reads the static definition table.
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

    /// Returns every loaded scope together with its entry, and the route each pair
    /// belongs to.
    ///
    /// One reading of one commit answers all three: the pairs come from a single
    /// read of the venue table, and a caller that needs both the route and what the
    /// scope supports therefore takes them from one commit. A selector's "publishes
    /// a catalog" and "supports the wanted capability" judgements and the route they
    /// apply to come from here, so no candidate can be drawn from one commit and
    /// judged against another's entries.
    ///
    /// [`Self::scopes`] answers the routes alone, in the same order, for a caller
    /// that needs only what may be addressed now.
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

    /// Returns the entry of one scope, or nothing when its venue is not loaded
    /// or does not publish that scope.
    ///
    /// This is the admission answer: a request for a scope this returns nothing
    /// for is refused without being encoded and without any request being sent.
    pub fn scope(&self, route: &VenueRoute) -> Option<Arc<ScopeEntry>> {
        self.venues
            .read()
            .get(&route.venue)?
            .scopes
            .iter()
            .find(|entry| entry.scope == route.scope)
            .cloned()
    }

    /// Takes the venue table's write lock for one commit.
    ///
    /// The commit of a venue writes three things in one critical section — this
    /// table, the route registry and the directory's publication pointer — so the
    /// guard is taken by the transaction before it reaches the other two and is
    /// handed to the publication as the callback that replaces this venue's entry.
    /// Nothing else takes this lock for writing.
    pub(crate) fn lock_for_commit(&self) -> VenueCommitGuard<'_> {
        VenueCommitGuard {
            venues: self.venues.write(),
            revisions: self.revisions.write(),
        }
    }
}

/// The venue table's write guard of one commit.
pub(crate) struct VenueCommitGuard<'a> {
    venues: RwLockWriteGuard<'a, BTreeMap<VenueId, Arc<LoadedVenue>>>,
    revisions: RwLockWriteGuard<'a, AHashMap<VenueId, u64>>,
}

impl VenueCommitGuard<'_> {
    /// Replaces one venue's entry with the commit being published.
    ///
    /// The slot is replaced whole and the venue's revision advances by one, so a
    /// reader sees either the previous commit or this one and never a mix of the
    /// two. The generation pinned here is the one the same critical section just
    /// published.
    ///
    /// A commit that publishes no scope at all is a commit like any other: the entry
    /// leaves the table ("in the map" means everything it publishes is
    /// available) and the revision it announces is still this commit's own, one
    /// higher than the previous one. A removal is therefore announced exactly once,
    /// with a revision no earlier commit of that venue used.
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
