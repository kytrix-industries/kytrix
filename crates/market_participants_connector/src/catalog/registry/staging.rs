//! The copy-on-write staging view a catalog source resolves identities in.
//!
//! A source can create the canonical identities its batch needs, but every write
//! lands in a private pending view. The catalog merges those views into the new
//! generation only after the whole batch — listings, references, outcome
//! relations and lineage — has been validated, so a refused batch leaves no
//! asset, reference or outcome identity behind. Nothing in this module can
//! publish a generation, change a published registry or choose an adapter,
//! endpoint or capability.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::catalog::registry::{
    asset::{AssetRegistry, AssetRegistryPending},
    outcome::{OutcomeRegistry, OutcomeRegistryPending},
    reference::ReferenceRegistryPending,
};

/// The published registries a staging transaction starts from.
#[derive(Clone, Debug)]
pub struct StagedRegistry {
    assets: Arc<AssetRegistry>,
    references: Arc<crate::catalog::registry::reference::ReferenceRegistry>,
    outcomes: Arc<OutcomeRegistry>,
}

impl StagedRegistry {
    /// Builds the published registry set of one generation.
    pub fn new(
        assets: Arc<AssetRegistry>,
        references: Arc<crate::catalog::registry::reference::ReferenceRegistry>,
        outcomes: Arc<OutcomeRegistry>,
    ) -> Self {
        Self {
            assets,
            references,
            outcomes,
        }
    }

    /// Returns an empty registry set.
    pub fn empty() -> Self {
        Self {
            assets: Arc::new(AssetRegistry::new()),
            references: Arc::new(crate::catalog::registry::reference::ReferenceRegistry::new()),
            outcomes: Arc::new(OutcomeRegistry::new()),
        }
    }

    /// Returns the published asset registry.
    pub fn assets(&self) -> &Arc<AssetRegistry> {
        &self.assets
    }

    /// Returns the published reference registry.
    pub fn references(&self) -> &Arc<crate::catalog::registry::reference::ReferenceRegistry> {
        &self.references
    }

    /// Returns the published outcome registry.
    pub fn outcomes(&self) -> &Arc<OutcomeRegistry> {
        &self.outcomes
    }
}

/// One in-flight registry transaction: the published registries plus everything
/// staged so far.
#[derive(Clone, Debug)]
pub struct CatalogStaging {
    base: StagedRegistry,
    assets: AssetRegistryPending,
    references: ReferenceRegistryPending,
    outcomes: OutcomeRegistryPending,
}

impl CatalogStaging {
    /// Opens a transaction on top of the published registries.
    pub fn new(base: StagedRegistry) -> Self {
        Self {
            base,
            assets: AssetRegistryPending::new(),
            references: ReferenceRegistryPending::new(),
            outcomes: OutcomeRegistryPending::new(),
        }
    }

    /// Returns the published registry set this transaction started from.
    pub fn base(&self) -> &StagedRegistry {
        &self.base
    }

    /// Returns the staged asset terms.
    pub fn pending_assets(&self) -> &AssetRegistryPending {
        &self.assets
    }

    /// Returns the staged asset terms for writing.
    pub fn pending_assets_mut(&mut self) -> &mut AssetRegistryPending {
        &mut self.assets
    }

    /// Returns the staged reference terms.
    pub fn pending_references(&self) -> &ReferenceRegistryPending {
        &self.references
    }

    /// Returns the staged reference terms for writing.
    pub fn pending_references_mut(&mut self) -> &mut ReferenceRegistryPending {
        &mut self.references
    }

    /// Returns the staged outcome relations.
    pub fn pending_outcomes(&self) -> &OutcomeRegistryPending {
        &self.outcomes
    }

    /// Returns the staged outcome relations for writing.
    pub fn pending_outcomes_mut(&mut self) -> &mut OutcomeRegistryPending {
        &mut self.outcomes
    }

    /// Returns `true` when nothing has been written to this transaction.
    pub fn is_empty(&self) -> bool {
        self.assets.is_empty() && self.references.is_empty() && self.outcomes.is_empty()
    }
}

/// The controlled view of the staging transaction that a catalog source holds.
///
/// The handle is cloneable and shares one transaction with every other clone,
/// which is what lets a source resolve identities from several tasks of one
/// fetch. It exposes the canonical registry operations and nothing else: it
/// cannot publish, cannot read a published generation and cannot change a route.
#[derive(Clone, Debug)]
pub struct CatalogStagingHandle {
    staging: Arc<Mutex<CatalogStaging>>,
}

impl CatalogStagingHandle {
    /// Opens a transaction handle on top of the published registries.
    pub fn new(base: StagedRegistry) -> Self {
        Self {
            staging: Arc::new(Mutex::new(CatalogStaging::new(base))),
        }
    }

    /// Runs `operation` against the shared transaction.
    pub fn with<R>(&self, operation: impl FnOnce(&mut CatalogStaging) -> R) -> R {
        operation(&mut self.staging.lock())
    }

    /// Returns the published registry set this transaction started from.
    pub fn base(&self) -> StagedRegistry {
        self.staging.lock().base.clone()
    }

    /// Returns `true` when nothing has been staged through this handle.
    pub fn is_empty(&self) -> bool {
        self.staging.lock().is_empty()
    }
}

impl PartialEq for CatalogStagingHandle {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.staging, &other.staging)
    }
}

impl Eq for CatalogStagingHandle {}
