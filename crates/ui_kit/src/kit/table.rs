use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use smol_str::{SmolStr, format_smolstr};
use taffy::style::{Dimension, Position};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole, A11ySort},
    app::App,
    error::EngineError,
    event::Action,
    interaction::{ActionCtx, ScrollHandle},
    math::{LogicalPoint, LogicalPx, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::{
        style::{Style, StylePatch},
        style_builder::StyleBuilder,
    },
    text::TextStyle,
    window::WindowId,
};

use crate::{
    kit::{
        list::{List, ListOptions, RowHeight, Selection, SelectionMode},
        scroll_bar::{ScrollBar, ScrollbarAxis, ScrollbarOptions, detach_proxy},
        text_value::TextValue,
    },
    theme::{handle::ThemeHandle, role::Role},
};

const SCROLLBAR_THICKNESS: LogicalPx = px(4.0);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ColumnWidth {
    Fixed(f32),
    Flex { weight: f32, min_width: LogicalPx },
}

impl ColumnWidth {
    #[must_use]
    pub fn weight(self) -> f32 {
        match self {
            Self::Fixed(_) => 0.0,
            Self::Flex { weight, .. } => weight.max(0.0),
        }
    }

    #[must_use]
    pub fn contribution(self) -> f32 {
        match self {
            Self::Fixed(width) => width,
            Self::Flex { min_width, .. } => min_width.value(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColumnAlign {
    #[default]
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortOrder {
    Ascending,
    Descending,
}

pub type SortState = Option<(usize, SortOrder)>;

#[must_use]
pub fn next_sort_order(current: SortState, column: usize) -> SortState {
    match current {
        Some((current_column, SortOrder::Ascending)) if current_column == column => {
            Some((column, SortOrder::Descending))
        }
        Some((current_column, _)) if current_column == column => {
            Some((column, SortOrder::Ascending))
        }
        _ => Some((column, SortOrder::Ascending)),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub header: SmolStr,
    pub header_binding: Option<TextValue>,
    pub width: ColumnWidth,
    pub align: ColumnAlign,
    pub sortable: bool,
    pub column_span: usize,
}

impl Column {
    #[must_use]
    pub fn fixed(header: impl Into<SmolStr>, width: f32) -> Self {
        Self {
            header: header.into(),
            header_binding: None,
            width: ColumnWidth::Fixed(width.max(1.0)),
            align: ColumnAlign::Start,
            sortable: false,
            column_span: 1,
        }
    }

    #[must_use]
    pub fn flex(header: impl Into<SmolStr>, weight: f32, min_width: LogicalPx) -> Self {
        Self {
            header: header.into(),
            header_binding: None,
            width: ColumnWidth::Flex {
                weight: weight.max(0.0),
                min_width: min_width.max(LogicalPx::ZERO),
            },
            align: ColumnAlign::Start,
            sortable: false,
            column_span: 1,
        }
    }

    #[must_use]
    pub fn header_binding(mut self, header: TextValue) -> Self {
        self.header_binding = Some(header);
        self
    }

    #[must_use]
    pub fn align(mut self, align: ColumnAlign) -> Self {
        self.align = align;
        self
    }

    #[must_use]
    pub fn sortable(mut self) -> Self {
        self.sortable = true;
        self
    }

    #[must_use]
    pub fn span(mut self, span: usize) -> Self {
        self.column_span = span.max(1);
        self
    }
}

fn span_cell_style(
    columns: &[Column],
    start: usize,
    span: usize,
    padding_x: LogicalPx,
    padding_y: LogicalPx,
) -> Style {
    let covered = &columns[start..start + span];
    let align = covered
        .last()
        .map_or(ColumnAlign::Start, |column| column.align);
    let contribution: f32 = covered
        .iter()
        .map(|column| column.width.contribution())
        .sum();
    let flex: f32 = covered.iter().map(|column| column.width.weight()).sum();
    let builder = StyleBuilder::new()
        .flex_row()
        .items_center()
        .min_width_0()
        .padding_sides(padding_y, padding_x, padding_y, padding_x);
    let builder = match align {
        ColumnAlign::Start => builder,
        ColumnAlign::Center => builder.justify_center(),
        ColumnAlign::End => builder.justify_end(),
    };
    if flex > 0.0 {
        let mut style = builder.flex_1().build();
        style
            .flex(flex, 1.0, Dimension::length(contribution))
            .min_width(taffy::style::LengthPercentageAuto::length(contribution));
        style
    } else {
        builder.width(px(contribution)).flex_none().build()
    }
}

fn clamp_span(requested: usize, columns: &[Column], start: usize) -> usize {
    requested.max(1).min(columns.len() - start)
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableOptions {
    pub row_height: RowHeight,
    pub selection: SelectionMode,
    pub scrollbar: bool,
}

impl TableOptions {
    #[must_use]
    pub fn fixed(row_height: LogicalPx) -> Self {
        Self {
            row_height: RowHeight::Fixed(row_height),
            selection: SelectionMode::Multi,
            scrollbar: true,
        }
    }

    #[must_use]
    pub fn variable(estimate: LogicalPx) -> Self {
        Self {
            row_height: RowHeight::Variable { estimate },
            ..Self::fixed(estimate)
        }
    }

    #[must_use]
    pub fn selection(mut self, mode: SelectionMode) -> Self {
        self.selection = mode;
        self
    }
}

struct TableInner {
    key: SmolStr,
    theme: ThemeHandle,
    columns: Vec<Column>,
    column_span: Rc<RefCell<Option<Rc<dyn Fn(usize, usize) -> usize>>>>,
    list: List,
    vbar: ScrollBar,
    hbar: ScrollBar,
    content_width: LogicalPx,
    scrollbar: bool,
    sticky_x: Cell<LogicalPx>,
    header_width: Cell<LogicalPx>,
    scope: SignalScope,
    sort: Signal<SortState>,
    on_sort: RefCell<Option<Rc<dyn Fn(&mut App, SortState)>>>,
    empty_text: Option<SmolStr>,
    row_count: Rc<Cell<usize>>,
    sort_action: Action,
    geometry: RefCell<TableGeometry>,
    header_height: LogicalPx,
    padding_x: LogicalPx,
}

#[derive(Default)]
struct TableGeometry {
    window: Option<WindowId>,
    outer: Option<NodeId>,
    header: Option<NodeId>,
    header_content: Option<NodeId>,
    header_row: Option<NodeId>,
    body: Option<NodeId>,
    list_handle: Option<ScrollHandle>,
    empty: Option<NodeId>,
}

#[derive(Clone)]
pub struct Table {
    inner: Rc<TableInner>,
}

impl Table {
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        key: impl Into<SmolStr>,
        columns: Vec<Column>,
        options: TableOptions,
        cells: impl Fn(&mut NodeTree, NodeId, usize, usize) + 'static,
        empty_text: impl Into<SmolStr>,
    ) -> Self {
        let key = key.into();
        let tokens = theme.tokens().medium;
        let padding_x = tokens.padding_x;
        let header_height = tokens.height;
        let row_count = Rc::new(Cell::new(0usize));
        let sort_action = Action::Custom(format_smolstr!("{key}.sort"));
        let scope = app.signal_scope();
        let sort = app.signal_scoped(scope, None);
        let table_key = key.clone();
        let list_columns = columns.clone();
        let list_theme = theme.clone();
        let count_for_rows = row_count.clone();
        let cells: Rc<dyn Fn(&mut NodeTree, NodeId, usize, usize)> = Rc::new(cells);
        let cells_for_rows = cells.clone();
        let column_span: Rc<RefCell<Option<Rc<dyn Fn(usize, usize) -> usize>>>> =
            Rc::new(RefCell::new(None));
        let column_span_for_rows = column_span.clone();
        let fixed_rows = matches!(options.row_height, RowHeight::Fixed(_));
        let empty_text: Option<SmolStr> =
            Some(empty_text.into()).filter(|text: &SmolStr| !text.is_empty());
        let content_width = px(columns
            .iter()
            .map(|column| column.width.contribution())
            .sum::<f32>());
        let list_content_width = (content_width > LogicalPx::ZERO).then_some(content_width);
        let list = List::new(
            app,
            theme.clone(),
            format_smolstr!("{key}:body"),
            ListOptions {
                row_height: options.row_height,
                overscan: 2,
                selection: options.selection,
                capacity: None,
                reverse: false,
                keep_on_newest: false,
                padding_x: LogicalPx::ZERO,
                padding_y: LogicalPx::ZERO,
                row_stretch: fixed_rows,
                scrollbar: false,
                separator: false,
                content_width: list_content_width,
            },
            move |tree, parent, row| {
                let count = count_for_rows.get();
                let column_style = StyleBuilder::new().flex_col().width_full().min_width_0();
                let column_style = if fixed_rows {
                    column_style.height_full().build()
                } else {
                    column_style.build()
                };
                let column = NodeBuilder::boxed()
                    .style(column_style)
                    .build(tree, parent)?;
                let cells_row_style = StyleBuilder::new().flex_row().width_full().min_width_0();
                let cells_row_style = if fixed_rows {
                    cells_row_style.flex_1().build()
                } else {
                    cells_row_style.build()
                };
                let cells_row = NodeBuilder::boxed()
                    .style(cells_row_style)
                    .build(tree, column)?;
                let mut column_index = 0;
                while column_index < list_columns.len() {
                    let column = &list_columns[column_index];
                    let requested = {
                        let callback = column_span_for_rows.borrow().clone();
                        callback.map_or(column.column_span, |callback| callback(row, column_index))
                    };
                    let span = clamp_span(requested, &list_columns, column_index);
                    let style = span_cell_style(
                        &list_columns,
                        column_index,
                        span,
                        tokens.padding_x,
                        px(4.0),
                    );
                    let cell_theme = list_theme.clone();
                    let cell = NodeBuilder::boxed()
                        .id(format_smolstr!("{table_key}:cell:{row}:{column_index}"))
                        .style(style)
                        .bind_style(move |cx| StylePatch {
                            foreground: Some(cell_theme.role(cx, Role::FgDefault)),
                            ..StylePatch::default()
                        })
                        .a11y(A11yOverride {
                            role: Some(A11yRole::Cell),
                            column_index: Some(column_index),
                            ..A11yOverride::default()
                        })
                        .build(tree, cells_row)?;
                    (cells_for_rows)(tree, cell, row, column_index);
                    column_index += span;
                }
                if row + 1 < count {
                    let divider_theme = list_theme.clone();
                    NodeBuilder::boxed()
                        .style(
                            StyleBuilder::new()
                                .width_full()
                                .height(px(1.0))
                                .flex_none()
                                .build(),
                        )
                        .bind_style(move |cx| StylePatch {
                            background: Some(divider_theme.role(cx, Role::LineDivider)),
                            ..StylePatch::default()
                        })
                        .build(tree, column)?;
                }
                Ok(())
            },
        );
        list.set_row_a11y(|_cx, _| A11yPatch {
            role: Some(A11yRole::Row),
            ..A11yPatch::default()
        });
        let vbar = ScrollBar::new(
            app,
            theme.clone(),
            format_smolstr!("{key}:body:scrollbar"),
            ScrollbarAxis::Vertical,
            ScrollbarOptions::default(),
        );
        let hbar = ScrollBar::new(
            app,
            theme.clone(),
            format_smolstr!("{key}:scrollbar-h"),
            ScrollbarAxis::Horizontal,
            ScrollbarOptions::default(),
        );
        Self {
            inner: Rc::new(TableInner {
                sort,
                scope,
                column_span,
                on_sort: RefCell::new(None),
                list,
                vbar,
                hbar,
                content_width,
                scrollbar: options.scrollbar,
                sticky_x: Cell::new(LogicalPx::ZERO),
                header_width: Cell::new(content_width),
                theme,
                columns,
                empty_text,
                row_count,
                sort_action,
                geometry: RefCell::new(TableGeometry::default()),
                header_height,
                padding_x,
                key,
            }),
        }
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    #[must_use]
    pub fn list(&self) -> &List {
        &self.inner.list
    }

    pub fn release(&self, app: &mut App) -> usize {
        app.off_action(&self.inner.sort_action);
        let (window, header) = {
            let geometry = self.inner.geometry.borrow();
            (geometry.window, geometry.header)
        };
        if let (Some(window), Some(header)) = (window, header) {
            detach_proxy(app, window, header);
        }
        let bars = self.inner.vbar.release(app) + self.inner.hbar.release(app);
        bars + self.inner.list.release(app) + app.release_scope(self.inner.scope)
    }

    pub fn set_initial_rows(&self, app: &mut App, count: usize) {
        self.inner.row_count.set(count);
        self.inner.list.set_initial_count(app, count);
    }

    pub fn set_rows(&self, app: &mut App, count: usize) -> Result<(), EngineError> {
        self.inner.row_count.set(count);
        self.inner.list.set_count(app, count)
    }

    #[must_use]
    pub fn rows(&self) -> usize {
        self.inner.row_count.get()
    }

    #[must_use]
    pub fn selection(&self, app: &App) -> Selection {
        self.inner.list.selection(app)
    }

    pub fn select(&self, app: &mut App, index: Option<usize>) -> bool {
        self.inner.list.select(app, index)
    }

    #[must_use]
    pub fn sort(&self, app: &App) -> SortState {
        app.read_with(&self.inner.sort, |sort| *sort)
    }

    #[must_use]
    pub fn sort_signal(&self) -> Signal<SortState> {
        self.inner.sort
    }

    pub fn on_sort(&self, callback: impl Fn(&mut App, SortState) + 'static) {
        *self.inner.on_sort.borrow_mut() = Some(Rc::new(callback));
    }

    pub fn set_column_span(&self, span: impl Fn(usize, usize) -> usize + 'static) {
        *self.inner.column_span.borrow_mut() = Some(Rc::new(span));
    }

    pub fn on_row_activate(&self, callback: impl Fn(&mut App, usize) + 'static) {
        self.inner.list.on_activate(callback);
    }

    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.inner.columns
    }

    pub fn build(&self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError> {
        let inner = &self.inner;
        let column_count = inner.columns.len();
        let mut outer_style = StyleBuilder::new()
            .flex_col()
            .size_full()
            .min_width_0()
            .min_height_0()
            .overflow_hidden()
            .relative()
            .build();
        if inner.scrollbar {
            outer_style.padding_sides(
                LogicalPx::ZERO,
                LogicalPx::ZERO,
                SCROLLBAR_THICKNESS,
                LogicalPx::ZERO,
            );
        }
        let outer = NodeBuilder::boxed()
            .id(format_smolstr!("{}:table", inner.key))
            .style(outer_style)
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                column_count: Some(column_count),
                ..A11yOverride::default()
            })
            .build(tree, parent)?;
        let header_theme = inner.theme.clone();
        let header = NodeBuilder::boxed()
            .id(format_smolstr!("{}:header", inner.key))
            .style(
                StyleBuilder::new()
                    .overflow_hidden()
                    .relative()
                    .width_full()
                    .height(inner.header_height + px(1.0))
                    .flex_none()
                    .build(),
            )
            .bind_style(move |cx| StylePatch {
                background: Some(header_theme.role(cx, Role::SurfaceRaised)),
                ..StylePatch::default()
            })
            .build(tree, outer)?;
        let header_content_style = StyleBuilder::new().height_full().flex_col();
        let header_content_style = if inner.content_width > LogicalPx::ZERO {
            header_content_style
                .absolute()
                .left(px(0.0))
                .top(px(0.0))
                .width(inner.content_width)
        } else {
            header_content_style.width_full()
        };
        let header_content = NodeBuilder::boxed()
            .id(format_smolstr!("{}:header:content", inner.key))
            .style(header_content_style.build())
            .build(tree, header)?;
        let header_row_count = inner.list.count_signal();
        let header_row = NodeBuilder::boxed()
            .id(format_smolstr!("{}:header:row", inner.key))
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .width_full()
                    .height(inner.header_height)
                    .build(),
            )
            .a11y(A11yOverride {
                role: Some(A11yRole::Row),
                ..A11yOverride::default()
            })
            .bind_a11y(move |cx| A11yPatch {
                row_count: Some(*cx.read(&header_row_count) + 1),
                ..A11yPatch::default()
            })
            .build(tree, header_content)?;
        let sort = inner.sort;
        let header_count = inner.list.count_signal();
        let mut index = 0;
        while index < inner.columns.len() {
            let column = &inner.columns[index];
            let span = clamp_span(column.column_span, &inner.columns, index);
            let theme = inner.theme.clone();
            let style = span_cell_style(
                &inner.columns,
                index,
                span,
                inner.padding_x,
                LogicalPx::ZERO,
            );
            let header_theme = theme.clone();
            let sortable = column.sortable;
            let a11y_sort = sort;
            let cell_count = header_count;
            let cell = NodeBuilder::boxed()
                .id(format_smolstr!("{}:header:{}", inner.key, index))
                .style(style)
                .bind_style(move |cx| StylePatch {
                    foreground: Some(header_theme.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .a11y(A11yOverride {
                    role: Some(A11yRole::ColumnHeader),
                    column_index: Some(index),
                    ..A11yOverride::default()
                })
                .bind_a11y(move |cx| A11yPatch {
                    row_count: Some(*cx.read(&cell_count) + 1),
                    sort: sortable.then(|| match *cx.read(&a11y_sort) {
                        Some((sorted, SortOrder::Ascending)) if sorted == index => {
                            A11ySort::Ascending
                        }
                        Some((sorted, SortOrder::Descending)) if sorted == index => {
                            A11ySort::Descending
                        }
                        _ => A11ySort::None,
                    }),
                    ..A11yPatch::default()
                });
            let cell = if column.sortable {
                cell.on_click(inner.sort_action.clone())
            } else {
                cell
            };
            let cell = cell.build(tree, header_row)?;
            let label_theme = theme.clone();
            let label = column
                .header_binding
                .clone()
                .unwrap_or_else(|| TextValue::Static(column.header.clone()));
            let initial = label.initial_text();
            let mut text = NodeBuilder::text(
                initial,
                TextStyle::new(inner.theme.tokens().medium.font_size),
            );
            if label.is_bound() {
                text = text.bind_text(move |cx| label.read(cx));
            }
            text.bind_style(move |cx| StylePatch {
                foreground: Some(label_theme.role(cx, Role::FgMuted)),
                ..StylePatch::default()
            })
            .build(tree, cell)?;
            if column.sortable {
                let indicator_theme = theme;
                let column_index = index;
                NodeBuilder::text(
                    SmolStr::new_static(""),
                    TextStyle::new(inner.theme.tokens().small.font_size),
                )
                .a11y(A11yOverride {
                    hidden: true,
                    ..A11yOverride::default()
                })
                .bind_text(move |cx| match *cx.read(&sort) {
                    Some((sorted, SortOrder::Ascending)) if sorted == column_index => {
                        SmolStr::new_static("▲")
                    }
                    Some((sorted, SortOrder::Descending)) if sorted == column_index => {
                        SmolStr::new_static("▼")
                    }
                    _ => SmolStr::new_static(""),
                })
                .bind_style(move |cx| StylePatch {
                    foreground: Some(indicator_theme.role(cx, Role::FgSubtle)),
                    ..StylePatch::default()
                })
                .build(tree, cell)?;
            }
            index += span;
        }
        let divider_theme = inner.theme.clone();
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .width_full()
                    .height(px(1.0))
                    .flex_none()
                    .build(),
            )
            .bind_style(move |cx| StylePatch {
                background: Some(divider_theme.role(cx, Role::LineDivider)),
                ..StylePatch::default()
            })
            .build(tree, header_content)?;
        let body = NodeBuilder::boxed()
            .id(format_smolstr!("{}:body", inner.key))
            .style(
                StyleBuilder::new()
                    .flex_1()
                    .relative()
                    .width_full()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .build(tree, outer)?;
        let _ = inner.list.build(tree, body)?;
        if inner.scrollbar {
            let vbar_rail = NodeBuilder::boxed()
                .id(format_smolstr!("{}:vbar-rail", inner.key))
                .style(
                    StyleBuilder::new()
                        .absolute()
                        .top(px(0.0))
                        .right(px(0.0))
                        .bottom(px(0.0))
                        .width(SCROLLBAR_THICKNESS)
                        .build(),
                )
                .build(tree, body)?;
            inner.vbar.build(tree, vbar_rail)?;
            let hbar_rail = NodeBuilder::boxed()
                .id(format_smolstr!("{}:hbar-rail", inner.key))
                .style(
                    StyleBuilder::new()
                        .absolute()
                        .left(px(0.0))
                        .right(SCROLLBAR_THICKNESS)
                        .bottom(px(0.0))
                        .height(SCROLLBAR_THICKNESS)
                        .build(),
                )
                .build(tree, outer)?;
            inner.hbar.build(tree, hbar_rail)?;
        }
        if let Some(empty_text) = inner.empty_text.clone() {
            let count = inner.list.count_signal();
            let empty_theme = inner.theme.clone();
            let empty = NodeBuilder::boxed()
                .id(format_smolstr!("{}:empty", inner.key))
                .style(
                    StyleBuilder::new()
                        .absolute()
                        .left(px(0.0))
                        .right(px(0.0))
                        .top(px(0.0))
                        .bottom(px(0.0))
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .build(),
                )
                .bind_style(move |cx| StylePatch {
                    visible: Some(*cx.read(&count) == 0),
                    ..StylePatch::default()
                })
                .build(tree, body)?;
            NodeBuilder::text(
                empty_text,
                TextStyle::new(inner.theme.tokens().medium.font_size),
            )
            .bind_style(move |cx| StylePatch {
                foreground: Some(empty_theme.role(cx, Role::FgMuted)),
                ..StylePatch::default()
            })
            .build(tree, empty)?;
            inner.geometry.borrow_mut().empty = Some(empty);
        }
        {
            let mut geometry = inner.geometry.borrow_mut();
            geometry.outer = Some(outer);
            geometry.header = Some(header);
            geometry.header_content = Some(header_content);
            geometry.body = Some(body);
            geometry.header_row = Some(header_row);
        }
        Ok(outer)
    }

    pub fn attach(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let outer = self.inner.geometry.borrow().outer;
        let Some(outer) = outer else {
            let Some(target) = app.window(window) else {
                return Err(EngineError::UnknownWindow { window });
            };
            return Err(EngineError::NotScrollable {
                node: target.tree().root(),
            });
        };
        let Some(viewport) = self.inner.list.viewport_node() else {
            return Err(EngineError::NotScrollable { node: outer });
        };
        if let Err(error) = self.inner.list.attach(app, window) {
            let _ = self.release(app);
            return Err(error);
        }
        let attached = (|| -> Result<NodeId, EngineError> {
            if self.inner.scrollbar {
                self.inner.vbar.attach(app, window, viewport)?;
                self.inner.hbar.attach(app, window, viewport)?;
            }
            let header = self
                .inner
                .geometry
                .borrow()
                .header
                .ok_or(EngineError::NotScrollable { node: outer })?;
            app.set_scroll_proxy(window, header, Some(viewport))?;
            Ok(header)
        })();
        let header = match attached {
            Ok(header) => header,
            Err(error) => {
                self.release(app);
                return Err(error);
            }
        };
        {
            let mut geometry = self.inner.geometry.borrow_mut();
            geometry.window = Some(window);
            geometry.header = Some(header);
            geometry.list_handle = app.scroll_handle(window, viewport);
        }
        let table = self.clone();
        app.on_action(self.inner.sort_action.clone(), move |ctx| {
            table.on_header_click(ctx);
        });
        Ok(())
    }

    pub fn mount(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let outer = {
            let Some(target) = app.window_mut(window) else {
                return Err(EngineError::UnknownWindow { window });
            };
            self.build(target.tree_mut(), parent)?
        };
        self.attach(app, window)?;
        Ok(outer)
    }

    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let mut changed = self.inner.list.sync(app)?;
        changed |= self.sync_header_content(app)?;
        if self.inner.scrollbar {
            changed |= self.inner.vbar.sync(app)?;
            changed |= self.inner.hbar.sync(app)?;
        }
        Ok(changed)
    }

    fn sync_header_content(&self, app: &mut App) -> Result<bool, EngineError> {
        if self.inner.content_width == LogicalPx::ZERO {
            return Ok(false);
        }
        let (Some(window), Some(header_content), Some(handle)) = (
            self.inner.geometry.borrow().window,
            self.inner.geometry.borrow().header_content,
            self.inner.geometry.borrow().list_handle,
        ) else {
            return Ok(false);
        };
        let Some(mut state) = app.scroll_state(&handle) else {
            return Ok(false);
        };
        let max_x = (self.inner.content_width - state.viewport.width).max(LogicalPx::ZERO);
        if state.offset.x > max_x {
            app.scroll_to(&handle, LogicalPoint::new(max_x, state.offset.y))?;
            if let Some(clamped) = app.scroll_state(&handle) {
                state = clamped;
            }
        }
        let x = state.offset.x.clamp(LogicalPx::ZERO, max_x);
        let width = self.inner.content_width.max(state.viewport.width);
        if x == self.inner.sticky_x.get() && width == self.inner.header_width.get() {
            return Ok(false);
        }
        let Some(target) = app.window_mut(window) else {
            return Ok(false);
        };
        let tree = target.tree_mut();
        let mut style = tree.get(header_content)?.style().clone();
        style.position(Position::Absolute);
        style.inset_top(LogicalPx::ZERO);
        style.inset_left(-x);
        style.width_length(width);
        style.height_percent(1.0);
        tree.set_style(header_content, style)?;
        self.inner.sticky_x.set(x);
        self.inner.header_width.set(width);
        app.request_repaint(window)?;
        Ok(true)
    }

    #[must_use]
    pub fn content_width(&self) -> LogicalPx {
        self.inner.content_width
    }

    #[must_use]
    pub fn vertical_bar(&self) -> &ScrollBar {
        &self.inner.vbar
    }

    #[must_use]
    pub fn horizontal_bar(&self) -> &ScrollBar {
        &self.inner.hbar
    }

    #[must_use]
    pub fn empty_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().empty
    }

    #[must_use]
    pub fn outer_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().outer
    }

    #[must_use]
    pub fn header_node(&self, app: &App, column: usize) -> Option<NodeId> {
        let geometry = self.inner.geometry.borrow();
        let window = geometry.window?;
        let row = geometry.header_row?;
        drop(geometry);
        let target = format_smolstr!("{}:header:{}", self.inner.key, column);
        let tree = app.window(window)?.tree();
        tree.get(row)
            .ok()?
            .children()
            .iter()
            .copied()
            .find(|node| tree.debug_id(*node) == Some(target.as_str()))
    }

    #[must_use]
    pub fn cell_node(&self, app: &App, row: usize, column: usize) -> Option<NodeId> {
        let window = self.inner.geometry.borrow().window?;
        let content = self.inner.list.row_content_node(app, row)?;
        let tree = app.window(window)?.tree();
        let column_container = tree.get(content).ok()?.children().first().copied()?;
        let cells_row = tree
            .get(column_container)
            .ok()?
            .children()
            .first()
            .copied()?;
        let target = format_smolstr!("{}:cell:{row}:{column}", self.inner.key);
        tree.get(cells_row)
            .ok()?
            .children()
            .iter()
            .copied()
            .find(|node| tree.debug_id(*node) == Some(target.as_str()))
    }

    fn on_header_click(&self, ctx: &mut ActionCtx<'_>) {
        let window = ctx.window();
        let node = ctx.node();
        let Some(id) = ctx
            .app_mut()
            .window(window)
            .and_then(|target| target.tree().debug_id(node).map(str::to_owned))
        else {
            return;
        };
        let prefix = format_smolstr!("{}:header:", self.inner.key);
        let Some(column) = id
            .strip_prefix(prefix.as_str())
            .and_then(|rest| rest.parse().ok())
        else {
            return;
        };
        let current = self.sort(ctx.app_mut());
        let next = next_sort_order(current, column);
        ctx.app_mut().write(&self.inner.sort, next);
        let callback = self.inner.on_sort.borrow().clone();
        if let Some(callback) = callback {
            callback(ctx.app_mut(), next);
        }
    }
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;
    use taffy::style::{ExpandedDimension, ExpandedLengthPercentageAuto};
    use ui_core::{
        a11y::{A11yAction, A11yRole, A11ySort},
        app::App,
        error::EngineError,
        event::{InputEvent, Modifiers, PointerButton},
        interaction::{ItemAlign, ScrollAxes, ScrollConfig},
        math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
        node::{
            builder::NodeBuilder,
            id::NodeId,
            tree::{NodeKind, NodeTree},
        },
        paint::style_builder::StyleBuilder,
        test_support::app::TestApp,
        text::TextStyle,
        window::WindowId,
    };

    use super::{
        Column, ColumnAlign, ColumnWidth, SortOrder, Table, TableOptions, next_sort_order,
    };
    use crate::{
        kit::{
            list::{RowHeight, SelectionMode},
            scroll_bar::ScrollBar,
            test_support::{a11y_of, assert_single_parent_per_node, frame_a11y, full_a11y},
            text_value::TextValue,
        },
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    #[test]
    fn sort_cycles_between_the_two_directions_of_one_column() {
        assert_eq!(next_sort_order(None, 1), Some((1, SortOrder::Ascending)));
        assert_eq!(
            next_sort_order(Some((1, SortOrder::Ascending)), 1),
            Some((1, SortOrder::Descending))
        );
        assert_eq!(
            next_sort_order(Some((1, SortOrder::Descending)), 1),
            Some((1, SortOrder::Ascending)),
            "降序后再点回到升序（两态循环）"
        );
        assert_eq!(
            next_sort_order(Some((0, SortOrder::Descending)), 2),
            Some((2, SortOrder::Ascending)),
            "换一列从升序开始"
        );
    }

    #[test]
    fn column_widths_keep_their_kind() {
        assert_eq!(ColumnWidth::Fixed(120.0).weight(), 0.0);
        assert_eq!(
            ColumnWidth::Flex {
                weight: 2.0,
                min_width: px(0.0)
            }
            .weight(),
            2.0
        );

        let fixed = Column::fixed("price", 90.0);
        assert_eq!(fixed.width, ColumnWidth::Fixed(90.0));
        assert!(!fixed.sortable);
        let flex = Column::flex("name", 2.0, px(0.0))
            .align(ColumnAlign::End)
            .sortable();
        assert_eq!(
            flex.width,
            ColumnWidth::Flex {
                weight: 2.0,
                min_width: px(0.0)
            }
        );
        assert_eq!(flex.align, ColumnAlign::End);
        assert!(flex.sortable);
    }

    #[test]
    fn table_options_default_to_multi_select() {
        let options = TableOptions::fixed(px(28.0));
        assert_eq!(options.selection, SelectionMode::Multi);
        assert_eq!(options.row_height, RowHeight::Fixed(px(28.0)));
        assert!(options.scrollbar);

        let variable = TableOptions::variable(px(40.0));
        assert_eq!(
            variable.row_height,
            RowHeight::Variable { estimate: px(40.0) }
        );
    }

    #[test]
    fn a_bound_header_text_updates_without_rebuilding_the_table() {
        let mut app = TestApp::new();
        let language = app.app_mut().signal(SmolStr::new_static("金额"));
        let table = table_with(
            &mut app,
            vec![
                Column::flex("value", 1.0, px(0.0)),
                Column::flex("", 1.0, px(0.0)).header_binding(TextValue::bound(language)),
            ],
            TableOptions::fixed(px(28.0)),
            4,
            text_cell,
        );
        mount_in(&mut app, &table, 320.0, 200.0);
        let header_static = table.header_node(app.app(), 0).expect("header 0");
        let header_bound = table.header_node(app.app(), 1).expect("header 1");
        assert_eq!(
            texts(&app, header_bound),
            vec![SmolStr::new_static("金额")],
            "绑定列的首帧文本来自信号（首个 text pass 求出真值）"
        );
        assert_eq!(
            texts(&app, header_static),
            vec![SmolStr::new_static("value")],
            "静态列不受绑定面影响"
        );

        app.app_mut()
            .write(&language, SmolStr::new_static("Amount"));
        let _ = app.drain().expect("frame");
        assert_eq!(
            texts(&app, header_bound),
            vec![SmolStr::new_static("Amount")],
            "写语言信号即更新表头文本"
        );
        assert_eq!(
            table.header_node(app.app(), 1),
            Some(header_bound),
            "表头节点没有被重建（同一 NodeId）"
        );
        assert_eq!(
            texts(&app, header_static),
            vec![SmolStr::new_static("value")],
            "静态列仍不受影响"
        );
    }

    fn text_cell(
        tree: &mut ui_core::node::tree::NodeTree,
        parent: ui_core::node::id::NodeId,
        row: usize,
        column: usize,
    ) {
        let label = smol_str::format_smolstr!("{row}:{column}");
        NodeBuilder::text(label, TextStyle::new(px(12.0)))
            .build(tree, parent)
            .expect("cell");
    }

    fn table_with_rows(app: &mut TestApp, rows: usize) -> Table {
        table_with(
            app,
            vec![
                Column::fixed("name", 120.0).sortable(),
                Column::flex("value", 1.0, px(0.0)),
            ],
            TableOptions::fixed(px(28.0)),
            rows,
            text_cell,
        )
    }

    fn table_with(
        app: &mut TestApp,
        columns: Vec<Column>,
        options: TableOptions,
        rows: usize,
        cells: impl Fn(&mut NodeTree, NodeId, usize, usize) + 'static,
    ) -> Table {
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let table = Table::new(
            app.app_mut(),
            theme,
            "table",
            columns,
            options,
            cells,
            "empty",
        );
        table.set_initial_rows(app.app_mut(), rows);
        table
    }

    fn mount_in(app: &mut TestApp, table: &Table, width: f32, height: f32) {
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let parent = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(px(width))
                        .height(px(height))
                        .build(),
                )
                .build(tree, root)
                .expect("parent")
        };
        table.mount(app.app_mut(), window, parent).expect("mount");
        settle(app, table);
    }

    fn mount(app: &mut TestApp, table: &Table) {
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        table.mount(app.app_mut(), window, root).expect("mount");
        settle(app, table);
    }

    fn settle(app: &mut TestApp, table: &Table) {
        for _ in 0..4 {
            let _ = app.drain().expect("frame");
            let _ = table.sync(app.app_mut()).expect("sync");
        }
        let _ = app.drain().expect("frame");
    }

    fn center(app: &TestApp, node: ui_core::node::id::NodeId) -> ui_core::math::LogicalPoint {
        let bounds = app.probe().bounds(node).expect("bounds");
        ui_core::math::LogicalPoint::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        )
    }

    fn click(app: &mut TestApp, point: ui_core::math::LogicalPoint) {
        let _ = app.inject(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
    }

    fn click_outcome(
        app: &mut TestApp,
        point: ui_core::math::LogicalPoint,
    ) -> ui_core::event::InputOutcome {
        let _ = app.inject(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ))
    }

    #[test]
    fn rows_are_virtualized_and_columns_align() {
        let mut app = TestApp::new();
        let table = table_with_rows(&mut app, 100);
        mount(&mut app, &table);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let _ = root;
        let state = table.list().state(app.app()).expect("state");
        assert!(state.mounted < 100, "100 行只挂可见窗口：{}", state.mounted);
        assert_eq!(state.first, 0);
        let cell = table.cell_node(app.app(), 0, 0).expect("first cell");
        let name = app.probe().bounds(cell).expect("cell bounds");
        assert!(
            (name.size.width - 120.0).abs() < px(0.5),
            "固定列宽 120：{name:?}"
        );
        let value = table.cell_node(app.app(), 0, 1).expect("second cell");
        let value_bounds = app.probe().bounds(value).expect("cell bounds");
        assert!(
            value_bounds.size.width > px(120.0),
            "弹性列占剩余宽度：{value_bounds:?}"
        );
        assert_eq!(
            value_bounds.origin.x,
            name.origin.x + name.size.width,
            "列在行内依次排开"
        );
    }

    #[test]
    fn clicking_rows_selects_multiple() {
        let mut app = TestApp::new();
        let table = table_with_rows(&mut app, 10);
        mount(&mut app, &table);
        let row3 = table.list().row_node(app.app(), 3).expect("row 3");
        let point3 = center(&app, row3);
        click(&mut app, point3);
        assert_eq!(table.selection(app.app()).first(), Some(3));
        let row5 = table.list().row_node(app.app(), 5).expect("row 5");
        let point5 = center(&app, row5);
        let _ = app.inject(InputEvent::pointer_down(
            point5,
            PointerButton::Primary,
            Modifiers::CONTROL,
        ));
        let _ = app.inject(InputEvent::pointer_up(
            point5,
            PointerButton::Primary,
            Modifiers::CONTROL,
        ));
        let selection = table.selection(app.app());
        assert_eq!(selection.iter().collect::<Vec<_>>(), vec![3, 5]);
    }

    #[test]
    fn clicking_a_sortable_header_reports_the_new_order() {
        let mut app = TestApp::new();
        let table = table_with_rows(&mut app, 3);
        mount(&mut app, &table);
        let header = table.header_node(app.app(), 0).expect("header cell");
        let point = center(&app, header);
        click(&mut app, point);
        assert_eq!(
            table.sort(app.app()),
            Some((0, SortOrder::Ascending)),
            "点击可排序表头写排序状态"
        );
        click(&mut app, point);
        assert_eq!(
            table.sort(app.app()),
            Some((0, SortOrder::Descending)),
            "再点切到降序"
        );
    }

    #[test]
    fn releasing_the_table_removes_its_sort_handler() {
        let mut app = TestApp::new();
        let table = table_with_rows(&mut app, 3);
        mount(&mut app, &table);
        let sort_action = table.inner.sort_action.clone();
        assert!(
            app.app().has_action_handler(&sort_action),
            "attach 必须注册排序 handler"
        );
        let header = table.header_node(app.app(), 0).expect("header cell");
        let point = center(&app, header);
        let click0 = click_outcome(&mut app, point);
        assert_eq!(
            click0.action,
            Some(sort_action.clone()),
            "释放前点击表头必须由排序 handler 消费"
        );
        assert_eq!(table.sort(app.app()), Some((0, SortOrder::Ascending)));

        assert!(table.release(app.app_mut()) > 0);
        assert!(
            !app.app().has_action_handler(&sort_action),
            "release 必须摘掉排序 handler"
        );
        let click1 = click_outcome(&mut app, point);
        assert_eq!(click1.action, None, "释放后重放点击不得再命中排序 handler");
    }

    #[test]
    fn an_empty_table_draws_its_message_until_rows_arrive() {
        let mut app = TestApp::new();
        let table = table_with_rows(&mut app, 0);
        mount(&mut app, &table);
        assert_eq!(table.list().mounted_rows(app.app()).len(), 0);
        let empty = table.empty_node().expect("empty overlay");
        let visible = |app: &TestApp, node| {
            let window = app.window_id();
            app.app()
                .window(window)
                .expect("window")
                .tree()
                .get(node)
                .expect("node")
                .style_patch()
                .visible
        };
        assert_eq!(visible(&app, empty), Some(true), "行数为 0 时提示可见");

        table.set_rows(app.app_mut(), 2).expect("rows");
        settle(&mut app, &table);
        assert_eq!(table.list().mounted_rows(app.app()).len(), 2);
        assert_eq!(visible(&app, empty), Some(false), "有行之后空态提示隐藏");
        let cell = table.cell_node(app.app(), 0, 0).expect("cell");
        assert!(app.probe().bounds(cell).is_some());
    }

    fn texts(app: &TestApp, node: NodeId) -> Vec<SmolStr> {
        let window = app.window_id();
        let tree = app.app().window(window).expect("window").tree();
        let mut out = Vec::new();
        fn walk(tree: &NodeTree, node: NodeId, out: &mut Vec<SmolStr>) {
            let Ok(target) = tree.get(node) else {
                return;
            };
            if let NodeKind::Text(content) = target.kind() {
                out.push(content.text.clone());
            }
            for child in target.children() {
                walk(tree, *child, out);
            }
        }
        walk(tree, node, &mut out);
        out
    }

    #[test]
    fn variable_table_rows_are_measured_from_cell_content() {
        let mut app = TestApp::new();
        let table = table_with(
            &mut app,
            vec![
                Column::fixed("name", 120.0),
                Column::flex("value", 1.0, px(0.0)),
            ],
            TableOptions::variable(px(20.0)),
            8,
            |tree, parent, row, _column| {
                let height = if row % 2 == 0 { 40.0 } else { 16.0 };
                NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .width_full()
                            .height(px(height))
                            .flex_none()
                            .build(),
                    )
                    .build(tree, parent)
                    .expect("cell content");
            },
        );
        mount_in(&mut app, &table, 400.0, 120.0);
        let heights = table.list().row_heights();
        assert_eq!(
            heights.get(0),
            Some(px(49.0)),
            "内容 40 + 单元格上下 padding 4*2 + 行分隔线 1"
        );
        assert_eq!(heights.get(1), Some(px(25.0)), "内容 16 + padding + 分隔线");
        let row0 = table.list().row_node(app.app(), 0).expect("row 0");
        let row1 = table.list().row_node(app.app(), 1).expect("row 1");
        let first = app.probe().bounds(row0).expect("row 0 bounds");
        let second = app.probe().bounds(row1).expect("row 1 bounds");
        assert!(
            (first.size.height - 49.0).abs() < px(0.5),
            "行高按实测内容：{first:?}"
        );
        assert!(
            (second.origin.y - first.origin.y - 49.0).abs() < px(0.5),
            "行位置跟前缀和走：{first:?} {second:?}"
        );
        table
            .list()
            .scroll_to_item(app.app_mut(), 2, ItemAlign::Start)
            .expect("scroll to item");
        let _ = app.drain().expect("frame");
        let _ = table.sync(app.app_mut()).expect("sync");
        assert_eq!(
            table.list().offset(app.app()),
            Some(px(74.0)),
            "变高前缀和上的滚动锚点"
        );
        let cell = table.cell_node(app.app(), 2, 0).expect("row 2 cell");
        let bounds = app.probe().bounds(cell).expect("cell bounds");
        assert!(
            (bounds.size.height - 48.0).abs() < px(0.5),
            "单元格高度 = 内容 40 + padding 8：{bounds:?}"
        );
    }

    #[test]
    fn merged_cells_span_the_columns_they_cover() {
        let mut app = TestApp::new();
        let table = table_with(
            &mut app,
            vec![
                Column::fixed("name", 100.0).span(2).sortable(),
                Column::flex("value", 1.0, px(0.0)),
                Column::fixed("size", 50.0),
                Column::flex("change", 1.0, px(0.0)),
            ],
            TableOptions::fixed(px(28.0)),
            4,
            text_cell,
        );
        mount_in(&mut app, &table, 400.0, 240.0);
        let header0 = table.header_node(app.app(), 0).expect("header 0");
        let header0 = app.probe().bounds(header0).expect("header 0 bounds");
        assert!(
            (header0.size.width - 219.0).abs() < px(0.5),
            "跨列表头 = 固定列 100 + 弹性列份额 119：{header0:?}"
        );
        assert!(
            table.header_node(app.app(), 1).is_none(),
            "被跨列的表头不单独绘制"
        );
        let cell0 = table.cell_node(app.app(), 0, 0).expect("cell 0/0");
        let cell0 = app.probe().bounds(cell0).expect("cell bounds");
        assert!(
            (cell0.size.width - 219.0).abs() < px(0.5),
            "跨列单元格 = 被跨列宽度之和：{cell0:?}"
        );
        assert!(
            table.cell_node(app.app(), 0, 1).is_none(),
            "被跨列覆盖的单元格不绘制"
        );
        let cell2 = table.cell_node(app.app(), 0, 2).expect("cell 0/2");
        let cell2 = app.probe().bounds(cell2).expect("cell bounds");
        assert!((cell2.size.width - 50.0).abs() < px(0.5), "{cell2:?}");
        assert!(
            (cell2.origin.x - cell0.origin.x - 219.0).abs() < px(0.5),
            "后续列紧接合并单元格排开：{cell0:?} {cell2:?}"
        );
        let header0_node = table.header_node(app.app(), 0).expect("header 0");
        let point = center(&app, header0_node);
        click(&mut app, point);
        let _ = app.drain().expect("frame");
        assert_eq!(table.sort(app.app()), Some((0, SortOrder::Ascending)));
        assert!(
            texts(&app, header0_node).contains(&SmolStr::new_static("▲")),
            "排序指示画在跨列表头里"
        );
        let cell0 = table.cell_node(app.app(), 0, 0).expect("cell 0/0");
        let point = center(&app, cell0);
        click(&mut app, point);
        assert_eq!(table.selection(app.app()).first(), Some(0));
    }

    #[test]
    fn per_row_spans_are_clamped_to_the_remaining_columns() {
        let mut app = TestApp::new();
        let table = table_with(
            &mut app,
            vec![
                Column::fixed("a", 100.0),
                Column::fixed("b", 50.0),
                Column::fixed("c", 75.0),
                Column::fixed("d", 25.0),
            ],
            TableOptions::fixed(px(24.0)),
            2,
            text_cell,
        );
        table.set_column_span(|row, column| if row == 0 && column == 1 { 99 } else { 1 });
        mount_in(&mut app, &table, 400.0, 200.0);
        let cell = table.cell_node(app.app(), 0, 1).expect("row 0 span start");
        let bounds = app.probe().bounds(cell).expect("cell bounds");
        assert!(
            (bounds.size.width - 150.0).abs() < px(0.5),
            "99 列夹到剩余 3 列 = 50 + 75 + 25：{bounds:?}"
        );
        assert!(table.cell_node(app.app(), 0, 2).is_none());
        assert!(table.cell_node(app.app(), 0, 3).is_none());
        let cell = table.cell_node(app.app(), 1, 1).expect("row 1 col 1");
        let bounds = app.probe().bounds(cell).expect("cell bounds");
        assert!((bounds.size.width - 50.0).abs() < px(0.5), "{bounds:?}");
    }

    #[test]
    fn virtualized_table_rows_hang_under_the_table() {
        let mut app = TestApp::new();
        let table = table_with_rows(&mut app, 100);
        mount(&mut app, &table);
        app.enable_a11y_capture();
        let _ = app.drain().expect("frame");
        app.assert_a11y_contract();
        let table_node = table.outer_node().expect("table node");
        let viewport = table.list().viewport_node().expect("viewport");
        let header0 = table.header_node(app.app(), 0).expect("header 0");
        let header_row = app
            .app()
            .window(app.window_id())
            .expect("window")
            .tree()
            .get(header0)
            .expect("header cell")
            .parent()
            .expect("header row");
        let mounted = table.list().mounted_rows(app.app());
        assert!(mounted.len() < 100, "100 行只挂可见窗口：{}", mounted.len());
        let snapshot = app.a11y().expect("a11y snapshot").clone();
        let table_a11y = a11y_of(&snapshot, table_node);
        assert_eq!(table_a11y.role, A11yRole::Table);
        assert_eq!(
            table_a11y.row_count,
            Some(101),
            "逻辑总行数 = 表头 1 行 + 100 个条目（含未挂载行）"
        );
        assert_eq!(table_a11y.column_count, Some(2));
        assert_eq!(
            table_a11y.size_of_set,
            Some(100),
            "集合大小 = 行窗口的条目总数（窗口自身被过滤）"
        );
        assert!(
            table_a11y.clips_children,
            "滚动容器的裁剪感知必须随语义上提"
        );
        let scroll = table_a11y
            .scroll
            .expect("Table 必须接管行窗口的 scroll（E9 R2 后半）");
        assert_eq!(
            scroll.orientation, None,
            "表体窗口是双轴容器：Both 不自动设置 orientation"
        );
        assert!(
            scroll.y_max > px(0.0),
            "滚动范围来自行窗口的内容高度：{scroll:?}"
        );
        assert_eq!(
            scroll.x_max,
            px(0.0),
            "宽窗口下没有横向滚动范围：{scroll:?}"
        );
        for action in [
            A11yAction::ScrollUp,
            A11yAction::ScrollDown,
            A11yAction::ScrollIntoView,
            A11yAction::SetScrollOffset,
        ] {
            assert!(
                table_a11y.actions.contains(&action),
                "Table 必须宣告 {action:?}（上提的滚动动作）"
            );
        }
        for action in [A11yAction::ScrollLeft, A11yAction::ScrollRight] {
            assert!(
                !table_a11y.actions.contains(&action),
                "x_max == 0 的宽表不得宣告 {action:?}：{:?}",
                table_a11y.actions
            );
        }
        let header = a11y_of(&snapshot, header_row);
        assert_eq!(header.role, A11yRole::Row);
        assert_eq!(header.row_index, Some(0));
        assert_eq!(header.row_count, Some(101));
        assert_eq!(
            header.parent,
            Some(table_a11y.id),
            "表头行的 a11y 父 = Table"
        );
        let header_cell = a11y_of(&snapshot, header0);
        assert_eq!(header_cell.role, A11yRole::ColumnHeader);
        assert_eq!(header_cell.parent, Some(header.id), "列头父 = 表头行");
        assert_eq!(header_cell.row_index, Some(0));
        assert_eq!(header_cell.row_count, Some(101));
        assert_eq!(header_cell.column_index, Some(0));
        let window = a11y_of(&snapshot, viewport);
        assert_eq!(
            window.role,
            A11yRole::GenericContainer,
            "集合行窗口不得再自动声明 List（否则 AT 读到「表格里套列表」）"
        );
        assert!(window.children.is_empty(), "行窗口不得再列行（E9 领养）");
        for (index, row) in &mounted {
            let row_a11y = a11y_of(&snapshot, *row);
            assert_eq!(row_a11y.role, A11yRole::Row);
            assert_eq!(
                row_a11y.parent,
                Some(table_a11y.id),
                "行父 = Table（不是 List）"
            );
            assert_eq!(row_a11y.row_index, Some(1 + index));
            assert_eq!(row_a11y.row_count, Some(101), "总数含未挂载的条目");
        }
        let row0 = table.list().row_node(app.app(), 0).expect("row 0");
        let cell00 = table.cell_node(app.app(), 0, 0).expect("cell 0/0");
        let cell = a11y_of(&snapshot, cell00);
        assert_eq!(cell.role, A11yRole::Cell);
        assert_eq!(cell.parent, Some(a11y_of(&snapshot, row0).id));
        assert_eq!(cell.row_index, Some(1));
        assert_eq!(cell.column_index, Some(0));
        assert_eq!(cell.row_count, Some(101));
        assert_eq!(cell.column_count, Some(2));
        assert_single_parent_per_node(&snapshot);
        table
            .list()
            .scroll_to_item(app.app_mut(), 60, ItemAlign::Start)
            .expect("scroll");
        frame_a11y(&mut app);
        let snapshot = full_a11y(&mut app);
        assert_single_parent_per_node(&snapshot);
        let table_a11y = a11y_of(&snapshot, table_node);
        assert_eq!(table_a11y.row_count, Some(101), "滚动不改变逻辑总行数");
        let mounted = table.list().mounted_rows(app.app());
        assert!(
            mounted.first().is_some_and(|(index, _)| *index > 0),
            "可见窗口换页"
        );
        for (index, row) in &mounted {
            let row_a11y = a11y_of(&snapshot, *row);
            assert_eq!(row_a11y.role, A11yRole::Row);
            assert_eq!(row_a11y.parent, Some(table_a11y.id));
            assert_eq!(row_a11y.row_index, Some(1 + index), "逻辑行号");
        }
    }

    #[test]
    fn merged_columns_keep_logical_column_semantics() {
        let mut app = TestApp::new();
        let table = table_with(
            &mut app,
            vec![
                Column::fixed("name", 100.0).span(2).sortable(),
                Column::flex("value", 1.0, px(0.0)),
                Column::fixed("size", 50.0),
                Column::flex("change", 1.0, px(0.0)),
            ],
            TableOptions::fixed(px(28.0)),
            4,
            text_cell,
        );
        mount_in(&mut app, &table, 400.0, 240.0);
        app.enable_a11y_capture();
        let _ = app.drain().expect("frame");
        let snapshot = app.a11y().expect("a11y snapshot").clone();
        let table_node = table.outer_node().expect("table");
        let table_a11y = a11y_of(&snapshot, table_node);
        assert_eq!(
            table_a11y.column_count,
            Some(4),
            "逻辑列数 4（一行只画出 3 个单元格）"
        );
        assert_eq!(table_a11y.row_count, Some(5), "表头 + 4 行");
        let header0 = table.header_node(app.app(), 0).expect("header 0");
        assert!(table.header_node(app.app(), 1).is_none(), "被跨列不绘制");
        let header2 = table.header_node(app.app(), 2).expect("header 2");
        let merged = a11y_of(&snapshot, header0);
        assert_eq!(merged.role, A11yRole::ColumnHeader);
        assert_eq!(merged.column_index, Some(0));
        assert_eq!(
            merged.column_count,
            Some(4),
            "列头列总数 = 表声明的逻辑列数"
        );
        assert_eq!(
            a11y_of(&snapshot, header2).column_index,
            Some(2),
            "被跨过的列号不连续（声明值优先于行内单元格序）"
        );
        let row0 = table.list().row_node(app.app(), 0).expect("row 0");
        let row_a11y = a11y_of(&snapshot, row0);
        for (column, expected) in [(0usize, 0usize), (2, 2), (3, 3)] {
            let cell = table.cell_node(app.app(), 0, column).expect("cell");
            let node = a11y_of(&snapshot, cell);
            assert_eq!(node.role, A11yRole::Cell);
            assert_eq!(node.parent, Some(row_a11y.id));
            assert_eq!(node.column_index, Some(expected));
            assert_eq!(node.column_count, Some(4));
            assert_eq!(node.row_index, Some(1));
        }
        assert_single_parent_per_node(&snapshot);
    }

    #[test]
    fn multi_selection_and_sort_reach_a11y() {
        let mut app = TestApp::new();
        let table = table_with_rows(&mut app, 10);
        mount(&mut app, &table);
        app.enable_a11y_capture();
        let _ = app.drain().expect("frame");
        let row3 = table.list().row_node(app.app(), 3).expect("row 3");
        let point3 = center(&app, row3);
        click(&mut app, point3);
        let row5 = table.list().row_node(app.app(), 5).expect("row 5");
        let point5 = center(&app, row5);
        let _ = app.inject(InputEvent::pointer_down(
            point5,
            PointerButton::Primary,
            Modifiers::CONTROL,
        ));
        let _ = app.inject(InputEvent::pointer_up(
            point5,
            PointerButton::Primary,
            Modifiers::CONTROL,
        ));
        let _ = app.drain().expect("selection frame");
        app.assert_a11y_contract();
        let update = app.a11y_update().expect("选中交付").clone();
        assert!(update.tree.is_none(), "选中变化是增量笔（不是全树）");
        let snapshot = full_a11y(&mut app);
        assert_single_parent_per_node(&snapshot);
        let selected: Vec<usize> = (0..10)
            .filter_map(|index| {
                let row = table.list().row_node(app.app(), index)?;
                (a11y_of(&snapshot, row).selected == Some(true)).then_some(index)
            })
            .collect();
        assert_eq!(selected, vec![3, 5], "多选状态逐行进 a11y");
        let header0 = table.header_node(app.app(), 0).expect("header 0");
        let header1 = table.header_node(app.app(), 1).expect("header 1");
        let point = center(&app, header0);
        click(&mut app, point);
        let _ = app.drain().expect("sort frame");
        let snapshot = full_a11y(&mut app);
        assert_eq!(
            a11y_of(&snapshot, header0).sort,
            Some(A11ySort::Ascending),
            "点击后升序"
        );
        assert_eq!(
            a11y_of(&snapshot, header1).sort,
            None,
            "不可排序列头不宣告排序状态"
        );
        click(&mut app, point);
        let _ = app.drain().expect("sort frame");
        let snapshot = full_a11y(&mut app);
        assert_eq!(
            a11y_of(&snapshot, header0).sort,
            Some(A11ySort::Descending),
            "再点切降序"
        );
    }

    #[test]
    fn empty_state_text_is_exposed_until_rows_arrive() {
        let mut app = TestApp::new();
        let table = table_with_rows(&mut app, 0);
        mount(&mut app, &table);
        app.enable_a11y_capture();
        let _ = app.drain().expect("frame");
        let snapshot = app.a11y().expect("a11y snapshot").clone();
        let table_node = table.outer_node().expect("table");
        assert_eq!(
            a11y_of(&snapshot, table_node).row_count,
            Some(1),
            "空表只有表头行"
        );
        let empty = table.empty_node().expect("empty overlay");
        let empty_a11y = a11y_of(&snapshot, empty);
        assert!(!empty_a11y.hidden, "空表时空态浮层可见");
        let labels: Vec<&str> = empty_a11y
            .children
            .iter()
            .filter_map(|id| snapshot.node(*id).and_then(|node| node.value.as_deref()))
            .collect();
        assert_eq!(labels, vec!["empty"], "空态文案作为 Label 暴露");
        table.set_rows(app.app_mut(), 2).expect("rows");
        settle(&mut app, &table);
        let snapshot = full_a11y(&mut app);
        assert!(a11y_of(&snapshot, empty).hidden, "有行后空态浮层 hidden");
        let row0 = table.list().row_node(app.app(), 0).expect("row 0");
        assert_eq!(a11y_of(&snapshot, row0).role, A11yRole::Row);
        assert_eq!(
            a11y_of(&snapshot, table_node).row_count,
            Some(3),
            "2 行 + 表头行"
        );
        let header0 = table.header_node(app.app(), 0).expect("header 0");
        let header_row = app
            .app()
            .window(app.window_id())
            .expect("window")
            .tree()
            .get(header0)
            .expect("header cell")
            .parent()
            .expect("header row");
        assert_eq!(a11y_of(&snapshot, header_row).row_count, Some(3));
        assert_eq!(a11y_of(&snapshot, header0).row_count, Some(3));
        assert_eq!(a11y_of(&snapshot, row0).row_count, Some(3));
        assert!(
            snapshot
                .visible_nodes_in_batch()
                .all(|node| node.value.as_deref() != Some("empty")),
            "空态文案不得出现在可见 a11y 节点里"
        );
    }

    #[test]
    fn releasing_the_table_scope_reclaims_its_signal_slots() {
        let mut app = TestApp::new();
        let table = table_with_rows(&mut app, 3);
        let released = table.release(app.app_mut());
        assert!(
            released >= 8,
            "排序状态 + 列表五条信号 + 两条滚动条各五条信号必须被回收（实际 {released}）"
        );
        assert_eq!(table.release(app.app_mut()), 0, "重复释放幂等");
    }

    const PAGE_H: f32 = 240.0;

    fn demo_columns() -> Vec<Column> {
        vec![
            Column::fixed("代码", 140.0).sortable(),
            Column::fixed("名称", 160.0).sortable(),
            Column::flex("备注", 2.0, px(80.0)),
            Column::flex("分类", 1.0, px(96.0)),
            Column::fixed("价格", 100.0).align(ColumnAlign::End),
            Column::fixed("涨跌", 100.0).align(ColumnAlign::End),
            Column::fixed("成交额", 120.0).align(ColumnAlign::End),
        ]
    }

    struct PageTable {
        app: TestApp,
        table: Table,
        page: ui_core::interaction::ScrollHandle,
        page_node: NodeId,
        header_content: NodeId,
    }

    fn page_table(width: f32, columns: Vec<Column>, rows: usize) -> PageTable {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let page = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .id("page")
                .style(
                    StyleBuilder::new()
                        .relative()
                        .width(px(width))
                        .height(px(PAGE_H))
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("page")
        };
        let page_handle = app
            .app_mut()
            .make_scroll(window, page, ScrollConfig::default())
            .expect("page scroll");
        app.app_mut()
            .set_scroll_content(&page_handle, LogicalSize::new(px(width), px(PAGE_H * 4.0)))
            .expect("page content");
        let table = table_with(
            &mut app,
            columns,
            TableOptions::fixed(px(28.0)),
            rows,
            text_cell,
        );
        table
            .mount(app.app_mut(), window, page)
            .expect("mount table");
        settle(&mut app, &table);
        let header_content = table
            .inner
            .geometry
            .borrow()
            .header_content
            .expect("header content node");
        PageTable {
            app,
            table,
            page: page_handle,
            page_node: page,
            header_content,
        }
    }

    impl PageTable {
        fn handle(&self) -> ui_core::interaction::ScrollHandle {
            let viewport = self.table.list().viewport_node().expect("viewport");
            self.app
                .app()
                .scroll_handle(self.app.window_id(), viewport)
                .expect("list handle")
        }

        fn state(&self) -> ui_core::interaction::ScrollState {
            self.app.app().scroll_state(&self.handle()).expect("state")
        }

        fn offset(&self) -> LogicalPoint {
            self.state().offset
        }

        fn page_offset(&self) -> LogicalPoint {
            self.app
                .app()
                .scroll_state(&self.page)
                .expect("page state")
                .offset
        }

        fn settle(&mut self) {
            for _ in 0..3 {
                let _ = self.table.sync(self.app.app_mut()).expect("sync");
                let _ = self.app.drain().expect("frame");
            }
        }

        fn point_of(&self, node: NodeId) -> LogicalPoint {
            let bounds = self.app.probe().bounds(node).expect("bounds");
            LogicalPoint::new(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            )
        }

        fn wheel(&mut self, point: LogicalPoint, delta: LogicalPoint) {
            let _ = self
                .app
                .inject(InputEvent::scroll_lines(point, delta, Modifiers::empty()));
            self.settle();
        }

        fn track_bounds(&self, bar: &ScrollBar) -> LogicalRect {
            let (track, _) = bar.nodes();
            self.app
                .probe()
                .bounds(track.expect("scrollbar track"))
                .expect("track bounds")
        }

        fn thumb_bounds(&self, bar: &ScrollBar) -> LogicalRect {
            let track = self.track_bounds(bar);
            let thumb = bar.thumb_rect(self.app.app()).expect("thumb rect");
            LogicalRect::new(
                track.origin.x + thumb.origin.x,
                track.origin.y + thumb.origin.y,
                thumb.size.width,
                thumb.size.height,
            )
        }

        fn drag(&mut self, from: LogicalPoint, delta: LogicalPoint) {
            let _ = self.app.inject(InputEvent::pointer_down(
                from,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            let middle = LogicalPoint::new(from.x + delta.x / 2.0, from.y + delta.y / 2.0);
            let _ = self
                .app
                .inject(InputEvent::pointer_move(middle, Modifiers::empty()));
            let end = LogicalPoint::new(from.x + delta.x, from.y + delta.y);
            let _ = self
                .app
                .inject(InputEvent::pointer_move(end, Modifiers::empty()));
            let _ = self.app.inject(InputEvent::pointer_up(
                end,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            self.settle();
        }

        fn header_style(&self) -> ui_core::paint::style::Style {
            self.app
                .app()
                .window(self.app.window_id())
                .expect("window")
                .tree()
                .get(self.header_content)
                .expect("header content")
                .style()
                .clone()
        }

        fn header_inset_left(&self) -> LogicalPx {
            match self.header_style().inner().inset.left.expand() {
                ExpandedLengthPercentageAuto::Length(value) => px(value),
                other => panic!("sticky header left must be an absolute length: {other:?}"),
            }
        }

        fn header_style_width(&self) -> LogicalPx {
            match self.header_style().inner().size.width.expand() {
                ExpandedDimension::Length(value) => px(value),
                other => panic!("sticky header width must be an absolute length: {other:?}"),
            }
        }

        fn drag_horizontal(&mut self, delta: f32) {
            let thumb = self.thumb_bounds(self.table.horizontal_bar());
            let origin = LogicalPoint::new(
                thumb.origin.x + thumb.size.width / 2.0,
                thumb.origin.y + thumb.size.height / 2.0,
            );
            self.drag(origin, LogicalPoint::new(px(delta), px(0.0)));
        }

        fn drag_vertical(&mut self, delta: f32) {
            let thumb = self.thumb_bounds(self.table.vertical_bar());
            let origin = LogicalPoint::new(
                thumb.origin.x + thumb.size.width / 2.0,
                thumb.origin.y + thumb.size.height / 2.0,
            );
            self.drag(origin, LogicalPoint::new(px(0.0), px(delta)));
        }

        fn resize_page(&mut self, width: f32) {
            let window = self.app.window_id();
            let tree = self
                .app
                .app_mut()
                .window_mut(window)
                .expect("window")
                .tree_mut();
            let mut style = tree.get(self.page_node).expect("page").style().clone();
            style.width_length(px(width));
            tree.set_style(self.page_node, style).expect("resize page");
        }

        fn drain_frames(&mut self, count: usize) {
            for _ in 0..count {
                let _ = self.app.drain().expect("frame");
            }
        }
    }

    fn ancestor_offset(app: &App, window: WindowId, node: NodeId) -> LogicalPoint {
        let Some(target) = app.window(window) else {
            return LogicalPoint::ZERO;
        };
        let tree = target.tree();
        let mut total = LogicalPoint::ZERO;
        let mut cursor = tree.get(node).ok().and_then(|target| target.parent());
        while let Some(current) = cursor {
            if let Some(handle) = app.scroll_handle(window, current)
                && let Some(state) = app.scroll_state(&handle)
            {
                total = LogicalPoint::new(total.x + state.offset.x, total.y + state.offset.y);
            }
            cursor = tree.get(current).ok().and_then(|target| target.parent());
        }
        total
    }

    fn screen_origin(app: &App, window: WindowId, node: NodeId) -> Option<LogicalPoint> {
        let target = app.window(window)?;
        let origin = target.tree().get(node).ok()?.layout().bounds.origin;
        let offset = ancestor_offset(app, window, node);
        Some(LogicalPoint::new(origin.x - offset.x, origin.y - offset.y))
    }

    #[test]
    fn wide_viewport_shows_no_horizontal_bar() {
        let mut app = TestApp::new();
        let table = table_with(
            &mut app,
            demo_columns(),
            TableOptions::fixed(px(28.0)),
            20,
            text_cell,
        );
        mount_in(&mut app, &table, 1100.0, 240.0);
        let viewport = table.list().viewport_node().expect("viewport");
        let handle = app
            .app()
            .scroll_handle(app.window_id(), viewport)
            .expect("handle");
        let state = app.app().scroll_state(&handle).expect("state");
        assert_eq!(
            state.content.width,
            px(796.0),
            "内容宽 = 固定列 + 弹性最小宽"
        );
        assert!(
            state.viewport.width >= state.content.width,
            "前置：宽视口不小于内容宽：{state:?}"
        );
        assert!(
            !table.horizontal_bar().scrollable(app.app()),
            "宽视口不出现水平条"
        );
        assert!(
            table.horizontal_bar().nodes().0.is_some(),
            "水平条节点仍然存在（不可见而非未构建）"
        );
    }

    #[test]
    fn narrow_viewport_shows_horizontal_bar_with_the_remaining_range() {
        let mut app = TestApp::new();
        let table = table_with(
            &mut app,
            demo_columns(),
            TableOptions::fixed(px(28.0)),
            20,
            text_cell,
        );
        mount_in(&mut app, &table, 420.0, 240.0);
        let viewport = table.list().viewport_node().expect("viewport");
        let handle = app
            .app()
            .scroll_handle(app.window_id(), viewport)
            .expect("handle");
        let state = app.app().scroll_state(&handle).expect("state");
        assert_eq!(state.axes, ScrollAxes::Both, "表体是双轴容器");
        assert_eq!(
            state.content.width - state.viewport.width,
            px(796.0) - px(420.0),
            "横向范围 = 内容宽 - 视口宽"
        );
        assert!(
            table.horizontal_bar().scrollable(app.app()),
            "窄视口出现水平条"
        );
        assert!(
            table.vertical_bar().scrollable(app.app()),
            "长内容出现垂直条"
        );
    }

    #[test]
    fn sticky_header_follows_the_list_offset_and_columns_align() {
        let mut harness = page_table(420.0, demo_columns(), 20);
        let handle = harness.handle();
        harness
            .app
            .app_mut()
            .scroll_to(&handle, LogicalPoint::new(px(100.0), LogicalPx::ZERO))
            .expect("horizontal scroll");
        harness.settle();
        assert_eq!(harness.offset().x, px(100.0), "前置：横向偏移");
        assert_eq!(
            harness.header_inset_left(),
            px(-100.0),
            "表头内容左移 = -list.offset.x"
        );
        assert_eq!(
            harness.header_style_width(),
            px(796.0),
            "窄视口下表头内容宽 = 内容宽"
        );

        let header_content = harness
            .app
            .probe()
            .bounds(harness.header_content)
            .expect("bounds");
        for column in [0usize, 2] {
            let header = harness
                .table
                .header_node(harness.app.app(), column)
                .expect("header cell");
            let cell = harness
                .table
                .cell_node(harness.app.app(), 0, column)
                .expect("body cell");
            let header = harness.app.probe().bounds(header).expect("header bounds");
            let cell = harness.app.probe().bounds(cell).expect("cell bounds");
            assert!(
                ((header.origin.x - header_content.origin.x) - cell.origin.x).abs() < px(0.5),
                "列 {column} 的表头/表体内容坐标必须一致：header {:?} content {:?} cell {:?}",
                header.origin,
                header_content.origin,
                cell.origin
            );
        }

        harness.wheel(
            harness.point_of(
                harness
                    .table
                    .header_node(harness.app.app(), 1)
                    .expect("header"),
            ),
            LogicalPoint::new(px(-1.0), px(0.0)),
        );
        assert!(
            harness.offset().x > px(100.0),
            "表头滚轮横向滚动表体：{:?}",
            harness.offset()
        );
        assert_eq!(
            harness.header_inset_left(),
            -harness.offset().x,
            "表头随表体偏移同步"
        );

        let window = harness.app.window_id();
        {
            let tree = harness
                .app
                .app_mut()
                .window_mut(window)
                .expect("window")
                .tree_mut();
            let mut style = tree.get(harness.page_node).expect("page").style().clone();
            style.width_length(px(900.0));
            tree.set_style(harness.page_node, style)
                .expect("resize page");
        }
        for _ in 0..4 {
            let _ = harness.app.drain().expect("frame");
            let _ = harness.table.sync(harness.app.app_mut()).expect("sync");
        }
        assert_eq!(
            harness.offset().x,
            LogicalPx::ZERO,
            "变宽后横向偏移被夹回 0"
        );
        let viewport_width = harness.state().viewport.width;
        assert_eq!(
            harness.header_style_width(),
            viewport_width,
            "宽视口下表头内容宽跟随视口（稳态列对齐）"
        );
        let header_content = harness
            .app
            .probe()
            .bounds(harness.header_content)
            .expect("bounds");
        for column in [0usize, 2] {
            let header = harness
                .table
                .header_node(harness.app.app(), column)
                .expect("header cell");
            let cell = harness
                .table
                .cell_node(harness.app.app(), 0, column)
                .expect("body cell");
            let header = harness.app.probe().bounds(header).expect("header bounds");
            let cell = harness.app.probe().bounds(cell).expect("cell bounds");
            assert!(
                ((header.origin.x - header_content.origin.x) - cell.origin.x).abs() < px(0.5),
                "resize 稳态列 {column} 必须对齐：header {:?} content {:?} cell {:?}",
                header.origin,
                header_content.origin,
                cell.origin
            );
        }
    }

    #[test]
    fn wheel_over_header_scrolls_the_body_and_not_the_page() {
        let mut harness = page_table(420.0, demo_columns(), 20);
        let header = harness
            .table
            .header_node(harness.app.app(), 2)
            .expect("header cell");
        harness.wheel(
            harness.point_of(header),
            LogicalPoint::new(px(-1.0), px(0.0)),
        );
        let offset = harness.offset();
        assert!(offset.x > px(0.0), "表头滚轮横向滚动表体：{offset:?}");
        assert_eq!(offset.y, LogicalPx::ZERO, "横向滚轮不动纵向");
        assert_eq!(
            harness.page_offset(),
            LogicalPoint::ZERO,
            "表头滚轮不得滚外层页面"
        );

        let header_cell = harness
            .table
            .header_node(harness.app.app(), 0)
            .expect("header");
        let header_parent = harness
            .app
            .app()
            .window(harness.app.window_id())
            .expect("window")
            .tree()
            .get(header_cell)
            .expect("header")
            .parent()
            .expect("header row");
        assert!(
            harness
                .app
                .app()
                .scroll_handle(harness.app.window_id(), header_parent)
                .is_none(),
            "表头不是滚动容器（无 ScrollSlot）"
        );

        harness.wheel(
            harness.point_of(header),
            LogicalPoint::new(px(0.0), px(-10.0)),
        );
        assert!(
            harness.offset().y > px(0.0),
            "表头纵向滚轮仍滚表体：{:?}",
            harness.offset()
        );
        assert_eq!(
            harness.page_offset(),
            LogicalPoint::ZERO,
            "表头纵向滚轮也不得滚页面"
        );
    }

    #[test]
    fn two_axis_drags_do_not_reset_each_other() {
        let mut harness = page_table(420.0, demo_columns(), 40);
        harness.drag_vertical(20.0);
        let after_vertical = harness.offset();
        assert!(
            after_vertical.y > px(0.0),
            "垂直条拖动必须滚动纵向：{after_vertical:?}"
        );
        assert_eq!(after_vertical.x, LogicalPx::ZERO, "垂直条不得复位横向");

        harness.drag_horizontal(30.0);
        let after_horizontal = harness.offset();
        assert!(
            after_horizontal.x > px(0.0),
            "水平条拖动必须滚动横向：{after_horizontal:?}"
        );
        assert_eq!(
            after_horizontal.y, after_vertical.y,
            "水平条不得复位纵向：{after_horizontal:?}"
        );

        let x = after_horizontal.x;
        harness.drag_vertical(20.0);
        let after_second_vertical = harness.offset();
        assert!(after_second_vertical.y > after_vertical.y);
        assert_eq!(after_second_vertical.x, x, "再拖垂直条仍保留横向偏移");
    }

    #[test]
    fn table_release_clears_the_header_proxy_and_is_idempotent() {
        let mut harness = page_table(420.0, demo_columns(), 20);
        let header = harness
            .table
            .header_node(harness.app.app(), 2)
            .expect("header");
        let header_point = harness.point_of(header);
        let released = harness.table.release(harness.app.app_mut());
        assert!(released >= 8, "表头/两条 bar/列表/排序必须回收：{released}");
        assert_eq!(
            harness.table.release(harness.app.app_mut()),
            0,
            "重复释放幂等"
        );
        let _ = harness.app.inject(InputEvent::scroll_lines(
            header_point,
            LogicalPoint::new(px(0.0), px(-1.0)),
            Modifiers::empty(),
        ));
        assert_eq!(
            harness.offset(),
            LogicalPoint::ZERO,
            "释放后表头不再代理到表体"
        );
        assert!(
            harness.page_offset().y > px(0.0),
            "释放后滚轮落到外层页面：{:?}",
            harness.page_offset()
        );

        let outer = harness.table.outer_node().expect("outer");
        let window = harness.app.window_id();
        harness
            .app
            .app_mut()
            .remove_node(window, outer)
            .expect("remove table subtree");
        assert_eq!(
            harness.table.release(harness.app.app_mut()),
            0,
            "节点卸载后重复释放不 panic"
        );
        let _ = harness.app.drain().expect("frame");
    }

    #[test]
    fn horizontal_scroll_state_keeps_both_axes_after_rewindowing() {
        let mut app = TestApp::new();
        let table = table_with(
            &mut app,
            demo_columns(),
            TableOptions::fixed(px(28.0)),
            100,
            text_cell,
        );
        mount_in(&mut app, &table, 420.0, 240.0);
        let viewport = table.list().viewport_node().expect("viewport");
        let handle = app
            .app()
            .scroll_handle(app.window_id(), viewport)
            .expect("handle");
        let state = app.app().scroll_state(&handle).expect("state");
        assert_eq!(state.axes, ScrollAxes::Both);
        assert_eq!(state.content.width, px(796.0));

        app.app_mut()
            .scroll_to(&handle, LogicalPoint::new(px(40.0), px(2_000.0)))
            .expect("scroll");
        let _ = app.drain().expect("frame");
        let _ = table.sync(app.app_mut()).expect("sync");
        let state = app.app().scroll_state(&handle).expect("state");
        assert!(state.offset.y > LogicalPx::ZERO, "纵向换页后：{state:?}");
        assert_eq!(
            state.content.width,
            px(796.0),
            "rewindow/extents 不得覆盖横向内容宽"
        );
    }

    #[test]
    fn table_horizontal_a11y_exposes_both_axes() {
        let mut app = TestApp::new();
        let table = table_with(
            &mut app,
            demo_columns(),
            TableOptions::fixed(px(28.0)),
            20,
            text_cell,
        );
        mount_in(&mut app, &table, 420.0, 240.0);
        app.enable_a11y_capture();
        let _ = app.drain().expect("frame");
        let snapshot = app.a11y().expect("a11y snapshot").clone();
        let table_node = table.outer_node().expect("table node");
        let table_a11y = a11y_of(&snapshot, table_node);
        let scroll = table_a11y.scroll.expect("Table 双轴滚动语义");
        assert!(scroll.x_max > px(0.0), "窄表宣告横向范围：{scroll:?}");
        assert!(scroll.y_max > px(0.0), "表体宣告纵向范围：{scroll:?}");
        assert_eq!(scroll.orientation, None, "Both 不自动设置 orientation");
        for action in [
            A11yAction::ScrollLeft,
            A11yAction::ScrollRight,
            A11yAction::ScrollUp,
            A11yAction::ScrollDown,
        ] {
            assert!(
                table_a11y.actions.contains(&action),
                "双轴表必须宣告 {action:?}"
            );
        }
        let header = table.header_node(app.app(), 0).expect("header");
        assert!(
            a11y_of(&snapshot, header).scroll.is_none(),
            "表头不是滚动容器"
        );
        let scroll_containers: Vec<A11yRole> = snapshot
            .visible_nodes_in_batch()
            .filter(|node| node.scroll.is_some())
            .map(|node| node.role)
            .collect();
        assert_eq!(
            scroll_containers,
            vec![A11yRole::Table, A11yRole::GenericContainer],
            "滚动语义只上提到 Table 与行窗口，表头/单元格不得再出现滚动容器"
        );
    }

    #[test]
    fn wide_table_does_not_advertise_horizontal_scroll_actions() {
        let mut app = TestApp::new();
        let table = table_with(
            &mut app,
            demo_columns(),
            TableOptions::fixed(px(28.0)),
            20,
            text_cell,
        );
        mount_in(&mut app, &table, 1100.0, 240.0);
        app.enable_a11y_capture();
        let _ = app.drain().expect("frame");
        let snapshot = app.a11y().expect("a11y snapshot").clone();
        let table_node = table.outer_node().expect("table node");
        let table_a11y = a11y_of(&snapshot, table_node);
        let scroll = table_a11y.scroll.expect("Table 双轴滚动语义");
        assert_eq!(scroll.x_max, px(0.0), "宽视口没有横向范围：{scroll:?}");
        assert!(scroll.y_max > px(0.0), "表体仍有纵向范围：{scroll:?}");
        for action in [
            A11yAction::ScrollUp,
            A11yAction::ScrollDown,
            A11yAction::ScrollIntoView,
            A11yAction::SetScrollOffset,
        ] {
            assert!(
                table_a11y.actions.contains(&action),
                "宽表仍必须宣告 {action:?}：{:?}",
                table_a11y.actions
            );
        }
        for action in [A11yAction::ScrollLeft, A11yAction::ScrollRight] {
            assert!(
                !table_a11y.actions.contains(&action),
                "x_max == 0 的宽表不得宣告 {action:?}：{:?}",
                table_a11y.actions
            );
        }
    }

    #[test]
    fn scrollbar_tail_gaps_do_not_overlap() {
        let mut app = TestApp::new();
        let table = table_with(
            &mut app,
            demo_columns(),
            TableOptions::fixed(px(28.0)),
            20,
            text_cell,
        );
        mount_in(&mut app, &table, 420.0, 240.0);
        let outer = table.outer_node().expect("outer");
        let outer = app.probe().bounds(outer).expect("outer bounds");
        let vertical = table.vertical_bar();
        let horizontal = table.horizontal_bar();
        let (vertical_track, _) = vertical.nodes();
        let (horizontal_track, _) = horizontal.nodes();
        let vertical_track = app
            .probe()
            .bounds(vertical_track.expect("vertical track"))
            .expect("vertical bounds");
        let horizontal_track = app
            .probe()
            .bounds(horizontal_track.expect("horizontal track"))
            .expect("horizontal bounds");
        assert!(
            (vertical_track.max().y - (outer.max().y - px(4.0))).abs() < px(0.5),
            "垂直条底边 = 表底 - 4：{:?} vs {:?}",
            vertical_track,
            outer
        );
        assert!(
            (horizontal_track.max().x - (outer.max().x - px(4.0))).abs() < px(0.5),
            "水平条右边 = 表右 - 4：{:?} vs {:?}",
            horizontal_track,
            outer
        );
        assert!(
            vertical_track.max().y <= horizontal_track.origin.y + px(0.01),
            "垂直条与水平条不得重叠：{vertical_track:?} {horizontal_track:?}"
        );
        assert!(
            horizontal_track.max().x <= vertical_track.origin.x + px(0.01),
            "水平条不得伸进垂直条：{horizontal_track:?} {vertical_track:?}"
        );
    }

    #[test]
    fn table_content_width_is_fixed_sum_plus_flex_min_sum() {
        let mut app = TestApp::new();
        let columns = vec![
            Column::fixed("a", 140.0).span(2),
            Column::fixed("b", 160.0),
            Column::flex("c", 2.0, px(80.0)),
            Column::flex("d", 1.0, px(96.0)).span(2),
            Column::fixed("e", 100.0),
        ];
        let table = table_with(
            &mut app,
            columns,
            TableOptions::fixed(px(28.0)),
            4,
            text_cell,
        );
        assert_eq!(
            table.content_width(),
            px(140.0 + 160.0 + 80.0 + 96.0 + 100.0),
            "内容宽 = 所有固定列宽 + 所有弹性列最小宽（被跨列覆盖的列仍计入）"
        );
        mount_in(&mut app, &table, 420.0, 240.0);
        let viewport = table.list().viewport_node().expect("viewport");
        let handle = app
            .app()
            .scroll_handle(app.window_id(), viewport)
            .expect("handle");
        let state = app.app().scroll_state(&handle).expect("state");
        assert_eq!(state.content.width, table.content_width());
        assert_eq!(
            state.content.width - state.viewport.width,
            px(576.0) - px(420.0),
            "横向范围按内容宽计算"
        );
    }

    #[test]
    fn all_flex_zero_min_width_tables_keep_the_viewport_layout() {
        let mut app = TestApp::new();
        let columns = (0..8)
            .map(|index| {
                let column = Column::flex(smol_str::format_smolstr!("c{index}"), 1.0, px(0.0));
                if index == 0 { column.span(6) } else { column }
            })
            .collect();
        let table = table_with(
            &mut app,
            columns,
            TableOptions::fixed(px(28.0)),
            8,
            text_cell,
        );
        table.set_column_span(|row, column| if row == 0 && column == 0 { 6 } else { 1 });
        mount_in(&mut app, &table, 240.0, 240.0);
        assert_eq!(
            table.content_width(),
            LogicalPx::ZERO,
            "全 flex 零最小宽：内容宽恒为 0"
        );
        let viewport = table.list().viewport_node().expect("viewport");
        let handle = app
            .app()
            .scroll_handle(app.window_id(), viewport)
            .expect("handle");
        let state = app.app().scroll_state(&handle).expect("state");
        assert_eq!(
            state.axes,
            ScrollAxes::Vertical,
            "零内容宽不得把 List 切成双轴：{state:?}"
        );
        assert_eq!(
            state.content.width, state.viewport.width,
            "零内容宽没有横向范围：{state:?}"
        );
        let merged = table.cell_node(app.app(), 0, 0).expect("merged cell");
        let merged = app.probe().bounds(merged).expect("merged bounds");
        assert!(
            merged.size.width > px(60.0),
            "跨 6 列单元格按视口摊开而不是塌到最小内容宽：{merged:?}"
        );
        let single = table.cell_node(app.app(), 0, 6).expect("single cell");
        let single = app.probe().bounds(single).expect("single bounds");
        assert!(
            merged.size.width > single.size.width,
            "跨列宽于单列：{merged:?} vs {single:?}"
        );
        assert!(single.size.width > px(20.0), "单列按视口八等分：{single:?}");
        assert!(
            !table.horizontal_bar().scrollable(app.app()),
            "零内容宽不出现水平条"
        );
        assert!(
            table.vertical_bar().scrollable(app.app()),
            "8 行 224 px 超过视口高：垂直条仍可滚"
        );
        let header = table.header_node(app.app(), 0).expect("header 0");
        let header = app.probe().bounds(header).expect("header bounds");
        assert!(
            header.size.width > px(60.0),
            "表头跨列与表体同宽：{header:?}"
        );
        assert!(
            !table.sync(app.app_mut()).expect("sync"),
            "稳态 sync 不再变化"
        );
    }

    #[test]
    fn flex_columns_require_an_explicit_min_width() {
        let column = Column::flex("flex", 2.0, px(80.0));
        assert_eq!(
            column.width,
            ColumnWidth::Flex {
                weight: 2.0,
                min_width: px(80.0)
            }
        );
        assert_eq!(column.width.contribution(), 80.0);
        assert_eq!(column.width.weight(), 2.0);
        let clamped = Column::flex("clamped", -1.0, px(-5.0));
        assert_eq!(
            clamped.width,
            ColumnWidth::Flex {
                weight: 0.0,
                min_width: px(0.0)
            }
        );
        let fixed = Column::fixed("fixed", 120.0);
        assert_eq!(fixed.width.contribution(), 120.0);
        assert_eq!(fixed.width.weight(), 0.0);
    }

    #[test]
    fn table_header_wheel_scrolls_the_body() {
        let mut harness = page_table(420.0, demo_columns(), 40);
        let header = harness
            .table
            .header_node(harness.app.app(), 0)
            .expect("header cell");
        harness.wheel(
            harness.point_of(header),
            LogicalPoint::new(px(0.0), px(-1.0)),
        );
        let offset = harness.offset();
        assert_eq!(offset.y, px(48.0), "一行滚动 = 48 px：{offset:?}");
        assert_eq!(offset.x, LogicalPx::ZERO);
        assert_eq!(
            harness.page_offset(),
            LogicalPoint::ZERO,
            "表头滚轮不得滚外层页面"
        );
    }

    #[test]
    fn attach_reports_a_missing_viewport_and_keeps_release_idempotent() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let table = table_with(
            &mut app,
            demo_columns(),
            TableOptions::fixed(px(28.0)),
            4,
            text_cell,
        );
        let error = table
            .attach(app.app_mut(), window)
            .expect_err("未建树的表不得 attach");
        assert!(
            matches!(error, EngineError::NotScrollable { .. }),
            "缺失可滚动视口必须报 NotScrollable：{error:?}"
        );
        assert!(table.release(app.app_mut()) > 0, "释放表的信号作用域");
        assert_eq!(table.release(app.app_mut()), 0, "重复释放幂等");
    }

    #[test]
    fn attach_failure_rolls_back_attached_bars_and_the_header_proxy() {
        for doomed in ["vertical", "header"] {
            let mut app = TestApp::new();
            let window = app.window_id();
            let root = app.app().window(window).expect("window").tree().root();
            let holder = {
                let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
                NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .width(px(420.0))
                            .height(px(240.0))
                            .build(),
                    )
                    .build(tree, root)
                    .expect("holder")
            };
            let table = table_with(
                &mut app,
                demo_columns(),
                TableOptions::fixed(px(28.0)),
                20,
                text_cell,
            );
            {
                let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
                table.build(tree, holder).expect("build");
            }
            let header = table.inner.geometry.borrow().header.expect("header");
            let vertical = table.vertical_bar().nodes().0.expect("vertical track");
            let horizontal = table.horizontal_bar().nodes().0.expect("horizontal track");
            let target = if doomed == "vertical" {
                vertical
            } else {
                header
            };
            app.app_mut()
                .remove_node(window, target)
                .expect("remove target");
            let error = table
                .attach(app.app_mut(), window)
                .expect_err("代理目标缺失必须失败");
            assert!(
                matches!(error, EngineError::StaleNode { .. }),
                "缺失代理目标必须报 StaleNode：{error:?}"
            );
            {
                let tree = app.app().window(window).expect("window").tree();
                assert!(
                    tree.scroll_proxy(header).is_none(),
                    "失败必须回滚表头代理（{doomed}）"
                );
                assert!(
                    tree.scroll_proxy(horizontal).is_none(),
                    "失败必须回滚水平条代理（{doomed}）"
                );
                if doomed == "header" {
                    assert!(
                        tree.scroll_proxy(vertical).is_none(),
                        "失败必须回滚垂直条代理"
                    );
                }
            }
            assert_eq!(
                table.release(app.app_mut()),
                0,
                "回滚后再次释放不得重复回收（{doomed}）"
            );
        }
    }

    #[test]
    fn attach_rolls_back_when_the_list_attach_fails() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let holder = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(px(420.0))
                        .height(px(240.0))
                        .build(),
                )
                .build(tree, root)
                .expect("holder")
        };
        let table = table_with(
            &mut app,
            demo_columns(),
            TableOptions::fixed(px(28.0)),
            20,
            text_cell,
        );
        {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            table.build(tree, holder).expect("build");
        }
        let header = table.inner.geometry.borrow().header.expect("header");
        let vertical = table.vertical_bar().nodes().0.expect("vertical track");
        let horizontal = table.horizontal_bar().nodes().0.expect("horizontal track");
        let viewport = table.list().viewport_node().expect("viewport");
        app.app_mut()
            .remove_node(window, viewport)
            .expect("remove viewport");
        let error = table
            .attach(app.app_mut(), window)
            .expect_err("列表 attach 失败必须上抛");
        assert!(
            matches!(error, EngineError::StaleNode { node } if node == viewport),
            "缺失列表视口必须报 StaleNode：{error:?}"
        );
        {
            let tree = app.app().window(window).expect("window").tree();
            assert!(tree.scroll_proxy(header).is_none(), "失败不得留下表头代理");
            assert!(
                tree.scroll_proxy(vertical).is_none(),
                "失败不得留下垂直条代理"
            );
            assert!(
                tree.scroll_proxy(horizontal).is_none(),
                "失败不得留下水平条代理"
            );
        }
        assert_eq!(
            table.release(app.app_mut()),
            0,
            "回滚后再次释放不得重复回收"
        );
    }

