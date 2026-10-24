//! The fake feed several test modules answer through.
//!
//! One fake is shared because every module asks the service the same questions and
//! answers them with the same document: a second copy per module would be the same
//! lines with a different name.

use std::sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicUsize, Ordering},
};

use super::{
    model::{PizzaDataError, PizzaLevel, PizzaReading, WireEnvelope},
    service::{PizzaFetchError, PizzaSource},
};

/// What a fetch should answer with.
#[derive(Clone, Debug)]
enum Answer {
    /// One document, exactly as the feed publishes it.
    Document(String),
    /// One failure, rather than a document.
    Failure(PizzaFetchError),
}

/// A feed whose answers, and whose number of reads, a test decides.
///
/// With nothing set it fails every read, so a test that forgot to answer gets a
/// failure rather than a reading nobody published.
#[derive(Clone, Default)]
pub(super) struct FakeSource {
    answer: Arc<Mutex<Option<Answer>>>,
    fetches: Arc<AtomicUsize>,
}

impl FakeSource {
    /// Answers every read with `document`.
    pub(super) fn answer(&self, document: String) {
        self.set(Answer::Document(document));
    }

    /// Answers every read with `error`.
    pub(super) fn fail(&self, error: PizzaFetchError) {
        self.set(Answer::Failure(error));
    }

    /// Answers every read with the feed's own refusal.
    pub(super) fn refuse(&self) {
        self.answer(r#"{"success":false,"error":"rate limited"}"#.to_owned());
    }

    /// How many times the feed has been read.
    pub(super) fn fetches(&self) -> usize {
        self.fetches.load(Ordering::Acquire)
    }

    fn set(&self, answer: Answer) {
        *self.answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(answer);
    }
}

impl PizzaSource for FakeSource {
    async fn fetch(&self) -> Result<WireEnvelope, PizzaFetchError> {
        self.fetches.fetch_add(1, Ordering::AcqRel);
        // The guard is dropped before the answer is used, so nothing is held across
        // a suspension point.
        let answer = self
            .answer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();

        match answer {
            Some(Answer::Document(document)) => serde_json::from_str(&document)
                .map_err(|_| PizzaFetchError::Data(PizzaDataError::Unreadable)),
            Some(Answer::Failure(error)) => Err(error),
            None => Err(PizzaFetchError::Data(PizzaDataError::Unreadable)),
        }
    }
}

/// One answer of the feed that states `level`.
pub(super) fn answer(level: u8) -> String {
    format!(
        r#"{{"success":true,"data":{{"timestamp":"{TIMESTAMP}","defcon_level":{level},"defcon_score":0}}}}"#
    )
}

/// The moment every fixture answer is stamped with, as the feed publishes it.
pub(super) const TIMESTAMP: &str = "2026-10-24T06:00:00.000Z";

/// The Unix millisecond [`TIMESTAMP`] names.
pub(super) const OBSERVED_AT: i64 = 1_792_821_600_000;

/// The reading a fixture answer of `level` stands for.
pub(super) fn reading(level: u8) -> PizzaReading {
    PizzaReading::for_test(
        PizzaLevel::from_number(level).expect("the fixture names a rung"),
        OBSERVED_AT,
    )
}

/// One document the feed would never publish, for a read that must fail.
pub(super) fn unreadable() -> String {
    String::from("<html>not the feed</html>")
}
