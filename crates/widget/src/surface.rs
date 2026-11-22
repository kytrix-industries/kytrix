//! Where a widget's state becomes what the user sees.
//!
//! This is the single implementation of the presentation rule. Widgets report
//! state; they never decide where it is shown, and they never assemble a banner or
//! a placeholder.
//!
//! Two rules, and the whole table follows from them:
//!
//! 1. **The banner is for failures only, and it needs content to sit above.** It is
//!    built only for a failure, so with none — or with no content for it to sit
//!    above — it is not in the tree at all and reserves no row. Neutral text is
//!    never a banner: a widget that has content and nothing wrong with it draws
//!    that content and nothing else.
//! 2. **A placeholder stands in for content that is not there.** It is reached
//!    only while the widget has no body — so nothing a widget reports can ever cost
//!    its own content its space. What it shows, in order of precedence, is the
//!    failure, then the widget's own [`Notice`], then the name of the work it is
//!    busy with.
//!
//! | what the widget has | its state | result |
//! | --- | --- | --- |
//! | content | `Failed` | the failure in a banner above the content |
//! | content | anything else | the content alone |
//! | no content | `Failed` | the failure in a placeholder, with its recovery |
//! | no content | `Waiting` | the notice's sentence (or, for a rich notice, its markdown) in a placeholder |
//! | no content | `Connecting` | a spinner under the name of the connection |
//! | no content | `Loading` or `Ready` | a spinner with no message of its own |
//!
//! So a failure never replaces content the user can still read, and a notice never
//! costs content its space. A failure appears in exactly one of the two surfaces, so
//! the same text never appears twice.
//!
//! # One tree, then sync
//!
//! The surface is built **once**: the root, the content slot and the placeholder
//! overlay exist for the widget's whole life, and only the banner — which reserves
//! layout space — is mounted and unmounted as failures come and go. Everything else
//! is switched by the presentation signal, which the slot's `bind_style` /
//! `bind_a11y` closures read: `StylePatch.visible` and `A11yPatch.hidden`, never
//! `display: none` (R-28: a display change is not re-sent through the a11y tree, so
//! a hidden descendant would stay announced).
//!
//! [`WidgetSurface::sync`] is the frame-driven half: it reads the state once, writes
//! the presentation signal when the picture changed, rebuilds the placeholder when
//! the state (or the language) moved, and drives the stateful components it owns.
//! The body itself is not its business: the widget builds it once when content
//! first appears.
//!
//! # The words are resolved on the frame that draws them
//!
//! Nothing this module draws was resolved when it was reported. A notice is a
//! catalogue reference and a failure is a typed error, and both are turned into
//! sentences here, through the translation context the frame was handed. A notice is
//! rebuilt when the language moves; a failure's text is a `bind_text` closure that
//! reads the language signal, so it follows a switch without a rebuild. A notice
//! whose catalogue value is authored as markdown/HTML — `common.market_not_loaded`
//! is, and so are several widgets' own messages such as `widget.news.logged_out` —
//! is drawn by the engine's markdown component, not as its source text; see
//! [`ui::placeholder`](crate::ui::placeholder) and [`Notice::is_markdown`].
//!
//! # The body is the widget's; the box around it is the framework's
//!
//! The content slot is what the user reads as the pane's content surface, and the
//! framework draws it: a sunken background, a divider border, a rounded corner and a
//! clip, all read from the theme through the style pass so an appearance switch
//! redraws them. Nothing a widget builds fills that box with a background of its
//! own, so every pane's body reads the same whatever is embedded in it.

use base_ui::{
    kit::button::Button,
    style_builder::{PaintBuilder, StyleBuilder},
    theme::role::Role,
};
use i18n::language::codes::LanguageCode;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yLive, A11yOverride, A11yPatch},
    app::App,
    error::EngineError,
    event::Event,
    node::{builder::NodeBuilder, id::NodeId},
    paint::style::StylePatch,
    text::TextStyle,
};

use crate::{
    cx::WidgetCx,
    status::{Failure, Notice, WidgetState, WidgetStatus},
    ui::placeholder::{self, Placeholder},
};

/// The banner's text size: small, so it sits beside its control rather than
/// competing with the content below.
const BANNER_TEXT_SIZE: f32 = 12.0;
/// The radius of the surface's boxes.
const SURFACE_RADIUS: f32 = 6.0;

