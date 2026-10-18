//! Where a widget's state becomes what the user sees.
//!
//! This is the single implementation of the presentation rule. Widgets report
//! state; they never decide where it is shown, and they never assemble a banner
//! or a placeholder.
//!
//! Two rules, and the whole table follows from them:
//!
//! 1. **The banner is for failures only, and it needs content to sit above.** It
//!    is built only for a failure, so with none — or with no content for it to sit
//!    above — it is not in the tree at all and reserves no row. Neutral text is
//!    never a banner: a widget that has content and nothing wrong with it draws
//!    that content and nothing else.
//! 2. **A placeholder stands in for content that is not there.** It is reached
//!    only while the widget has no body — so nothing a widget reports can ever
//!    cost its own content its space. What it shows, in order of precedence, is
//!    the failure, then the widget's own [`Notice`], then the name of the work it
//!    is busy with.
//!
//! | what the widget has | its state | result |
//! | --- | --- | --- |
//! | content | `Failed` | the failure in a banner above the content |
//! | content | anything else | the content alone |
//! | no content | `Failed` | the failure in a placeholder, with its recovery |
//! | no content | `Waiting` | the notice's sentence in a placeholder |
//! | no content | `Connecting` | a spinner under the name of the connection |
//! | no content | `Loading` or `Ready` | a spinner with no message of its own |
//!
//! So a failure never replaces content the user can still read, and a notice
//! never costs content its space. A failure appears in exactly one of the two
//! surfaces, so the same text never appears twice.
//!
//! # The words are resolved on the frame that draws them
//!
//! Nothing this module draws was resolved when it was reported. A notice is a
//! catalogue reference and a failure is a typed error, and both are turned into
//! sentences here, in the language showing now, on every frame that shows them.
//!
//! That is what makes the language switch work without a single observer. The
//! application contract is `App::set_global(LanguageCode)` followed by
//! `App::refresh_windows`; that frame forces every `.cached()` subtree to rebuild,
//! so this surface re-resolves everything it states and no widget has to re-report
//! anything. The framework registers no observation of the language, and a widget
//! neither.
//!
//! Re-resolving every frame costs nothing for a message without arguments, which is
//! one table lookup and one handle clone. A notice that names venues does allocate,
//! so it goes through [`WidgetSurface::message`]'s memo: the pair of the notice and
//! the language it was resolved in is what decides whether the document is read
//! again, and both inputs are read every frame.
//!
//! # The body is always cached
//!
//! The widget body is the only part of a pane that can be expensive or update at
//! a high rate, so the framework caches it unconditionally rather than making
//! every widget decide. Its box is always the same: the remaining space of the
//! pane, under the header and the banner. That is what makes caching sound — a
//! cache hit never measures the contents, so the box has to be knowable from the
//! layout alone, and a body is never auto-sized.
//!
//! The banner is the opposite: it measures its own contents, so its height
//! follows its message and its control rather than a number chosen here.
//!
//! # The box around the body belongs to the framework
//!
//! The slot the body is cached in is also what the user reads as the pane's content
//! surface, and the framework draws it: `surface.sunken` behind the content, a
//! `line.divider` edging it, and a clip to the rounded corners so that content drawn
//! out to the edge cannot square them off. Nothing that fills the slot therefore paints a
//! background, radius or clip of its own — a body is its content and nothing else, and
//! every pane's body reads the same whatever is embedded in it. The placeholder that
//! stands in for a body is drawn in that same box, so a waiting pane and a full one
//! share one surface.
//!
//! That box is a real element, because the style a cached view is given is painted by
//! nobody: it lays the view out and nothing more. Were the body's colour put there, it
//! would never reach the screen.
//!
//! # This surface owns its presentation
//!
//! The document a notice is drawn from is one per widget instance, and it lives
//! here rather than on the state: it is a parse, not something the widget reports.
//! It is also why this is an entity at all — not to save frames, since GPUI rebuilds
//! the tree from the root every frame, but so that the document, the memo, the body
//! and the answer about content have one owner, and [`WidgetStatus`] stays a state
//! machine with nothing to draw in it.

use std::rc::Rc;

use base_ui::{
    button::Button,
    interactive::InteractiveVariantExt,
    markdown::view::MarkdownState,
    size::{UnifiedSizable, UnifiedSize},
    style::theme::ThemeExt,
};
use gpui_kit::{
    AnyElement, AnyView, App, AppContext, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, Role, SharedString, StatefulInteractiveElement, StyleRefinement, Styled,
    Window, base::Disableable, div, prelude::FluentBuilder,
};
use i18n::{language::codes::LanguageCode, t};

use super::{
    any::HasContent,
    status::{Failure, Notice, WidgetState, WidgetStatus},
    ui::placeholder::WidgetPlaceholder,
};

/// The weight of everything in a banner: its message and the text its control
/// would use.
const BANNER_SIZE: UnifiedSize = UnifiedSize::Small;

/// A widget's body, plus whatever the state it reported requires around it.
///
/// It holds everything the presentation needs and nothing a widget reports twice:
/// the body, the state cell the widget writes into, the one document a notice is
/// drawn from, and the memo that says when that document is out of date.
pub struct WidgetSurface {
    status: Entity<WidgetStatus>,
    body: AnyView,
    has_content: HasContent,
    /// The document a notice is drawn from: one per widget instance, read again
    /// only when the memo below says the words moved.
    message: Entity<MarkdownState>,
    /// The notice and the language the document was last read from.
    ///
    /// Both inputs are read on every frame, so the memo expires exactly when one of
    /// them moves — which is also the whole invalidation rule there is. Holding the
    /// notice costs one handle for a sentence of the catalogue and a list of at most
    /// three venues for one that names markets, which is the price of not resolving
    /// it again on every frame that draws it.
    rendered: Option<(Notice, LanguageCode)>,
    /// How many times a notice has been resolved into the document.
    ///
    /// Its only reader is this module's own tests, and it exists because the document
    /// cannot answer the question the memo is about: `MarkdownState::set_source`
    /// already ignores a source equal to the one it holds, so a surface that resolved
    /// the same sentence again would leave the document exactly as a memo hit does.
    /// This counts the resolutions themselves, and no production path reads it.
    #[cfg(test)]
    resolutions: usize,
}

