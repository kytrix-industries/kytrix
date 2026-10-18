use std::collections::VecDeque;

use gpui_kit::{
    AnyElement, App, ElementId, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    ScrollHandle, StatefulInteractiveElement, StyleRefinement, Styled, Window,
    base::{ScrollbarMode, StyledExt},
    div,
};

use crate::scroll_bar::Scrollbar;

#[derive(IntoElement)]
pub struct GridList {
    id: ElementId,
    cols: u16,
    children: VecDeque<AnyElement>,
    scroll_handle: Option<ScrollHandle>,
    style: StyleRefinement,
}

impl GridList {
    pub fn new(id: impl Into<ElementId>, cols: u16) -> Self {
        Self {
            id: id.into(),
            cols,
            children: VecDeque::new(),
            scroll_handle: None,
            style: StyleRefinement::default(),
        }
    }

    /// Scrolls the grid through `scroll_handle` rather than through a handle it keeps for
    /// itself.
    ///
    /// A grid that is given none keeps a handle of its own, which is made for the grid and
    /// lives as long as the grid is drawn. A caller that hands one in holds where the grid is
    /// scrolled to instead, so it can put the grid back to its start when what the grid draws
    /// is replaced — which is not something the grid can tell has happened.
    pub fn scroll_handle(mut self, scroll_handle: ScrollHandle) -> Self {
        self.scroll_handle = Some(scroll_handle);
        self
    }
}

impl Styled for GridList {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl ParentElement for GridList {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements)
    }
}

impl RenderOnce for GridList {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self {
            id,
            cols,
            mut children,
            scroll_handle,
            style,
        } = self;

        // A grid that was handed a handle is scrolled by it. One that was not keeps a handle
        // of its own, made for the grid where it is drawn and living as long as it is.
        let scroll_handle = match scroll_handle {
            Some(scroll_handle) => scroll_handle,
            None => window
                .use_keyed_state((id.clone(), "scroll"), cx, |_, _| ScrollHandle::new())
                .read(cx)
                .clone(),
        };

        let mut base = div()
            .id(id)
            .debug_selector(|| String::from("TEST_GRID_BASE"))
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&scroll_handle);

        while !children.is_empty() {
            let mut filled_cols = 0;
            let mut row = div().grid().grid_rows(1).grid_cols(cols);

            while filled_cols < cols {
                if let Some(child) = children.pop_front() {
                    row = row.child(child);
                    filled_cols += 1;
                } else {
                    break;
                }
            }

            base = base.child(row);
        }

        div()
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(base)
            .child(Scrollbar::vertical(scroll_handle).mode(ScrollbarMode::Always))
            .refine_style(&style)
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{
        Context, InteractiveElement, IntoElement, Modifiers, ParentElement, Render, ScrollDelta,
        ScrollHandle, ScrollWheelEvent, Styled, TestAppContext, TouchPhase, Window, div, point, px,
    };

    use super::GridList;
    use crate::style;

    /// The height one item is drawn at, which is what every row of a grid is as tall as.
    const ITEM_HEIGHT: f32 = 78.;

    /// The height the box around the grid is given, which is less than the rows need.
    const BOX_HEIGHT: f32 = 300.;

    /// How far one wheel event moves the grid.
    const SCROLL_DISTANCE: f32 = 200.;

    /// A window whose only child is a box of a fixed height holding the grid.
    struct Harness {
        /// The handle the grid is scrolled through, when the window holds where the grid is
        /// scrolled to rather than the grid keeping that to itself.
        scroll: Option<ScrollHandle>,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let grid = GridList::new("grid", 5).children((0..50).map(|index| {
                div()
                    .debug_selector(move || format!("TEST_GRID_ITEM_{index}"))
                    .w_full()
                    .h(px(ITEM_HEIGHT))
            }));
            let grid = match self.scroll.clone() {
                Some(scroll) => grid.scroll_handle(scroll),
                None => grid,
            };

            div()
                .size_full()
                .flex()
                .flex_col()
                .gap_4()
                .p_6()
                .child(div().flex_none().h(px(24.)).child("heading"))
                .child(
                    div()
                        .flex_none()
                        .w_full()
                        .h(px(BOX_HEIGHT))
                        .border_1()
                        .overflow_hidden()
                        .child(grid),
                )
        }
    }

    /// A grid is drawn in the height it is given rather than at the height of its rows, and the
    /// rows that do not fit are reached by scrolling.
    #[gpui_kit::test]
    fn a_grid_should_scroll_the_rows_that_do_not_fit(cx: &mut TestAppContext) {
        cx.update(style::theme::install);
        let (_, cx) = cx.add_window_view(|_, _| Harness { scroll: None });

        let grid = cx
            .debug_bounds("TEST_GRID_BASE")
            .expect("the grid should be drawn");
        assert_eq!(
            grid.size.height,
            px(BOX_HEIGHT - 2.),
            "the grid should be drawn in its box, less the border the box draws"
        );

        let first = cx
            .debug_bounds("TEST_GRID_ITEM_0")
            .expect("the first item should be drawn");

        cx.simulate_event(ScrollWheelEvent {
            position: point(px(100.), px(200.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-SCROLL_DISTANCE))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        });

        let scrolled = cx
            .debug_bounds("TEST_GRID_ITEM_0")
            .expect("the first item should still be drawn");
        assert_eq!(
            first.top() - scrolled.top(),
            px(SCROLL_DISTANCE),
            "the rows should move by the distance the wheel asked for"
        );
    }

    /// A grid that was handed a scroll handle is scrolled through it, so the caller that holds
    /// the handle is the one that decides where the grid is scrolled to.
    #[gpui_kit::test]
    fn a_grid_should_scroll_the_handle_it_was_given(cx: &mut TestAppContext) {
        cx.update(style::theme::install);
        let scroll = ScrollHandle::new();
        let (_, cx) = cx.add_window_view({
            let scroll = scroll.clone();
            |_, _| Harness {
                scroll: Some(scroll),
            }
        });

        let first = cx
            .debug_bounds("TEST_GRID_ITEM_0")
            .expect("the first item should be drawn");

        cx.simulate_event(ScrollWheelEvent {
            position: point(px(100.), px(200.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-SCROLL_DISTANCE))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        });
        assert_eq!(
            scroll.offset().y,
            px(-SCROLL_DISTANCE),
            "the grid should be scrolled through the handle it was given"
        );

        scroll.set_offset(point(px(0.), px(0.)));
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();

        let back = cx
            .debug_bounds("TEST_GRID_ITEM_0")
            .expect("the first item should still be drawn");
        assert_eq!(
            back.top(),
            first.top(),
            "putting the handle back should put the grid back"
        );
    }
}