    #[test]
    fn header_stays_pinned_while_the_body_scrolls_vertically() {
        let mut harness = page_table(420.0, demo_columns(), 40);
        let window = harness.app.window_id();
        let header = harness
            .table
            .header_node(harness.app.app(), 0)
            .expect("header cell");
        let header_y = screen_origin(harness.app.app(), window, header)
            .expect("header screen position")
            .y;
        let content_y = screen_origin(harness.app.app(), window, harness.header_content)
            .expect("header content screen position")
            .y;
        let viewport = harness.table.list().viewport_node().expect("viewport");
        harness.wheel(
            harness.point_of(viewport),
            LogicalPoint::new(px(0.0), px(-2.0)),
        );
        let _ = harness.app.drain().expect("frame");
        assert!(
            harness.offset().y > LogicalPx::ZERO,
            "表体纵向滚动生效：{:?}",
            harness.offset()
        );
        assert_eq!(
            screen_origin(harness.app.app(), window, header)
                .expect("header screen position")
                .y,
            header_y,
            "表体纵向滚动后 header 的屏幕 y 不变"
        );
        assert_eq!(
            screen_origin(harness.app.app(), window, harness.header_content)
                .expect("header content screen position")
                .y,
            content_y,
            "表体纵向滚动后 header_content 的屏幕 y 不变"
        );
        assert_eq!(harness.page_offset(), LogicalPoint::ZERO, "不滚外层页面");
    }

