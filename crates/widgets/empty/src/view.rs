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

const HINT: &str = "widget.empty.hint";

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EmptyConfig;

impl WidgetConfig for EmptyConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

pub struct EmptyWidget {
    status: WidgetStatus,
}

impl WidgetView for EmptyWidget {
    type Config = EmptyConfig;

    const ID: WidgetId = WidgetId::new_static("kytrix.empty");
    const DISPLAY_NAME: &'static str = "widget.names.empty";
    const CATEGORY: WidgetCategory = WidgetCategory::Market;
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
        Ok(Self { status })
    }

    fn load(&mut self, cx: &mut WidgetCx<'_>) {
        self.status
            .report(cx.app_mut(), WidgetState::waiting(Notice::message(HINT)));
    }

    fn has_content(&self) -> bool {
        false
    }

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

    fn product_translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    fn harness() -> WidgetTestApp {
        WidgetTestApp::with_translate(product_translate(LanguageCode::En))
    }

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
