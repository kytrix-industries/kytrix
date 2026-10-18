use std::{collections::VecDeque, ops::Range, rc::Rc};

use gpui_kit::{
    AnyElement, App, Context, ElementId, InteractiveElement, IntoElement, ParentElement, Pixels,
    Render, ScrollStrategy, StyleRefinement, Styled, UniformListScrollHandle, Window,
    base::{ScrollbarMode, StyledExt},
    div, uniform_list,
};

use crate::scroll_bar::Scrollbar;

/// A list that holds the newest `capacity` items and draws every place at one height.
///
/// The height is not handed in: the framework measures the first place the render closure
/// draws and lays every other place out at that height, which is what makes a list of
/// same-sized rows cheap to scroll. A row drawn taller or shorter than the first one is
/// therefore laid out out of step with its neighbours rather than measured on its own, so the
/// closure is expected to draw one size of row — a feed of rows that differ in height is what
/// [`DequeUnsizedList`](crate::deque_unsized_list::DequeUnsizedList) is for.
pub struct DequeSizedList<T> {
    id: ElementId,
    capacity: usize,
    scroll: UniformListScrollHandle,
    items: VecDeque<T>,
    render: Rc<dyn Fn(usize, &T, &mut Window, &mut App) -> AnyElement>,
    render_reverse: bool,
    keep_on_newest: bool,
    style: StyleRefinement,
}

