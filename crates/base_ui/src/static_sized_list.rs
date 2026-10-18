use std::{ops::Range, rc::Rc};

use gpui_kit::{
    AnyElement, App, Context, ElementId, InteractiveElement, IntoElement, ParentElement, Render,
    ScrollStrategy, StyleRefinement, Styled, UniformListScrollHandle, Window,
    base::{ScrollbarMode, StyledExt},
    div, uniform_list,
};

use crate::scroll_bar::Scrollbar;

/// A list that draws the items it is given, at one height for every place.
///
/// What it draws is a set rather than a feed: it is given the items of a catalogue, of a
/// filter's answer or of an account's rows, all at once, which is a set whose length is known
/// before it is drawn — a feed that grows at one end is what
/// [`DequeSizedList`](crate::deque_sized_list::DequeSizedList) is for.
///
/// The height is not handed in either: the framework measures the first place the render
/// closure draws and lays every other place out at that height, which is what makes a list of
/// same-sized rows cheap to scroll. A row drawn taller or shorter than the first one is
/// therefore laid out out of step with its neighbours rather than measured on its own, so the
/// closure is expected to draw one size of row — rows that differ in height are what
/// [`StaticUnsizedList`](crate::static_unsized_list::StaticUnsizedList) is for.
pub struct StaticSizedList<T> {
    id: ElementId,
    scroll: UniformListScrollHandle,
    items: Vec<T>,
    render: Rc<dyn Fn(usize, &T, &mut Window, &mut App) -> AnyElement>,
    style: StyleRefinement,
}

impl<T> StaticSizedList<T> {
    /// A list holding nothing yet, drawing each item through `render`.
    ///
    /// The closure is given the item's place among the items, which is the order they are
    /// drawn in. It is expected to draw the same size for every item, because the size of the
    /// first place it draws is what every place is laid out at.
    pub fn new(
        id: ElementId,
        render: impl Fn(usize, &T, &mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        StaticSizedList {
            id,
            scroll: UniformListScrollHandle::new(),
            items: Vec::new(),
            render: Rc::new(render),
            style: StyleRefinement::default(),
        }
    }

    /// Holds exactly the items given, in the order the list draws them, and drops the ones it
    /// held.
    ///
    /// Nothing is cut and nothing is kept: what the list draws is what it was given, because a
    /// set that is known before it is drawn has no end to grow at and no capacity to stay
    /// within. What it held is gone along with the reader's place in it, so the viewport goes
    /// back to the item the list draws first rather than staying on a place that no longer
    /// holds what they were reading.
    pub fn replace(&mut self, items: impl IntoIterator<Item = T>) {
        self.items = items.into_iter().collect();
        self.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
    }
}

impl<T> Styled for StaticSizedList<T> {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl<T> Render for StaticSizedList<T>
where
    T: 'static,
{
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.items.len();

        div()
            .id(self.id.clone())
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(
                uniform_list(
                    (self.id.clone(), "items"),
                    count,
                    cx.processor(|this, range: Range<usize>, window, cx| {
                        range
                            .map(|index| {
                                let child = (this.render)(index, &this.items[index], window, cx);
                                div().w_full().px_1().pt_1().child(child)
                            })
                            .collect()
                    }),
                )
                .pb_1()
                .track_scroll(&self.scroll)
                .size_full(),
            )
            .child(Scrollbar::vertical(self.scroll.clone()).mode(ScrollbarMode::Always))
            .refine_style(&self.style)
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{
        AnyElement, App, AppContext, Context, Entity, InteractiveElement, IntoElement,
        ParentElement, Render, Styled, TestAppContext, VisualTestContext, Window, div, px,
    };

    use super::StaticSizedList;
    use crate::style;

    /// The height a row is drawn at, which is the size every place of a list is laid out at.
    const ROW_HEIGHT: f32 = 20.;

    /// The padding the list itself wraps around every row it draws, which is added to the row's
    /// own height to make the place a uniform list measures.
    const ROW_PADDING: f32 = 4.;

    /// The height of the viewport the drawing tests give their list, which is a few rows.
    const VIEWPORT_HEIGHT: f32 = 100.;

    /// A row is named after the item it draws, so a test can ask for the bounds of one row
    /// instead of whichever row happened to be drawn last.
    const ROW_SELECTORS: [&str; 4] = [
        "TEST_STATIC_SIZED_ROW_0",
        "TEST_STATIC_SIZED_ROW_1",
        "TEST_STATIC_SIZED_ROW_2",
        "TEST_STATIC_SIZED_ROW_3",
    ];

    /// A window holding nothing but the list.
    struct Harness {
        list: Entity<StaticSizedList<(usize, f32)>>,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .w_full()
                .h(px(VIEWPORT_HEIGHT))
                .child(self.list.clone())
        }
    }

