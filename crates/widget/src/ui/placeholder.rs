//! What a widget body shows while it has nothing of its own to render.
//!
//! A placeholder replaces the body, never accompanies it: either the widget has
//! content and draws it, or it does not and this stands in its place. Anything
//! the user must read while content *is* on screen belongs to a banner instead.
//!
//! It fills its stand-in box and places its own contents; what that box looks
//! like — its background and its corners — is the body slot's, drawn by
//! [`crate::surface`], so a placeholder reads as the same surface a real body
//! does.
//!
//! The widget's own message is drawn by the markdown component, so a copy that
//! points at a control can carry that control's icon and its label inside one
//! `<kbd>`. The other two places this stands in — the name of the work in
//! progress, and a failure's detail — are plain text: a failure has to read
//! exactly as it was reported, and it is drawn from the same string in the banner
//! beside content.
//!
//! The component draws its document across the whole width it is given, so every
//! line of a message starts at the same left edge, one line or several. A message
//! is therefore laid out like the rest of the interface's prose rather than
//! centred line by line; the plain-text stands-in above are centred because they
//! are single runs of text this element can place itself.
//!
//! This lives with the framework rather than with the shared component library
//! because it is not a general-purpose component: it exists to be a widget
//! body's stand-in, and only the framework decides when one is shown. The
//! primitives it is built from — the spinner, the style, the buttons — stay in
//! [`ui`], which this crate depends on.

use base_ui::{
    markdown::view::{Markdown, MarkdownState},
    spinner::Spinner,
    style::theme::ThemeExt,
};
use gpui_kit::{prelude::FluentBuilder, *};

/// The stand-in a widget body uses while it has nothing to render.
///
/// One of three things: the widget is busy and this names the work under a
/// spinner, the widget has a notice to show, or the widget failed. Only the
/// failure is announced as an alert — a notice is a status, not a problem.
#[derive(IntoElement)]
pub struct WidgetPlaceholder {
    message: PlaceholderMessage,
    kind: PlaceholderKind,
    action: Option<AnyElement>,
    id: ElementId,
}

/// What a stand-in says.
enum PlaceholderMessage {
    /// Words drawn as they stand: the name of the work in progress, and a failure's detail.
    Plain(SharedString),
    /// The document a notice resolves to, drawn by the markdown component.
    Markdown(Entity<MarkdownState>),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PlaceholderKind {
    /// Busy, under the name of the work in progress.
    Busy,
    /// A notice: an empty state, a signed-out state.
    Message,
    /// A failure, announced as an alert.
    Failure,
}

impl WidgetPlaceholder {
    /// Stands in for a widget that is establishing its connection.
    pub fn connect(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        Self::busy(id, text)
    }

    /// Stands in for a widget that is fetching or refreshing data.
    pub fn load(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        Self::busy(id, text)
    }

    fn busy(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        Self {
            message: PlaceholderMessage::Plain(text.into()),
            kind: PlaceholderKind::Busy,
            action: None,
            id: id.into(),
        }
    }

    /// Stands in for a widget with a notice and nothing to show yet.
    ///
    /// `document` is the markdown the notice's sentence resolves to, and it belongs to the
    /// widget instance: the framework re-reads it into that one document rather than
    /// building a new one per frame, so the parse is reused until the sentence moves.
    pub fn message(id: impl Into<ElementId>, document: &Entity<MarkdownState>) -> Self {
        Self {
            message: PlaceholderMessage::Markdown(document.clone()),
            kind: PlaceholderKind::Message,
            action: None,
            id: id.into(),
        }
    }

    /// Stands in for a widget that failed before it had anything to show.
    ///
    /// `text` is the failure detail exactly as the widget reported it. A
    /// placeholder adds nothing to it — no prefix, no wrapper — so a failure
    /// reads the same here as it does in a banner beside content.
    pub fn failure(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        Self {
            message: PlaceholderMessage::Plain(text.into()),
            kind: PlaceholderKind::Failure,
            action: None,
            id: id.into(),
        }
    }