impl WidgetSurface {
    /// Presents one widget instance.
    ///
    /// Takes `&mut impl AppContext` rather than `&mut App` because the registry
    /// builds it inside `cx.new(..)`, where the context is a `Context<Self>`.
    pub fn new(
        status: Entity<WidgetStatus>,
        body: AnyView,
        has_content: HasContent,
        cx: &mut impl AppContext,
    ) -> Self {
        Self {
            status,
            body,
            has_content,
            message: cx.new(|cx| MarkdownState::new(SharedString::default(), cx)),
            rendered: None,
            #[cfg(test)]
            resolutions: 0,
        }
    }

    /// The state cell this surface presents.
    pub const fn status(&self) -> &Entity<WidgetStatus> {
        &self.status
    }

    /// The body this surface presents.
    ///
    /// The body has one owner, this surface; `AnyWidget::config` reads it back
    /// through here to ask the widget about the options it persists, so the
    /// accessor is part of the production path rather than a window for tests.
    pub const fn body(&self) -> &AnyView {
        &self.body
    }

    /// The document this surface draws a notice from.
    ///
    /// Its only readers are this module's own tests, which read the sentence the
    /// frame resolved for them; no production path needs the document itself, so
    /// neither does anyone outside the crate.
    #[cfg(test)]
    pub(crate) const fn message(&self) -> &Entity<MarkdownState> {
        &self.message
    }

    /// How many times a notice has been resolved into this surface's document.
    ///
    /// Its only readers are this module's own tests (see the field): it is what says
    /// that a notice which has not moved was not resolved again, which the document
    /// itself cannot say.
    #[cfg(test)]
    pub(crate) const fn resolutions(&self) -> usize {
        self.resolutions
    }

    /// Reads the notice the widget reported into this surface's document, unless
    /// the memo already holds it.
    ///
    /// The memo is the pair of the notice and the language it was resolved in, and
    /// both of its inputs are read on every frame — so "has it moved" is asked of
    /// the inputs themselves rather than of a stamp on what they produced. A notice
    /// that names venues is the reason it exists: the sentence costs an allocation
    /// and a locked render cache, and a waiting pane asks for it on every frame.
    fn resolve_notice(&mut self, notice: &Notice, language: LanguageCode, cx: &mut App) {
        let stale = self
            .rendered
            .as_ref()
            .is_none_or(|(previous, shown)| previous != notice || *shown != language);

        if !stale {
            return;
        }

        let source = notice.resolve(cx);
        self.message
            .update(cx, |document, cx| document.set_source(source, cx));
        self.rendered = Some((notice.clone(), language));
        #[cfg(test)]
        {
            self.resolutions += 1;
        }
    }
}

impl Render for WidgetSurface {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The answer about content is asked of the body's own view on every frame,
        // which is also what puts the body entity in this frame's accessed set: a
        // body that notifies itself redraws the pane that shows it.
        let has_content = (self.has_content)(&self.body, cx);
        // The whole state is taken in one read: every fact below comes out of it, so
        // nothing here reads the cell a second time.
        let state = self
            .status
            .read_with(cx, |status, _| status.state().clone());

        // A pane with content draws its body and nothing else, so the notice it
        // reported is not resolved at all: a notice is what stands in for a body, and
        // with one there is no stand-in to build. The language is read here rather
        // than above so that the common case does not read a global it has no use for.
        if !has_content {
            let language = *cx.global::<LanguageCode>();

            if let Some(notice) = state.notice() {
                self.resolve_notice(notice, language, cx);
            }
        }

        // The banner exists only for a failure, and only while there is content for it
        // to sit above — the same failure without content belongs to the placeholder.
        // Nothing is added to the tree otherwise, so an absent banner reserves no row.
        let banner = if has_content {
            state
                .failure()
                .map(|failure| banner(failure, failure_parts(failure, cx), cx))
        } else {
            None
        };

        // What fills the slot: the widget's own content, or the placeholder that stands
        // in for it. A failure never replaces the body, it is shown above it, and the
        // body is cached — so its box has to be knowable from layout alone, which the
        // slot's box is.
        let slot = if has_content {
            // Layout only: the style a cached view is given is never painted, so the
            // body's surface belongs to the slot around it rather than here.
            self.body
                .clone()
                .cached(StyleRefinement::default().flex_1().min_w_0().min_h_0())
                .into_any_element()
        } else {
            without_content(&self.message, state, cx)
        };

        frame(banner, slot, cx)
    }
}

/// The frame of one widget instance: the surface box, the failure above the slot when
/// there is content for it to sit above, and the slot itself.
///
/// The slot is the framework's content surface whatever fills it — the cached body, or
/// the placeholder standing in for one — so a waiting pane and a full one read as one
/// surface, and nothing embedded in the slot paints a background, radius or clip of its
/// own.
fn frame(banner: Option<AnyElement>, slot: AnyElement, cx: &App) -> AnyElement {
    let theme = cx.theme();

    div()
        .debug_selector(|| "WIDGET_SURFACE".into())
        .size_full()
        .min_w_0()
        .min_h_0()
        .flex()
        .flex_col()
        .gap_1()
        .children(banner)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .rounded_sm()
                .border_1()
                .border_color(theme.roles.line.divider.hsla)
                .bg(theme.roles.surface.sunken.hsla)
                .overflow_hidden()
                .child(slot),
        )
        .into_any_element()
}

/// What stands in for the content of a widget that has none: the failure, the notice,
/// or the name of the work in progress.
fn without_content(
    message: &Entity<MarkdownState>,
    state: WidgetState,
    cx: &mut App,
) -> AnyElement {
    let placeholder_id = "WidgetPlaceholder";

    match state {
        WidgetState::Failed(failure) => {
            let parts = failure_parts(&failure, cx);
            let text = failure.text(cx);

            WidgetPlaceholder::failure(placeholder_id, text)
                .action(placeholder_button(parts))
                .into_any_element()
        }
        WidgetState::Waiting(_) => {
            WidgetPlaceholder::message(placeholder_id, message).into_any_element()
        }
        WidgetState::Connecting => {
            WidgetPlaceholder::connect(placeholder_id, t!(cx, "common.connecting"))
                .into_any_element()
        }
        WidgetState::Loading | WidgetState::Ready => {
            WidgetPlaceholder::load(placeholder_id, t!(cx, "common.loading")).into_any_element()
        }
    }
}

