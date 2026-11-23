//! The pane that has nothing yet: the hint that a widget is chosen from the pane's own
//! controls.
//!
//! This is the widget a pane falls back to. A pane the user has not given a widget, a
//! pane split off its neighbour, and a pane whose stored widget this build no longer has
//! all show it, so it is the one state every pane starts from. It is registered like any
//! other widget — a pane is restored by ID, so the fallback has to be creatable by ID —
//! but it is not offered for selection: the gallery lists the widgets a user may pick,
//! and this is what a pane shows instead of one of them.
//!
//! # The smallest a widget can be
//!
//! This pane is the contract's minimal shape, and it is deliberately the one widget with
//! no data source of its own: it reports the hint and draws nothing else.
//!
//! - [`WidgetView::new`] only keeps the state cell; it starts no work and reports
//!   nothing.
//! - [`WidgetView::load`] reports the hint as a [`Notice`] — a catalogue reference, not
//!   a sentence. There is nothing to wait for, so the pane leaves `Connecting` on that
//!   one report and never reports another state.
//! - [`WidgetView::has_content`] is always `false`: the hint *is* the pane's content, and
//!   a pane with nothing of its own to show is exactly what the framework's stand-in
//!   draws.
//! - [`WidgetView::body`] is therefore never reached; it is implemented empty because the
//!   contract requires the method, not because the pane has a body.
//! - there is no `sync`, `header` or `release` work: the pane owns no kit component, no
//!   action key, no timer and no node of its own, so the trait's no-op defaults are the
//!   whole of those phases.
//!
//! # Why the pane does not draw the hint itself
//!
//! The copy names the control the user has to reach for, at the top right of the pane's
//! own header. The catalogue authors it as rich text: the button's name is bound by the
//! same `<kbd>` the rest of the interface uses for keys, and the control's own puzzle
//! icon precedes it, so the sentence points at the control the way the control is drawn.
//!
//! A widget with nothing to show and a notice to say instead of it is the framework's
//! `Waiting` placeholder, and that placeholder resolves the notice in the language
//! showing on the frame that draws it, handing a rich one to the markdown component
//! (`crates/widget/src/ui/placeholder.rs`, decided by [`Notice::is_markdown`]). So the
//! pane holds no copy and parses no markdown: a language switch invalidates the surface,
//! which rebuilds the stand-in in the language now showing. What is left of the widget is
//! the contract itself, which is what makes this crate the skeleton the content widgets
//! grow from.

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use serde::{Deserialize, Serialize};
use ui_core::{error::EngineError, node::id::NodeId};
use widget::{
    category::WidgetCategory,
    config::WidgetConfigChange,
    cx::WidgetCx,
    identity::{WidgetId, WidgetInstanceId},
    status::{Notice, WidgetAction, WidgetState, WidgetStatus},
    view::{ReloadHandle, WidgetView},
};

/// The message the hint is read from.
///
/// It is a catalogue key, not copy: the surface resolves it on the frame that draws it,
/// in the language showing then.
const HINT: &str = "widget.empty.hint";

/// The pane persists nothing about this widget; the type exists because every widget
/// carries the schema version its config is stored under.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EmptyConfig;

impl WidgetConfig for EmptyConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// The pane nothing has been chosen for.
///
/// There is nothing to keep but the state cell: the pane owns no data, no component and
/// no node.
pub struct EmptyWidget {
    status: WidgetStatus,
}

impl WidgetView for EmptyWidget {
    type Config = EmptyConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.empty");
    const DISPLAY_NAME: &'static str = "widget.names.empty";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
    /// The framework builds this widget; it is never something the user picks, so the
    /// gallery does not list it.
    const SELECTABLE: bool = false;

    fn new(
        _instance_id: WidgetInstanceId,
        _config: Self::Config,
        status: WidgetStatus,
        _reload: ReloadHandle,
        _config_change: WidgetConfigChange,
        _recovery: WidgetAction,
        _cx: &mut WidgetCx<'_>,
    ) -> Result<Self, EngineError> {
        // The pane has nothing to wait for and nothing to fail at, so the state cell is
        // the whole of its construction; the framework calls `load` once this returns.
        Ok(Self { status })
    }

