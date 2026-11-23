use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ahash::{AHashMap, AHashSet};
use app_config::{settings::Language, translation::TranslationModel};
use market_participants_model::prediction::PredictionEventCard;
use smol_str::{SmolStr, format_smolstr};

const CARDS_PER_CHUNK: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranslationSetup {
    pub directory: Option<PathBuf>,
    pub model: Option<TranslationModel>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranslationSegment {
    pub id: SmolStr,
    pub text: SmolStr,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranslatedSegment {
    pub id: SmolStr,
    pub text: SmolStr,
}

#[derive(Clone, Debug, Default)]
pub struct TranslationCancel(Arc<AtomicBool>);

impl TranslationCancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranslationFailure {
    Cancelled,
    ModelDirectoryUnset,
    ModelNotSelected,
    ModelBusy,
    ContentTooLong,
    Response,
    Engine,
}

pub trait LocalTranslator: Send + Sync + 'static {
    fn prepare(
        &self,
        setup: &TranslationSetup,
        cancel: TranslationCancel,
    ) -> Result<(), TranslationFailure>;

    fn translate(
        &self,
        setup: &TranslationSetup,
        target: Language,
        segments: Vec<TranslationSegment>,
        cancel: TranslationCancel,
    ) -> Result<Vec<TranslatedSegment>, TranslationFailure>;
}

pub struct TranslationChunk {
    segments: Vec<TranslationSegment>,
}

impl TranslationChunk {
    #[must_use]
    pub fn segments(&self) -> &[TranslationSegment] {
        &self.segments
    }
}

#[must_use]
pub fn plan_translation(
    cards: &[PredictionEventCard],
    already: &AHashMap<SmolStr, SmolStr>,
) -> Vec<TranslationChunk> {
    let texts: usize = cards
        .iter()
        .map(|card| 1 + card.options.len())
        .sum::<usize>();
    let mut requested: AHashSet<SmolStr> = AHashSet::with_capacity(texts);
    let mut chunks: Vec<TranslationChunk> =
        Vec::with_capacity(cards.len().div_ceil(CARDS_PER_CHUNK));

    for group in cards.chunks(CARDS_PER_CHUNK) {
        let capacity: usize = group.iter().map(|card| 1 + card.options.len()).sum();
        let mut segments: Vec<TranslationSegment> = Vec::with_capacity(capacity);

        for card in group {
            let labels = card.options.iter().map(|option| &option.label);
            for text in std::iter::once(&card.title).chain(labels) {
                if text.trim().is_empty()
                    || already.contains_key(text)
                    || !requested.insert(text.clone())
                {
                    continue;
                }
                segments.push(TranslationSegment {
                    id: format_smolstr!("s{:02}", segments.len()),
                    text: text.clone(),
                });
            }
        }

        if !segments.is_empty() {
            chunks.push(TranslationChunk { segments });
        }
    }

    chunks
}

#[must_use]
pub fn page_has_translatable_text(cards: &[PredictionEventCard]) -> bool {
    cards.iter().any(|card| {
        !card.title.trim().is_empty()
            || card
                .options
                .iter()
                .any(|option| !option.label.trim().is_empty())
    })
}

#[cfg(test)]
mod tests;