    /// Adds the recovery control offered alongside the text.
    ///
    /// A busy placeholder shows no control, so this is ignored while the widget
    /// is working.
    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.action = Some(action.into_any_element());
        self
    }
}

impl RenderOnce for WidgetPlaceholder {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            message,
            kind,
            action,
            id,
        } = self;

        let theme = cx.theme();
        let (role, color) = match kind {
            PlaceholderKind::Busy => (Role::ProgressIndicator, theme.roles.fg.default),
            PlaceholderKind::Message => (Role::Status, theme.roles.fg.default),
            PlaceholderKind::Failure => (Role::Alert, theme.roles.status.danger.base),
        };

        // Whatever shape the message has, the stand-in is announced as one name: a failure's
        // detail as it stands, and a markdown message as the text of the document it was
        // drawn from — never the markup of it, and never a control's icon spelled out.
        let (label, content) = match message {
            // A failure reads exactly as it was reported: the banner draws the same string
            // as plain text, and passing it through a parser here would let the two disagree.
            PlaceholderMessage::Plain(text) => (text.clone(), div().child(text).into_any_element()),
            PlaceholderMessage::Markdown(document) => (
                document.read(cx).plain_text(),
                Markdown::new(&document)
                    .text_color(color.hsla)
                    .into_any_element(),
            ),
        };

        div()
            .id(id)
            .debug_selector(|| "WIDGET_PLACEHOLDER".into())
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .px_4()
            .text_center()
            .text_sm()
            .text_color(color.hsla)
            .role(role)
            .aria_label(label)
            .when(kind == PlaceholderKind::Busy, |this| {
                this.child(
                    div()
                        .debug_selector(|| "WIDGET_PLACEHOLDER_SPINNER".into())
                        .size_4()
                        .child(Spinner::new("widget-placeholder-spinner")),
                )
            })
            .child(
                div()
                    .debug_selector(|| "WIDGET_PLACEHOLDER_MESSAGE".into())
                    .min_w_0()
                    .max_w_full()
                    .child(content),
            )
            .when_some(action, |this, action| {
                this.child(
                    div()
                        .debug_selector(|| "WIDGET_PLACEHOLDER_ACTION".into())
                        .flex_none()
                        .child(action),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use base_ui::{markdown::view::MarkdownState, style::theme::Theme};
    use gpui_kit::{
        App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render,
        Styled, TestAppContext, VisualTestContext, Window, div, px, size,
    };
    use i18n::t;

    use super::WidgetPlaceholder;
    use crate::ui::test_support::install_translator;

    fn connecting() -> WidgetPlaceholder {
        WidgetPlaceholder::connect("test-placeholder", "Connecting")
    }

    fn loading() -> WidgetPlaceholder {
        WidgetPlaceholder::load("test-placeholder", "Loading")
    }

    /// The document a notice of one widget instance is drawn from, read from the fixture
    /// catalogue.
    ///
    /// The `test.notice` fixture is markdown that names a control the user has to reach
    /// for, so the stand-in draws that control — its icon and its label inside one
    /// `<kbd>` — the way the copy is written rather than as the sentence it would read
    /// as. `test.waiting` is the same drawer with a sentence that fits on one line.
    fn document(cx: &mut App, key: &'static str) -> Entity<MarkdownState> {
        let source = t!(cx, key);
        cx.new(|cx| MarkdownState::new(source, cx))
    }

    /// The document, built over a context that has the fixture catalogue installed.
    ///
    /// `open` installs it too, but a fixture that reads a key before the window exists
    /// needs it first.
    fn fixture_document(cx: &mut TestAppContext, key: &'static str) -> Entity<MarkdownState> {
        cx.update(|cx| {
            install_translator(cx);
            document(cx, key)
        })
    }

    fn message(document: &Entity<MarkdownState>) -> WidgetPlaceholder {
        WidgetPlaceholder::message("test-placeholder", document)
    }

    /// The width the stand-in leaves its message: what it is given, less its own
    /// horizontal padding (`px_4` on each side).
    fn block_expected_width(area: gpui_kit::Bounds<gpui_kit::Pixels>) -> f32 {
        area.size.width.as_f32() - 32.
    }

    fn failure() -> WidgetPlaceholder {
        WidgetPlaceholder::failure("test-placeholder", "route unavailable")
            .action(div().debug_selector(|| "TEST_ACTION".into()).h_4().w_8())
    }

    /// The same failure with a longer detail, so a test can tell the text the
    /// placeholder was given apart from any text it might have added itself.
    fn longer_failure() -> WidgetPlaceholder {
        WidgetPlaceholder::failure(
            "test-placeholder",
            "route unavailable, and the fallback failed too",
        )
    }

    struct Harness {
        /// Builds this frame's stand-in. The message fixture captures the document entity
        /// its widget instance owns, so every frame draws the same one — which is what the
        /// framework does with the document it holds.
        placeholder: Box<dyn Fn() -> WidgetPlaceholder>,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child((self.placeholder)())
        }
    }

    fn open(
        cx: &mut TestAppContext,
        placeholder: impl Fn() -> WidgetPlaceholder + 'static,
    ) -> &mut VisualTestContext {
        cx.update(|cx| {
            install_translator(cx);
            cx.set_global(Theme::default());
        });
        let (_, cx) = cx.add_window_view(move |_, _| Harness {
            placeholder: Box::new(placeholder),
        });
        cx.simulate_resize(size(px(240.0), px(160.0)));
        cx.run_until_parked();

        cx
    }

    #[gpui_kit::test]
    fn a_busy_placeholder_should_show_a_spinner_above_the_name_of_its_work(
        cx: &mut TestAppContext,
    ) {
        for placeholder in [connecting, loading] {
            let cx = open(cx, placeholder);

            let area = cx
                .debug_bounds("WIDGET_PLACEHOLDER")
                .expect("the placeholder should render");
            let spinner = cx
                .debug_bounds("WIDGET_PLACEHOLDER_SPINNER")
                .expect("a busy placeholder should show a spinner");
            let message = cx
                .debug_bounds("WIDGET_PLACEHOLDER_MESSAGE")
                .expect("the placeholder should name the work in progress");

            assert_eq!(area.size, size(px(240.0), px(160.0)));
            assert!(spinner.bottom() <= message.top());
            assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_ACTION").is_none());
        }
    }

    #[gpui_kit::test]
    fn a_failure_placeholder_should_show_no_spinner_and_offer_its_action(cx: &mut TestAppContext) {
        let cx = open(cx, failure);

        let area = cx
            .debug_bounds("WIDGET_PLACEHOLDER")
            .expect("the placeholder should render");
        let message = cx
            .debug_bounds("WIDGET_PLACEHOLDER_MESSAGE")
            .expect("the placeholder should show its message");
        let action = cx
            .debug_bounds("WIDGET_PLACEHOLDER_ACTION")
            .expect("a failure placeholder should offer its action");

        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_SPINNER").is_none());
        assert!(message.bottom() <= action.top());
        assert!(action.bottom() <= area.bottom());
        assert!(action.left() >= area.left());
        assert!(action.right() <= area.right());
    }

    #[gpui_kit::test]
    fn a_message_placeholder_should_show_neither_spinner_nor_action(cx: &mut TestAppContext) {
        let document = fixture_document(cx, "test.notice");
        let cx = open(cx, move || message(&document));

        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_MESSAGE").is_some());
        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_SPINNER").is_none());
        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_ACTION").is_none());
    }

    /// A neutral message is drawn by the markdown component as the document it is: the
    /// stand-in gives it a box and adds nothing of its own around it. What the copy does
    /// inside that box — its lines, the control it names — belongs to the component, so
    /// the assertion is about where the document is drawn, not about the shape of the
    /// sentence.
    #[gpui_kit::test]
    fn a_message_placeholder_should_draw_its_message_as_markdown(cx: &mut TestAppContext) {
        let document = fixture_document(cx, "test.notice");
        let cx = open(cx, move || message(&document));

        let area = cx
            .debug_bounds("WIDGET_PLACEHOLDER")
            .expect("the placeholder should render");
        let message = cx
            .debug_bounds("WIDGET_PLACEHOLDER_MESSAGE")
            .expect("the placeholder should show its message");
        let block = cx
            .debug_bounds("BASE_UI_MARKDOWN_BLOCK_0")
            .expect("the message should be drawn as markdown");

        assert!(
            block.top() >= message.top() && block.bottom() <= message.bottom(),
            "the document should stand inside the box the placeholder gives it"
        );
        assert!(
            block.left() >= area.left() && block.right() <= area.right(),
            "and inside the stand-in itself, however the copy wraps"
        );
        assert!(
            (block.size.width.as_f32() - block_expected_width(area)).abs() < 0.5,
            "the document should be drawn across the width the stand-in gives it, so the \
             column of text is one box rather than one per line: the block is {:?} wide in a \
             placeholder of {:?}",
            block.size.width,
            area.size.width
        );
        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_SPINNER").is_none());
        assert!(cx.debug_bounds("WIDGET_PLACEHOLDER_ACTION").is_none());
    }

    /// A message that fits on one line still fills the block it is drawn in, so its words
    /// start at the left edge of the stand-in rather than in the middle of it.
    ///
    /// This is the layout the markdown component gives every document, and pinning it here
    /// is what keeps the module's account of the alignment honest: should the block ever
    /// come to shrink to its words, this fails and the sentence above it has to be rewritten
    /// along with the assertion.
    #[gpui_kit::test]
    fn a_single_line_message_should_fill_the_block_it_is_drawn_in(cx: &mut TestAppContext) {
        let document = fixture_document(cx, "test.waiting");
        let cx = open(cx, move || message(&document));

        let area = cx
            .debug_bounds("WIDGET_PLACEHOLDER")
            .expect("the placeholder should render");
        let block = cx
            .debug_bounds("BASE_UI_MARKDOWN_BLOCK_0")
            .expect("the message should be drawn as markdown");

        assert_eq!(
            block.left(),
            area.left() + px(16.),
            "which is where that copy is painted from"
        );
        assert!(
            (block.size.width.as_f32() - block_expected_width(area)).abs() < 0.5,
            "one line of copy should still be laid out across the stand-in: the block is \
             {:?} wide in a placeholder of {:?}",
            block.size.width,
            area.size.width
        );
    }

    /// A placeholder adds nothing to the failure it is given.
    ///
    /// Which text ends up on screen is decided by the widget's error type alone.
    /// A prefix, a wrapper, or a rewritten message here would make the same
    /// failure read differently from the banner that shows it whenever the widget
    /// does have content, and this is the only place that can happen: the banner
    /// renders the reported text as it stands.
    ///
    /// The text box grows with the text, and the failure fixture is a single
    /// short line, so its width is the text's own measure. Two lengths are
    /// compared instead of one against a constant, which keeps the assertion
    /// independent of the font size the placeholder happens to use.
    #[gpui_kit::test]
    fn a_failure_placeholder_should_not_change_the_text_it_was_given(cx: &mut TestAppContext) {
        let cx = open(cx, failure);
        let short = cx
            .debug_bounds("WIDGET_PLACEHOLDER_MESSAGE")
            .expect("the placeholder should show its message")
            .size
            .width;

        let cx = open(cx, longer_failure);
        let long = cx
            .debug_bounds("WIDGET_PLACEHOLDER_MESSAGE")
            .expect("the placeholder should show its message")
            .size
            .width;

        assert!(
            long > short,
            "the placeholder must render the detail it was given: a message of {short:?} \
             that does not grow with its text is not that text"
        );
    }
}
