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
//! # Why the hint is markdown
//!
//! The copy names the control the user has to reach for, at the top right of the pane's
//! own header. A paragraph of text can only say it; the markdown component draws it: the
//! button's name is bound by the same `<kbd>` the rest of the interface uses for keys,
//! and the control's own puzzle icon precedes it, so the sentence points at the control
//! the way the control is drawn. The document is the catalogue's, so it reads in
//! whatever language the interface is showing — the text is re-read when the language
//! moves, which is the one thing that can make a source read once go stale.

use app_config::widget::{WidgetConfig, WidgetConfigSchemaVersion};
use base_ui::markdown::view::{Markdown, MarkdownState};
use gpui_kit::{
    App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
    Styled, Window, div,
};
use i18n::{language::codes::LanguageCode, t};
use serde::{Deserialize, Serialize};
use widget::{
    category::WidgetCategory,
    config::WidgetConfigChange,
    identity::{WidgetId, WidgetInstanceId},
    status::{WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
    view::WidgetView,
};

/// The message the hint is read from.
const HINT: &str = "widget.empty.hint";

/// The pane persists nothing about this widget; the type exists because every widget
/// carries the schema version its config is stored under.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EmptyConfig;

impl WidgetConfig for EmptyConfig {
    const SCHEMA_VERSION: WidgetConfigSchemaVersion = WidgetConfigSchemaVersion::V1;
}

/// The pane nothing has been chosen for.
pub struct EmptyWidget {
    markdown: Entity<MarkdownState>,
    status: Entity<WidgetStatus>,
    /// The language the markdown source was read in.
    ///
    /// Parsing is the markdown component's expensive step, so the source is read when
    /// the widget is built rather than on every frame; a language the interface moved
    /// to since is the one thing that makes it stale, and this is how the frame that
    /// notices compares.
    language: LanguageCode,
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
        status: Entity<WidgetStatus>,
        _reload: impl Fn(&mut App) + 'static,
        _config_change: WidgetConfigChange,
        _recovery: WidgetAction,
        cx: &mut Context<Self>,
    ) -> Self {
        // The hint is read where it is built rather than when it is asked for: the
        // widget has nothing to wait for, so its state is complete the moment it
        // exists.
        let language = *cx.global::<LanguageCode>();
        let hint = t!(cx, HINT);
        let markdown = cx.new(|cx| MarkdownState::new(hint, cx));

        Self {
            markdown,
            status,
            language,
        }
    }

    /// A pane with nothing to read has nothing to wait for: it is ready the moment it
    /// exists.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.status.report(WidgetState::Ready, cx);
    }

    /// The hint is the pane's whole content, so a pane showing this widget has content
    /// from its first frame.
    fn has_content(&self) -> bool {
        true
    }
}

impl Render for EmptyWidget {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let language = *cx.global::<LanguageCode>();
        if language != self.language {
            self.language = language;
            let hint = t!(cx, HINT);
            self.markdown
                .update(cx, |markdown, cx| markdown.set_source(hint, cx));
        }

        // The body draws what the pane has to say and nothing around it: the surface
        // owns the box, its colour and its corners, so this is only the hint's own
        // shape — centred in the pane, at the size the framework's stand-in text uses.
        div()
            .debug_selector(|| "TEST_EMPTY_WIDGET".into())
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .text_sm()
            .child(Markdown::new(&self.markdown).min_w_0())
    }
}

#[cfg(test)]
mod tests {
    use app_config::widget::{
        VersionedWidgetConfig, WidgetConfig, WidgetConfigSchemaVersion, decode_or_default,
    };
    use gpui_kit::{
        App, AppContext, Context, Entity, IntoElement, ParentElement, Pixels, Render, Styled,
        TestAppContext, VisualTestContext, Window, div, px, size,
    };
    use i18n::{language::codes::LanguageCode, t};
    use widget::{
        category::WidgetCategory,
        config::WidgetConfigChange,
        identity::WidgetInstanceId,
        status::{WidgetAction, WidgetStatus, WidgetStatusExt},
        view::WidgetView,
    };

    use super::{EmptyConfig, EmptyWidget, HINT};

    /// The hint the catalogue spells in `language`.
    fn hint_in(cx: &mut TestAppContext, language: LanguageCode) -> String {
        cx.update(|cx| {
            cx.set_global(language);
            t!(cx, HINT).to_string()
        })
    }

    /// A widget built the way the framework builds one: state, then one load.
    fn widget(cx: &mut TestAppContext) -> Entity<EmptyWidget> {
        cx.update(|cx| {
            base_ui::style::theme::install(cx);
            i18n::global::embedded::install(cx).expect("the embedded locales should load");

            let status = cx.new(|_| WidgetStatus::new());
            let widget = cx.new(move |cx| {
                let weak: gpui_kit::WeakEntity<EmptyWidget> = cx.weak_entity();
                let recovery = WidgetAction::new("common.retry", move |cx: &mut App| {
                    let _ = weak.update(cx, |widget, cx| widget.load(cx));
                });

                EmptyWidget::new(
                    WidgetInstanceId::new(),
                    EmptyConfig,
                    status,
                    |_| {},
                    WidgetConfigChange::default(),
                    recovery,
                    cx,
                )
            });
            widget.update(cx, |widget, cx| widget.load(cx));

            widget
        })
    }

    /// A window holding nothing but the widget's body, at a size a hint sits in.
    struct BodyHarness {
        widget: Entity<EmptyWidget>,
    }