    /// An item is drawn as tall as the height it carries, which is what the list measures its
    /// first place for.
    fn row(
        _index: usize,
        (item, height): &(usize, f32),
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        div()
            .debug_selector({
                let selector = ROW_SELECTORS[*item];
                move || String::from(selector)
            })
            .w_full()
            .h(px(*height))
            .into_any_element()
    }

    /// A window holding this list and nothing else, drawn once.
    fn harness(
        cx: &mut TestAppContext,
        list: StaticSizedList<(usize, f32)>,
    ) -> (
        Entity<StaticSizedList<(usize, f32)>>,
        &mut VisualTestContext,
    ) {
        // The scrollbar the list draws under itself reads its colors from the style.
        cx.update(style::theme::install);
        let list = cx.new(|_| list);
        let (_, cx) = cx.add_window_view(|_, _| Harness { list: list.clone() });

        (list, cx)
    }

    /// The place the list is holding a scroll to, if it is holding one.
    fn pending_scroll(list: &StaticSizedList<(usize, f32)>) -> Option<usize> {
        list.scroll
            .0
            .borrow()
            .deferred_scroll_to_item
            .as_ref()
            .map(|deferred| deferred.item_index)
    }

    /// A list holds what it was given rather than what it held, however much that is.
    #[test]
    fn replacing_should_hold_the_items_the_list_was_given() {
        let mut list = StaticSizedList::new("list".into(), row);

        list.replace([(0, ROW_HEIGHT), (1, ROW_HEIGHT), (2, ROW_HEIGHT)]);
        assert_eq!(list.items.len(), 3, "a list holds every item it was given");

        list.replace([(3, ROW_HEIGHT)]);
        assert_eq!(
            list.items.iter().map(|(item, _)| *item).collect::<Vec<_>>(),
            [3],
            "what the list held is gone"
        );
    }

    /// A list whose items are replaced is put back to the item it draws first, because the
    /// place a reader was left on no longer holds what they were reading.
    #[test]
    fn replacing_should_put_the_viewport_back_where_the_list_starts() {
        let mut list = StaticSizedList::new("list".into(), row);
        list.replace([(0, ROW_HEIGHT)]);

        assert_eq!(
            pending_scroll(&list),
            Some(0),
            "a replaced list is read from the item it draws first"
        );
    }

    /// What a row is drawn as is the render closure's decision, and the height it draws plus the
    /// padding the list wraps it in is what every place of the list is laid out at.
    #[gpui_kit::test]
    fn every_place_should_be_laid_out_at_the_height_of_the_first_one(cx: &mut TestAppContext) {
        let mut list = StaticSizedList::new("list".into(), row);
        list.replace([(0, ROW_HEIGHT), (1, ROW_HEIGHT * 3.)]);
        let (_, cx) = harness(cx, list);

        let first = cx
            .debug_bounds(ROW_SELECTORS[0])
            .expect("the first item is drawn");
        let second = cx
            .debug_bounds(ROW_SELECTORS[1])
            .expect("the second item is drawn");

        assert_eq!(
            first.size.height,
            px(ROW_HEIGHT),
            "the place the list measured is drawn at the height it draws"
        );
        assert_eq!(
            second.top() - first.top(),
            px(ROW_HEIGHT + ROW_PADDING),
            "every place is laid out at the height of the first one"
        );
    }

    /// What the list draws is the set it was given last, so a list that is given another set
    /// draws that one.
    #[gpui_kit::test]
    fn replacing_the_items_should_draw_the_ones_the_list_was_given(cx: &mut TestAppContext) {
        let mut list = StaticSizedList::new("list".into(), row);
        list.replace([(0, ROW_HEIGHT), (1, ROW_HEIGHT)]);
        let (list, cx) = harness(cx, list);

        assert!(
            cx.debug_bounds(ROW_SELECTORS[0]).is_some(),
            "the items the list was given are drawn"
        );

        list.update(cx, |list, _| {
            list.replace([(2, ROW_HEIGHT), (3, ROW_HEIGHT)]);
        });
        cx.update(|window, _| window.refresh());

        assert!(
            cx.debug_bounds(ROW_SELECTORS[0]).is_none(),
            "what the list held is no longer drawn"
        );
        assert!(
            cx.debug_bounds(ROW_SELECTORS[2]).is_some(),
            "the items it was given instead are drawn"
        );
    }
}
