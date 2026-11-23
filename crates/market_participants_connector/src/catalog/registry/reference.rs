use ahash::AHashMap;
use market_participants_model::{
    asset::{IndexId, IndexTerms},
    reference::{
        ExternalReferenceId, ExternalReferenceTerms, PriceReferenceId, PriceReferenceTerms,
    },
};

use crate::catalog::error::RegistryError;

#[derive(Clone, Debug, Default)]
pub struct ReferenceRegistry {
    indices: AHashMap<IndexId, IndexTerms>,
    external_references: AHashMap<ExternalReferenceId, ExternalReferenceTerms>,
    price_references: AHashMap<PriceReferenceId, PriceReferenceTerms>,
}

impl ReferenceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn index(&self, id: &IndexId) -> Option<&IndexTerms> {
        self.indices.get(id)
    }

    pub fn external_reference(&self, id: &ExternalReferenceId) -> Option<&ExternalReferenceTerms> {
        self.external_references.get(id)
    }

    pub fn price_reference(&self, id: &PriceReferenceId) -> Option<&PriceReferenceTerms> {
        self.price_references.get(id)
    }

    pub fn price_references(
        &self,
    ) -> impl Iterator<Item = (&PriceReferenceId, &PriceReferenceTerms)> {
        self.price_references.iter()
    }

    pub fn indices(&self) -> impl Iterator<Item = (&IndexId, &IndexTerms)> {
        self.indices.iter()
    }

    pub fn external_references(
        &self,
    ) -> impl Iterator<Item = (&ExternalReferenceId, &ExternalReferenceTerms)> {
        self.external_references.iter()
    }

    pub fn len(&self) -> usize {
        self.indices.len() + self.external_references.len() + self.price_references.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Debug, Default)]
pub struct ReferenceRegistryPending {
    pub indices: AHashMap<IndexId, IndexTerms>,
    pub external_references: AHashMap<ExternalReferenceId, ExternalReferenceTerms>,
    pub price_references: AHashMap<PriceReferenceId, PriceReferenceTerms>,
}

impl ReferenceRegistryPending {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn price_references(
        &self,
    ) -> impl Iterator<Item = (&PriceReferenceId, &PriceReferenceTerms)> {
        self.price_references.iter()
    }

    pub fn register_index(&mut self, terms: IndexTerms) -> IndexId {
        let id = IndexId::of_terms(&terms);
        self.indices.insert(id, terms);
        id
    }

    pub fn register_external_reference(
        &mut self,
        terms: ExternalReferenceTerms,
    ) -> Result<ExternalReferenceId, RegistryError> {
        let id = terms.reference_id()?;
        self.external_references.insert(id, terms);
        Ok(id)
    }

    pub fn register_price_reference(
        &mut self,
        terms: PriceReferenceTerms,
    ) -> Result<PriceReferenceId, RegistryError> {
        let id = terms.reference_id()?;
        self.price_references.insert(id, terms);
        Ok(id)
    }

    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
            && self.external_references.is_empty()
            && self.price_references.is_empty()
    }

    pub fn publish(
        &self,
        base: &ReferenceRegistry,
        referenced: &crate::catalog::batch::ReferencedTerms,
    ) -> ReferenceRegistry {
        let mut registry = base.clone();
        for (id, terms) in &self.indices {
            if referenced.indices.contains(id) {
                registry.indices.insert(*id, terms.clone());
            }
        }
        for (id, terms) in &self.external_references {
            if referenced.external_references.contains(id) {
                registry.external_references.insert(*id, terms.clone());
            }
        }
        for (id, terms) in &self.price_references {
            if referenced.price_references.contains(id) {
                registry.price_references.insert(*id, terms.clone());
            }
        }
        registry
    }
}