    impl Render for BodyHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.widget.clone())
        }
    }

    fn draw(cx: &mut TestAppContext, widget: Entity<EmptyWidget>) -> &mut VisualTestContext {
        let (_, cx) = cx.add_window_view(move |_, _| BodyHarness { widget });
        cx.simulate_resize(size(px(640.0), px(360.0)));
        cx.run_until_parked();

        cx
    }

    /// The source the widget's markdown state currently holds.
    fn source(widget: &Entity<EmptyWidget>, cx: &mut VisualTestContext) -> String {
        cx.read(|cx| widget.read(cx).markdown.read(cx).source().to_string())
    }

    #[test]
    fn the_widget_should_have_a_stable_identity() {
        let id = EmptyWidget::ID;
        let identity = (
            id.as_str(),
            EmptyWidget::DISPLAY_NAME,
            EmptyWidget::CATEGORY,
            EmptyWidget::SELECTABLE,
            EmptyConfig::SCHEMA_VERSION,
        );

        assert_eq!(
            identity,
            (
                "kytrix.empty",
                "widget.names.empty",
                WidgetCategory::Market,
                false,
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
    #[gpui_kit::test]
    fn every_message_this_widget_shows_should_resolve_to_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            i18n::global::embedded::install(cx).expect("the embedded locales should load");

            for language in [LanguageCode::En, LanguageCode::ZhCn] {
                cx.set_global(language);

                for key in [HINT, EmptyWidget::DISPLAY_NAME] {
                    assert!(!t!(cx, key).is_empty(), "{key} has no copy in {language}");
                }
            }
        });
    }

    /// A pane showing this widget has content from its first frame and nothing wrong
    /// with it, so the framework draws the hint rather than a stand-in or a spinner.
    #[gpui_kit::test]
    fn the_widget_should_be_ready_and_have_content_the_moment_it_is_built(cx: &mut TestAppContext) {
        let widget = widget(cx);

        cx.read(|cx| {
            let widget = widget.read(cx);

            assert!(widget.has_content());
            assert!(
                !widget.status.is_busy(cx),
                "the pane is not waiting for anything"
            );
            assert!(!widget.status.is_failed(cx));
        });
    }

    /// The hint is what the body draws, and the pane reads its language from the
    /// catalogue: the source the markdown state holds is the message itself.
    #[gpui_kit::test]
    fn the_body_should_draw_the_hint(cx: &mut TestAppContext) {
        let widget = widget(cx);
        let expected = hint_in(cx, LanguageCode::En);
        let cx = draw(cx, widget.clone());

        assert_eq!(source(&widget, cx), expected);
        assert!(
            cx.debug_bounds("TEST_EMPTY_WIDGET").is_some(),
            "the body should render"
        );
        assert!(
            cx.debug_bounds("BASE_UI_MARKDOWN").is_some(),
            "the body is the markdown component"
        );
    }

    /// The hint is centred in the pane, so a pane too wide for one line of text still
    /// reads as one sentence rather than as a line against its left edge.
    #[gpui_kit::test]
    fn the_hint_should_be_centred_in_the_body(cx: &mut TestAppContext) {
        let widget = widget(cx);
        let cx = draw(cx, widget);

        let body = cx
            .debug_bounds("TEST_EMPTY_WIDGET")
            .expect("the body should render");
        let hint = cx
            .debug_bounds("BASE_UI_MARKDOWN_BLOCK_0")
            .expect("the hint should draw its paragraph");

        let body_centre = body.center().x;
        let hint_centre = hint.center().x;
        assert!(
            (body_centre - hint_centre).abs() < px(1.0),
            "the hint should sit at the centre of the body: {hint:?} against {body:?}"
        );
    }

    /// The hint follows a language chosen after the widget was built: the source is
    /// re-read when the frame notices the interface moved, so a pane waiting for a
    /// widget reads in the language everything else is read in.
    #[gpui_kit::test]
    fn the_hint_should_follow_a_language_chosen_after_the_widget_was_built(
        cx: &mut TestAppContext,
    ) {
        let widget = widget(cx);
        let english = hint_in(cx, LanguageCode::En);
        let cx = draw(cx, widget.clone());
        assert_eq!(source(&widget, cx), english);

        cx.update(|_, cx| cx.set_global(LanguageCode::ZhCn));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let chinese = hint_in(cx, LanguageCode::ZhCn);
        assert_ne!(chinese, english, "the fixture languages have to differ");
        assert_eq!(
            source(&widget, cx),
            chinese,
            "the same widget should read the copy the interface now shows"
        );
    }

    /// Not every line fits the box it is given, and the hint has to be readable when one
    /// does not: the text is measured against the body rather than drawn past its edge.
    #[gpui_kit::test]
    fn a_hint_narrower_than_its_text_should_stay_inside_the_body(cx: &mut TestAppContext) {
        let widget = widget(cx);
        let (_, cx) = cx.add_window_view(move |_, _| BodyHarness { widget });
        cx.simulate_resize(size(px(160.0), px(360.0)));
        cx.run_until_parked();

        let body = cx
            .debug_bounds("TEST_EMPTY_WIDGET")
            .expect("the body should render");
        let hint = cx
            .debug_bounds("BASE_UI_MARKDOWN_BLOCK_0")
            .expect("the hint should draw its paragraph");

        assert!(
            hint.left() >= body.left() - px(1.0) && hint.right() <= body.right() + px(1.0),
            "the hint should be measured against the body it is drawn in: {hint:?} against {body:?}"
        );
        assert!(
            hint.size.height > Pixels::ZERO,
            "the text should still be drawn, wrapped"
        );
    }
}
