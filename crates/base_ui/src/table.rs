use gpui_kit::{
    base::{Align, StyledExt},
    prelude::FluentBuilder,
    *,
};
use smol_str::ToSmolStr;

use crate::{scroll_bar::Scrollbar, size::UnifiedSize, style::theme::ThemeExt};

#[derive(IntoElement)]
pub struct Table<D, const COLUMNS: usize>
where
    D: TableDataSet<COLUMNS> + 'static,
{
    id: ElementId,
    size: UnifiedSize,
    dataset: Entity<D>,
}

impl<D, const COLUMNS: usize> Table<D, COLUMNS>
where
    D: TableDataSet<COLUMNS> + 'static,
{
    pub fn new(id: impl Into<ElementId>, dataset: &Entity<D>) -> Self {
        Self {
            id: id.into(),
            size: UnifiedSize::Medium,
            dataset: dataset.clone(),
        }
    }
}

impl<D, const COLUMNS: usize> RenderOnce for Table<D, COLUMNS>
where
    D: TableDataSet<COLUMNS> + 'static,
{
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let Self { id, size, dataset } = self;

        let scroll_handle = window.use_keyed_state((id.clone(), "scroll"), cx, |_, _| {
            UniformListScrollHandle::new()
        });

        let theme = cx.theme();

        let (rows_count, headers, column_aligns, column_widths) = {
            let dataset = dataset.read(cx);
            (
                dataset.rows_count(),
                dataset.headers(cx),
                dataset.column_aligns(),
                dataset.column_widths(),
            )
        };

        let header = {
            let id: ElementId = (id.clone(), "header").into();
            gpui_kit::base::TableHeader::new(id.clone()).child({
                let id: ElementId = (id, "row").into();
                gpui_kit::base::TableRow::new(id.clone(), 1)
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(size.gap())
                    .bg(theme.roles.surface.raised.hsla)
                    .border_b_1()
                    .border_color(theme.roles.line.divider.hsla)
                    .children(headers.into_iter().enumerate().map(|(index, header)| {
                        let id: ElementId = (id.clone(), index.to_smolstr().as_str()).into();
                        let align = column_aligns[index];
                        gpui_kit::base::TableHead::new(id, index)
                            .font_bold()
                            .text_size(size.text_size())
                            .h(size.h())
                            .px(size.px())
                            .w(column_widths[index])
                            .flex()
                            .items_center()
                            .text_color(theme.roles.fg.muted.hsla)
                            .when(matches!(align, Align::Start), |this| this.justify_start())
                            .when(matches!(align, Align::Center), |this| this.justify_center())
                            .when(matches!(align, Align::End), |this| this.justify_end())
                            .child(div().min_w_0().truncate().child(header))
                    }))
            })
        };

        // A dataset with no rows has no row to draw a cell in, so what it has to say about
        // being empty is one element rather than a cell per column.
        let empty = if rows_count == 0 {
            dataset.read(cx).empty(cx)
        } else {
            None
        };

        let body = {
            let id: ElementId = (id.clone(), "body").into();
            let body = gpui_kit::base::TableBody::new(id.clone())
                .relative()
                .overflow_hidden();

            match empty {
                Some(empty) => body.child(
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(empty),
                ),
                None => body
                    .child({
                        let id: ElementId = (id, "list").into();
                        gpui_kit::uniform_list(id.clone(), rows_count, {
                            move |range, _, cx| {
                                let theme = cx.theme();
                                let row_foreground = theme.roles.fg.default.hsla;
                                let row_divider = theme.roles.line.divider.hsla;
                                let row_hover = theme.roles.veil.hover.hsla;
                                let dataset = dataset.read(cx);
                                let range_last_index = range.end;
                                range
                                    .map(|row_index| {
                                        let id: ElementId =
                                            (id.clone(), row_index.to_smolstr().as_str()).into();
                                        let aligns = dataset.row_aligns(row_index);

                                        let mut cells = Vec::with_capacity(COLUMNS);
                                        let mut column_index = 0;
                                        while column_index < COLUMNS {
                                            // A cell that stands in for more than one column takes
                                            // their widths and their alignment, and the columns it
                                            // covers are not drawn at all: a row lays its cells out
                                            // at declared widths, so a cell spanning several of them
                                            // is a cell that asks for the room they would have taken.
                                            let span = dataset
                                                .column_span(row_index, column_index)
                                                .clamp(1, COLUMNS - column_index);
                                            let covered = (column_index + span).min(COLUMNS);
                                            let width = columns_width(
                                                &column_widths[column_index..covered],
                                            );
                                            let align = aligns[covered - 1];
                                            let cell_id: ElementId =
                                                (id.clone(), column_index.to_smolstr().as_str())
                                                    .into();

                                            cells.push(
                                                gpui_kit::base::TableCell::new(
                                                    cell_id,
                                                    column_index,
                                                )
                                                .h(size.h())
                                                .px(size.px())
                                                .text_size(size.text_size())
                                                .w(width)
                                                .flex()
                                                .items_center()
                                                .when(matches!(align, Align::Start), |this| {
                                                    this.justify_start()
                                                })
                                                .when(matches!(align, Align::Center), |this| {
                                                    this.justify_center()
                                                })
                                                .when(matches!(align, Align::End), |this| {
                                                    this.justify_end()
                                                })
                                                .child(
                                                    div()
                                                        .min_w_0()
                                                        .truncate()
                                                        .text_color(row_foreground)
                                                        .child(dataset.cell(
                                                            row_index,
                                                            column_index,
                                                            cx,
                                                        )),
                                                ),
                                            );
                                            column_index = covered;
                                        }

                                        gpui_kit::base::TableRow::new(id, row_index)
                                            .w_full()
                                            .flex()
                                            .flex_row()
                                            // §8.4: the row itself paints nothing, so a hovered row
                                            // is the neutral veil over the sunken face the table is
                                            // drawn on rather than a fill of its own.
                                            .hover(move |this| this.bg(row_hover))
                                            .when(
                                                row_index.saturating_add(1) != range_last_index,
                                                |this| this.border_b_1().border_color(row_divider),
                                            )
                                            .items_center()
                                            .gap(size.gap())
                                            .children(cells)
                                    })
                                    .collect()
                            }
                        })
                        .track_scroll(scroll_handle.read(cx))
                        .size_full()
                    })
                    // Nothing to list is nothing to scroll.
                    .child(Scrollbar::vertical(scroll_handle.read(cx).clone())),
            }
        };

        gpui_kit::base::Table::new(id)
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(header.flex_none())
            .child(body.flex_1())
    }
}