/// Which of the surface's two interchangeable slots is drawn.
///
/// It is a signal so that writing it invalidates exactly the nodes that read it —
/// the style pass owns the visibility patch, the surface does not touch node state
/// directly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Presentation {
    content: bool,
    placeholder: bool,
}

/// What the placeholder currently shows, and the language it was resolved in.
///
/// The language is part of the key because a notice's sentence is resolved into a
/// static text node: moving the language means rebuilding the stand-in.
#[derive(Clone, Debug, PartialEq)]
enum PlaceholderKey {
    Busy { connecting: bool },
    Notice(Notice),
    Failure(Failure),
}

/// A widget's body, plus whatever the state it reported requires around it.
///
/// It holds the nodes it built, the state cell it presents, and the components it
/// must drive. It never holds the body: the widget builds and owns that.
pub struct WidgetSurface {
    root: NodeId,
    slot: NodeId,
    content: NodeId,
    placeholder: NodeId,
    banner: Option<NodeId>,
    status: WidgetStatus,
    language: ui_core::node::signal::Signal<LanguageCode>,
    presentation: ui_core::node::signal::Signal<Presentation>,
    last_presentation: Presentation,
    rendered_placeholder: Option<(PlaceholderKey, LanguageCode, Placeholder)>,
    rendered_banner: Option<(Failure, LanguageCode, Option<Button>)>,
}

impl WidgetSurface {
    /// Builds the shell under `parent` and presents `status`.
    ///
    /// The widget's body is not built here: the framework calls
    /// [`WidgetView::body`](crate::view::WidgetView::body) the first frame
    /// [`WidgetView::has_content`](crate::view::WidgetView::has_content) answers
    /// `true`, into [`WidgetSurface::content_slot`].
    pub fn build(
        cx: &mut WidgetCx<'_>,
        parent: NodeId,
        status: WidgetStatus,
    ) -> Result<Self, EngineError> {
        let language = cx.language();
        let presentation = cx.signal(Presentation::default());
        let theme = cx.theme().clone();

        let root_deps = status.signal();
        let root_language = language;
        let root_generation = cx.services().catalog_generation;
        let root_config = cx.services().config;
        let root = NodeBuilder::boxed()
            .id("WIDGET_SURFACE")
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .gap(4.0)
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            // The root is the surface's dependency anchor: a state report, a language
            // switch, a new catalogue generation or a new settings snapshot invalidates
            // it, which is what requests the frame that runs `sync`. It draws nothing
            // itself.
            //
            // The directory dependency belongs here rather than in the widget's body:
            // a widget waiting for the market has no body, yet it is exactly the one
            // that must re-check the gate when the directory publishes. The widget
            // compares the value in `sync` and reloads; it never schedules the frame
            // itself.
            //
            // The settings dependency belongs here for the same reason: a widget that
            // follows a setting (the account a pane is scoped to, the display time
            // zone) compares it in `sync`, and while it has no body there is no
            // `bind_*` in its own tree to carry the edge — a signed-out pane that has
            // no content yet must still wake when the settings move.
            .bind_style(move |read| {
                let _state = read.read(&root_deps);
                let _language = read.read(&root_language);
                let _generation = read.read(&root_generation);
                let _config = read.read(&root_config);
                StylePatch::default()
            })
            .build(cx.tree_mut(), parent)?;

        let built = build_shell(cx, root, theme, presentation);
        let (slot, content, placeholder) = match built {
            Ok(shell) => shell,
            // A surface that failed halfway must not leave the nodes it already
            // built behind: a pane would keep a shell no widget will ever present.
            Err(error) => {
                let window = cx.window();
                let _ = cx.app_mut().remove_node(window, root);
                return Err(error);
            }
        };

        Ok(Self {
            root,
            slot,
            content,
            placeholder,
            banner: None,
            status,
            language,
            presentation,
            last_presentation: Presentation::default(),
            rendered_placeholder: None,
            rendered_banner: None,
        })
    }

    /// The surface's root node: what a pane embeds.
    pub const fn root(&self) -> NodeId {
        self.root
    }

    /// The node a widget builds its body into.
    pub const fn content_slot(&self) -> NodeId {
        self.content
    }

