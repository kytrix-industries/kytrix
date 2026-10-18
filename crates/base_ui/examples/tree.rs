mod support;

use base_ui::{assets::Assets, style::theme::ThemeExt, tree::Tree};
use gpui_kit::{
    AppContext, Bounds, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    Subscription, Window, WindowBounds, WindowOptions,
    base::{TreeEvent, TreeItem, TreeState},
    div, px, size,
};
use smol_str::{SmolStr, format_smolstr};

/// What the tree is given: one small project, written the way a file browser draws it.
///
/// An entry with children is a folder, and an expanded one is an entry whose children are in the
/// list from the first draw. An entry's id is what a fold reports, so ids stay unique paths even
/// where two folders hold a file of the same name.
fn project() -> Vec<TreeItem> {
    vec![
        TreeItem::new("src", "src")
            .expanded(true)
            .child(
                TreeItem::new("src/components", "components")
                    .expanded(true)
                    .child(TreeItem::new("src/components/button.rs", "button.rs"))
                    .child(TreeItem::new("src/components/tree.rs", "tree.rs")),
            )
            .child(
                TreeItem::new("src/layout", "layout")
                    .child(TreeItem::new("src/layout/pane.rs", "pane.rs"))
                    .child(TreeItem::new("src/layout/split.rs", "split.rs")),
            )
            .child(TreeItem::new("src/lib.rs", "lib.rs"))
            .child(TreeItem::new("src/main.rs", "main.rs")),
        TreeItem::new("tests", "tests")
            .expanded(true)
            .child(TreeItem::new("tests/list_test.rs", "list_test.rs"))
            .child(TreeItem::new("tests/tree_test.rs", "tree_test.rs")),
        TreeItem::new("docs", "docs")
            .child(TreeItem::new("docs/architecture.md", "architecture.md"))
            .child(TreeItem::new(
                "docs/getting_started.md",
                "getting_started.md",
            )),
        TreeItem::new("Cargo.toml", "Cargo.toml"),
        // A disabled entry answers neither clicks nor keys.
        TreeItem::new("README.md", "README.md").disabled(true),
    ]
}

struct TreeExample {
    /// Everything the tree draws and everything it has selected, which this view owns: the element
    /// holds no state of its own.
    tree: Entity<TreeState>,
    /// The last folder that was opened or closed, as the tree reported it.
    last_change: Option<TreeEvent>,
    /// Held because the tree moves its own selection, which the status line follows.
    _selection: Subscription,
    /// Held because a fold arrives as an event rather than as a change of fields.
    _folding: Subscription,
}

impl TreeExample {
    fn new(cx: &mut Context<Self>) -> Self {
        let tree = cx.new(|cx| TreeState::new(cx).items(project()));

        let selection = cx.observe(&tree, |_, _, cx| cx.notify());
        let folding = cx.subscribe(&tree, |this, _, event: &TreeEvent, cx| {
            this.last_change = Some(event.clone());
            cx.notify();
        });

        Self {
            tree,
            last_change: None,
            _selection: selection,
            _folding: folding,
        }
    }

    /// What the tree reports about itself: the entry under the selection, and the last folder that
    /// was opened or closed.
    fn status(&self, cx: &Context<Self>) -> impl IntoElement {
        let muted = cx.theme().roles.fg.muted.hsla;

        let selected = match self.tree.read(cx).selected_entry() {
            Some(entry) => format_smolstr!("{} at depth {}", entry.item().label, entry.depth()),
            None => SmolStr::new_static("nothing yet"),
        };
        let change = match &self.last_change {
            Some(TreeEvent::Expanded(id)) => format_smolstr!("opened {id}"),
            Some(TreeEvent::Collapsed(id)) => format_smolstr!("closed {id}"),
            None => SmolStr::new_static("nothing yet"),
        };

        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_1()
            .text_xs()
            .text_color(muted)
            .child(SharedString::new(format_smolstr!("Selected: {selected}")))
            .child(SharedString::new(format_smolstr!("Last fold: {change}")))
    }
}

impl Render for TreeExample {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let content = theme.roles.fg.default.hsla;
        let muted = theme.roles.fg.muted.hsla;

        div()
            .relative()
            .size_full()
            .bg(theme.roles.surface.canvas.hsla)
            .flex()
            .flex_col()
            .gap_4()
            .p_6()
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .text_color(content)
                            .child("Click a folder to open or close it; the arrow keys move the selection while the tree has focus."),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .whitespace_normal()
                            .text_xs()
                            .text_color(muted)
                            .child("A folder keeps its children out of the list until it is opened, and the list draws only the entries it can see. README.md is disabled, so it takes neither clicks nor keys."),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .rounded_sm()
                    .border_1()
                    .border_color(theme.roles.line.divider.hsla)
                    .overflow_hidden()
                    .child(Tree::new("tree-example".into(), self.tree.clone())),
            )
            .child(self.status(cx))
    }
}

fn main() {
    gpui_kit::application().with_assets(Assets).run(|cx| {
        support::init::init(cx);
        let bounds = Bounds::centered(None, size(px(560.), px(520.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_window, cx| cx.new(TreeExample::new),
        )
        .expect("failed to open window");
    });
}
