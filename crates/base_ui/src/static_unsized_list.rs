use std::rc::Rc;

use gpui_kit::{
    AnyElement, App, Context, ElementId, Empty, InteractiveElement, IntoElement, ListAlignment,
    ListState, ParentElement, Pixels, Render, StyleRefinement, Styled, Window,
    base::{ScrollbarMode, StyledExt},
    div, list,
};

use crate::scroll_bar::Scrollbar;

/// A list that draws the items it is given, measuring every place it draws.
///
/// What it draws is a set rather than a feed: it is given the items of a catalogue, of a
/// filter's answer or of an account's rows, all at once, which is a set whose length is known
/// before it is drawn — a feed that grows at one end is what
/// [`DequeUnsizedList`](crate::deque_unsized_list::DequeUnsizedList) is for. The height of a
/// row is the render closure's decision, so the places of this list are measured one by one;
/// rows that are all one size are what [`StaticSizedList`](crate::static_sized_list::StaticSizedList)
/// is for.
pub struct StaticUnsizedList<T> {
    id: ElementId,
    list_state: ListState,
    separator: Option<Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>>,
    items: Vec<T>,
    render: Rc<dyn Fn(usize, &T, &mut Window, &mut App) -> AnyElement>,
    footer: Option<Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>>,
    style: StyleRefinement,
}

impl<T> StaticUnsizedList<T> {
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
        overdraw: Pixels,
        render: impl Fn(usize, &T, &mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        StaticUnsizedList {
            id,
            list_state: ListState::new(0, align, overdraw).measure_all(),
            separator: None,
            items: Vec::new(),
            render: Rc::new(render),
            footer: None,
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
        self.separator = Some(Rc::new(separator));
        self
    }

    /// Draws one more place after the last item, which is never an item itself.
    ///
    /// The footer is not counted among the items and no set of items drops it, so it follows
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

    /// The places the items take, which is where a footer follows them.
    fn item_places(&self) -> usize {
        if self.separator.is_some() {
            self.items.len().saturating_mul(2).saturating_sub(1)
        } else {
            self.items.len()
        }
    }

    /// Holds exactly the items given, in the order the list draws them, and drops the ones it
    /// held.
    ///
    /// Nothing is cut and nothing is kept: what the list draws is what it was given, because a
    /// set that is known before it is drawn has no end to grow at and no capacity to stay
    /// within. What it held is gone along with the reader's place in it and with what was
    /// measured of it, so a reader starts over on the item the list draws first rather than
    /// staying on a place that no longer holds what they were reading.
    pub fn replace(&mut self, items: impl IntoIterator<Item = T>) {
        self.items = items.into_iter().collect();
        self.list_state
            .reset(self.item_places() + usize::from(self.footer.is_some()));
    }
}

impl<T> Styled for StaticUnsizedList<T> {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl<T> Render for StaticUnsizedList<T>
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
            .overflow_y_hidden()
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
        ListAlignment, ParentElement, Pixels, Render, Styled, TestAppContext, VisualTestContext,
        Window, div, px,
    };

    use super::StaticUnsizedList;
    use crate::style;

    /// How far above and below the viewport the lists these tests build measure, which nothing
    /// under test depends on.
    const OVERDRAW: Pixels = px(100.);

    /// A row is named after the item it draws, so a test can ask for the bounds of one row
    /// instead of whichever row happened to be drawn last.
    const ROW_SELECTORS: [&str; 3] = [
        "TEST_STATIC_UNSIZED_ROW_0",
        "TEST_STATIC_UNSIZED_ROW_1",
        "TEST_STATIC_UNSIZED_ROW_2",
    ];

    /// The mark a list draws after its last item.
    const FOOTER_SELECTOR: &str = "TEST_STATIC_UNSIZED_FOOTER";

    /// The mark a list draws between two items, and how tall it is: what a separator looks
    /// like is the caller's decision, so a test makes one it can ask the bounds of.
    const SEPARATOR_SELECTOR: &str = "TEST_STATIC_UNSIZED_SEPARATOR";
    const SEPARATOR_HEIGHT: Pixels = px(3.);

