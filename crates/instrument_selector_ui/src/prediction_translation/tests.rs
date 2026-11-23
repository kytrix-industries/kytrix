use ahash::AHashMap;
use market_participants_model::{
    identifier::{IdentifierValue, VenueId, VenueScopeId},
    listing::route::VenueRoute,
    outcome::{OutcomeGroupId, OutcomeLegRef, OutcomeMemberId, OutcomeSideId},
    prediction::{
        PredictionCategory, PredictionEventCard, PredictionEventRef, PredictionMarketRef,
        PredictionOptionCard,
    },
};
use smol_str::{SmolStr, format_smolstr};

use super::{
    LocalTranslator, TranslatedSegment, TranslationCancel, TranslationFailure, TranslationSegment,
    TranslationSetup, page_has_translatable_text, plan_translation,
};

fn identity(digit: char) -> String {
    format!("v1/{}", digit.to_string().repeat(64))
}

fn leg() -> OutcomeLegRef {
    OutcomeLegRef {
        group: OutcomeGroupId::from_identity_value(&identity('1'))
            .expect("the fixture group identity should be valid"),
        member: OutcomeMemberId::from_identity_value(&identity('2'))
            .expect("the fixture member identity should be valid"),
        side: OutcomeSideId::from_identity_value(&identity('3'))
            .expect("the fixture side identity should be valid"),
    }
}

fn option(event: &PredictionEventRef, label: &str) -> PredictionOptionCard {
    PredictionOptionCard {
        market: PredictionMarketRef {
            event: event.clone(),
            native_market: IdentifierValue::new("market")
                .expect("the fixture market identity should be valid"),
        },
        leg: leg(),
        label: SmolStr::new(label),
        probability: None,
    }
}

fn card(index: usize, title: impl Into<SmolStr>, labels: &[&str]) -> PredictionEventCard {
    let event = PredictionEventRef {
        route: VenueRoute::new(
            VenueId::Polymarket,
            VenueScopeId::new("predict").expect("the fixture scope should be valid"),
        ),
        native_event: IdentifierValue::new(format_smolstr!("event-{index}"))
            .expect("the fixture event identity should be valid"),
    };
    PredictionEventCard {
        event: event.clone(),
        title: title.into(),
        category: PredictionCategory::new("politics", "Politics"),
        ends_at: None,
        volume: None,
        liquidity: None,
        options: labels.iter().map(|label| option(&event, label)).collect(),
        remaining_options: 0,
        market_count: 1,
        remaining_markets: 0,
    }
}

fn texts(chunk: &super::TranslationChunk) -> Vec<&str> {
    chunk
        .segments()
        .iter()
        .map(|segment| segment.text.as_str())
        .collect()
}

fn ids(chunk: &super::TranslationChunk) -> Vec<&str> {
    chunk
        .segments()
        .iter()
        .map(|segment| segment.id.as_str())
        .collect()
}

#[test]
fn a_full_page_should_split_into_two_chunks_with_local_sequential_ids() {
    let cards: Vec<PredictionEventCard> = (0..8)
        .map(|index| card(index, format_smolstr!("Event {index}"), &["Yes", "No"]))
        .collect();

    let chunks = plan_translation(&cards, &AHashMap::new());

    assert_eq!(chunks.len(), 2, "eight cards should plan two requests");
    assert_eq!(
        ids(&chunks[0]),
        ["s00", "s01", "s02", "s03", "s04", "s05"],
        "the first request should name its segments locally and in order"
    );
    assert_eq!(
        texts(&chunks[0]),
        ["Event 0", "Yes", "No", "Event 1", "Event 2", "Event 3"],
        "a title should come before its options, and a shared label should not repeat"
    );
    assert_eq!(
        ids(&chunks[1]),
        ["s00", "s01", "s02", "s03"],
        "the second request should start its ids again"
    );
    assert_eq!(
        texts(&chunks[1]),
        ["Event 4", "Event 5", "Event 6", "Event 7"],
        "the second request should only carry text the first one did not"
    );
}

#[test]
fn shared_labels_should_appear_only_in_the_first_chunk_that_needs_them() {
    let cards: Vec<PredictionEventCard> = (0..8)
        .map(|index| {
            card(
                index,
                format_smolstr!("Event {index}"),
                &["Yes", "No", "Other"],
            )
        })
        .collect();

    let chunks = plan_translation(&cards, &AHashMap::new());

    assert_eq!(chunks.len(), 2);
    assert_eq!(
        texts(&chunks[0]),
        [
            "Event 0", "Yes", "No", "Other", "Event 1", "Event 2", "Event 3"
        ]
    );
    for shared in ["Yes", "No", "Other"] {
        assert!(
            !texts(&chunks[1]).contains(&shared),
            "{shared} should not be requested again by the second chunk"
        );
    }
}