    /// The state cell this surface presents.
    pub const fn status(&self) -> WidgetStatus {
        self.status
    }

    /// Applies the presentation the current state requires.
    ///
    /// `has_content` is the widget's answer for this frame; the wrapper that owns the
    /// widget asks it before calling this, so the surface never reaches into a view.
    pub fn sync(&mut self, cx: &mut WidgetCx<'_>, has_content: bool) -> Result<(), EngineError> {
        let state = self.status.state(cx.app());
        let language = *cx.read(&self.language);

        let next = Presentation {
            content: has_content,
            placeholder: !has_content,
        };
        if next != self.last_presentation {
            cx.write(&self.presentation, next);
            self.last_presentation = next;
        }

        self.sync_banner(cx, has_content, &state, language)?;

        if has_content {
            // A hidden spinner must not keep an animation alive: the stand-in is
            // released, not hidden, once content is on screen.
            self.clear_placeholder(cx);
        } else {
            let key = placeholder_key(&state);
            let stale = self.rendered_placeholder.as_ref().is_none_or(
                |(rendered, rendered_language, _)| {
                    rendered != &key || *rendered_language != language
                },
            );
            if stale {
                self.clear_placeholder(cx);
                let built = build_placeholder(cx, self.placeholder, &state)?;
                self.rendered_placeholder = Some((key, language, built));
            }
            if let Some((_, _, placeholder)) = &mut self.rendered_placeholder {
                placeholder.sync(cx.app_mut())?;
            }
        }

        Ok(())
    }

    /// Forwards one platform event to the controls the surface built.
    ///
    /// Returns whether one of them consumed it.
    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        let banner = self
            .rendered_banner
            .as_ref()
            .and_then(|(_, _, button)| button.as_ref())
            .is_some_and(|button| button.dispatch(app, event));
        let placeholder = self
            .rendered_placeholder
            .as_ref()
            .is_some_and(|(_, _, placeholder)| placeholder.dispatch(app, event));

        banner || placeholder
    }

    /// Releases the stateful components the surface owns.
    ///
    /// The nodes are removed by the owner of the widget, after this call and before
    /// the scope is released — the order the engine documents for teardown.
    pub fn release(&self, app: &mut App) -> usize {
        let banner = self
            .rendered_banner
            .as_ref()
            .and_then(|(_, _, button)| button.as_ref())
            .map_or(0, |button| button.release(app));
        let placeholder = self
            .rendered_placeholder
            .as_ref()
            .map_or(0, |(_, _, placeholder)| placeholder.release(app));

        banner + placeholder
    }

    /// Synchronizes (or mounts, unmounts) the failure banner.
    fn sync_banner(
        &mut self,
        cx: &mut WidgetCx<'_>,
        has_content: bool,
        state: &WidgetState,
        language: LanguageCode,
    ) -> Result<(), EngineError> {
        let failure = has_content.then(|| state.failure()).flatten().cloned();

        let Some(failure) = failure else {
            self.clear_banner(cx)?;
            return Ok(());
        };

        let stale = self
            .rendered_banner
            .as_ref()
            .is_none_or(|(rendered, rendered_language, _)| {
                rendered != &failure || *rendered_language != language
            });
        if stale {
            self.clear_banner(cx)?;
            let (node, button) = build_banner(cx, self.root, &failure)?;
            // The banner belongs above the slot. The slot was built first, so it is
            // moved to the end: reparenting keeps its subtree and its node identity.
            cx.tree_mut()
                .append_child(self.root, self.slot)
                .map_err(|_| EngineError::StaleNode { node: self.slot })?;
            self.banner = Some(node);
            self.rendered_banner = Some((failure, language, button));
        }
        if let Some((_, _, Some(button))) = &self.rendered_banner {
            button.sync(cx.app_mut())?;
        }

        Ok(())
    }

    /// Removes the banner: its nodes first, then its component's scope and handler.
    fn clear_banner(&mut self, cx: &mut WidgetCx<'_>) -> Result<(), EngineError> {
        let button = self
            .rendered_banner
            .take()
            .and_then(|(_, _, button)| button);
        if let Some(banner) = self.banner.take() {
            let window = cx.window();
            cx.app_mut().remove_node(window, banner)?;
        }
        if let Some(button) = button {
            let _ = button.release(cx.app_mut());
        }

        Ok(())
    }

    /// Removes the placeholder's contents, then releases its components.
    fn clear_placeholder(&mut self, cx: &mut WidgetCx<'_>) {
        let children: Vec<NodeId> = cx
            .tree()
            .get(self.placeholder)
            .map(|node| node.children().to_vec())
            .unwrap_or_default();
        let window = cx.window();
        for child in children {
            let _ = cx.app_mut().remove_node(window, child);
        }
        if let Some((_, _, placeholder)) = self.rendered_placeholder.take() {
            let _ = placeholder.release(cx.app_mut());
        }
    }
}

