//! What a widget body shows while it has nothing of its own to render.
//!
//! A placeholder replaces the body, never accompanies it: either the widget has
//! content and draws it, or it does not and this stands in its place. Anything the
//! user must read while content *is* on screen belongs to a banner instead.
//!
//! The three stand-ins are built from the shared component library:
//!
//! | situation | components |
//! |---|---|
//! | busy (`Connecting`, `Loading`, `Ready`) | [`kit::spinner`](base_ui::kit::spinner), labelled with the name of the work |
//! | waiting (a [`Notice`]) | the notice's sentence in a muted [`kit::empty_state`](base_ui::kit::empty_state) |
//! | failed ([`Failure`]) | the failure's detail plus a [`kit::button`](base_ui::kit::button) that runs the recovery |
//!
//! This lives with the framework rather than with the shared component library
//! because it is not a general-purpose component: it exists to be a widget body's
//! stand-in, and only the framework decides when one is shown.
//!
//! # Ownership
//!
//! A [`Placeholder`] owns the stateful components it built (a spinner, a recovery
//! button), so the surface can drive their `sync`, forward events their way, and
//! release them when the state moves on. The nodes themselves are removed by the
//! surface through [`App::remove_node`], in the documented order: remove the nodes
//! first, then release the components' scopes and action handlers.

use base_ui::{
    kit::{button::Button, empty_state::EmptyState, spinner::Spinner},
    theme::role::Role,
};
use i18n::{language::codes::LanguageCode, t_smol, translate::Translate};
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    event::Event,
    node::{builder::NodeBuilder, id::NodeId},
    paint::style::StylePatch,
    text::TextStyle,
};

use crate::{
    cx::WidgetCx,
    status::{Failure, Notice},
};

/// The size the stand-in's own text is drawn at.
const PLACEHOLDER_TEXT_SIZE: f32 = 13.0;
/// The spinner size: visible without competing with the pane's content.
const SPINNER_SIZE: f32 = 18.0;

/// The components one placeholder is built from.
///
/// The nodes are already in the tree; this is what the surface keeps to drive them.
#[derive(Default)]
pub struct Placeholder {
    spinner: Option<Spinner>,
    button: Option<Button>,
}

impl Placeholder {
    /// Builds the busy stand-in under `parent`.
    ///
    /// `label` names the work in progress, drawn under the spinner; `None` draws the
    /// spinner alone, which is what a widget that is ready with nothing to say shows.
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
            NodeBuilder::text(label, TextStyle::new(PLACEHOLDER_TEXT_SIZE))
                .bind_style(move |read| StylePatch {
                    foreground: Some(theme.role(read, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .build(cx.tree_mut(), parent)?;
        }

        Ok(Self {
            spinner: Some(spinner),
            button: None,
        })
    }

    /// Builds a widget's own notice under `parent`: the sentence the catalogue holds
    /// in the language showing now, drawn as a muted, centred stand-in.
    ///
    /// The text is resolved here rather than in a `bind_text` closure because the
    /// notice is structural: the surface rebuilds the placeholder when the notice or
    /// the language moves, which is one rebuild per change instead of a live binding
    /// on every text leaf.
    pub fn notice(
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        notice: &Notice,
    ) -> Result<Self, EngineError> {
        let text = notice.resolve(cx.translate());
        EmptyState::new(cx.theme().clone(), notice_key(parent))
            .text(text)
            .build(cx.tree_mut(), parent)?;

        Ok(Self::default())
    }

    /// Builds a failure under `parent`: its detail exactly as the error states it,
    /// plus the recovery control the failure offers.
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
        })
    }

    /// Frame-driven synchronization of the stateful components.
    ///
    /// Called only while the placeholder is on screen: a hidden spinner must not keep
    /// an animation alive, so the surface releases the placeholder instead of hiding
    /// it.
    pub fn sync(&self, app: &mut App) -> Result<(), EngineError> {
        if let Some(spinner) = &self.spinner {
            spinner.sync(app)?;
        }
        if let Some(button) = &self.button {
            button.sync(app)?;
        }
        Ok(())
    }

    /// Forwards one platform event to the controls the placeholder built.
    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        self.button
            .as_ref()
            .is_some_and(|button| button.dispatch(app, event))
    }

    /// Releases the components' scopes and action handlers. Returns the number of
    /// signal values released.
    pub fn release(&self, app: &mut App) -> usize {
        let spinner = self
            .spinner
            .as_ref()
            .map_or(0, |spinner| spinner.release(app));
        let button = self.button.as_ref().map_or(0, |button| button.release(app));

        spinner + button
    }
}

/// Builds a failure's detail under `parent`.
///
/// The text is a live binding: the closure reads the language signal and then
/// renders through the translator, so moving the language redraws the sentence
/// without the surface rebuilding anything. This is the pattern every widget's own
/// language-dependent text follows.
pub(crate) fn failure_text(
    cx: &mut WidgetCx<'_>,
    parent: NodeId,
    failure: &Failure,
) -> Result<NodeId, EngineError> {
    let language = cx.language();
    let translate = cx.translate().clone();
    let theme = cx.theme().clone();
    let failure = failure.clone();

    NodeBuilder::text(SmolStr::default(), TextStyle::new(PLACEHOLDER_TEXT_SIZE))
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

/// Builds a failure's recovery control under `parent`.
///
/// A control that does not accept input is given no click handler at all: `disabled`
/// only draws that state, it does not stop a click.
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

/// A per-parent spinner key: a spinner's debug identity, unique inside the tree.
fn spinner_key(parent: NodeId) -> SmolStr {
    format_smolstr!("widget.placeholder.spinner.{}", parent.to_bits())
}

/// A per-parent notice key, for the same reason.
fn notice_key(parent: NodeId) -> SmolStr {
    format_smolstr!("widget.placeholder.notice.{}", parent.to_bits())
}

/// A per-parent recovery-button key. The engine's action identity is
/// `"{key}.click"`, so this must be unique across the whole application.
fn button_key(parent: NodeId) -> SmolStr {
    format_smolstr!("widget.placeholder.action.{}", parent.to_bits())
}

/// The name of the work in progress, in the language showing now.
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