    #[test]
    fn resizing_from_wide_to_narrow_clamps_the_horizontal_offset_on_sync() {
        let mut harness = page_table(300.0, demo_columns(), 40);
        let handle = harness.handle();
        harness
            .app
            .app_mut()
            .scroll_to(&handle, LogicalPoint::new(px(400.0), LogicalPx::ZERO))
            .expect("horizontal scroll");
        harness.settle();
        let content = harness.state().content.width;
        assert_eq!(harness.offset().x, px(400.0), "前置：窄视口横向偏移");
        assert!(
            harness.state().viewport.width < content,
            "前置：窄视口内容溢出"
        );

        harness.resize_page(900.0);
        harness.drain_frames(3);
        assert_eq!(
            harness.offset().x,
            px(400.0),
            "宽视口不夹虚拟列表的 x：越界偏移保留到 sync"
        );

        harness.resize_page(420.0);
        harness.drain_frames(3);
        let viewport = harness.state().viewport;
        assert!(
            (viewport.width - px(420.0)).abs() < px(1.0),
            "视口已缩窄：{viewport:?}"
        );
        assert!(
            harness.table.sync(harness.app.app_mut()).expect("sync"),
            "宽→窄后 sync 必须夹回越界 x"
        );
        let state = harness.state();
        let max_x = (content - state.viewport.width).max(LogicalPx::ZERO);
        assert_eq!(
            state.offset.x, max_x,
            "宽→窄后 x 夹回 max_x：{:?}",
            state.offset
        );
        assert_eq!(harness.header_inset_left(), -state.offset.x, "表头跟随夹紧");
        assert_eq!(
            harness.header_style_width(),
            content,
            "窄视口下表头内容宽 = 内容宽"
        );
        let header_content = harness
            .app
            .probe()
            .bounds(harness.header_content)
            .expect("header content bounds");
        for column in [0usize, 2] {
            let header = harness
                .table
                .header_node(harness.app.app(), column)
                .expect("header cell");
            let cell = harness
                .table
                .cell_node(harness.app.app(), 0, column)
                .expect("body cell");
            let header = harness.app.probe().bounds(header).expect("header bounds");
            let cell = harness.app.probe().bounds(cell).expect("cell bounds");
            assert!(
                ((header.origin.x - header_content.origin.x) - cell.origin.x).abs() < px(0.5),
                "resize 夹紧后列 {column} 必须对齐：header {:?} content {:?} cell {:?}",
                header.origin,
                header_content.origin,
                cell.origin
            );
        }
        let _ = harness.app.drain().expect("frame");
        assert!(
            !harness.table.sync(harness.app.app_mut()).expect("sync"),
            "夹紧后 sync 稳态不再变化"
        );
    }
}