impl std::fmt::Debug for WidgetSurface {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WidgetSurface")
            .field("root", &self.root)
            .field("slot", &self.slot)
            .field("banner", &self.banner)
            .finish_non_exhaustive()
    }
}

/// What the placeholder must show, as an equality key: the same key means the
/// stand-in on screen is still the right one. The language is compared beside it,
/// because a notice's sentence is resolved into a static text node.
fn placeholder_key(state: &WidgetState) -> PlaceholderKey {
    match state {
        WidgetState::Failed(failure) => PlaceholderKey::Failure(failure.clone()),
        WidgetState::Waiting(notice) => PlaceholderKey::Notice(notice.clone()),
        WidgetState::Connecting => PlaceholderKey::Busy { connecting: true },
        WidgetState::Loading | WidgetState::Ready => PlaceholderKey::Busy { connecting: false },
    }
}

/// Builds the stand-in the state asks for under `parent`.
fn build_placeholder(
    cx: &mut WidgetCx<'_>,
    parent: NodeId,
    state: &WidgetState,
) -> Result<Placeholder, EngineError> {
    match state {
        WidgetState::Failed(failure) => Placeholder::failure(cx, parent, failure),
        WidgetState::Waiting(notice) => Placeholder::notice(cx, parent, notice),
        WidgetState::Connecting => Placeholder::busy(
            cx,
            parent,
            Some(placeholder::busy_label(cx.translate(), true)),
        ),
        WidgetState::Loading | WidgetState::Ready => Placeholder::busy(cx, parent, None),
    }
}

/// Builds the shell's three interchangeable nodes under `root`.
///
/// Split out of [`WidgetSurface::build`] so a half-built shell can be removed as a
/// whole when one of the nodes cannot be created.
fn build_shell(
    cx: &mut WidgetCx<'_>,
    root: NodeId,
    theme: base_ui::theme::handle::ThemeHandle,
    presentation: ui_core::node::signal::Signal<Presentation>,
) -> Result<(NodeId, NodeId, NodeId), EngineError> {
    let slot_theme = theme;
    let slot_placeholder = presentation;
    let slot_content = presentation;
    let slot = NodeBuilder::boxed()
        .id("WIDGET_SLOT")
        .style(
            StyleBuilder::new()
                .flex_1()
                .min_width_0()
                .min_height_0()
                .relative()
                .overflow_hidden()
                .build(),
        )
        .paint(PaintBuilder::new().rounded(SURFACE_RADIUS).build())
        .bind_style(move |read| StylePatch {
            background: Some(slot_theme.role(read, Role::SurfaceSunken)),
            border: Some(ui_core::paint::primitive::Border {
                width: 1.0,
                color: slot_theme.role(read, Role::LineDivider),
            }),
            ..StylePatch::default()
        })
        .build(cx.tree_mut(), root)?;

    let content = NodeBuilder::boxed()
        .id("WIDGET_CONTENT")
        .style(
            StyleBuilder::new()
                .flex_col()
                .flex_1()
                .min_width_0()
                .min_height_0()
                .build(),
        )
        .bind_style(move |read| {
            let presentation = *read.read(&slot_content);
            StylePatch {
                visible: Some(presentation.content),
                ..StylePatch::default()
            }
        })
        .bind_a11y(move |read| {
            let presentation = *read.read(&slot_content);
            A11yPatch {
                hidden: Some(!presentation.content),
                ..A11yPatch::default()
            }
        })
        .build(cx.tree_mut(), slot)?;

    let placeholder = NodeBuilder::boxed()
        .id("WIDGET_PLACEHOLDER")
        .style(
            StyleBuilder::new()
                .absolute()
                .top(0.0)
                .right(0.0)
                .bottom(0.0)
                .left(0.0)
                .flex_col()
                .items_center()
                .justify_center()
                .gap(8.0)
                .padding(16.0)
                .build(),
        )
        .paint(PaintBuilder::new().rounded(SURFACE_RADIUS).build())
        .bind_style(move |read| {
            let presentation = *read.read(&slot_placeholder);
            StylePatch {
                visible: Some(presentation.placeholder),
                ..StylePatch::default()
            }
        })
        .bind_a11y(move |read| {
            let presentation = *read.read(&slot_placeholder);
            A11yPatch {
                hidden: Some(!presentation.placeholder),
                ..A11yPatch::default()
            }
        })
        .build(cx.tree_mut(), slot)?;

    Ok((slot, content, placeholder))
}