    /// A pane with nothing to read has nothing to wait for: it is ready the moment it
    /// exists, and the one thing it has to say is the hint.
    ///
    /// The notice is a catalogue reference, so this report does not have to be repeated
    /// when the language moves: the surface resolves the key in the language showing on
    /// the frame that draws it.
    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        self.status
            .report(cx.app_mut(), WidgetState::waiting(Notice::message(HINT)));
    }

    /// The hint is the pane's whole content, and the framework draws it: this widget
    /// never has a body of its own.
    fn has_content(&self) -> bool {
        false
    }

    /// Never called: [`has_content`](WidgetView::has_content) is `false`, so the framework
    /// always presents the hint instead of building a body. The contract requires the
    /// method, and a widget with content of its own builds that subtree here.
    fn body(&mut self, _parent: NodeId, _cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use app_config::widget::{
        VersionedWidgetConfig, WidgetConfig, WidgetConfigSchemaVersion, decode_or_default,
    };
    use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
    use widget::{
        category::WidgetCategory,
        status::{Notice, WidgetState},
        test_support::WidgetTestApp,
        view::WidgetView,
    };

    use super::{EmptyConfig, EmptyWidget, HINT};

    /// The translator the product ships: the hint is catalogue copy, so these tests
    /// assert what the reader actually gets.
    fn product_translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    /// The framework's headless host, over the product catalogue.
    fn harness() -> WidgetTestApp {
        WidgetTestApp::with_translate(product_translate(LanguageCode::En))
    }

    /// The hint's sentence in `language`.
    fn hint_in(language: LanguageCode) -> String {
        let translate = product_translate(language);
        t_smol!(&translate, HINT).to_string()
    }

    #[test]
    fn the_widget_should_have_a_stable_identity() {
        let id = EmptyWidget::ID;
        let identity = (
            id.as_str(),
            EmptyWidget::DISPLAY_NAME,
            EmptyWidget::CATEGORY,
            EmptyWidget::SELECTABLE,
            EmptyWidget::DESCRIPTION,
            EmptyWidget::ICON,
            EmptyConfig::SCHEMA_VERSION,
        );

        assert_eq!(
            identity,
            (
                "kytrix.empty",
                "widget.names.empty",
                WidgetCategory::Market,
                false,
                None,
                None,
                WidgetConfigSchemaVersion::V1,
            ),
            "a pane stores this widget by ID, and the gallery must keep it out of the \
             list of widgets a user picks"
        );
    }

    #[test]
    fn the_config_should_round_trip_through_its_envelope() {
        let stored =
            VersionedWidgetConfig::encode(&EmptyConfig).expect("an empty config should encode");

        assert_eq!(stored.schema_version(), WidgetConfigSchemaVersion::V1);
        assert_eq!(stored.decode::<EmptyConfig>(), EmptyConfig);
        assert_eq!(decode_or_default::<EmptyConfig>(None), EmptyConfig);
    }

    /// A missing key is an empty string on screen rather than an error, so a gap in the
    /// catalogue is what a typo looks like: this is the test that turns it into a failure.
    #[test]
    fn every_message_this_widget_shows_should_resolve_to_text() {
        for language in [LanguageCode::En, LanguageCode::ZhCn] {
            let translate = product_translate(language);

            for key in [HINT, EmptyWidget::DISPLAY_NAME] {
                assert!(
                    !t_smol!(&translate, key).is_empty(),
                    "{key} has no copy in {language}"
                );
            }
        }
    }

    #[test]
    fn a_mounted_pane_should_report_the_hint_and_show_it() {
        let mut harness = harness();
        let widget = harness.mount::<EmptyWidget>(EmptyConfig);

        // Registration: the framework built the surface and ran the first load. There is
        // nothing to wait for, so the pane goes straight from `Connecting` to the hint.
        assert_eq!(
            harness.tree().debug_id(widget.surface()),
            Some("WIDGET_SURFACE")
        );
        assert_eq!(
            widget.status().state(harness.app()),
            WidgetState::waiting(Notice::message(HINT))
        );
        assert!(!widget.status().is_busy(harness.app()));
        assert!(!widget.status().is_failed(harness.app()));
        assert!(
            widget.config().is_none(),
            "the pane persists nothing, so the layout stores no config for it"
        );

        harness.drain();

        let surface = widget.surface();
        let placeholder = harness
            .find_by_debug_prefix(surface, "WIDGET_PLACEHOLDER")
            .expect("the framework stands the hint in for the content");
        assert!(harness.visible(placeholder));
        assert!(
            harness
                .find_by_debug_prefix(placeholder, "MARKDOWN")
                .is_some(),
            "the catalogue authors the hint as rich text, so it is drawn by the markdown \
             component: {:?}",
            harness.text_of(placeholder)
        );
        let text = harness.text_of(placeholder);
        assert!(
            text.contains("upper-right corner") && text.contains("select a widget"),
            "the hint is the catalogue's sentence: {text:?}"
        );
        assert!(
            !text.contains("<kbd>") && !text.contains("![](") && !text.contains("<p "),
            "the notice's source markup never reaches the reader: {text:?}"
        );
        assert!(
            harness.has_inline_boxes(placeholder),
            "the kbd icon is an inline box, not source text"
        );
        assert!(harness.rendered(), "the hint is what the frame painted");

        // The widget has no body of its own, so the content slot the framework offers it
        // is hidden and stays empty.
        let content = harness
            .find_by_debug_prefix(surface, "WIDGET_CONTENT")
            .expect("the surface's content slot exists");
        assert!(
            !harness.visible(content),
            "nothing is drawn in the content slot"
        );
        assert_eq!(
            harness
                .tree()
                .get(content)
                .expect("the content slot exists")
                .children()
                .len(),
            0,
            "the pane never builds a body"
        );
    }

    /// The pane has nothing that changes on its own: once the hint is up, a frame paints
    /// nothing, asks for nothing and rebuilds nothing (I3).
    #[test]
    fn the_pane_should_ask_for_no_frames_once_the_hint_is_up() {
        let mut harness = harness();
        let widget = harness.mount::<EmptyWidget>(EmptyConfig);
        harness.drain();

        let placeholder = harness
            .find_by_debug_prefix(widget.surface(), "WIDGET_PLACEHOLDER")
            .expect("the framework stands the hint in for the content");
        let text = harness.text_of(placeholder);
        let nodes = harness.node_count();
        let requests = harness.frame_requests();

        harness.drain();

        assert_eq!(
            harness.frame_requests(),
            requests,
            "a steady pane asks for no frame"
        );
        assert!(!harness.rendered(), "and the round paints nothing");
        assert_eq!(
            harness.node_count(),
            nodes,
            "the stand-in is not rebuilt when nothing moved"
        );
        assert_eq!(harness.text_of(placeholder), text);
    }

    /// The hint follows a language chosen after the pane was mounted: the widget reports a
    /// catalogue reference, and the surface redraws it in the language showing now.
    #[test]
    fn the_hint_should_follow_a_language_chosen_after_the_pane_was_mounted() {
        let mut harness = harness();
        let widget = harness.mount::<EmptyWidget>(EmptyConfig);
        harness.drain();

        let placeholder = harness
            .find_by_debug_prefix(widget.surface(), "WIDGET_PLACEHOLDER")
            .expect("the framework stands the hint in for the content");
        let english = harness.text_of(placeholder);
        assert!(english.contains("select a widget"), "{english:?}");
        assert_ne!(
            hint_in(LanguageCode::En),
            hint_in(LanguageCode::ZhCn),
            "the catalogue's two languages have to differ"
        );

        let requests = harness.frame_requests();
        harness.switch_language(LanguageCode::ZhCn);
        assert!(
            harness.frame_requests() > requests,
            "the surface root read the language signal while rendering, so the switch \
             schedules the frame that redraws the hint"
        );
        harness.drain();

        let chinese = harness.text_of(placeholder);
        assert_ne!(chinese, english);
        assert!(
            chinese.contains("选择一个部件"),
            "the hint reads in the language showing now: {chinese:?}"
        );
        assert!(
            !chinese.contains("<kbd>") && !chinese.contains("![]("),
            "the Chinese copy is rich text too, never its source: {chinese:?}"
        );
        assert!(
            harness.has_inline_boxes(placeholder),
            "the kbd icon is an inline box in the new language as well"
        );
        assert_eq!(
            widget.status().state(harness.app()),
            WidgetState::waiting(Notice::message(HINT)),
            "the hint is a catalogue reference, so the language move needed no re-report"
        );
    }

    /// Two panes on one application: each has its own surface, both show the hint, and
    /// one pane's release leaves the other's surface and state alone (the pane owns no
    /// kit key, so there is no action identity to collide over either).
    #[test]
    fn two_panes_should_show_the_hint_side_by_side() {
        let mut harness = harness();
        let first = harness.mount::<EmptyWidget>(EmptyConfig);
        let second = harness.mount::<EmptyWidget>(EmptyConfig);
        harness.drain();

        assert_ne!(first.surface(), second.surface());
        assert_eq!(harness.router().len(), 2, "both panes route their frames");

        let first_placeholder = harness
            .find_by_debug_prefix(first.surface(), "WIDGET_PLACEHOLDER")
            .expect("the first pane's hint is up");
        let second_placeholder = harness
            .find_by_debug_prefix(second.surface(), "WIDGET_PLACEHOLDER")
            .expect("the second pane's hint is up");
        assert!(harness.visible(first_placeholder));
        assert!(harness.visible(second_placeholder));

        // One language write reaches both panes on the frame it schedules.
        harness.switch_language(LanguageCode::ZhCn);
        harness.drain();
        for placeholder in [first_placeholder, second_placeholder] {
            assert!(
                harness.text_of(placeholder).contains("选择一个部件"),
                "each pane's hint follows the language: {:?}",
                harness.text_of(placeholder)
            );
        }

        assert!(first.release(harness.app_mut()) > 0);
        assert_eq!(
            harness.router().len(),
            1,
            "only the released pane's route is gone"
        );
        assert!(harness.tree().get(first.surface()).is_err());
        assert!(harness.tree().get(second.surface()).is_ok());
        assert!(harness.visible(second_placeholder));
        assert_eq!(
            second.status().state(harness.app()),
            WidgetState::waiting(Notice::message(HINT))
        );
        harness.drain();
        assert!(
            harness.text_of(second_placeholder).contains("选择一个部件"),
            "the pane still on screen keeps its hint"
        );
    }

    /// Releasing a pane gives back everything the mount took: its nodes, its frame route
    /// and its value slots. Release is idempotent, and mounting again reuses the slots
    /// instead of growing the table (a leaked pane shows up in `live`, a table that only
    /// ever appends in `slots`).
    #[test]
    fn releasing_the_pane_should_leave_nothing_behind() {
        let mut harness = harness();
        let baseline_nodes = harness.node_count();
        let baseline = harness.signal_slot_stats();

        let widget = harness.mount::<EmptyWidget>(EmptyConfig);
        harness.drain();
        assert_eq!(harness.router().len(), 1);
        assert!(
            harness.signal_slot_stats().live > baseline.live,
            "the mounted pane minted its own values"
        );

        assert!(
            widget.release(harness.app_mut()) > 0,
            "the scope owned the pane's signals"
        );
        assert!(widget.is_released());
        assert_eq!(harness.router().len(), 0, "the frame route is unregistered");
        assert!(
            harness.tree().get(widget.surface()).is_err(),
            "the surface's nodes are gone"
        );
        assert_eq!(harness.node_count(), baseline_nodes);
        assert_eq!(
            harness.signal_slot_stats().live,
            baseline.live,
            "release hands every value slot back"
        );
        assert_eq!(
            widget.release(harness.app_mut()),
            0,
            "a second release is a no-op"
        );

        // The first mount grew the table to the pane's peak; every later round has to
        // reuse those slots instead of growing it further.
        let released = harness.signal_slot_stats();
        for _ in 0..8 {
            let widget = harness.mount::<EmptyWidget>(EmptyConfig);
            widget.release(harness.app_mut());
        }
        assert_eq!(
            harness.signal_slot_stats(),
            released,
            "eight more mount/release rounds reuse the slots instead of growing the table"
        );
        assert_eq!(harness.node_count(), baseline_nodes);
    }
}
