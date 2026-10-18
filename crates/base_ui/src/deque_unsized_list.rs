use std::{collections::VecDeque, rc::Rc};

use gpui_kit::{
    AnyElement, App, Context, ElementId, Empty, FollowMode, InteractiveElement, IntoElement,
    ListAlignment, ListOffset, ListState, ParentElement, Pixels, Render, StyleRefinement, Styled,
    Window,
    base::{ScrollbarMode, StyledExt},
    div, list, px,
};

use crate::scroll_bar::Scrollbar;

/// A list that holds the newest `capacity` items and measures every place it draws, so the
/// render closure decides how tall an item is.
pub struct DequeUnsizedList<T> {
    id: ElementId,
    capacity: usize,
    list_state: ListState,
    separator: Option<Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>>,
    items: VecDeque<T>,
    render: Rc<dyn Fn(usize, &T, &mut Window, &mut App) -> AnyElement>,
    footer: Option<Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>>,
    render_reverse: bool,
    keep_on_newest: bool,
    style: StyleRefinement,
}

impl<T> DequeUnsizedList<T> {
    /// A list holding nothing yet, drawing each item through `render`.
    ///
    /// The closure is given the item's place among the items, which is the order they are
    /// drawn in rather than the index of a place: a separator is not an item, so a row that
    /// names itself after what it draws has one name per item.
    ///
    /// `overdraw` is how far above and below the viewport the list measures the places it
    /// cannot see, which is what keeps a place from appearing a moment after it was scrolled
    /// to. What it should be belongs to whoever builds the list: measuring costs what a row is
    /// made of, so a row of text wants far less of it than a row of pictures.
    pub fn new(
        id: ElementId,
        align: ListAlignment,
        capacity: usize,
        overdraw: Pixels,
        render: impl Fn(usize, &T, &mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        DequeUnsizedList {
            id,
            capacity,
            list_state: ListState::new(0, align, overdraw).measure_all(),
            separator: None,
            items: VecDeque::with_capacity(capacity),
            render: Rc::new(render),
            footer: None,
            render_reverse: false,
            keep_on_newest: false,
            style: StyleRefinement::default(),
        }
    }

    /// Draws what `separator` builds between two neighbouring items.
    ///
    /// The separator is a place of its own between the two items rather than a border on
    /// either of them, so an item never has to draw the gap in front of it and the list
    /// draws one separator per pair, and none at either end. What it is drawn as is the
    /// caller's decision, and it is built by a closure rather than handed over as an element
    /// because an element is drawn once, while the same separator is drawn again on every
    /// frame its pair is in.
    pub fn separator(
        mut self,
        separator: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        // Two places per item after the first: the item and its separator.
        self.items = VecDeque::with_capacity(self.capacity.saturating_mul(2).saturating_sub(1));
        self.separator = Some(Rc::new(separator));
        self
    }

    /// Draws one more place after the last item, which is never an item itself.
    ///
    /// The footer is not counted against the capacity and no push drops it, so it follows
    /// whichever item the list draws last. It is built by a closure rather than handed over
    /// as an element because an element is drawn once, while the same footer is drawn again
    /// on every frame its end of the list is in.
    pub fn footer(
        mut self,
        footer: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        // Its place is added here rather than taken when it is drawn, so a list with a footer
        // holds one place more than it holds items from the start.
        self.list_state
            .splice(self.item_places()..self.item_places(), 1);
        self.footer = Some(Rc::new(footer));
        self
    }

    /// Draws the newest item first instead of last.
    ///
    /// Which end the newest item is at is what [`Self::keep_on_newest`] follows, so the two
    /// settle together whichever order they are asked for.
    pub fn render_reverse(mut self) -> Self {
        self.render_reverse = true;
        self.apply_keep_on_newest();
        self
    }

    /// Keeps the newest item in view as items are pushed.
    ///
    /// A list that draws its newest item last follows its tail, and follows it again once the
    /// reader scrolls back to it. A list that draws its newest item first has no tail to
    /// follow — the item just pushed is the first one — so it goes back to the top instead,
    /// but only for a reader who is on the newest item already: one who went to read older
    /// items is left where they are, the way following a tail leaves them.
    pub fn keep_on_newest(mut self) -> Self {
        self.keep_on_newest = true;
        self.apply_keep_on_newest();
        self
    }

    /// Puts the list on the newest item the way a list like this can.
    ///
    /// Following the tail is the part of it the list can hold on to between pushes, so it is
    /// settled here; going back to the top needs a push to happen, so it is done by
    /// [`Self::push`].
    fn apply_keep_on_newest(&mut self) {
        let follow = if self.keep_on_newest && !self.render_reverse {
            FollowMode::Tail
        } else {
            FollowMode::Normal
        };

        self.list_state.set_follow_mode(follow);
    }

    /// The places the items take, which is where a footer follows them.
    fn item_places(&self) -> usize {
        if self.separator.is_some() {
            self.items.len().saturating_mul(2).saturating_sub(1)
        } else {
            self.items.len()
        }
    }

    /// Adds one item, dropping the oldest one once `capacity` items are held.
    ///
    /// The list holds one place per item, and two for every item after the first when it
    /// draws a separator between them: the item's own place and the separator in front of it.
    /// A push is reported as the insert and the removal it is, so a place that keeps its item
    /// keeps its measured height, a place that leaves takes its height with it, and a reader
    /// stays on the item they were reading.
    pub fn push(&mut self, data: T) {
        let full = self.items.len() == self.capacity;
        // The first item has nothing in front of it to be separated from.
        let places = if self.separator.is_some() && !self.items.is_empty() {
            2
        } else {
            1
        };

        if self.render_reverse {
            // A reader who is on the newest item stays on it; one who went to read older items
            // keeps the item they are on. Place indexes move as the places below are inserted,
            // so what they are on now is read before that happens.
            let top = self.list_state.logical_scroll_top();
            let on_newest = self.keep_on_newest && top.item_ix == 0 && top.offset_in_item == px(0.);

            // The newest item is the one drawn first, so it goes in at the front and every
            // place after it moves down together with the item it belongs to.
            if full {
                self.items.pop_back();
            }

            self.items.push_front(data);
            self.list_state.splice(0..0, places);

            if full {
                // The dropped item and the separator in front of it are the last item places,
                // which the footer follows.
                let end = self.list_state.item_count() - usize::from(self.footer.is_some());
                self.list_state.splice(end - places..end, 0);
            }

            if on_newest {
                // There is no tail to follow on a list that draws its newest item first, so
                // the top is where the reader is put back, on the item just pushed.
                self.list_state.scroll_to(ListOffset {
                    item_ix: 0,
                    offset_in_item: px(0.),
                });
            }
        } else {
            if full {
                // The dropped item and the separator behind it are the first places.
                self.items.pop_front();
                self.list_state.splice(0..places, 0);
            }

            // The newest item is the one drawn last, so it goes in after every item, which is
            // in front of the footer rather than at the end of the list.
            let index = self.item_places();
            self.items.push_back(data);
            self.list_state.splice(index..index, places);
        }
    }
}

impl<T> Styled for DequeUnsizedList<T> {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl<T> Render for DequeUnsizedList<T>
where
    T: 'static,
{
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(self.id.clone())
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(
                list(
                    self.list_state.clone(),
                    cx.processor(|this, index, window, cx| {
                        // The one place after the items holds the footer.
                        if index >= this.item_places() {
                            return this.footer.as_ref().map_or_else(
                                || Empty.into_any_element(),
                                |footer| footer(window, cx),
                            );
                        }

                        // A list that draws separators holds one in every odd place, so the
                        // even ones hold the item it stands between.
                        let Some(separator) = this.separator.as_ref() else {
                            return (this.render)(index, &this.items[index], window, cx);
                        };

                        if index % 2 == 1 {
                            return separator(window, cx);
                        }

                        (this.render)(index / 2, &this.items[index / 2], window, cx)
                    }),
                )
                .size_full(),
            )
            .child(Scrollbar::vertical(self.list_state.clone()).mode(ScrollbarMode::Always))
            .refine_style(&self.style)
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{
        AnyElement, App, AppContext, Context, Empty, Entity, InteractiveElement, IntoElement,
        ListAlignment, ListOffset, ParentElement, Pixels, Render, Styled, TestAppContext,
        VisualTestContext, Window, div, px,
    };

    use super::DequeUnsizedList;
    use crate::style;

    /// How far above and below the viewport the lists these tests build measure, which nothing
    /// under test depends on.
    const OVERDRAW: Pixels = px(100.);

    /// A row is named after the item it draws, so a test can ask for the bounds of one row
    /// instead of whichever row happened to be drawn last.
    const ROW_SELECTORS: [&str; 4] = [
        "TEST_DEQUE_ROW_0",
        "TEST_DEQUE_ROW_1",
        "TEST_DEQUE_ROW_2",
        "TEST_DEQUE_ROW_3",
    ];

    /// The mark a list draws after its last item.
    const FOOTER_SELECTOR: &str = "TEST_DEQUE_FOOTER";

    /// The mark a list draws between two items, and how tall it is: what a separator looks
    /// like is the caller's decision, so a test makes one it can ask the bounds of.
    const SEPARATOR_SELECTOR: &str = "TEST_DEQUE_SEPARATOR";
    const SEPARATOR_HEIGHT: Pixels = px(3.);

    /// A window holding nothing but the list.
    struct Harness {
        list: Entity<DequeUnsizedList<(usize, f32)>>,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.list.clone())
        }
    }