/// Builds the failure banner under `root`, returning its node and its control.
fn build_banner(
    cx: &mut WidgetCx<'_>,
    root: NodeId,
    failure: &Failure,
) -> Result<(NodeId, Option<Button>), EngineError> {
    let theme = cx.theme().clone();
    let banner = NodeBuilder::boxed()
        .id("WIDGET_BANNER")
        .style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .gap(8.0)
                .padding(4.0)
                .width_full()
                .flex_none()
                .min_width_0()
                .build(),
        )
        .paint(PaintBuilder::new().rounded(SURFACE_RADIUS).build())
        .bind_style(move |read| StylePatch {
            background: Some(theme.role(read, Role::StatusDangerSurface)),
            foreground: Some(theme.role(read, Role::StatusDangerBase)),
            border: Some(ui_core::paint::primitive::Border {
                width: 1.0,
                color: theme.role(read, Role::StatusDangerBorder),
            }),
            ..StylePatch::default()
        })
        .a11y(A11yOverride {
            live: Some(A11yLive::Assertive),
            ..A11yOverride::default()
        })
        .build(cx.tree_mut(), root)?;

    NodeBuilder::text(SmolStr::default(), TextStyle::new(BANNER_TEXT_SIZE))
        .bind_text({
            let failure = failure.clone();
            let translate = cx.translate().clone();
            let language = cx.language();
            move |read| {
                read.read(&language);
                failure.text(&translate)
            }
        })
        .build(cx.tree_mut(), banner)?;

    let button = placeholder::recovery_button(
        cx,
        banner,
        failure,
        format_smolstr!("widget.banner.action.{}", banner.to_bits()),
    )?;

    Ok((banner, button))
}

#[cfg(test)]
mod tests {
    use std::{rc::Rc, sync::Arc};

    use i18n::{language::codes::LanguageCode, translate::Translate};
    use market_participants_model::identifier::VenueId;

    use crate::{
        status::{Notice, WidgetAction, WidgetState},
        test_support::{FixtureFailure, WidgetTestApp},
    };

    fn failure() -> WidgetState {
        WidgetState::failed(
            Arc::new(FixtureFailure),
            WidgetAction::new("common.retry", |_| {}),
        )
    }

    /// The translator the product ships, in the language a test asks for.
    fn product_translate(language: LanguageCode) -> Translate {
        Translate::new(
            Rc::new(i18n::embedded::load().expect("the embedded locales load")),
            language,
        )
    }

    #[test]
    fn placeholder_key_distinguishes_the_three_stand_ins() {
        let failed = failure();
        let notice = WidgetState::waiting(Notice::message("test.waiting"));

        assert_ne!(
            super::placeholder_key(&WidgetState::Connecting),
            super::placeholder_key(&WidgetState::Loading),
            "connecting and loading draw different labels"
        );
        assert_ne!(
            super::placeholder_key(&notice),
            super::placeholder_key(&failed),
        );
        assert_eq!(
            super::placeholder_key(&WidgetState::Connecting),
            super::placeholder_key(&WidgetState::Connecting),
        );
    }

    /// The six-row truth table, one case at a time. See the module documentation for
    /// the rule each row comes from.
    #[test]
    fn the_six_rows_of_the_presentation_table() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();

        // 1. content + Failed → banner above, content visible, no placeholder.
        harness.report(failure());
        harness.sync_surface(&mut surface, true);
        let banner = surface.banner.expect("the banner is mounted");
        assert_eq!(
            harness.tree().get(surface.root()).expect("root").children()[0],
            banner,
            "the banner sits above the slot"
        );
        assert!(harness.visible(surface.content_slot()));
        assert!(!harness.visible(surface.placeholder));

