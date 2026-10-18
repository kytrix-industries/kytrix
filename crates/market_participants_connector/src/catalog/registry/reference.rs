//! The single creation boundary of index, external and price reference identities.
//!
//! A reference identity is derived from its canonical terms, which include the
//! observed quantity, the session and settlement convention and the terms schema
//! version. Two venues can only share a reference identity when those terms are
//! identical: a shared ticker, a shared endpoint or a shared display role never
//! merges two references.

use ahash::AHashMap;
use market_participants_model::{
    asset::{IndexId, IndexTerms},
    reference::{
        ExternalReferenceId, ExternalReferenceTerms, PriceReferenceId, PriceReferenceTerms,
    },
};

use crate::catalog::error::RegistryError;

/// The reference identities of one catalog generation.
#[derive(Clone, Debug, Default)]
pub struct ReferenceRegistry {
    indices: AHashMap<IndexId, IndexTerms>,
    external_references: AHashMap<ExternalReferenceId, ExternalReferenceTerms>,
    price_references: AHashMap<PriceReferenceId, PriceReferenceTerms>,
}

impl ReferenceRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the registered index terms.
    pub fn index(&self, id: &IndexId) -> Option<&IndexTerms> {
        self.indices.get(id)
    }

    /// Returns the registered external reference terms.
    pub fn external_reference(&self, id: &ExternalReferenceId) -> Option<&ExternalReferenceTerms> {
        self.external_references.get(id)
    }

    /// Returns the registered price reference terms.
    pub fn price_reference(&self, id: &PriceReferenceId) -> Option<&PriceReferenceTerms> {
        self.price_references.get(id)
    }

    /// Returns every registered price reference with its terms.
    pub fn price_references(
        &self,
    ) -> impl Iterator<Item = (&PriceReferenceId, &PriceReferenceTerms)> {
        self.price_references.iter()
    }

    /// Returns every registered index with its terms.
    ///
    /// The catalog publishes a display code for every index a generation names, including
    /// the ones no listing of that generation tracks, so the publication reads the
    /// whole registry rather than the indexes its own candidates happen to use.
    pub fn indices(&self) -> impl Iterator<Item = (&IndexId, &IndexTerms)> {
        self.indices.iter()
    }

    /// Returns every registered external reference with its terms.
    ///
    /// The external half of `external_reference_codes` has the same rule as
    /// the index half: the code of a registered reference is published whether or
    /// not a listing of this generation names it.
    pub fn external_references(
        &self,
    ) -> impl Iterator<Item = (&ExternalReferenceId, &ExternalReferenceTerms)> {
        self.external_references.iter()
    }

    /// Returns the number of registered references of every family.
    pub fn len(&self) -> usize {
        self.indices.len() + self.external_references.len() + self.price_references.len()
    }

    /// Returns `true` when no reference is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The reference terms staged by one in-flight catalog batch.
#[derive(Clone, Debug, Default)]
pub struct ReferenceRegistryPending {
    /// The staged index terms.
    pub indices: AHashMap<IndexId, IndexTerms>,
    /// The staged external reference terms.
    pub external_references: AHashMap<ExternalReferenceId, ExternalReferenceTerms>,
    /// The staged price reference terms.
    pub price_references: AHashMap<PriceReferenceId, PriceReferenceTerms>,
}

impl ReferenceRegistryPending {
    /// Creates an empty pending view.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns every staged price reference with its terms.
    ///
    /// The publication keeps only the references its own listings track, so the
    /// staged set is the one place a reference a *sibling* publication registered
    /// is still visible: the display gate reads the whole staged set to decide
    /// which references share one display scope.
    pub fn price_references(
        &self,
    ) -> impl Iterator<Item = (&PriceReferenceId, &PriceReferenceTerms)> {
        self.price_references.iter()
    }

    /// Stages an index identity.
    pub fn register_index(&mut self, terms: IndexTerms) -> IndexId {
        let id = IndexId::of_terms(&terms);
        self.indices.insert(id.clone(), terms);
        id
    }

    /// Stages an external reference identity.
    pub fn register_external_reference(
        &mut self,
        terms: ExternalReferenceTerms,
    ) -> Result<ExternalReferenceId, RegistryError> {
        let id = terms.reference_id()?;
        self.external_references.insert(id.clone(), terms);
        Ok(id)
    }

    /// Stages a price reference identity.
    pub fn register_price_reference(
        &mut self,
        terms: PriceReferenceTerms,
    ) -> Result<PriceReferenceId, RegistryError> {
        let id = terms.reference_id()?;
        self.price_references.insert(id.clone(), terms);
        Ok(id)
    }

    /// Returns `true` when nothing was staged.
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
            && self.external_references.is_empty()
            && self.price_references.is_empty()
    }

    /// Publishes this pending view on top of the generation's registry.
    ///
    /// Only the references the batch actually tracks reach the new registry, so a
    /// staged identity that no published listing uses is dropped instead of
    /// becoming an orphan.
    pub fn publish(
        &self,
        base: &ReferenceRegistry,
        referenced: &crate::catalog::batch::ReferencedTerms,
    ) -> ReferenceRegistry {
        let mut registry = base.clone();
        for (id, terms) in &self.indices {
            if referenced.indices.contains(id) {
                registry.indices.insert(id.clone(), terms.clone());
            }
        }
        for (id, terms) in &self.external_references {
            if referenced.external_references.contains(id) {
                registry
                    .external_references
                    .insert(id.clone(), terms.clone());
            }
        }
        for (id, terms) in &self.price_references {
            if referenced.price_references.contains(id) {
                registry.price_references.insert(id.clone(), terms.clone());
            }
        }
        registry
    }
}