pub trait TableDataSet<const COLUMNS: usize> {
    fn rows_count(&self) -> usize;
    /// Returns the localized column headers, in column order.
    fn headers(&self, cx: &App) -> [SharedString; COLUMNS];
    fn column_aligns(&self) -> [Align; COLUMNS];
    fn column_widths(&self) -> [Length; COLUMNS];
    fn cell(&self, row_index: usize, column_index: usize, cx: &App) -> impl IntoElement;

    /// How each column of one row is aligned, in column order.
    ///
    /// The default is [`TableDataSet::column_aligns`], which is the alignment of the columns
    /// themselves: a table whose alignment is a property of its columns answers the same thing
    /// for every row. A dataset whose rows say different things in the same column — one row a
    /// number, another a sentence across several columns — answers per row, and a cell that
    /// stands in for several columns is aligned by the last of them.
    fn row_aligns(&self, _row_index: usize) -> [Align; COLUMNS] {
        self.column_aligns()
    }

    /// How many columns the cell at `column_index` of `row_index` stands in for.
    ///
    /// One — the default — is a cell of its own column. More than one makes it a cell that
    /// holds the columns it covers: it is drawn as wide as their widths together, aligned like
    /// the last of them, and none of the others is drawn. A span that reaches past the last
    /// column is read as reaching it, and a span of zero as one, so a dataset cannot describe a
    /// row whose cells do not add up. The headers are unaffected: a column is what a reader
    /// reads the values below it as, whatever one row merges.
    ///
    /// [`TableDataSet::cell`] is asked for the first column of a group and for no other, so a
    /// dataset that merges columns answers for the group in one cell. What the merged cell is
    /// aligned by is [`TableDataSet::row_aligns`], which is where a dataset says that one row's
    /// cells read differently from its columns.
    fn column_span(&self, _row_index: usize, _column_index: usize) -> usize {
        1
    }

