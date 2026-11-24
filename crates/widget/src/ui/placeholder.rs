use std::{borrow::Cow, sync::Arc};

use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    event::Event,
    math::{LogicalPx, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::style::StylePatch,
    text::TextStyle,
};
use ui_kit::{
    kit::{
        button::Button,
        empty_state::EmptyState,
        markdown::{parse::parse_document, source::MarkdownAssets, view::MarkdownNode},
        spinner::Spinner,
    },
    theme::role::Role,
};

use crate::{
    cx::WidgetCx,
    status::{Failure, Notice},
};

const PLACEHOLDER_TEXT_SIZE: f32 = 13.0;
const SPINNER_SIZE: LogicalPx = LogicalPx::new(18.0);

struct EmbeddedAssets;

impl MarkdownAssets for EmbeddedAssets {
    fn load(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        ui_kit::kit::assets::bytes(path).map(Cow::Borrowed)
    }
}

#[derive(Default)]
pub struct Placeholder {
    spinner: Option<Spinner>,
    button: Option<Button>,
    markdown: Option<MarkdownNode>,
}

impl Placeholder {
    pub fn busy(
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        label: Option<SmolStr>,
    ) -> Result<Self, EngineError> {
        let key = spinner_key(parent);
        let theme = cx.theme().clone();
        let window = cx.window();
        let mut spinner = Spinner::new(cx.app_mut(), theme, key)
            .with_size(SPINNER_SIZE)
            .with_role(Role::BrandBase);
        if let Some(label) = label.as_ref() {
            spinner = spinner.with_label(label.clone());
        }
        spinner.build(cx.app_mut(), window, parent)?;

        if let Some(label) = label {
            let theme = cx.theme().clone();
            NodeBuilder::text(label, TextStyle::new(px(PLACEHOLDER_TEXT_SIZE)))
                .bind_style(move |read| StylePatch {
                    foreground: Some(theme.role(read, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .build(cx.tree_mut(), parent)?;
        }

        Ok(Self {
            spinner: Some(spinner),
            button: None,
            markdown: None,
        })
    }

    pub fn notice(
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        notice: &Notice,
    ) -> Result<Self, EngineError> {
        if notice.is_markdown(cx.translate()) {
            let source = notice.resolve(cx.translate());
            let document = parse_document(source.as_str(), &EmbeddedAssets);
            let window = cx.window();
            let theme = cx.theme().clone();
            let markdown =
                MarkdownNode::mount(cx.app_mut(), window, parent, theme, Arc::new(document))?;

            return Ok(Self {
                spinner: None,
                button: None,
                markdown: Some(markdown),
            });
        }

        let text = notice.resolve(cx.translate());
        EmptyState::new(cx.theme().clone(), notice_key(parent))
            .text(text)
            .build(cx.tree_mut(), parent)?;

        Ok(Self::default())
    }

    pub fn failure(
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        failure: &Failure,
    ) -> Result<Self, EngineError> {
        failure_text(cx, parent, failure)?;
        let button = recovery_button(cx, parent, failure, button_key(parent))?;

        Ok(Self {
            spinner: None,
            button,
            markdown: None,
        })
    }

    pub fn sync(&mut self, app: &mut App) -> Result<(), EngineError> {
        if let Some(spinner) = &self.spinner {
            spinner.sync(app)?;
        }
        if let Some(button) = &self.button {
            button.sync(app)?;
        }
        if let Some(markdown) = &mut self.markdown {
            markdown.sync(app);
        }
        Ok(())
    }

    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        self.button
            .as_ref()
            .is_some_and(|button| button.dispatch(app, event))
    }

    pub fn release(&self, app: &mut App) -> usize {
        let spinner = self
            .spinner
            .as_ref()
            .map_or(0, |spinner| spinner.release(app));
        let button = self.button.as_ref().map_or(0, |button| button.release(app));

        spinner + button
    }
}

pub(crate) fn failure_text(
    cx: &mut WidgetCx<'_>,
    parent: NodeId,
    failure: &Failure,
) -> Result<NodeId, EngineError> {
    let language = cx.language();
    let translate = cx.translate().clone();
    let theme = cx.theme().clone();
    let failure = failure.clone();

    NodeBuilder::text(
        SmolStr::default(),
        TextStyle::new(px(PLACEHOLDER_TEXT_SIZE)),
    )
    .bind_text(move |read| {
        let _language: ui_core::node::signal::SignalRef<LanguageCode> = read.read(&language);
        failure.text(&translate)
    })
    .bind_style(move |read| StylePatch {
        foreground: Some(theme.role(read, Role::StatusDangerBase)),
        ..StylePatch::default()
    })
    .build(cx.tree_mut(), parent)
}

pub(crate) fn recovery_button(
    cx: &mut WidgetCx<'_>,
    parent: NodeId,
    failure: &Failure,
    key: SmolStr,
) -> Result<Option<Button>, EngineError> {
    let action = failure.action();
    let label = action.label(cx.translate());
    let enabled = action.is_enabled();
    let run = action.run_handle();
    let theme = cx.theme().clone();
    let window = cx.window();

    let mut button = Button::new(cx.app_mut(), theme, key).outline().label(label);
    if enabled {
        button = button.on_click(move |action_ctx| {
            let run = run.clone();
            run(action_ctx.app_mut());
        });
    } else {
        button = button.disabled(true);
    }
    button.build(cx.app_mut(), window, parent)?;

    Ok(Some(button))
}

fn spinner_key(parent: NodeId) -> SmolStr {
    format_smolstr!("widget.placeholder.spinner.{}", parent.to_bits())
}

fn notice_key(parent: NodeId) -> SmolStr {
    format_smolstr!("widget.placeholder.notice.{}", parent.to_bits())
}

fn button_key(parent: NodeId) -> SmolStr {
    format_smolstr!("widget.placeholder.action.{}", parent.to_bits())
}

pub(crate) fn busy_label(translate: &Translate, connecting: bool) -> SmolStr {
    if connecting {
        t_smol!(translate, "common.connecting")
    } else {
        t_smol!(translate, "common.loading")
    }
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;
    use ui_core::node::tree::NodeKind;

    use super::busy_label;
    use crate::{
        status::{Notice, WidgetAction, WidgetState},
        test_support::{FixtureFailure, WidgetTestApp},
    };

    #[test]
    fn the_busy_label_names_the_work_in_progress() {
        let harness = WidgetTestApp::new();
        let translate = harness.services().translate.clone();

        assert_eq!(busy_label(&translate, true), "Connecting");
        assert_eq!(busy_label(&translate, false), "Loading");
    }

    #[test]
    fn a_notice_placeholder_draws_the_sentence_the_catalogue_holds() {
        let mut harness = WidgetTestApp::new();
        let parent = harness.placeholder_parent();

        harness.with_cx(|cx| {
            super::Placeholder::notice(cx, parent, &Notice::message("test.waiting"))
                .expect("the notice builds");
        });
        harness.drain();

        assert!(
            harness.text_of(parent).contains("Waiting for orders"),
            "the notice's sentence is drawn: {:?}",
            harness.text_of(parent)
        );
        assert!(
            harness.find_by_debug_prefix(parent, "MARKDOWN").is_none(),
            "a plain sentence is drawn by the empty state, not the markdown component"
        );
    }

    #[test]
    fn a_rich_message_notice_goes_through_the_markdown_component() {
        let mut harness = WidgetTestApp::new();
        let parent = harness.placeholder_parent();

        harness.with_cx(|cx| {
            super::Placeholder::notice(cx, parent, &Notice::message("test.rich"))
                .expect("the rich notice builds");
        });
        harness.drain();

        let text = harness.text_of(parent);
        assert!(
            harness.find_by_debug_prefix(parent, "MARKDOWN").is_some(),
            "the message's catalogue value is rich text: {text:?}"
        );
        assert!(
            !text.contains("<p ") && !text.contains("<kbd>") && !text.contains("![]("),
            "the source markup is not drawn as text: {text:?}"
        );
        assert!(
            harness.has_inline_boxes(parent),
            "the `<kbd>` icon is an inline box, not source text: {text:?}"
        );
    }

    #[test]
    fn a_busy_placeholder_draws_a_spinner() {
        let mut harness = WidgetTestApp::new();
        let parent = harness.placeholder_parent();

        harness.with_cx(|cx| {
            super::Placeholder::busy(cx, parent, Some(SmolStr::new_static("Loading")))
                .expect("the spinner builds");
        });
        harness.drain();

        let tree = harness.tree();
        let canvas = tree
            .get(parent)
            .expect("the placeholder container exists")
            .children()
            .iter()
            .any(|child| matches!(tree.get(*child).expect("child").kind(), NodeKind::Canvas(_)));

        assert!(
            canvas,
            "the spinner is a canvas leaf: {:?}",
            tree.get(parent)
        );
    }

    #[test]
    fn a_failure_placeholder_draws_its_detail_and_its_recovery() {
        let mut harness = WidgetTestApp::new();
        let parent = harness.placeholder_parent();
        let failure = WidgetState::failed(
            std::sync::Arc::new(FixtureFailure),
            WidgetAction::new("common.retry", |_| {}),
        )
        .failure()
        .expect("the state carries the failure")
        .clone();

        harness.with_cx(|cx| {
            super::Placeholder::failure(cx, parent, &failure).expect("the failure stand-in builds");
        });
        harness.drain();

        let text = harness.text_of(parent);
        assert!(
            text.contains("the fixture failed"),
            "the detail is drawn: {text}"
        );
        assert!(
            harness
                .find_by_debug_prefix(parent, "widget.placeholder.action.")
                .is_some(),
            "the recovery control is drawn"
        );
    }
}
