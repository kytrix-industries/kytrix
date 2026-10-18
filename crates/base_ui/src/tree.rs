use gpui_kit::{
    AnyElement, App, ElementId, Entity, IntoElement, ParentElement, RenderOnce, StyleRefinement,
    Styled, Transformation, Window,
    base::{Disableable, Easing, Selectable, Transition, TreeEntry, TreeEntryState, transition},
    div,
    prelude::FluentBuilder,
    radians,
};

use crate::{
    animation::ANIMATION_TICK,
    button::Button,
    icon::{Icon, IconKind},
    interactive::InteractiveVariantExt,
};

#[derive(IntoElement)]
pub struct Tree {
    id: ElementId,
    entity: Entity<gpui_kit::base::TreeState>,
}

impl Tree {
    pub fn new(id: ElementId, entity: Entity<gpui_kit::base::TreeState>) -> Self {
        Self { id, entity }
    }
}

impl RenderOnce for Tree {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let Self { id, entity } = self;

        gpui_kit::base::Tree::new(&entity)
            .size_full()
            .list_style(StyleRefinement::default().size_full())
            .item(render_tree_item(id))
    }
}

fn render_tree_item(
    id: ElementId,
) -> impl Fn(usize, &TreeEntry, TreeEntryState, &mut Window, &mut App) -> AnyElement {
    move |_, entry, state, window, cx| {
        let id: ElementId = (id.clone(), entry.item().id.clone()).into();

        let icon = entry.is_folder().then(|| {
            // How far the folder is from folded, which is what its caret points along. The
            // transition is keyed by the entry, and an element drawn for the first time takes the
            // value it is given instead of travelling to it: a caret that appears — including one
            // whose row only moved because another folder was folded — is drawn where its folder
            // already is, and only the caret of the folder that was folded turns.
            let unfolded = transition(
                (id.clone(), "caret_animation"),
                if entry.is_expanded() { 1.0f32 } else { 0.0f32 },
                Transition::new(ANIMATION_TICK).easing(Easing::EaseInOut),
                window,
                cx,
            );

            Icon::new(IconKind::CaretDown)
                .size_4()
                .transformation(Transformation::rotate(radians(
                    -std::f32::consts::PI * unfolded,
                )))
        });

        let depth = entry.depth();
        let depth = if depth == 0 && !entry.is_folder() {
            1
        } else if !entry.is_folder() {
            depth + 1
        } else {
            depth
        };

        Button::new(id)
            .ghost()
            .selected(state.is_selected())
            .disabled(entry.is_disabled())
            .gap_1()
            .p_1()
            .justify_start()
            .when(depth > 0, |this| {
                this.children(std::iter::repeat_with(|| div().size_4().flex_none()).take(depth))
            })
            .when_some(icon, |this, icon| this.child(icon))
            .child(entry.item().label.clone())
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{
        AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, TestAppContext,
        VisualTestContext, Window,
        base::{TreeItem, TreeState},
        div,
        test::TestWindowExt as _,
    };

    use super::Tree;
    use crate::{animation::ANIMATION_TICK, style};

    /// Folders with leaves between them, so that folding the first one moves every row below it:
    /// the rows that move are the ones a caret must survive without turning.
    fn items() -> Vec<TreeItem> {
        vec![
            TreeItem::new("first", "first").child(TreeItem::new("first/child", "child")),
            TreeItem::new("second", "second").child(TreeItem::new("second/child", "child")),
            TreeItem::new("leaf", "leaf"),
            TreeItem::new("third", "third").child(TreeItem::new("third/child", "child")),
        ]
    }

    /// A window holding nothing but this tree, so that the frames a tree leaves behind are the
    /// only ones a test counts.
    struct Harness {
        tree: Entity<TreeState>,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .child(Tree::new("tree-under-test".into(), self.tree.clone()))
        }
    }

    /// A window holding this tree and nothing else.
    ///
    /// `instant` asks for reduced motion. A caret transfer that is allowed to travel takes frames
    /// of its own, so the tests that count frames ask for a transfer that completes where it
    /// stands and only the tests that are about travelling let it travel.
    fn harness(
        cx: &mut TestAppContext,
        instant: bool,
    ) -> (Entity<TreeState>, &mut VisualTestContext) {
        cx.update(|cx| {
            style::theme::install(cx);
            cx.set_reduce_motion(instant);
        });
        let tree = cx.new(|cx| TreeState::new(cx).items(items()));
        let (_, cx) = cx.add_window_view(|_, _| Harness { tree: tree.clone() });
        (tree, cx)
    }

    /// How many elements are turning: the frames already asked for are delivered first, and then
    /// one frame is drawn and its requests are counted, so the count is of one frame alone.
    fn turning(cx: &mut VisualTestContext) -> usize {
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            window.render_frame(cx);
            window.simulate_next_frame(cx)
        })
    }

    /// Lets every animation the last frame started finish, so the next click starts from rest.
    fn settle(cx: &mut VisualTestContext) {
        cx.executor().advance_clock(ANIMATION_TICK * 2);
        turning(cx);
    }

    /// Folds the folder at visible `row`, with the pointer already on it and the row already
    /// selected, and reports how many frame requests the fold made.
    ///
    /// A click is also a pointer move and a selection change, and each of those starts a colour
    /// transition of its own. Moving the pointer and selecting the row beforehand — and letting
    /// those transitions finish — leaves the caret as the only thing the click starts, which is
    /// what the count is about.
    fn fold(cx: &mut VisualTestContext, tree: &Entity<TreeState>, row: usize) -> usize {
        tree.update(cx, |tree, cx| tree.set_selected_index(Some(row), cx));
        cx.update(|window, cx| window.hover(row, cx));
        settle(cx);

        cx.update(|window, cx| window.click(row, cx));
        turning(cx)
    }

    /// A caret drawn for the first time is drawn where its folder already is rather than turned
    /// into place, so mounting a tree turns nothing.
    #[gpui_kit::test]
    fn mounting_a_tree_should_leave_its_carets_where_they_are(cx: &mut TestAppContext) {
        let (_, cx) = harness(cx, true);

        assert_eq!(
            turning(cx),
            0,
            "a tree that is drawn for the first time must not turn any caret into place"
        );
    }

    /// Folding a folder turns that folder's caret and no other: a fold that moves every row below
    /// it turns no more carets than one that moves none, because the caret of a row that merely
    /// moved is drawn where the row is rather than turned to it.
    #[gpui_kit::test]
    fn folding_a_folder_should_turn_only_its_own_caret(cx: &mut TestAppContext) {
        let (tree, cx) = harness(cx, false);
        settle(cx);

        // The last folder has nothing below it, so folding it moves no other row.
        let alone = fold(cx, &tree, 3);
        settle(cx);

        // The second folder has rows below it, so folding it moves them.
        let moving = fold(cx, &tree, 1);

        assert!(
            alone > 0,
            "the caret of the folder that was folded should be turning"
        );
        assert_eq!(
            moving, alone,
            "a fold that moves other rows must turn only its own caret"
        );
    }

    /// A caret that has arrived stops asking for frames, so a tree at rest draws nothing.
    #[gpui_kit::test]
    fn a_turned_caret_should_ask_for_no_further_frames(cx: &mut TestAppContext) {
        let (_, cx) = harness(cx, false);
        settle(cx);

        cx.update(|window, cx| window.click(3usize, cx));
        assert!(
            turning(cx) > 0,
            "the caret of the folder that was folded should be turning"
        );

        settle(cx);

        assert_eq!(
            turning(cx),
            0,
            "a caret that has turned must not keep a tree drawing"
        );
    }
}