    /// A window holding nothing but the list.
    struct Harness {
        list: Entity<StaticUnsizedList<(usize, f32)>>,
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
        list: StaticUnsizedList<(usize, f32)>,
    ) -> (
        Entity<StaticUnsizedList<(usize, f32)>>,
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
    fn empty_list(separated: bool) -> StaticUnsizedList<(usize, f32)> {
        let mut list =
            StaticUnsizedList::new("list".into(), ListAlignment::Top, OVERDRAW, |_, _, _, _| {
                Empty.into_any_element()
            });

        if separated {
            list = list.separator(row_separator);
        }

        list
    }

    /// Every item has a place, every separator has one, and the footer keeps its own, however
    /// many items the list is given.
    #[test]
    fn replacing_should_leave_one_place_per_item_and_keep_the_footers() {
        for separated in [false, true] {
            let mut list = empty_list(separated).footer(footer);
            let places = |items: usize| {
                let item_places = if separated {
                    (items * 2).saturating_sub(1)
                } else {
                    items
                };
                item_places + 1
            };

            for items in 1..5 {
                list.replace((0..items).map(|item| (item, 20.)));

                assert_eq!(
                    list.items.len(),
                    items,
                    "a list holds every item it was given ({separated})"
                );
                assert_eq!(
                    list.list_state.item_count(),
                    places(items),
                    "a place per item, per separator and for the footer ({separated})"
                );
            }

            list.replace([]);

            assert!(list.items.is_empty());
            assert_eq!(
                list.list_state.item_count(),
                1,
                "a list given nothing holds nothing but its footer ({separated})"
            );
        }
    }

    /// What a row is drawn as is the render closure's decision, height included, which is what
    /// makes this the list for rows that differ in height.
    #[gpui_kit::test]
    fn a_row_should_be_drawn_as_tall_as_its_render_makes_it(cx: &mut TestAppContext) {
        let mut list = StaticUnsizedList::new("list".into(), ListAlignment::Top, OVERDRAW, row);
        list.replace([(0, 20.), (1, 60.)]);
        let (_, cx) = harness(cx, list);

        let first = cx
            .debug_bounds(ROW_SELECTORS[0])
            .expect("the first item is drawn");
        let second = cx
            .debug_bounds(ROW_SELECTORS[1])
            .expect("the second item is drawn");

        assert_eq!(first.size.height, px(20.));
        assert_eq!(second.top() - first.top(), px(20.));
        assert_eq!(second.size.height, px(60.));
    }

    /// An item that draws a separator between two of them draws the caller's own element,
    /// which is a place of its own rather than something either item draws.
    #[gpui_kit::test]
    fn a_separated_list_should_draw_the_separator_it_was_given(cx: &mut TestAppContext) {
        let mut list = StaticUnsizedList::new("list".into(), ListAlignment::Top, OVERDRAW, row)
            .separator(row_separator);
        list.replace([(0, 20.), (1, 60.)]);
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

        assert_eq!(first.size.height, px(20.));
        assert_eq!(separator.top() - first.top(), px(20.));
        assert_eq!(separator.size.height, SEPARATOR_HEIGHT);
        assert_eq!(
            second.top() - first.top(),
            px(23.),
            "one separator stands between the two items"
        );
    }

    /// A footer follows the last item the list draws, which is where the set it was given
    /// ends.
    #[gpui_kit::test]
    fn a_footer_should_be_drawn_after_the_last_item(cx: &mut TestAppContext) {
        let mut list = StaticUnsizedList::new("list".into(), ListAlignment::Top, OVERDRAW, row)
            .separator(row_separator)
            .footer(footer);
        list.replace([(0, 20.), (1, 20.)]);
        let (_, cx) = harness(cx, list);

        let second = cx
            .debug_bounds(ROW_SELECTORS[1])
            .expect("the last item is drawn");
        let end = cx
            .debug_bounds(FOOTER_SELECTOR)
            .expect("the footer is drawn");

        assert_eq!(
            end.top() - second.top(),
            px(20.),
            "the footer follows the last item rather than standing between two of them"
        );
    }

    /// What the list draws is the set it was given last, so a list that is given another set
    /// draws that one.
    #[gpui_kit::test]
    fn replacing_the_items_should_draw_the_ones_the_list_was_given(cx: &mut TestAppContext) {
        let mut list = StaticUnsizedList::new("list".into(), ListAlignment::Top, OVERDRAW, row);
        list.replace([(0, 20.), (1, 20.)]);
        let (list, cx) = harness(cx, list);

        assert!(
            cx.debug_bounds(ROW_SELECTORS[0]).is_some(),
            "the items the list was given are drawn"
        );

        list.update(cx, |list, _| {
            list.replace([(2, 20.)]);
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