impl<T> DequeSizedList<T> {
    /// A list holding nothing yet, drawing each item through `render`.
    ///
    /// The closure is given the item's place among the items, which is the order they are
    /// drawn in. It is expected to draw the same size for every item, because the size of the
    /// first place it draws is what every place is laid out at.
    ///
    /// `capacity` is how many items the list keeps and how much room it reserves for them,
    /// which belongs to whoever builds the list: an item past the capacity is dropped rather
    /// than grown into.
    pub fn new(
        id: ElementId,
        capacity: usize,
        render: impl Fn(usize, &T, &mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        DequeSizedList {
            id,
            capacity,
            scroll: UniformListScrollHandle::new(),
            items: VecDeque::with_capacity(capacity),
            render: Rc::new(render),
            render_reverse: false,
            keep_on_newest: false,
            style: StyleRefinement::default(),
        }
    }

    /// Draws the newest item first instead of last.
    ///
    /// The newest item is then the one the list opens on, which is also the end its places are
    /// inserted at, so this is the order for a list read from the newest item down.
    pub fn render_reverse(mut self) -> Self {
        self.render_reverse = true;
        self
    }

    /// Keeps the newest item in view as items are pushed, for a reader who is on it.
    ///
    /// A list that draws its newest item last follows its tail: an item pushed into a list its
    /// reader is at the end of is brought into view. A list that draws its newest item first has
    /// no tail to follow — the item just pushed is the first place — so a reader at the top,
    /// which is where that list keeps its newest item, is put back on the top.
    ///
    /// Only that reader is spoken for: one who went to read older items is left where they are,
    /// the way following a tail leaves them.
    pub fn keep_on_newest(mut self) -> Self {
        self.keep_on_newest = true;
        self
    }

    /// Adds one item, dropping the oldest one once `capacity` items are held.
    ///
    /// Items are held in the order they are drawn, so a list that draws its newest item first
    /// holds them newest first.
    ///
    /// Nobody but a reader [`Self::keep_on_newest`] speaks for is scrolled: the item just pushed
    /// is brought into view for one who is on the newest item already, and anyone else is left
    /// where they scrolled to, with what they are reading moved along by the place the push adds
    /// or drops in front of it.
    pub fn push(&mut self, data: T) {
        let full = self.items.len() == self.capacity;

        if self.render_reverse {
            // The newest item is the one drawn first, so it goes in at the front, and the oldest
            // one, which a full list drops, leaves from the back. Whether the reader is on the
            // newest item is what the item about to be put in front of them is read against, so
            // it is read before the list changes under them.
            let on_newest = self.keep_on_newest
                && self.scroll.0.borrow().base_handle.offset().y == Pixels::ZERO;

            if full {
                self.items.pop_back();
            }
            self.items.push_front(data);

            if on_newest {
                // There is no tail to follow on a list that draws its newest item first, so the
                // top is where the reader is put back, on the item just pushed.
                self.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
            }

            return;
        }

        // Whether the reader is at the end is what the item about to be appended to it is read
        // against, so it is read before the list changes under them. A list with nothing to
        // scroll is at its end as much as one that has been scrolled there.
        let on_newest = self.keep_on_newest && self.scroll.is_scrolled_to_end() != Some(false);

        if full {
            self.items.pop_front();
        }
        self.items.push_back(data);

        if on_newest {
            self.scroll.scroll_to_bottom();
        }
    }
}

impl<T> Styled for DequeSizedList<T> {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl<T> Render for DequeSizedList<T>
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
                                div().w_full().mx_1().my_0p5().child(child)
                            })
                            .collect()
                    }),
                )
                .py_0p5()
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
        AnyElement, App, AppContext, Context, Empty, Entity, InteractiveElement, IntoElement,
        ParentElement, Pixels, Render, ScrollStrategy, Styled, TestAppContext, VisualTestContext,
        Window, div, px,
    };

    use super::DequeSizedList;
    use crate::style;

    /// How many items every list of these tests holds.
    const CAPACITY: usize = 3;

    /// How many items a list that has to be scrolled holds, which is more than its viewport
    /// draws.
    const ROWS: usize = 10;

    /// The height a row is drawn at, which is the size every place of a list is laid out at.
    const ROW_HEIGHT: f32 = 20.;

    /// The height of the viewport the drawing tests give their list, which is a few rows.
    const VIEWPORT_HEIGHT: f32 = 100.;

    /// A row is named after the item it draws, so a test can ask for the bounds of one row
    /// instead of whichever row happened to be drawn last.
    const ROW_SELECTORS: [&str; 12] = [
        "TEST_DEQUE_SIZED_ROW_0",
        "TEST_DEQUE_SIZED_ROW_1",
        "TEST_DEQUE_SIZED_ROW_2",
        "TEST_DEQUE_SIZED_ROW_3",
        "TEST_DEQUE_SIZED_ROW_4",
        "TEST_DEQUE_SIZED_ROW_5",
        "TEST_DEQUE_SIZED_ROW_6",
        "TEST_DEQUE_SIZED_ROW_7",
        "TEST_DEQUE_SIZED_ROW_8",
        "TEST_DEQUE_SIZED_ROW_9",
        "TEST_DEQUE_SIZED_ROW_10",
        "TEST_DEQUE_SIZED_ROW_11",
    ];

    /// A window holding nothing but the list.
    struct Harness {
        list: Entity<DequeSizedList<(usize, f32)>>,
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
        list: DequeSizedList<(usize, f32)>,
    ) -> (Entity<DequeSizedList<(usize, f32)>>, &mut VisualTestContext) {
        // The scrollbar the list draws under itself reads its colors from the style.
        cx.update(style::theme::install);
        let list = cx.new(|_| list);
        let (_, cx) = cx.add_window_view(|_, _| Harness { list: list.clone() });

        (list, cx)
    }

    /// An empty list that draws nothing, for the tests that only read back what the list
    /// holds and where it was told to scroll to.
    fn empty_list(reverse: bool) -> DequeSizedList<(usize, f32)> {
        let mut list = DequeSizedList::new("list".into(), CAPACITY, |_, _, _, _| {
            Empty.into_any_element()
        });

        if reverse {
            list = list.render_reverse();
        }

        list
    }

    /// The place the list is holding a scroll to and how it was told to place it, if it is
    /// holding one.
    fn pending_scroll(list: &DequeSizedList<(usize, f32)>) -> Option<(usize, ScrollStrategy)> {
        list.scroll
            .0
            .borrow()
            .deferred_scroll_to_item
            .as_ref()
            .map(|deferred| (deferred.item_index, deferred.strategy))
    }

    /// Where the list is scrolled to, as the frame that drew it last left it.
    fn offset(list: &DequeSizedList<(usize, f32)>) -> Pixels {
        list.scroll.0.borrow().base_handle.offset().y
    }

    /// A list holds the newest items and the item that leaves takes its place with it,
    /// whichever way it draws them.
    #[test]
    fn pushing_should_hold_the_newest_items_and_drop_the_oldest() {
        for reverse in [false, true] {
            let mut list = empty_list(reverse);

            for item in 0..5 {
                list.push((item, ROW_HEIGHT));

                assert_eq!(
                    list.items.len(),
                    usize::min(item + 1, CAPACITY),
                    "a list holds its newest items up to the capacity ({reverse})"
                );
            }

            let kept: Vec<usize> = list.items.iter().map(|(item, _)| *item).collect();
            assert_eq!(
                kept,
                if reverse {
                    vec![4, 3, 2]
                } else {
                    vec![2, 3, 4]
                },
                "the newest three are kept, the newest first in reverse ({reverse})"
            );
        }
    }

    /// A push is a reason to scroll only for the reader the list keeps on the newest: the tail
    /// of a list that draws its newest item last, the top of one that draws it first.
    #[test]
    fn pushing_should_scroll_only_for_a_reader_kept_on_the_newest() {
        let mut list = empty_list(false).keep_on_newest();
        list.push((0, ROW_HEIGHT));

        assert_eq!(
            pending_scroll(&list),
            Some((usize::MAX, ScrollStrategy::Bottom)),
            "an item pushed into a list the reader is at the end of is brought into view"
        );

        let mut free = empty_list(false);
        free.push((0, ROW_HEIGHT));

        assert_eq!(
            pending_scroll(&free),
            None,
            "a list that does not keep on the newest scrolls for nobody"
        );

        let mut reverse = empty_list(true).keep_on_newest();
        reverse.push((0, ROW_HEIGHT));

        assert_eq!(
            pending_scroll(&reverse),
            Some((0, ScrollStrategy::Top)),
            "a reader on the newest item of a list that draws it first is put back on the top"
        );

        let mut reverse_free = empty_list(true);
        reverse_free.push((0, ROW_HEIGHT));

        assert_eq!(
            pending_scroll(&reverse_free),
            None,
            "and a list that keeps on nothing scrolls for nobody either way"
        );
    }

    /// Every place is laid out at the height of the place the list measured, so a row drawn
    /// taller than that one is drawn out of step with the rows around it.
    #[gpui_kit::test]
    fn every_place_should_be_laid_out_at_the_height_of_the_first_one(cx: &mut TestAppContext) {
        let mut list = DequeSizedList::new("list".into(), 2, row);
        list.push((0, ROW_HEIGHT));
        list.push((1, ROW_HEIGHT * 3.));
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
            px(ROW_HEIGHT),
            "every place is laid out at the height of the first one"
        );
        assert_eq!(
            second.size.height,
            px(ROW_HEIGHT * 3.),
            "a taller row is still drawn at its own height, which is what puts it out of step"
        );
    }

    /// Rendering in reverse draws the newest item first, and the item that leaves takes its
    /// place with it.
    #[gpui_kit::test]
    fn the_newest_item_should_be_drawn_first_when_the_list_renders_in_reverse(
        cx: &mut TestAppContext,
    ) {
        let mut list = DequeSizedList::new("list".into(), CAPACITY, row).render_reverse();
        for item in 0..4 {
            list.push((item, ROW_HEIGHT));
        }
        let (_, cx) = harness(cx, list);

        assert!(
            cx.debug_bounds(ROW_SELECTORS[0]).is_none(),
            "the oldest item is no longer drawn"
        );

        let newest = cx
            .debug_bounds(ROW_SELECTORS[3])
            .expect("the newest item is drawn");
        let next = cx
            .debug_bounds(ROW_SELECTORS[2])
            .expect("the item after it is drawn");

        assert_eq!(
            next.top() - newest.top(),
            px(ROW_HEIGHT),
            "the newest item is drawn first"
        );
        assert_eq!(newest.size.height, px(ROW_HEIGHT));
    }

    /// The item that leaves the deque frees its place, and the items that stay are drawn as
    /// the stack they are.
    #[gpui_kit::test]
    fn dropping_the_oldest_item_should_keep_the_other_places(cx: &mut TestAppContext) {
        let mut list = DequeSizedList::new("list".into(), CAPACITY, row);
        for item in 0..4 {
            list.push((item, ROW_HEIGHT));
        }
        let (_, cx) = harness(cx, list);

        assert!(
            cx.debug_bounds(ROW_SELECTORS[0]).is_none(),
            "the oldest item is no longer drawn"
        );

        let second = cx
            .debug_bounds(ROW_SELECTORS[1])
            .expect("the second item is drawn");
        let third = cx
            .debug_bounds(ROW_SELECTORS[2])
            .expect("the third item is drawn");
        let fourth = cx
            .debug_bounds(ROW_SELECTORS[3])
            .expect("the newest item is drawn");

        assert_eq!(second.size.height, px(ROW_HEIGHT));
        assert_eq!(
            third.top() - second.top(),
            px(ROW_HEIGHT),
            "the kept items are still stacked"
        );
        assert_eq!(fourth.top() - third.top(), px(ROW_HEIGHT));
    }

    /// A list that keeps on the newest stays on its tail while the reader is there, and leaves
    /// them alone once they have scrolled away from it.
    #[gpui_kit::test]
    fn keeping_on_the_newest_should_follow_the_tail_and_leave_a_reader_who_scrolled_away(
        cx: &mut TestAppContext,
    ) {
        let mut list = DequeSizedList::new("list".into(), 20, row).keep_on_newest();
        for item in 0..10 {
            list.push((item, ROW_HEIGHT));
        }
        let (list, cx) = harness(cx, list);

        // The viewport holds a few rows, so a list of ten of them is at its end after the
        // pushes that filled it, which is where the newest item is drawn.
        assert!(
            cx.debug_bounds(ROW_SELECTORS[9]).is_some(),
            "a list that keeps on the newest follows its tail"
        );

        // A reader who went back to the top of the list is reading an item no push touches, so
        // the item pushed after that is not one they are taken to.
        list.update(cx, |list, _| {
            list.scroll.scroll_to_item_strict(0, ScrollStrategy::Top);
        });
        cx.update(|window, _| window.refresh());
        let scrolled = list.read_with(cx, |list, _| offset(list));

        assert!(
            cx.debug_bounds(ROW_SELECTORS[0]).is_some(),
            "the reader scrolled back to the top of the list"
        );
        assert!(
            cx.debug_bounds(ROW_SELECTORS[9]).is_none(),
            "the newest item is below the fold they are reading"
        );

        list.update(cx, |list, _| list.push((10, ROW_HEIGHT)));
        cx.update(|window, _| window.refresh());

        assert_eq!(
            list.read_with(cx, |list, _| offset(list)),
            scrolled,
            "a push leaves a reader who scrolled away from the tail where they are"
        );
        assert!(
            cx.debug_bounds(ROW_SELECTORS[0]).is_some(),
            "the reader keeps the item they scrolled back to"
        );
        assert!(
            cx.debug_bounds(ROW_SELECTORS[10]).is_none(),
            "an item pushed while they read an older one is not taken as a reason to follow"
        );
    }

    /// A push into a full list that draws its newest item last leaves a reader who is not at the
    /// end where they put themselves: the oldest item leaving takes a place out in front of what
    /// they are reading, which is drawn one place higher for it.
    #[gpui_kit::test]
    fn a_push_should_leave_a_reader_who_is_not_at_the_end_where_they_are(cx: &mut TestAppContext) {
        let mut list = DequeSizedList::new("list".into(), ROWS, row).keep_on_newest();
        for item in 0..ROWS {
            list.push((item, ROW_HEIGHT));
        }
        let (list, cx) = harness(cx, list);

        list.update(cx, |list, _| {
            list.scroll.scroll_to_item_strict(4, ScrollStrategy::Top);
        });
        cx.update(|window, _| window.refresh());
        // The reader is on the row after the one the list opens on, so the place the oldest item
        // leaves is one the viewport they are reading from still holds.
        let reading = cx
            .debug_bounds(ROW_SELECTORS[5])
            .expect("the reader is reading the sixth item");
        let scrolled = list.read_with(cx, |list, _| offset(list));

        list.update(cx, |list, _| list.push((ROWS, ROW_HEIGHT)));
        cx.update(|window, _| window.refresh());

        assert_eq!(
            list.read_with(cx, |list, _| offset(list)),
            scrolled,
            "the list is not followed to the end the reader is not at"
        );
        let still = cx
            .debug_bounds(ROW_SELECTORS[5])
            .expect("the item they were reading is still drawn");
        assert_eq!(
            still.top(),
            reading.top() - px(ROW_HEIGHT),
            "the oldest item leaving took a place out in front of what they are reading"
        );
        assert!(
            cx.debug_bounds(ROW_SELECTORS[ROWS]).is_none(),
            "the item just pushed is below the fold the reader is at"
        );
    }

    /// A list that draws its newest item first has no tail to follow: a push into one leaves a
    /// reader who is not on the newest item where they are, and the place the item just pushed
    /// took is one their reading moves down by.
    #[gpui_kit::test]
    fn a_reversed_push_should_leave_a_reader_who_is_not_on_the_newest_item_where_they_are(
        cx: &mut TestAppContext,
    ) {
        let mut list = DequeSizedList::new("list".into(), ROWS, row)
            .render_reverse()
            .keep_on_newest();
        for item in 0..ROWS {
            list.push((item, ROW_HEIGHT));
        }
        let (list, cx) = harness(cx, list);

        list.update(cx, |list, _| {
            list.scroll.scroll_to_item_strict(3, ScrollStrategy::Top);
        });
        cx.update(|window, _| window.refresh());
        // The items are held newest first, so the fourth place holds the seventh item pushed,
        // and there are three more of them below it for the reader to be reading from.
        let reading = cx
            .debug_bounds(ROW_SELECTORS[6])
            .expect("the reader is reading an older item");
        let scrolled = list.read_with(cx, |list, _| offset(list));

        list.update(cx, |list, _| list.push((ROWS, ROW_HEIGHT)));
        cx.update(|window, _| window.refresh());

        assert_eq!(
            list.read_with(cx, |list, _| offset(list)),
            scrolled,
            "the top is not where this reader is, so the list is not put back on it"
        );
        let still = cx
            .debug_bounds(ROW_SELECTORS[6])
            .expect("the item they were reading is still drawn");
        assert_eq!(
            still.top(),
            reading.top() + px(ROW_HEIGHT),
            "the item just pushed took the place in front of what they are reading"
        );
        assert!(
            cx.debug_bounds(ROW_SELECTORS[ROWS]).is_none(),
            "the item just pushed is above the fold they are at"
        );
    }

    /// A reader who is on the newest item of a list that draws it first is the one reader
    /// keeping on the newest speaks for: the item just pushed lands where they are looking.
    #[gpui_kit::test]
    fn a_reversed_push_should_keep_a_reader_on_the_newest_item_on_it(cx: &mut TestAppContext) {
        let mut list = DequeSizedList::new("list".into(), ROWS, row)
            .render_reverse()
            .keep_on_newest();
        for item in 0..ROWS {
            list.push((item, ROW_HEIGHT));
        }
        let (list, cx) = harness(cx, list);

        assert!(
            cx.debug_bounds(ROW_SELECTORS[ROWS - 1]).is_some(),
            "the newest item is the one the list opens on"
        );

        list.update(cx, |list, _| list.push((ROWS, ROW_HEIGHT)));
        cx.update(|window, _| window.refresh());

        assert_eq!(
            list.read_with(cx, |list, _| offset(list)),
            Pixels::ZERO,
            "a reader who was on the newest item is put back on the top"
        );
        assert!(
            cx.debug_bounds(ROW_SELECTORS[ROWS]).is_some(),
            "the item just pushed is the one the reader is looking at"
        );
        assert!(
            cx.debug_bounds(ROW_SELECTORS[0]).is_none(),
            "and the oldest item is the one a full list dropped"
        );
    }
}