    /// What the body draws instead of a row when [`TableDataSet::rows_count`] is zero, drawn
    /// centred across every column: a dataset with no rows has no column for it to belong to.
    ///
    /// `None` — the default — draws a body with nothing in it.
    fn empty(&self, _cx: &App) -> Option<AnyElement> {
        None
    }
}

/// The room one cell takes: the declared widths of the columns it stands in for, added up.
///
/// A cell that stands in for one column — every cell of a dataset that merges nothing —
/// keeps that column's own declaration, whatever kind it is: an `auto` column is as wide as
/// its content in the cell as it is in the column. Widths of one kind add up to that kind,
/// which is what makes a row of relative columns still fill a pane of any width. A row that
/// mixed the two kinds is one this cannot answer for, so it keeps the fractions it read, and
/// an `auto` among columns that state a width takes no room: a column whose width is its
/// content has no width to give away. Columns that all say `auto` say nothing to add up at
/// all, so the cell keeps `auto` and is as wide as what it holds.
fn columns_width(widths: &[Length]) -> Length {
    if let [width] = widths {
        return *width;
    }

    let mut fraction = 0.0;
    let mut absolute = None;
    let mut all_auto = true;
    for width in widths {
        match width {
            Length::Definite(DefiniteLength::Fraction(of_row)) => {
                fraction += of_row;
                all_auto = false;
            }
            Length::Definite(DefiniteLength::Absolute(length)) => {
                // Two absolute widths of a kind add up; two of different kinds cannot, and the
                // one read first is what the cell keeps.
                let total = match (absolute, length) {
                    (None, length) => *length,
                    (Some(total), length) => match (total, *length) {
                        (AbsoluteLength::Pixels(total), AbsoluteLength::Pixels(length)) => {
                            AbsoluteLength::Pixels(total + length)
                        }
                        (AbsoluteLength::Rems(total), AbsoluteLength::Rems(length)) => {
                            AbsoluteLength::Rems(total + length)
                        }
                        (total, _) => total,
                    },
                };
                absolute = Some(total);
                all_auto = false;
            }
            Length::Auto => {}
        }
    }

    if all_auto {
        return Length::Auto;
    }

    match absolute {
        Some(absolute) if fraction == 0.0 => Length::Definite(absolute.into()),
        _ => Length::Definite(relative(fraction)),
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{
        AbsoluteLength, AnyElement, App, AppContext as _, Context, DefiniteLength, Entity,
        InteractiveElement, IntoElement, Length, ParentElement, Render, SharedString, Styled,
        TestAppContext, VisualTestContext, Window, base::Align, div, px, relative,
    };

    use super::{Table, TableDataSet, columns_width};
    use crate::style;

    /// Where the harness draws the table, which is what a centred message is centred in.
    const AREA_SELECTOR: &str = "TEST_TABLE_AREA";

    /// What the body draws in place of the rows of a dataset that has none.
    const EMPTY_SELECTOR: &str = "TEST_TABLE_EMPTY";

    /// The bounds of the first cell of the first row, which is what a body with a row draws.
    const FIRST_CELL_SELECTOR: &str = "TEST_TABLE_CELL_0_0";

    /// A dataset whose rows are the test's to set, so the table can be drawn with none and
    /// with some, and whose first cell can be made to stand in for both columns.
    struct Rows {
        rows: usize,
        span: usize,
    }

    impl TableDataSet<2> for Rows {
        fn rows_count(&self) -> usize {
            self.rows
        }

        fn headers(&self, _cx: &App) -> [SharedString; 2] {
            [
                SharedString::new_static("first"),
                SharedString::new_static("second"),
            ]
        }

        fn column_aligns(&self) -> [Align; 2] {
            [Align::Start, Align::Start]
        }

        fn column_widths(&self) -> [Length; 2] {
            [relative(0.5).into(), relative(0.5).into()]
        }

        fn column_span(&self, _row_index: usize, column_index: usize) -> usize {
            if column_index == 0 { self.span } else { 1 }
        }

        fn cell(&self, row_index: usize, column_index: usize, _cx: &App) -> impl IntoElement {
            div()
                .debug_selector(move || format!("TEST_TABLE_CELL_{row_index}_{column_index}"))
                .child(SharedString::new(format!(
                    "cell {row_index}-{column_index}"
                )))
        }

        fn empty(&self, _cx: &App) -> Option<AnyElement> {
            Some(
                div()
                    .debug_selector(|| String::from(EMPTY_SELECTOR))
                    .child(SharedString::new_static("nothing to list"))
                    .into_any_element(),
            )
        }
    }

    /// A window holding this table and nothing else.
    struct Harness {
        dataset: Entity<Rows>,
    }

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .debug_selector(|| String::from(AREA_SELECTOR))
                .size_full()
                .child(Table::new("test-table", &self.dataset))
        }
    }

    /// A window holding the table over a dataset that has `rows` rows, drawn once.
    fn harness(cx: &mut TestAppContext, rows: usize) -> (Entity<Rows>, &mut VisualTestContext) {
        harness_with_span(cx, rows, 1)
    }

    /// A window holding the table over a dataset that has `rows` rows, whose first cell
    /// stands in for `span` columns, drawn once.
    fn harness_with_span(
        cx: &mut TestAppContext,
        rows: usize,
        span: usize,
    ) -> (Entity<Rows>, &mut VisualTestContext) {
        // The table reads its own colors, and so does the scrollbar of a body that lists.
        cx.update(style::theme::install);
        let dataset = cx.new(|_| Rows { rows, span });
        let (_, cx) = cx.add_window_view(|_, _| Harness {
            dataset: dataset.clone(),
        });

        (dataset, cx)
    }

    /// A dataset with no rows has no column to draw in, so the one thing it has to say is
    /// drawn across the whole body rather than in the first column of a row of its own.
    #[gpui_kit::test]
    fn an_empty_dataset_should_draw_its_message_across_the_body(cx: &mut TestAppContext) {
        let (_, cx) = harness(cx, 0);

        let message = cx
            .debug_bounds(EMPTY_SELECTOR)
            .expect("a dataset with no rows should draw the message it was given");
        let area = cx
            .debug_bounds(AREA_SELECTOR)
            .expect("the harness should draw the table it was given");
        let offset = (message.center().x - area.center().x).abs();

        assert!(
            offset < px(1.0),
            "the message belongs across the whole body rather than in one column: it is \
             {message:?} in {area:?}"
        );
        assert!(
            cx.debug_bounds(FIRST_CELL_SELECTOR).is_none(),
            "a dataset with no rows has no row for the body to draw cells in"
        );
    }

    /// The message is for the emptiness and nothing else: a row is what the body draws once
    /// the dataset has one.
    #[gpui_kit::test]
    fn a_row_should_take_the_place_of_the_message_when_the_dataset_gains_one(
        cx: &mut TestAppContext,
    ) {
        let (dataset, cx) = harness(cx, 0);
        assert!(
            cx.debug_bounds(EMPTY_SELECTOR).is_some(),
            "the dataset starts with no rows, which is what the message is drawn for"
        );

        dataset.update(cx, |rows, cx| {
            rows.rows = 1;
            cx.notify();
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(FIRST_CELL_SELECTOR).is_some(),
            "the row the dataset gained should be drawn in the body"
        );
        assert!(
            cx.debug_bounds(EMPTY_SELECTOR).is_none(),
            "a dataset with a row must not draw the message it draws with none"
        );
    }

    /// A cell that stands in for several columns is the only cell drawn across them: the
    /// columns it covers have no cell of their own, and the columns around it keep theirs.
    ///
    /// Whether the one cell is drawn as wide as the columns it stands in for is what
    /// [`Table::render`] asks [`TableDataSet::column_widths`] for, and it is not readable
    /// from a debug selector: a selector reports the bounds of the element, and an element
    /// whose width is its own content reports that content's size rather than the box the row
    /// would have given it — the widths are pinned by the unit test of `columns_width` instead.
    #[gpui_kit::test]
    fn a_cell_standing_in_for_several_columns_should_be_the_only_cell_drawn_across_them(
        cx: &mut TestAppContext,
    ) {
        let (_, cx) = harness_with_span(cx, 1, 2);

        assert!(
            cx.debug_bounds("TEST_TABLE_CELL_0_0").is_some(),
            "the row draws the cell its first column is asked for"
        );
        assert!(
            cx.debug_bounds("TEST_TABLE_CELL_0_1").is_none(),
            "and not the column that cell covers"
        );
    }

    /// The room a cell asks for is the declared widths of the columns it stands in for,
    /// added up — which is the one thing about a span that cannot be read off a painted
    /// frame.
    #[test]
    fn a_merged_cell_should_ask_for_the_widths_of_the_columns_it_covers() {
        let of_row = |fraction| Length::Definite(relative(fraction));

        assert_eq!(
            columns_width(&[of_row(0.25)]),
            of_row(0.25),
            "a cell of one column keeps that column's own declaration"
        );
        assert_eq!(
            columns_width(&[of_row(0.25), of_row(0.5), of_row(0.25)]),
            of_row(1.0),
            "and a cell across three of them asks for their shares together"
        );
        assert_eq!(
            columns_width(&[Length::Auto]),
            Length::Auto,
            "an automatic column is as wide as its content, which a cell of it keeps"
        );
        assert_eq!(
            columns_width(&[Length::Auto, of_row(0.5)]),
            of_row(0.5),
            "and one among columns that state a width takes no room from them"
        );
        assert_eq!(
            columns_width(&[Length::Auto, Length::Auto]),
            Length::Auto,
            "while columns that all say automatic say nothing to add up"
        );
        // A rem column is an absolute width like any other, so a cell across two of them asks
        // for their rems together — the accumulator keeps the kind of the width it read first.
        assert_eq!(
            columns_width(&[
                Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Rems(
                    gpui_kit::rems(2.0)
                ))),
                Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Rems(
                    gpui_kit::rems(3.0)
                )))
            ]),
            Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Rems(
                gpui_kit::rems(5.0)
            ))),
            "and two rems add up as rems, whatever kind of absolute width they are"
        );
        assert_eq!(
            columns_width(&[of_row(0.25), px(120.0).into()]),
            of_row(0.25),
            "a row that mixed the two kinds keeps the fractions it read, because a cell \
             cannot state half of each"
        );
        assert_eq!(
            columns_width(&[px(120.0).into(), of_row(0.5)]),
            of_row(0.5),
            "whichever order the two kinds appear in"
        );
        assert_eq!(
            columns_width(&[px(120.0).into(), px(80.0).into()]),
            px(200.0).into(),
            "absolute widths add up the same way, so a row of them needs no pane to be read"
        );
    }
}