/// Everything a control needs to be drawn on this frame.
///
/// It is read out of the action once, as owned values, because the click handler
/// outlives the frame that builds it and so cannot borrow the action.
struct ActionParts {
    run: Rc<dyn Fn(&mut App) + 'static>,
    label: SharedString,
    enabled: bool,
}

fn failure_parts(failure: &Failure, cx: &App) -> ActionParts {
    let action = failure.action();

    ActionParts {
        run: action.run_handle(),
        label: action.label(cx),
        enabled: action.is_enabled(),
    }
}

/// The banner a failure is shown in while the widget still has content.
///
/// The banner has no height of its own: its message and its control set it, so a
/// longer control or a wrapped message is not clipped by a number picked elsewhere.
/// Its control is built small for the same reason it exists at all — to sit beside
/// the message rather than compete with it.
fn banner(failure: &Failure, parts: ActionParts, cx: &App) -> AnyElement {
    let text = failure.text(cx);
    let theme = cx.theme();

    div()
        .id("WidgetBanner")
        .debug_selector(|| "WIDGET_BANNER".into())
        .role(Role::Alert)
        .w_full()
        .flex_none()
        .min_w_0()
        .flex()
        .items_center()
        .gap_2()
        .p_1()
        .bg(theme.roles.status.danger.surface.hsla)
        .text_color(theme.roles.status.danger.base.hsla)
        .rounded_sm()
        .overflow_hidden()
        .occlude()
        .child(
            div()
                .debug_selector(|| "WIDGET_BANNER_MESSAGE".into())
                .min_w_0()
                .flex_1()
                .truncate()
                .text_size(BANNER_SIZE.text_size())
                .child(text),
        )
        .child(
            div()
                .debug_selector(|| "WIDGET_BANNER_ACTION".into())
                .flex_none()
                // The same size as the text beside it, so the two share a baseline
                // instead of one inflating the row for the other.
                .child(
                    Button::new("WidgetBannerAction")
                        .with_unified_size(BANNER_SIZE)
                        .outline()
                        .disabled(!parts.enabled)
                        .aria_label(parts.label.clone())
                        .when(parts.enabled, |button| {
                            let run = parts.run.clone();

                            // A recovery that does not accept input is given no
                            // handler at all: `disabled` only draws that state, it
                            // does not stop a click.
                            button.on_click(move |_, _, cx| run(cx))
                        })
                        .child(parts.label),
                ),
        )
        .into_any_element()
}