#[test]
fn already_translated_text_should_be_skipped_and_a_newly_empty_chunk_dropped() {
    let cards: Vec<PredictionEventCard> = (0..8)
        .map(|index| card(index, format_smolstr!("Event {index}"), &[]))
        .collect();
    let mut already = AHashMap::new();
    for index in 0..4 {
        already.insert(
            format_smolstr!("Event {index}"),
            SmolStr::new_static("Translated"),
        );
    }

    let chunks = plan_translation(&cards, &already);

    assert_eq!(
        chunks.len(),
        1,
        "the all-translated chunk should be dropped"
    );
    assert_eq!(
        texts(&chunks[0]),
        ["Event 4", "Event 5", "Event 6", "Event 7"]
    );
    assert_eq!(
        ids(&chunks[0]),
        ["s00", "s01", "s02", "s03"],
        "the remaining chunk should renumber its ids from zero"
    );
}

#[test]
fn empty_and_whitespace_only_text_should_be_skipped() {
    let cards = [card(0, "  ", &["", "\t", "Yes", " \n "])];

    let chunks = plan_translation(&cards, &AHashMap::new());

    assert_eq!(chunks.len(), 1);
    assert_eq!(texts(&chunks[0]), ["Yes"]);
    assert_eq!(ids(&chunks[0]), ["s00"]);
}

#[test]
fn a_page_should_be_translatable_only_while_a_title_or_label_is_not_blank() {
    assert!(!page_has_translatable_text(&[]));
    assert!(!page_has_translatable_text(&[card(
        0,
        "  ",
        &["", "\t", " \n "]
    )]));
    assert!(page_has_translatable_text(&[card(0, "  ", &[" ", "Yes"])]));
    assert!(page_has_translatable_text(&[card(0, "Event", &[])]));
}

#[test]
fn a_page_of_five_cards_should_yield_a_four_card_and_a_one_card_chunk() {
    let cards: Vec<PredictionEventCard> = (0..5)
        .map(|index| card(index, format_smolstr!("Event {index}"), &[]))
        .collect();

    let chunks = plan_translation(&cards, &AHashMap::new());

    assert_eq!(chunks.len(), 2);
    assert_eq!(
        texts(&chunks[0]),
        ["Event 0", "Event 1", "Event 2", "Event 3"]
    );
    assert_eq!(texts(&chunks[1]), ["Event 4"]);
    assert_eq!(ids(&chunks[1]), ["s00"]);
}

#[test]
fn no_cards_should_yield_no_chunks() {
    let chunks = plan_translation(&[], &AHashMap::new());

    assert!(chunks.is_empty());
}

#[test]
fn a_cancellation_should_start_clear_and_be_observable_through_its_flag() {
    use std::sync::atomic::Ordering;

    let cancel = TranslationCancel::new();
    assert!(!cancel.is_cancelled());
    assert!(!TranslationCancel::default().is_cancelled());
    assert!(!cancel.flag().load(Ordering::Acquire));

    cancel.cancel();

    assert!(cancel.is_cancelled());
    assert!(cancel.flag().load(Ordering::Acquire));
}

#[test]
fn a_translator_round_trips_the_segments_a_run_planned() {
    struct Echo;

    impl LocalTranslator for Echo {
        fn prepare(
            &self,
            _setup: &TranslationSetup,
            _cancel: TranslationCancel,
        ) -> Result<(), TranslationFailure> {
            Ok(())
        }

        fn translate(
            &self,
            _setup: &TranslationSetup,
            _target: app_config::settings::Language,
            segments: Vec<TranslationSegment>,
            _cancel: TranslationCancel,
        ) -> Result<Vec<TranslatedSegment>, TranslationFailure> {
            Ok(segments
                .into_iter()
                .map(|segment| TranslatedSegment {
                    id: segment.id,
                    text: format_smolstr!("zh:{}", segment.text),
                })
                .collect())
        }
    }

    let cards = [card(0, "Event", &["Yes"])];
    let chunks = plan_translation(&cards, &AHashMap::new());
    let translator: std::sync::Arc<dyn LocalTranslator> = std::sync::Arc::new(Echo);
    let setup = TranslationSetup {
        directory: None,
        model: None,
    };

    let translated = translator
        .translate(
            &setup,
            app_config::settings::Language::ZhCn,
            chunks[0].segments().to_vec(),
            TranslationCancel::new(),
        )
        .expect("the echo translator answers");

    assert_eq!(
        translated
            .iter()
            .map(|segment| (segment.id.as_str(), segment.text.as_str()))
            .collect::<Vec<_>>(),
        [("s00", "zh:Event"), ("s01", "zh:Yes")]
    );
}