    /// An item is drawn as tall as the height it carries, which is the render closure's
    /// decision and not something the list is told.
    fn row(
        _index: usize,
        (row, height): &(usize, f32),
        _window: &mut Window,
        _cx: &mut App,
    ) -> AnyElement {
        div()
            .debug_selector({
                let selector = ROW_SELECTORS[*row];
                move || String::from(selector)
            })
            .w_full()
            .h(px(*height))
            .into_any_element()
    }

    /// The mark a list draws after its last item.
    fn footer(_window: &mut Window, _cx: &mut App) -> AnyElement {
        div()
            .debug_selector(|| String::from(FOOTER_SELECTOR))
            .w_full()
            .h(px(10.))
            .into_any_element()
    }

    /// What a list that was given one draws between two items, which is a place of its own
    /// rather than something either item draws.
    fn row_separator(_window: &mut Window, _cx: &mut App) -> AnyElement {
        div()
            .debug_selector(|| String::from(SEPARATOR_SELECTOR))
            .w_full()
            .h(SEPARATOR_HEIGHT)
            .into_any_element()
    }

    /// A window holding this list and nothing else, drawn once.
    fn harness(
        cx: &mut TestAppContext,
        list: DequeUnsizedList<(usize, f32)>,
    ) -> (
        Entity<DequeUnsizedList<(usize, f32)>>,
        &mut VisualTestContext,
    ) {
        // The scrollbar the list draws under itself reads its colors from the style.
        cx.update(style::theme::install);
        let list = cx.new(|_| list);
        let (_, cx) = cx.add_window_view(|_, _| Harness { list: list.clone() });

        (list, cx)
    }

