use std::sync::Arc;

use parking_lot::Mutex;

use crate::catalog::registry::{
    asset::{AssetRegistry, AssetRegistryPending},
    outcome::{OutcomeRegistry, OutcomeRegistryPending},
    reference::ReferenceRegistryPending,
};

#[derive(Clone, Debug)]
pub struct StagedRegistry {
    assets: Arc<AssetRegistry>,
    references: Arc<crate::catalog::registry::reference::ReferenceRegistry>,
    outcomes: Arc<OutcomeRegistry>,
}

impl StagedRegistry {
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

    pub fn empty() -> Self {
        Self {
            assets: Arc::new(AssetRegistry::new()),
            references: Arc::new(crate::catalog::registry::reference::ReferenceRegistry::new()),
            outcomes: Arc::new(OutcomeRegistry::new()),
        }
    }

    pub fn assets(&self) -> &Arc<AssetRegistry> {
        &self.assets
    }

    pub fn references(&self) -> &Arc<crate::catalog::registry::reference::ReferenceRegistry> {
        &self.references
    }

    pub fn outcomes(&self) -> &Arc<OutcomeRegistry> {
        &self.outcomes
    }
}

#[derive(Clone, Debug)]
pub struct CatalogStaging {
    base: StagedRegistry,
    assets: AssetRegistryPending,
    references: ReferenceRegistryPending,
    outcomes: OutcomeRegistryPending,
}

impl CatalogStaging {
    pub fn new(base: StagedRegistry) -> Self {
        Self {
            base,
            assets: AssetRegistryPending::new(),
            references: ReferenceRegistryPending::new(),
            outcomes: OutcomeRegistryPending::new(),
        }
    }

    pub fn base(&self) -> &StagedRegistry {
        &self.base
    }

    pub fn pending_assets(&self) -> &AssetRegistryPending {
        &self.assets
    }

    pub fn pending_assets_mut(&mut self) -> &mut AssetRegistryPending {
        &mut self.assets
    }

    pub fn pending_references(&self) -> &ReferenceRegistryPending {
        &self.references
    }

    pub fn pending_references_mut(&mut self) -> &mut ReferenceRegistryPending {
        &mut self.references
    }

    pub fn pending_outcomes(&self) -> &OutcomeRegistryPending {
        &self.outcomes
    }

    pub fn pending_outcomes_mut(&mut self) -> &mut OutcomeRegistryPending {
        &mut self.outcomes
    }

    pub fn is_empty(&self) -> bool {
        self.assets.is_empty() && self.references.is_empty() && self.outcomes.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct CatalogStagingHandle {
    staging: Arc<Mutex<CatalogStaging>>,
}

impl CatalogStagingHandle {
    pub fn new(base: StagedRegistry) -> Self {
        Self {
            staging: Arc::new(Mutex::new(CatalogStaging::new(base))),
        }
    }

    pub fn with<R>(&self, operation: impl FnOnce(&mut CatalogStaging) -> R) -> R {
        operation(&mut self.staging.lock())
    }

    pub fn base(&self) -> StagedRegistry {
        self.staging.lock().base.clone()
    }

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