/// The control a failure without content offers, sized and styled for standing on
/// its own rather than for sharing a row with a message.
fn placeholder_button(parts: ActionParts) -> AnyElement {
    Button::new("WidgetPlaceholderAction")
        .outline()
        .disabled(!parts.enabled)
        .aria_label(parts.label.clone())
        .when(parts.enabled, |button| {
            let run = parts.run.clone();

            // Disabled the same way the banner's is: a control that does not accept
            // input is given no handler.
            button.on_click(move |_, _, cx| run(cx))
        })
        .child(parts.label)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        sync::Arc,
    };

    use base_ui::{markdown::view::MarkdownState, style};
    use gpui_kit::{
        App, AppContext, Context, Entity, InteractiveElement, IntoElement, Modifiers,
        ParentElement, Render, SharedString, Styled, TestAppContext, VisualTestContext, Window,
        div, px, size,
    };
    use i18n::{language::codes::LanguageCode, t};

    use super::WidgetSurface;
    use crate::{
        error::LocalizedError,
        status::{Notice, WidgetAction, WidgetState, WidgetStatus, WidgetStatusExt},
        ui::test_support::{WaitingWidget, install_translator},
        view::WidgetView,
    };

    /// The sentence one frame resolved into the document it draws a notice from.
    fn message_source(
        document: &Entity<MarkdownState>,
        cx: &mut VisualTestContext,
    ) -> SharedString {
        let document = document.clone();
        cx.update(|_, cx| document.read(cx).source().clone())
    }

    const FAILURE_EN: &str = "route unavailable";
    const FAILURE_ZH: &str = "路线不可用";

    /// A strongly typed failure whose detail follows the language, so a test can
    /// tell text that was resolved on this frame from text that was not.
    struct TestFailure;

    impl LocalizedError for TestFailure {
        fn localized_message(&self, cx: &App) -> SharedString {
            t!(cx, "sample.failure")
        }
    }

    /// The fixture error type behind a shared handle.
    fn test_failure() -> Arc<dyn LocalizedError> {
        Arc::new(TestFailure)
    }

    /// Stands in for a widget body, so the surface's own elements are
    /// distinguishable from what it wraps — and so its renders are countable.
    struct Body {
        renders: Counter,
    }

    impl Render for Body {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            self.renders.bump();

            div().debug_selector(|| "SURFACE_BODY".into()).size_full()
        }
    }

    /// Counts renders, so a cache hit is observable.
    #[derive(Clone, Default)]
    struct Counter(Rc<Cell<usize>>);

    impl Counter {
        fn get(&self) -> usize {
            self.0.get()
        }

        fn bump(&self) {
            self.0.set(self.0.get() + 1);
        }
    }

    /// An entity the harness renders beside the surface. Notifying it rebuilds
    /// the frame without refreshing the window, which is what an unrelated
    /// update elsewhere in the app looks like from inside this pane.
    struct Marker;

    impl Render for Marker {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .debug_selector(|| "SURFACE_MARKER".into())
                .into_any_element()
        }
    }

    /// A widget body that asks for one piece of element state under a fixed id.
    ///
    /// The id is deliberately the same in every body: what the test reads is whether the
    /// framework keeps two bodies of one kind apart without the id saying so.
    struct ElementStateProbe {
        inits: Counter,
    }

    impl Render for ElementStateProbe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let inits = self.inits.clone();
            let _ = window.use_keyed_state(
                gpui_kit::ElementId::Name(SharedString::new_static("element-state-probe")),
                cx,
                move |_, _| {
                    inits.bump();
                    0usize
                },
            );

            div().debug_selector(|| "SURFACE_PROBE".into()).size_full()
        }
    }

    /// Two widgets of one kind, embedded from two surface entities.
    struct ProbeHarness {
        first: Entity<WidgetSurface>,
        second: Entity<WidgetSurface>,
    }

    impl Render for ProbeHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .child(self.first.clone())
                .child(self.second.clone())
        }
    }

    /// The mutable half of the harness: which body the surface is built over.
    ///
    /// Every piece of state a test moves goes through this or through the status — the
    /// way a widget moves its own — and never through the surface, which owns what it
    /// presents but decides none of it.
    struct SurfaceState {
        body: Option<Entity<Body>>,
        renders: Counter,
    }

    /// The window root of a test pane: a surface and an entity beside it whose
    /// notification makes an unrelated frame.
    struct SurfaceHarness {
        surface: Entity<WidgetSurface>,
        marker: Entity<Marker>,
    }

    impl Render for SurfaceHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(self.surface.clone())
                .child(self.marker.clone())
        }
    }

    struct Surface {
        state: Rc<RefCell<SurfaceState>>,
        /// Whether the widget has content this frame, which is what the surface asks.
        ///
        /// It is a flag rather than a consequence of the body's presence because the
        /// surface holds the body for its whole life: what changes is the widget's
        /// answer, not the handle.
        content: Rc<Cell<bool>>,
        marker: Entity<Marker>,
        surface: Entity<WidgetSurface>,
        retries: Rc<Cell<usize>>,
    }

    impl Surface {
        /// Republishes the state the way a widget does, through the cell it holds.
        fn report(
            &self,
            cx: &mut VisualTestContext,
            report: impl FnOnce(&Entity<WidgetStatus>, &mut App),
        ) {
            let status = cx.update(|_, cx| self.surface.read(cx).status().clone());
            cx.update(|_, cx| report(&status, cx));
            cx.run_until_parked();
        }

        /// The control the fixture failure offers, counting the times it runs.
        fn recovery(&self) -> WidgetAction {
            let retries = self.retries.clone();

            WidgetAction::new("common.retry", move |_| retries.set(retries.get() + 1))
        }

        /// A failure of the fixture error type, with that control on it.
        fn failure(&self, enabled: bool) -> WidgetState {
            WidgetState::failed(test_failure(), self.recovery().enabled(enabled))
        }

        /// Makes the widget report that it has nothing to render.
        fn drop_body(&self, cx: &mut VisualTestContext) {
            self.content.set(false);
            // The harness is the window root, so re-rendering it is a refresh.
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
        }

        /// Removes the state, as a widget does when it recovers.
        fn recover(&self, cx: &mut VisualTestContext) {
            self.report(cx, |status, cx| status.report(WidgetState::Ready, cx));
        }

        fn retries(&self) -> usize {
            self.retries.get()
        }

        fn body_renders(&self) -> usize {
            self.state.borrow().renders.get()
        }

        /// The words the notice document currently holds.
        fn message_source(&self, cx: &mut VisualTestContext) -> SharedString {
            let document = cx.update(|_, cx| self.surface.read(cx).message().clone());

            message_source(&document, cx)
        }

        /// How many times this surface has resolved a notice into its document.
        fn resolutions(&self, cx: &mut VisualTestContext) -> usize {
            cx.update(|_, cx| self.surface.read(cx).resolutions())
        }

        /// Renders one more frame because something else in the app changed.
        fn unrelated_frame(&self, cx: &mut VisualTestContext) {
            let marker = self.marker.clone();
            cx.update(|_, cx| marker.update(cx, |_, cx| cx.notify()));
            cx.run_until_parked();
        }

        /// Notifies the body the way a widget does when what it draws changes.
        fn notify_body(&self, cx: &mut VisualTestContext) {
            let body = self
                .state
                .borrow()
                .body
                .clone()
                .expect("the body should still be set");
            cx.update(|_, cx| body.update(cx, |_, cx| cx.notify()));
            cx.run_until_parked();
        }
        /// Switches the language the way the application does — the global, and the
        /// frame that re-resolves everything drawn from it.
        fn switch_language(&self, cx: &mut VisualTestContext, language: LanguageCode) {
            cx.update(|window, cx| {
                cx.set_global(language);
                window.refresh();
            });
            cx.run_until_parked();
        }
    }

    /// Opens the harness over a pane that hosts a widget.
    fn open(cx: &mut TestAppContext) -> (&mut VisualTestContext, Surface) {
        cx.update(|cx| {
            install_translator(cx);
            style::theme::install(cx);
        });

        let retries = Rc::new(Cell::new(0));
        let state = Rc::new(RefCell::new(SurfaceState {
            body: None,
            renders: Counter::default(),
        }));
        let content = Rc::new(Cell::new(true));
        let status = cx.new(|_| WidgetStatus::new());
        let marker = cx.new(|_| Marker);
        let surface = cx.update({
            let state = state.clone();
            let content = content.clone();
            move |cx| {
                let renders = state.borrow().renders.clone();
                let body = cx.new(|_| Body { renders });
                state.borrow_mut().body = Some(body.clone());

                cx.new(|cx| {
                    WidgetSurface::new(status, body.into(), Rc::new(move |_, _| content.get()), cx)
                })
            }
        });
        let (_, cx) = cx.add_window_view({
            let surface = surface.clone();
            let marker = marker.clone();
            move |_, _| SurfaceHarness { surface, marker }
        });
        cx.simulate_resize(size(px(320.0), px(200.0)));
        cx.run_until_parked();

        (
            cx,
            Surface {
                state,
                content,
                marker,
                surface,
                retries,
            },
        )
    }

    #[gpui_kit::test]
    fn an_unrelated_frame_should_not_rebuild_the_body(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        let before = surface.body_renders();

        surface.unrelated_frame(cx);

        assert_eq!(
            surface.body_renders(),
            before,
            "the body is cached, so a frame it did not ask for must reuse it"
        );
    }

    #[gpui_kit::test]
    fn notifying_the_body_should_rebuild_it(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        let before = surface.body_renders();

        surface.notify_body(cx);

        assert_eq!(
            surface.body_renders(),
            before + 1,
            "a cached body is only repainted by its own notification"
        );
    }

    /// The body slot carries no id of its own, so what tells two widgets of one kind apart is the
    /// cached view the framework embeds around the body: the body entity itself. That is the
    /// reason a widget's own element ids do not have to carry its instance — two panes showing one
    /// widget type, or a pane and its drag preview, are two bodies, and the element state they ask
    /// for under the same id is still separate.
    #[gpui_kit::test]
    fn two_bodies_of_one_kind_should_not_share_element_state(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install_translator(cx);
            style::theme::install(cx);
        });

        let first = Counter::default();
        let second = Counter::default();
        let status = cx.new(|_| WidgetStatus::new());
        let first_probe = cx.new(|_| ElementStateProbe {
            inits: first.clone(),
        });
        let second_probe = cx.new(|_| ElementStateProbe {
            inits: second.clone(),
        });
        let (_, cx) = cx.add_window_view(move |_, cx| {
            let mut surface = |body: Entity<ElementStateProbe>| {
                let status = status.clone();

                cx.new(|cx| WidgetSurface::new(status, body.into(), Rc::new(|_, _| true), cx))
            };

            ProbeHarness {
                first: surface(first_probe),
                second: surface(second_probe),
            }
        });
        cx.simulate_resize(size(px(320.0), px(200.0)));
        cx.run_until_parked();

        assert_eq!(first.get(), 1, "the first body's element state is its own");
        assert_eq!(
            second.get(),
            1,
            "and the second body's is not the first one's"
        );

        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert_eq!(first.get(), 1, "a later frame reuses the state it kept");
        assert_eq!(second.get(), 1, "for both bodies");
    }

    /// A surface hands out the two things the widget that owns it is asked for: the state
    /// it presents, and the body whose config `AnyWidget::config` reads back.
    #[gpui_kit::test]
    fn a_surface_should_hand_out_the_body_and_the_state_it_presents(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        let state = cx.new(|_| WidgetStatus::new());
        let body = cx.new(|_| Body {
            renders: Counter::default(),
        });

        let presented = cx.update(|_, cx| {
            cx.new(|cx| {
                WidgetSurface::new(state.clone(), body.clone().into(), Rc::new(|_, _| true), cx)
            })
        });

        cx.update(|_, cx| {
            assert_eq!(presented.read(cx).status(), &state);
            assert_eq!(presented.read(cx).body().entity_id(), body.entity_id());
            assert_eq!(
                surface.surface.read(cx).body().entity_id(),
                surface
                    .state
                    .borrow()
                    .body
                    .clone()
                    .expect("the harness builds its surface over a body")
                    .entity_id(),
                "the body a surface was built over is the one it hands back"
            );
        });
    }

    /// The body fills the surface's own box, inside the hairline the slot draws: the
    /// framework gives it that box, and an absent banner reserves no row out of it.
    #[gpui_kit::test]
    fn a_widget_with_content_and_no_failure_should_show_only_its_body(cx: &mut TestAppContext) {
        let (cx, _surface) = open(cx);

        let surface = cx
            .debug_bounds("WIDGET_SURFACE")
            .expect("the surface should render");
        let body = cx
            .debug_bounds("SURFACE_BODY")
            .expect("the body should render");

        assert!(cx.debug_bounds("WIDGET_BANNER").is_none());
        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER").is_none());
        assert_eq!(
            body,
            surface.inset(px(1.0)),
            "with no banner to sit above, the body is the whole surface inside its edge"
        );
    }

    /// A widget that has content and nothing wrong with it draws its content and
    /// nothing else. A notice is a placeholder matter, so it neither becomes
    /// a banner nor takes a row away from the content.
    ///
    /// The body's box is asserted through the cache rather than through
    /// `debug_bounds`: a cache hit reuses the previous frame's paint range, and
    /// `debug_bounds` is populated by painting. Not re-rendering the body *is*
    /// the proof that it still holds the same box, because a changed box is what
    /// busts the cache.
    #[gpui_kit::test]
    fn a_notice_should_change_nothing_while_there_is_content(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        let before = surface.body_renders();

        surface.report(cx, |status, cx| {
            status.report(WidgetState::waiting(Notice::message("test.notice")), cx);
        });

        assert!(
            cx.debug_bounds("WIDGET_BANNER").is_none(),
            "a notice is never a banner"
        );
        assert!(
            cx.debug_bounds("WIDGET_PLACEHOLDER").is_none(),
            "there is content, so nothing stands in for it"
        );
        assert_eq!(
            surface.body_renders(),
            before,
            "the body kept its box and was reused"
        );
    }

    /// The document a notice is drawn from is filled by the frame that draws it, and not
    /// before: a pane with content never pays for the sentence it reported, because a
    /// notice only ever stands in for a body. The language is moved underneath the pane
    /// to show that no frame of it reads the document either — the memo would have
    /// expired, and there is nothing here to expire it for.
    #[gpui_kit::test]
    fn a_notice_nothing_draws_should_not_be_read(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);

        surface.report(cx, |status, cx| {
            status.report(WidgetState::waiting(Notice::message("test.notice")), cx);
        });
        surface.unrelated_frame(cx);

        assert_eq!(
            surface.message_source(cx),
            SharedString::default(),
            "a notice nothing draws stays unread, however many frames are built"
        );
        assert_eq!(
            surface.resolutions(cx),
            0,
            "and nothing resolved it, because nothing drew it"
        );

        surface.switch_language(cx, LanguageCode::ZhCn);

        assert_eq!(
            surface.message_source(cx),
            SharedString::default(),
            "and it stays unread when the language moves, because no frame draws it"
        );
        assert_eq!(
            surface.resolutions(cx),
            0,
            "the language moving is not a reason to resolve a notice nothing draws"
        );
    }

    #[gpui_kit::test]
    fn a_failure_with_content_should_appear_in_a_banner_above_the_body(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        let released = cx
            .debug_bounds("SURFACE_BODY")
            .expect("the body should render");
        let renders_before = surface.body_renders();

        let failure = surface.failure(true);
        surface.report(cx, |status, cx| status.report(failure, cx));

        assert_eq!(
            surface.body_renders(),
            renders_before + 1,
            "the banner takes a row out of the top, so the box the body is cached in \
             changes — and a changed box is what busts the cache"
        );

        let banner = cx
            .debug_bounds("WIDGET_BANNER")
            .expect("the banner should render");
        // A banner changes the body's box, and a changed box busts the cache on its
        // own; the refresh is kept so this read does not depend on that implicit
        // mechanism. The counter above is the evidence that the body was built again:
        // bounds cannot show it, because the box it reports is the same either way.
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        let body = cx
            .debug_bounds("SURFACE_BODY")
            .expect("the body should render");
        // Not frame evidence: the two boxes differ because the banner occupies a row
        // of the surface and pushes the body down inside it.
        assert!(
            body.top() > released.top(),
            "the banner takes a row out of the top, so the body starts lower: \
             released {released:?} vs now {body:?}"
        );

        assert!(banner.bottom() <= body.top());
        assert_eq!(
            body.bottom(),
            released.bottom(),
            "the banner takes its row out of the top: the body keeps the bottom of \
             the box it had"
        );
        assert!(
            cx.debug_bounds("WIDGET_PLACEHOLDER").is_none(),
            "a failure is shown in exactly one surface"
        );
    }

    /// The same failure reads the same in both places it can appear.
    ///
    /// The two surfaces build their own elements, and they are free to size and
    /// style their control differently, but neither may touch the text: whatever
    /// the widget's error type produced is exactly what the user reads. That is a
    /// property of the placeholder's own body, where the failure text is rendered
    /// verbatim, so it is asserted there rather than through the two layouts —
    /// the banner's message is a stretching box, and comparing the two widths
    /// would compare two different boxes rather than two texts.
    ///
    /// What this checks is the part both surfaces share: one failure, reported
    /// once, is the only text either of them has to show.
    #[gpui_kit::test]
    fn one_failure_should_be_the_only_text_either_surface_shows(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        let failure = surface.failure(true);
        surface.report(cx, |status, cx| status.report(failure, cx));

        assert!(
            cx.debug_bounds("WIDGET_BANNER_MESSAGE").is_some(),
            "the banner shows the reported failure"
        );
        assert!(
            cx.debug_bounds("WIDGET_PLACEHOLDER_MESSAGE").is_none(),
            "and the placeholder does not repeat it"
        );

        surface.drop_body(cx);

        assert!(
            cx.debug_bounds("WIDGET_PLACEHOLDER_MESSAGE").is_some(),
            "without content the placeholder shows the same failure"
        );
        assert!(
            cx.debug_bounds("WIDGET_BANNER_MESSAGE").is_none(),
            "and the banner is gone, so the text appears exactly once"
        );
    }

    /// Nothing in the banner may be clipped, which is the reason it measures its
    /// contents instead of trusting a constant.
    #[gpui_kit::test]
    fn the_banner_should_contain_its_message_and_its_control(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        let failure = surface.failure(true);
        surface.report(cx, |status, cx| status.report(failure, cx));

        let banner = cx
            .debug_bounds("WIDGET_BANNER")
            .expect("the banner should render");
        let message = cx
            .debug_bounds("WIDGET_BANNER_MESSAGE")
            .expect("the banner should show its message");
        let action = cx
            .debug_bounds("WIDGET_BANNER_ACTION")
            .expect("the banner should offer its recovery");

        for contained in [message, action] {
            assert!(contained.top() >= banner.top());
            assert!(contained.bottom() <= banner.bottom());
        }
    }

    /// A failure with nothing to sit above belongs to the placeholder, which fills the box
    /// the body would have occupied.
    #[gpui_kit::test]
    fn a_failure_without_content_should_appear_in_a_placeholder(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        surface.drop_body(cx);

        let failure = surface.failure(true);
        surface.report(cx, |status, cx| status.report(failure, cx));

        let placeholder = cx
            .debug_bounds("WIDGET_PLACEHOLDER")
            .expect("the placeholder should render");
        assert!(
            cx.debug_bounds("WIDGET_BANNER").is_none(),
            "there is no content for a banner to sit above"
        );
        assert!(cx.debug_bounds("SURFACE_BODY").is_none());
        assert_eq!(
            placeholder,
            cx.debug_bounds("WIDGET_SURFACE")
                .expect("the surface should render")
                .inset(px(1.0)),
            "a placeholder stands in for the body, so it fills the same box"
        );
    }

    /// A notice's sentence is resolved by the very frame that draws it, so a pane
    /// that has one on screen follows a language change with no reload, no
    /// notification from anyone, and nothing the widget has to do about it.
    #[gpui_kit::test]
    fn a_notice_without_content_should_come_back_in_the_language_showing_now(
        cx: &mut TestAppContext,
    ) {
        let (cx, surface) = open(cx);
        surface.drop_body(cx);
        surface.report(cx, |status, cx| {
            status.report(WidgetState::waiting(Notice::message("test.notice")), cx);
        });

        let english = cx.update(|_, cx| t!(cx, "test.notice"));
        assert_eq!(
            surface.message_source(cx),
            english,
            "the sentence the notice names is what the document holds"
        );

        surface.switch_language(cx, LanguageCode::ZhCn);

        assert_eq!(
            surface.message_source(cx),
            cx.update(|_, cx| t!(cx, "test.notice")),
            "the frame after the switch re-resolves the notice"
        );
    }

    /// A notice that has not moved, in a language that has not moved, is not
    /// resolved again: the memo is what makes a notice that names venues cost
    /// nothing on the frames that draw it unchanged.
    ///
    /// Unrelated frames are what that costs: they rebuild this subtree — the pane
    /// around it did not move, so the cached body is reused, but the placeholder is
    /// built again — and what a repeated report of the same state adds to them is
    /// nothing at all.
    ///
    /// Whether the sentence was resolved again is asked of [`WidgetSurface::resolutions`]
    /// rather than of the document: resolving the same sentence a second time leaves the
    /// document as it was, because `MarkdownState::set_source` ignores a source equal to
    /// the one it holds — so the document reads the same whether the memo held or not.
    #[gpui_kit::test]
    fn a_notice_that_has_not_moved_should_not_be_resolved_again(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        surface.drop_body(cx);
        let notice = || {
            WidgetState::waiting(Notice::MarketNotLoaded {
                venues: vec![market_participants_model::identifier::VenueId::Okx],
                supported: vec![
                    market_participants_model::identifier::VenueId::Binance,
                    market_participants_model::identifier::VenueId::Okx,
                ],
            })
        };

        surface.report(cx, |status, cx| status.report(notice(), cx));
        let resolved = surface.message_source(cx);
        let renders = surface.body_renders();
        assert!(
            resolved.contains("OKX"),
            "the fixture names its market: {resolved}"
        );
        assert_eq!(
            surface.resolutions(cx),
            1,
            "the frame that first draws the notice resolves it once"
        );

        surface.report(cx, |status, cx| status.report(notice(), cx));
        surface.unrelated_frame(cx);

        assert_eq!(
            surface.message_source(cx),
            resolved,
            "a notice and a language that did not move leave the sentence where it was"
        );
        assert_eq!(
            surface.body_renders(),
            renders,
            "and the body the placeholder stands in for is still cached"
        );
        assert_eq!(
            surface.resolutions(cx),
            1,
            "a notice that has not moved is not resolved again, however many frames draw it"
        );

        surface.switch_language(cx, LanguageCode::ZhCn);

        let translated = surface.message_source(cx);
        assert_ne!(
            translated, resolved,
            "the switch is what expires the memo, so the sentence is read again"
        );
        assert!(
            translated.contains("欧易"),
            "and it is read in the language showing now: {translated}"
        );
        assert_eq!(
            surface.resolutions(cx),
            2,
            "the switch is one more resolution, and the frames after it add none"
        );
    }

    /// The memo's other input, so that neither half of it can be dropped unnoticed: a
    /// notice that moved is read again, in the language that did not move.
    ///
    /// The move is the venues the sentence lists, which is what a report that waits for
    /// something else says; the fixture binds both arguments so the state is what moved and
    /// the sentence is what it re-reads.
    #[gpui_kit::test]
    fn a_notice_that_has_moved_should_be_resolved_again(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        surface.drop_body(cx);
        let notice = |venue, supported| {
            WidgetState::waiting(Notice::MarketNotLoaded {
                venues: vec![venue],
                supported: vec![supported],
            })
        };

        surface.report(cx, |status, cx| {
            status.report(
                notice(
                    market_participants_model::identifier::VenueId::Okx,
                    market_participants_model::identifier::VenueId::Okx,
                ),
                cx,
            );
        });
        let okx = surface.message_source(cx);
        assert!(okx.contains("OKX"), "the first notice is read: {okx}");
        assert_eq!(
            surface.resolutions(cx),
            1,
            "once, for the notice that arrived"
        );

        surface.report(cx, |status, cx| {
            status.report(
                notice(
                    market_participants_model::identifier::VenueId::Binance,
                    market_participants_model::identifier::VenueId::Binance,
                ),
                cx,
            );
        });

        let binance = surface.message_source(cx);
        assert!(
            binance.contains("Binance") && !binance.contains("OKX"),
            "a notice that moved is read again, in the language where it was: {binance}"
        );
        assert_eq!(
            surface.resolutions(cx),
            2,
            "the memo expires on the notice alone, without the language moving"
        );
    }

    /// A widget's own notice is markdown: the copy names a control the user has to reach
    /// for, and the component draws it — the control's icon inside its own `<kbd>` — where
    /// the placeholder gives it room.
    #[gpui_kit::test]
    fn a_notice_without_content_should_be_drawn_by_the_markdown_component(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        surface.drop_body(cx);
        surface.report(cx, |status, cx| {
            status.report(WidgetState::waiting(Notice::message("test.notice")), cx);
        });

        // The placeholder is not part of the cached body, but a selector is still answered
        // from the last paint, so the frame is forced before anything is read.
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let message = cx
            .debug_bounds("WIDGET_PLACEHOLDER_MESSAGE")
            .expect("the placeholder should show the message");
        let markdown = cx
            .debug_bounds("BASE_UI_MARKDOWN")
            .expect("the message should be drawn as markdown");
        let block = cx
            .debug_bounds("BASE_UI_MARKDOWN_BLOCK_0")
            .expect("the message should be one paragraph");

        assert!(
            block.top() >= message.top() && block.bottom() <= message.bottom(),
            "the document should stand inside the message box"
        );
        assert!(
            markdown.left() >= message.left() && markdown.right() <= message.right(),
            "and inside its width: the box is the placeholder's, not the document's"
        );
        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_SPINNER").is_none());
        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_ACTION").is_none());
        assert!(
            cx.debug_bounds("WIDGET_BANNER").is_none(),
            "a notice is a stand-in for a body, never a banner"
        );
    }

    /// A failure reads exactly as it was reported, in both surfaces that can show it, so
    /// the placeholder never passes one through a parser.
    #[gpui_kit::test]
    fn a_failure_placeholder_should_draw_no_markdown(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        surface.drop_body(cx);
        let failure = surface.failure(true);
        surface.report(cx, |status, cx| status.report(failure, cx));

        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_MESSAGE").is_some());
        assert!(
            cx.debug_bounds("BASE_UI_MARKDOWN").is_none(),
            "a failure's detail is not markdown, here or in the banner"
        );
    }

    #[gpui_kit::test]
    fn a_recovery_should_be_clickable_from_the_placeholder(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        surface.drop_body(cx);
        let failure = surface.failure(true);
        surface.report(cx, |status, cx| status.report(failure, cx));

        let action = cx
            .debug_bounds("WIDGET_PLACEHOLDER_ACTION")
            .expect("a failure with a recovery should offer it");

        cx.simulate_click(action.center(), Modifiers::none());

        assert_eq!(surface.retries(), 1);
    }

    #[gpui_kit::test]
    fn a_recovery_should_be_clickable_from_the_banner(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        let failure = surface.failure(true);
        surface.report(cx, |status, cx| status.report(failure, cx));

        let action = cx
            .debug_bounds("WIDGET_BANNER_ACTION")
            .expect("a failure with content should offer its recovery");

        cx.simulate_click(action.center(), Modifiers::none());

        assert_eq!(surface.retries(), 1);
    }

    #[gpui_kit::test]
    fn a_disabled_recovery_should_not_run(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        let failure = surface.failure(false);
        surface.report(cx, |status, cx| status.report(failure, cx));

        let action = cx
            .debug_bounds("WIDGET_BANNER_ACTION")
            .expect("the recovery should still be shown while it is disabled");
        cx.simulate_click(action.center(), Modifiers::none());

        assert_eq!(surface.retries(), 0);
    }

    /// The two surfaces style their control differently, but a disabled action
    /// must be disabled in both: they report the same `enabled` flag.
    #[gpui_kit::test]
    fn a_disabled_recovery_should_not_run_from_the_placeholder_either(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        surface.drop_body(cx);
        let failure = surface.failure(false);
        surface.report(cx, |status, cx| status.report(failure, cx));

        let action = cx
            .debug_bounds("WIDGET_PLACEHOLDER_ACTION")
            .expect("the recovery should still be shown while it is disabled");
        cx.simulate_click(action.center(), Modifiers::none());

        assert_eq!(surface.retries(), 0);
    }

    #[gpui_kit::test]
    fn an_idle_widget_without_content_should_still_show_a_spinner(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);

        // A widget that is idle, has no content and has nothing to say.
        surface.report(cx, |status, cx| status.report(WidgetState::Ready, cx));
        surface.drop_body(cx);

        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_SPINNER").is_some());
        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_MESSAGE").is_some());
        assert!(cx.debug_bounds("WIDGET_BANNER").is_none());
    }

    #[gpui_kit::test]
    fn recovering_from_a_failure_without_content_should_return_to_the_spinner(
        cx: &mut TestAppContext,
    ) {
        let (cx, surface) = open(cx);
        surface.drop_body(cx);
        let failure = surface.failure(true);
        surface.report(cx, |status, cx| status.report(failure, cx));
        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_ACTION").is_some());

        surface.recover(cx);

        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_ACTION").is_none());
        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_SPINNER").is_some());
    }

    #[gpui_kit::test]
    fn publishing_the_same_state_twice_should_repaint_identically(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        surface.drop_body(cx);
        surface.report(cx, |status, cx| {
            status.report(WidgetState::Loading, cx);
        });

        let first = cx
            .debug_bounds("WIDGET_PLACEHOLDER")
            .expect("the placeholder should render");
        surface.report(cx, |status, cx| {
            status.report(WidgetState::Loading, cx);
        });
        let second = cx
            .debug_bounds("WIDGET_PLACEHOLDER")
            .expect("the placeholder should still render");

        assert_eq!(first, second);
    }

    /// A failure's detail and its control's label are both resolved by the frame
    /// that draws them, so both follow a language change — and the widget that
    /// reported the failure is not asked for anything.
    ///
    /// The first half reads the two sentences where they are written into the
    /// elements; the second reads the frame, because a sentence that resolves
    /// correctly and is not drawn would still be a failure of this contract.
    #[gpui_kit::test]
    fn a_failure_and_its_control_should_read_in_the_language_showing_now(cx: &mut TestAppContext) {
        let (cx, surface) = open(cx);
        surface.drop_body(cx);
        surface.report(cx, |status, cx| {
            status.report(
                WidgetState::failed(test_failure(), WidgetAction::new("common.retry", |_| {})),
                cx,
            );
        });

        cx.update(|_, cx| {
            let status = surface.surface.read(cx).status().clone();
            let state = status.state(cx);
            let failure = state
                .failure()
                .expect("the failure the widget reported is the state the pane reads");

            assert_eq!(failure.text(cx).as_ref(), FAILURE_EN);
            assert_eq!(failure.action().label(cx).as_ref(), "Retry");
        });
        assert!(
            cx.debug_bounds("WIDGET_PLACEHOLDER_ACTION").is_some(),
            "a failure always offers the way out it was reported with"
        );

        surface.switch_language(cx, LanguageCode::ZhCn);

        cx.update(|_, cx| {
            let status = surface.surface.read(cx).status().clone();
            let state = status.state(cx);
            let failure = state
                .failure()
                .expect("a language change re-reports nothing");

            assert_eq!(failure.text(cx).as_ref(), FAILURE_ZH);
            assert_eq!(failure.action().label(cx).as_ref(), "重试");
        });
        assert_eq!(
            surface.message_source(cx),
            SharedString::default(),
            "a failure is not drawn through the notice document"
        );
    }

    /// A pane hosting the framework's own widget shape, so the surface draws what the widget
    /// reported rather than what a test wrote into a status cell.
    struct WidgetHarness {
        widget: Entity<WaitingWidget>,
    }

    impl Render for WidgetHarness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.widget.clone())
        }
    }

    /// The whole path of a language change, driven by the framework's own fixture:
    /// the widget reports a notice once, its load path is never run again, and the
    /// switch re-resolves both the sentence the pane reads and the document the
    /// surface draws from it.
    ///
    /// Nothing here observes the language. What makes the frame follow it is the
    /// application's contract — the global, then `refresh_windows` — plus the fact
    /// that both are resolved while that frame is being built.
    #[gpui_kit::test]
    fn a_notice_should_follow_a_language_change_without_being_reported_again(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            install_translator(cx);
            style::theme::install(cx);
        });

        let widget = WaitingWidget::build(cx);
        let (_, cx) = cx.add_window_view({
            let widget = widget.clone();
            move |_, _| WidgetHarness { widget }
        });
        cx.simulate_resize(size(px(320.0), px(200.0)));
        cx.run_until_parked();

        // A pane that is waiting: the widget reported its notice from its own load
        // path, and that is the only state it ever wrote.
        cx.update(|_, cx| widget.update(cx, |widget, cx| widget.load(cx)));
        cx.run_until_parked();

        let english = cx.update(|_, cx| t!(cx, "test.notice"));
        let chinese = {
            cx.update(|_, cx| cx.set_global(LanguageCode::ZhCn));
            cx.update(|_, cx| t!(cx, "test.notice"))
        };
        assert_ne!(
            chinese, english,
            "the fixture spells this copy differently in each language"
        );

        /// The words the surface's notice document holds.
        fn notice_source(
            widget: &Entity<WaitingWidget>,
            cx: &mut VisualTestContext,
        ) -> SharedString {
            let document = cx.update(|_, cx| widget.read(cx).surface().read(cx).message().clone());

            message_source(&document, cx)
        }

        /// The state the widget last wrote.
        fn state(widget: &Entity<WaitingWidget>, cx: &mut VisualTestContext) -> WidgetState {
            cx.update(|_, cx| widget.read(cx).state(cx))
        }

        assert_eq!(
            cx.update(|_, cx| widget.read(cx).writes()),
            1,
            "the load path reports the notice once"
        );
        assert_eq!(
            notice_source(&widget, cx),
            english,
            "and the surface draws it"
        );
        assert_eq!(
            state(&widget, cx),
            WidgetState::waiting(Notice::message("test.notice")),
            "the state carries the reference, not the sentence"
        );

        // The switch, and nothing else: no reload, and no report from the test. The
        // frame is forced because the test moves the global itself rather than going
        // through the settings that would refresh the window with it.
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        assert_eq!(
            cx.update(|_, cx| widget.read(cx).writes()),
            1,
            "the surface re-resolved the notice, so the widget had nothing to re-report"
        );
        assert_eq!(
            notice_source(&widget, cx),
            chinese,
            "the document the surface draws moved with the language"
        );
        assert_eq!(
            state(&widget, cx),
            WidgetState::waiting(Notice::message("test.notice")),
            "and the state is the same reference it always was"
        );
        assert!(
            cx.debug_bounds("BASE_UI_MARKDOWN").is_some(),
            "the copy is drawn from the document it moved into"
        );
    }
}