        // 2. content + anything else → the content alone.
        harness.report(WidgetState::Ready);
        harness.sync_surface(&mut surface, true);
        assert!(surface.banner.is_none(), "no failure, no banner");
        assert!(harness.visible(surface.content_slot()));
        assert!(!harness.visible(surface.placeholder));

        // 3. no content + Failed → the failure in the placeholder.
        harness.report(failure());
        harness.sync_surface(&mut surface, false);
        assert!(surface.banner.is_none());
        assert!(!harness.visible(surface.content_slot()));
        assert!(harness.visible(surface.placeholder));
        assert!(harness.has_text(surface.placeholder, "the fixture failed"));

        // 4. no content + Waiting → the notice's sentence.
        harness.report(WidgetState::waiting(Notice::message("test.waiting")));
        harness.sync_surface(&mut surface, false);
        assert!(
            harness
                .text_of(surface.placeholder)
                .contains("Waiting for orders")
        );

        // 5. no content + Connecting → a spinner under the name of the connection.
        harness.report(WidgetState::Connecting);
        harness.sync_surface(&mut surface, false);
        assert!(harness.has_canvas(surface.placeholder));
        assert!(harness.text_of(surface.placeholder).contains("Connecting"));

        // 6. no content + Loading / Ready → a spinner with no message of its own.
        harness.report(WidgetState::Loading);
        harness.sync_surface(&mut surface, false);
        assert!(harness.has_canvas(surface.placeholder));
        assert_eq!(harness.text_of(surface.placeholder), "");