    /// An empty list that draws nothing, for the tests that only read back what the list
    /// holds.
    fn empty_list(reverse: bool, separated: bool) -> DequeUnsizedList<(usize, f32)> {
        let mut list = DequeUnsizedList::new(
            "list".into(),
            ListAlignment::Top,
            3,
            OVERDRAW,
            |_, _, _, _| Empty.into_any_element(),
        );

        if reverse {
            list = list.render_reverse();
        }
        if separated {
            list = list.separator(row_separator);
        }

        list
    }

    /// Every item has a place, every separator has one, and the item that leaves the deque
    /// takes its places with it, whichever way the list grows.
    #[test]
    fn pushing_should_leave_one_place_per_item_and_drop_the_oldest() {
        for reverse in [false, true] {
            for separated in [false, true] {
                let mut list = empty_list(reverse, separated);

                for item in 0..5 {
                    list.push((item, 20.));

                    let expected = if separated {
                        list.items.len() * 2 - 1
                    } else {
                        list.items.len()
                    };
                    assert_eq!(
                        list.list_state.item_count(),
                        expected,
                        "the list holds a place per item and per separator ({reverse}, {separated})"
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
    }

    /// A list that is full moves its items along with their places, so the item a reader is
    /// on stays the item they are reading rather than whatever moves into its index.
    #[test]
    fn a_full_list_should_keep_the_reader_on_the_item_they_are_reading() {
        for reverse in [false, true] {
            let mut list = empty_list(reverse, false);

            for item in 0..3 {
                list.push((item, 20.));
            }
            // The reader is on the item in the middle.
            list.list_state.scroll_to(ListOffset {
                item_ix: 1,
                offset_in_item: px(0.),
            });

            list.push((3, 20.));

            assert_eq!(
                list.list_state.logical_scroll_top().item_ix,
                if reverse { 2 } else { 0 },
                "the reader keeps the item they were reading ({reverse})"
            );
        }
    }

    /// A list that draws its newest item last keeps it in view by following its tail, the way
    /// the framework follows one, so a push does not drag a reader who scrolled away back.
    #[test]
    fn keeping_on_the_newest_should_follow_the_tail_of_a_list_that_draws_it_last() {
        let mut list = empty_list(false, false).keep_on_newest();

        assert!(list.list_state.is_following_tail());

        for item in 0..5 {
            list.push((item, 20.));
        }
        assert_eq!(
            list.list_state.logical_scroll_top().item_ix,
            list.list_state.item_count(),
            "the list stays on its last place"
        );

        list.list_state.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: px(0.),
        });
        list.push((5, 20.));

        assert!(
            !list.list_state.is_following_tail(),
            "a push does not take the reader back to the tail they scrolled away from"
        );
    }

    /// A list that draws its newest item first has no tail to follow, so it is being on the
    /// newest item that a push keeps, whichever order the two choices were asked for in.
    #[test]
    fn keeping_on_the_newest_should_keep_a_reader_who_is_on_it_on_a_reversed_list() {
        for reverse_first in [false, true] {
            let mut list = empty_list(false, false);
            list = if reverse_first {
                list.render_reverse().keep_on_newest()
            } else {
                list.keep_on_newest().render_reverse()
            };

            assert!(!list.list_state.is_following_tail());

            for item in 0..3 {
                list.push((item, 20.));

                assert_eq!(
                    list.list_state.logical_scroll_top().item_ix,
                    0,
                    "a reader on the newest item is kept on it ({reverse_first})"
                );
            }

            // A reader who went to an older item is left where they are: what the push moves
            // is the places, not the reader.
            list.list_state.scroll_to(ListOffset {
                item_ix: 1,
                offset_in_item: px(0.),
            });
            list.push((3, 20.));

            assert_eq!(
                list.list_state.logical_scroll_top().item_ix,
                2,
                "the item they were reading moved down one place ({reverse_first})"
            );
        }
    }

    /// A push that drops the oldest item drops the places it held, and the footer is not one of
    /// the items the capacity counts.
    #[test]
    fn pushing_should_leave_the_footer_where_the_last_item_leaves_it() {
        let mut list = empty_list(true, true).footer(footer);
        let footer_places = 1;

        for item in 0..3 {
            list.push((item, 20.));
        }
        assert_eq!(
            list.list_state.item_count(),
            3 * 2 - 1 + footer_places,
            "a place per item and per separator, and one for the footer"
        );

        for item in 3..7 {
            list.push((item, 20.));
            assert_eq!(
                list.list_state.item_count(),
                3 * 2 - 1 + footer_places,
                "a push that drops the oldest item drops its places rather than the footer's"
            );
        }

        let kept: Vec<usize> = list.items.iter().map(|(item, _)| *item).collect();
        assert_eq!(kept, [6, 5, 4], "and the list holds its newest items");
    }

    /// What a pushed item is drawn as is the render closure's decision, height included.
    #[gpui_kit::test]
    fn a_pushed_item_should_be_drawn_as_tall_as_its_render_makes_it(cx: &mut TestAppContext) {
        let mut list = DequeUnsizedList::new("list".into(), ListAlignment::Top, 3, OVERDRAW, row);
        list.push((0, 20.));
        list.push((1, 60.));
        let (_, cx) = harness(cx, list);

        let first = cx
            .debug_bounds(ROW_SELECTORS[0])
            .expect("the first item is drawn");
        let second = cx
            .debug_bounds(ROW_SELECTORS[1])
            .expect("the second item is drawn");

        assert_eq!(first.top(), px(0.));
        assert_eq!(first.size.height, px(20.));
        assert_eq!(second.top(), px(20.));
        assert_eq!(second.size.height, px(60.));
    }

    /// Rendering in reverse draws the newest item first, and the item that leaves takes its
    /// height with it.
    #[gpui_kit::test]
    fn the_newest_item_should_be_drawn_first_when_the_list_renders_in_reverse(
        cx: &mut TestAppContext,
    ) {
        let mut list = DequeUnsizedList::new("list".into(), ListAlignment::Top, 3, OVERDRAW, row)
            .render_reverse();
        for item in 0..4 {
            list.push((item, 20. * (item + 1) as f32));
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

        assert_eq!(newest.top(), px(0.), "the newest item is drawn first");
        assert_eq!(newest.size.height, px(80.));
        assert_eq!(next.top(), px(80.));
        assert_eq!(next.size.height, px(60.));
    }

    /// The item that leaves the deque frees its place, and the items that stay keep the
    /// height they were measured at.
    #[gpui_kit::test]
    fn dropping_the_oldest_item_should_keep_the_other_places(cx: &mut TestAppContext) {
        let mut list = DequeUnsizedList::new("list".into(), ListAlignment::Top, 3, OVERDRAW, row);
        for item in 0..4 {
            list.push((item, 20. * (item + 1) as f32));
        }
        let (_, cx) = harness(cx, list);

        assert!(
            cx.debug_bounds(ROW_SELECTORS[0]).is_none(),
            "the oldest item is no longer drawn"
        );

        let second = cx
            .debug_bounds(ROW_SELECTORS[1])
            .expect("the second item is drawn");
        let fourth = cx
            .debug_bounds(ROW_SELECTORS[3])
            .expect("the newest item is drawn");

        assert_eq!(second.size.height, px(40.));
        assert_eq!(
            second.top(),
            px(0.),
            "the oldest kept item moved to the top"
        );
        assert_eq!(fourth.size.height, px(80.));
        assert_eq!(fourth.top(), px(100.), "the kept items are still stacked");
    }

    /// An item that draws a separator between two of them draws the caller's own element,
    /// which is a place of its own rather than something either item draws.
    #[gpui_kit::test]
    fn a_separated_list_should_draw_the_separator_it_was_given(cx: &mut TestAppContext) {
        let mut list = DequeUnsizedList::new("list".into(), ListAlignment::Top, 3, OVERDRAW, row)
            .separator(row_separator);
        list.push((0, 20.));
        list.push((1, 60.));
        let (_, cx) = harness(cx, list);

        let first = cx
            .debug_bounds(ROW_SELECTORS[0])
            .expect("the first item is drawn");
        let separator = cx
            .debug_bounds(SEPARATOR_SELECTOR)
            .expect("the separator the list was given is drawn");
        let second = cx
            .debug_bounds(ROW_SELECTORS[1])
            .expect("the second item is drawn");

        assert_eq!(
            first.top(),
            px(0.),
            "the list draws no separator in front of the first item"
        );
        assert_eq!(first.size.height, px(20.));
        assert_eq!(
            separator.top(),
            px(20.),
            "the separator follows the item it stands after"
        );
        assert_eq!(separator.size.height, SEPARATOR_HEIGHT);
        assert_eq!(
            second.top(),
            px(23.),
            "one separator stands between the two items"
        );
        assert_eq!(second.size.height, px(60.));
    }

    /// A footer follows the last item a list draws, which for a list that draws its newest
    /// item first is its oldest one.
    #[gpui_kit::test]
    fn a_footer_should_be_drawn_after_the_last_item(cx: &mut TestAppContext) {
        let mut list = DequeUnsizedList::new("list".into(), ListAlignment::Top, 2, OVERDRAW, row)
            .separator(row_separator)
            .render_reverse()
            .footer(footer);
        for item in 0..4 {
            list.push((item, 20.));
        }
        let (_, cx) = harness(cx, list);

        assert!(
            cx.debug_bounds(ROW_SELECTORS[0]).is_none(),
            "what a full list drops is its oldest item rather than its footer"
        );

        let oldest = cx
            .debug_bounds(ROW_SELECTORS[2])
            .expect("the oldest kept item is drawn");
        let end = cx
            .debug_bounds(FOOTER_SELECTOR)
            .expect("the footer is drawn");

        assert_eq!(oldest.top(), px(23.));
        assert_eq!(
            end.top(),
            px(43.),
            "the footer follows the last item rather than standing between two of them"
        );
    }
}