        harness.report(WidgetState::Ready);
        harness.sync_surface(&mut surface, false);
        assert!(harness.has_canvas(surface.placeholder));
        assert_eq!(harness.text_of(surface.placeholder), "");
    }

    /// A language switch redraws the sentence the placeholder shows without the
    /// widget reporting anything again.
    #[test]
    fn a_waiting_placeholder_should_come_back_in_the_language_showing_now() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        harness.report(WidgetState::waiting(Notice::message("test.waiting")));
        harness.sync_surface(&mut surface, false);
        assert!(
            harness
                .text_of(surface.placeholder)
                .contains("Waiting for orders")
        );

        harness.switch_language(LanguageCode::ZhCn);
        harness.sync_surface(&mut surface, false);

        assert!(
            harness
                .text_of(surface.placeholder)
                .contains("正在等待订单"),
            "the stand-in is resolved in the language showing now: {}",
            harness.text_of(surface.placeholder)
        );
    }

    /// A failure's detail follows the language through its live binding, and the
    /// control's label is rebuilt in the new language with the banner.
    #[test]
    fn a_failure_banner_should_read_in_the_language_showing_now() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        harness.report(failure());
        harness.sync_surface(&mut surface, true);
        let banner = surface.banner.expect("the banner is mounted");
        assert!(harness.has_text(banner, "the fixture failed"));

        harness.switch_language(LanguageCode::ZhCn);
        harness.sync_surface(&mut surface, true);

        let banner = surface.banner.expect("the rebuilt banner");
        assert!(harness.has_text(banner, "示例失败"));
        assert!(
            harness.has_text(banner, "重试"),
            "the control reads in the new language: {}",
            harness.text_of(banner)
        );
    }

    /// The failing row's recovery runs the widget's action through the engine's real
    /// action dispatch.
    #[test]
    fn a_recovery_should_be_clickable_from_the_placeholder() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        let retries = Rc::new(std::cell::Cell::new(0));
        let counted = retries.clone();
        harness.report(WidgetState::failed(
            Arc::new(FixtureFailure),
            WidgetAction::new("common.retry", move |_| counted.set(counted.get() + 1)),
        ));
        harness.sync_surface(&mut surface, false);

        harness.click_recovery(surface.placeholder);

        assert_eq!(retries.get(), 1);
    }

    #[test]
    fn a_disabled_recovery_should_not_run() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        let retries = Rc::new(std::cell::Cell::new(0));
        let counted = retries.clone();
        harness.report(WidgetState::failed(
            Arc::new(FixtureFailure),
            WidgetAction::new("common.retry", move |_| counted.set(counted.get() + 1))
                .enabled(false),
        ));
        harness.sync_surface(&mut surface, false);

        // A control that refuses input registered no handler; the click finds no
        // action identity and changes nothing.
        let button = harness
            .find_by_debug_prefix(surface.placeholder, "widget.placeholder.action.")
            .expect("the control is drawn");
        let key = harness
            .tree()
            .debug_id(button)
            .expect("the control carries its id")
            .trim_end_matches(":interactive")
            .to_owned();
        let action = ui_core::event::Action::Custom(smol_str::format_smolstr!("{key}.click"));
        assert!(!harness.app_mut().has_action_handler(&action));
        assert_eq!(retries.get(), 0);
    }

    /// A widget with content and nothing wrong shows its body and nothing else.
    #[test]
    fn a_widget_with_content_and_no_failure_shows_only_its_body() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        let body = harness.build_body(surface.content_slot());

        harness.sync_surface(&mut surface, true);

        assert!(harness.visible(body));
        assert!(!harness.visible(surface.placeholder));
        assert!(surface.banner.is_none());
    }

    /// I3: a steady surface does not request frames. Synchronizing an unchanged
    /// state and language writes nothing and schedules nothing.
    #[test]
    fn a_steady_surface_requests_no_frame() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        harness.report(WidgetState::Ready);
        harness.sync_surface(&mut surface, false);
        let requests = harness.app().frame_requests();

        harness.sync_surface(&mut surface, false);

        assert_eq!(
            harness.app().frame_requests(),
            requests,
            "a steady surface scheduled no frame"
        );
    }

    /// The surface's root is the dependency anchor: it reads the language signal
    /// while it renders, so writing that signal requests the frame which presents the
    /// new language. A read that is not inside a `bind_*` closure would not register
    /// the edge, and the switch would leave the old language on screen.
    #[test]
    fn writing_the_language_signal_requests_the_redrawing_frame() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        harness.report(WidgetState::Ready);
        harness.sync_surface(&mut surface, false);
        let requests = harness.app().frame_requests();
        let language = harness.services().language;

        assert!(harness.app_mut().write(&language, LanguageCode::ZhCn));

        assert!(
            harness.app().frame_requests() > requests,
            "the surface read the language signal while rendering, so writing it \
             requests a frame (no state re-report involved)"
        );
    }

    /// The surface root is the directory dependency anchor: a widget waiting for the
    /// market has no body of its own, so the framework registers the generation read
    /// here — a published generation is what schedules the frame which lets the widget
    /// re-check its gate.
    #[test]
    fn writing_the_directory_generation_requests_the_redrawing_frame() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        harness.report(WidgetState::waiting(Notice::message("test.waiting")));
        harness.sync_surface(&mut surface, false);
        let requests = harness.app().frame_requests();
        let generation = harness.services().catalog_generation;

        assert!(harness.app_mut().write(&generation, 1));

        assert!(
            harness.app().frame_requests() > requests,
            "the surface root read the generation signal while rendering, so the write \
             requests a frame (no widget body involved)"
        );
    }

    /// `WidgetStatus::report` is a signal write: when a node renders from the state
    /// cell, reporting a new state requests the frame that presents it.
    #[test]
    fn reporting_a_state_requests_the_presenting_frame() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        harness.report(WidgetState::Ready);
        harness.sync_surface(&mut surface, false);
        let requests = harness.app().frame_requests();

        harness.report(WidgetState::Loading);

        assert!(
            harness.app().frame_requests() > requests,
            "the surface reads the state cell while rendering, so a report requests a frame"
        );
    }

    /// The surface root is also the **settings** dependency anchor. A widget that
    /// follows a setting (the account its pane is scoped to, the display time zone)
    /// compares it in `sync`, and while it has no content it has no `bind_*` of its
    /// own to carry the edge — a signed-out pane that has nothing to show must still
    /// wake when the settings move.
    #[test]
    fn writing_the_settings_signal_requests_the_redrawing_frame() {
        let mut harness = WidgetTestApp::new();
        let mut surface = harness.build_surface();
        harness.report(WidgetState::waiting(Notice::message("test.waiting")));
        harness.sync_surface(&mut surface, false);
        let requests = harness.app().frame_requests();
        let config = harness.services().config;
        let mut moved = (*harness.app().read(&config)).clone();
        moved.data_time_zone =
            app_config::time_zone::DataTimeZone::from_hours(8).expect("UTC+8 is offered");

        assert!(harness.app_mut().write(&config, moved));

        assert!(
            harness.app().frame_requests() > requests,
            "the surface root read the settings signal while rendering, so the write \
             requests a frame (no widget body involved)"
        );
    }

    /// R-28: the slot that is not drawn is hidden in the a11y tree, not merely
    /// invisible in the style pass. Assistive technology receives the same switch the
    /// user sees, and the hidden subtree is not announced.
    #[test]
    fn the_swapped_out_subtree_is_hidden_from_assistive_technology() {
        let mut harness = WidgetTestApp::new();
        harness.enable_a11y_capture();
        let mut surface = harness.build_surface();
        harness.report(WidgetState::Ready);
        harness.sync_surface(&mut surface, false);

        assert!(
            harness.a11y_hidden(surface.content_slot()),
            "no content: the content slot is hidden in the a11y tree"
        );
        assert!(
            !harness.a11y_hidden(surface.placeholder),
            "no content: the placeholder is announced"
        );

        harness.sync_surface(&mut surface, true);

        assert!(
            !harness.a11y_hidden(surface.content_slot()),
            "content: the content slot is announced"
        );
        assert!(
            harness.a11y_hidden(surface.placeholder),
            "content: the placeholder is hidden in the a11y tree"
        );
    }

    /// A notice the catalogue authors as markdown/HTML is drawn by the markdown
    /// component: the reader gets the sentence, the kbd icon and the venue list, not
    /// `<p align="center">…` / `<kbd>` / `![](file://…)` source text.
    #[test]
    fn a_rich_notice_is_drawn_as_rich_text() {
        let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
        let mut surface = harness.build_surface();
        harness.report(WidgetState::waiting(Notice::MarketNotLoaded {
            venues: vec![VenueId::Binance],
            supported: vec![VenueId::Binance, VenueId::Okx],
        }));

        harness.sync_surface(&mut surface, false);

        let text = harness.text_of(surface.placeholder);
        assert!(
            harness
                .find_by_debug_prefix(surface.placeholder, "MARKDOWN")
                .is_some(),
            "the rich notice goes through the markdown component"
        );
        assert!(
            !text.contains("<kbd>") && !text.contains("<p ") && !text.contains("![]("),
            "the notice's source markup is not drawn as text: {text:?}"
        );
        assert!(
            text.contains("Binance") && text.contains("OKX"),
            "the venues are listed by name: {text:?}"
        );
        assert!(
            harness.has_inline_boxes(surface.placeholder),
            "the kbd icon and the venue logos are inline boxes, not source text"
        );
    }

    /// The catalogue also authors widgets' **own** messages as rich text
    /// (`widget.news.logged_out` is a `<p align="center">` paragraph, and W1's news
    /// pane reported it as a plain `Notice::message`), so the markdown decision is
    /// made from the resolved value rather than from the notice variant. A rich
    /// message goes through the markdown component exactly like
    /// `common.market_not_loaded`, and its source markup never reaches the reader.
    #[test]
    fn a_rich_message_notice_is_drawn_as_rich_text() {
        for (key, has_inline_box) in [
            ("widget.news.logged_out", false),
            ("widget.empty.hint", true),
        ] {
            let mut harness = WidgetTestApp::with_translate(product_translate(LanguageCode::En));
            let mut surface = harness.build_surface();
            harness.report(WidgetState::waiting(Notice::message(key)));

            harness.sync_surface(&mut surface, false);

            let text = harness.text_of(surface.placeholder);
            assert!(
                harness
                    .find_by_debug_prefix(surface.placeholder, "MARKDOWN")
                    .is_some(),
                "{key} is authored as rich text and goes through the markdown component"
            );
            assert!(
                !text.contains("<p ") && !text.contains("<kbd>") && !text.contains("![]("),
                "{key}'s source markup is not drawn as text: {text:?}"
            );
            if has_inline_box {
                assert!(
                    harness.has_inline_boxes(surface.placeholder),
                    "{key}'s kbd icon is an inline box, not source text"
                );
            }
        }
    }
}
