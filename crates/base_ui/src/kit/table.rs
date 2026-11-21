//! 表格（P2-2）：列定义 + 固定表头 + 行虚拟化（复用 [`List`] 内核）+ 选中 / 多选。
//!
//! # 结构
//!
//! ```text
//! Table（flex_col）
//! ├── 表头（固定，不随行滚动）：单元格 + 排序指示 + 底部分隔线
//! └── 行体（flex_1，min 0）
//!     ├── List（引擎虚拟列表 + 自绘滚动条）  ← 行 = 单元格行 + 行分隔线
//!     └── 空态浮层（absolute；`visible = count == 0`）
//! ```
//!
//! 行内容的虚拟化 / 复用 / 行高（含单元格换行的变高表格）/ 选中 / hover 全部走 [`List`]，
//! 本模块只负责「一行里怎么摆单元格」。
//!
//! # 列宽
//!
//! [`ColumnWidth::Fixed`] 是常量宽度；[`ColumnWidth::Flex`] 是权重（表头与行体的单元格用
//! **同一份** `Style` 计算，所以两处列宽严格对齐）。引擎滚动容器是单轴纵向的，因此
//! 「横向滚动的冻结列」不在本批范围（需要横向滚动容器 + 表头/行体两份窗口，属后续批次）。
//!
//! # 排序
//!
//! 表头可排序列点击后把 [`SortOrder`] 写进 `Signal` 并调用 [`Table::on_sort`] 注册的回调
//! （表格不持数据：真正的行重排由应用在回调里完成）。排序状态是纯逻辑
//! [`next_sort_order`]，有单测。
//!
//! # 空态
//!
//! [`Table::new`] 的 `empty_text` 非空时，行数为 0 的帧上画一个居中提示（`bind_style.visible`
//! 读行数信号）；行数为 0 时行体没有行也没有滚动条（滚动条自身按 `scrollable = false` 淡出）。

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use smol_str::{SmolStr, format_smolstr};
use taffy::style::Dimension;
use ui_core::{
    app::App,
    error::EngineError,
    event::Action,
    interaction::ActionCtx,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::style::{Style, StylePatch},
    text::TextStyle,
    window::WindowId,
};

use crate::{
    kit::list::{List, ListOptions, RowHeight, Selection, SelectionMode},
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};

/// 列的宽度策略。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ColumnWidth {
    /// 常量宽度（逻辑像素）。
    Fixed(f32),
    /// 权重：按剩余宽度分配（`flex: w 1 0`）。
    Flex(f32),
}

impl ColumnWidth {
    /// 权重（固定宽度列返回 `0`）。
    #[must_use]
    pub fn weight(self) -> f32 {
        match self {
            Self::Fixed(_) => 0.0,
            Self::Flex(weight) => weight.max(0.0),
        }
    }
}

/// 单元格水平对齐。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ColumnAlign {
    /// 靠左。
    #[default]
    Start,
    /// 居中。
    Center,
    /// 靠右。
    End,
}

/// 排序方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortOrder {
    /// 升序。
    Ascending,
    /// 降序。
    Descending,
}

/// 排序状态：`(列下标, 方向)`。
pub type SortState = Option<(usize, SortOrder)>;

/// 点击列头后的下一个排序状态（纯逻辑：不同列从升序开始；同列升序 → 降序 → 升序）。
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

/// 一列的定义。
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    /// 列头文案。
    pub header: SmolStr,
    /// 宽度策略。
    pub width: ColumnWidth,
    /// 单元格对齐。
    pub align: ColumnAlign,
    /// 是否可排序（可排序的列头响应点击）。
    pub sortable: bool,
}

impl Column {
    /// 固定宽度的列。
    #[must_use]
    pub fn fixed(header: impl Into<SmolStr>, width: f32) -> Self {
        Self {
            header: header.into(),
            width: ColumnWidth::Fixed(width.max(1.0)),
            align: ColumnAlign::Start,
            sortable: false,
        }
    }

    /// 按权重分配剩余宽度的列。
    #[must_use]
    pub fn flex(header: impl Into<SmolStr>, weight: f32) -> Self {
        Self {
            header: header.into(),
            width: ColumnWidth::Flex(weight.max(0.0)),
            align: ColumnAlign::Start,
            sortable: false,
        }
    }

    /// 对齐。
    #[must_use]
    pub fn align(mut self, align: ColumnAlign) -> Self {
        self.align = align;
        self
    }

    /// 可排序。
    #[must_use]
    pub fn sortable(mut self) -> Self {
        self.sortable = true;
        self
    }

    fn cell_style(&self, padding_x: f32, padding_y: f32) -> Style {
        let builder = StyleBuilder::new()
            .flex_row()
            .items_center()
            .min_width_0()
            .padding_sides(padding_y, padding_x, padding_y, padding_x);
        let builder = match self.align {
            ColumnAlign::Start => builder,
            ColumnAlign::Center => builder.justify_center(),
            ColumnAlign::End => builder.justify_end(),
        };
        match self.width {
            ColumnWidth::Fixed(width) => builder.width(width).flex_none().build(),
            ColumnWidth::Flex(weight) => {
                let mut style = builder.flex_1().build();
                style.inner.flex_grow = weight.max(0.0);
                style.inner.flex_shrink = 1.0;
                style.inner.flex_basis = Dimension::length(0.0);
                style.inner.min_size.width = taffy::style::LengthPercentageAuto::length(0.0);
                style
            }
        }
    }
}

/// 表格参数。
#[derive(Clone, Debug, PartialEq)]
pub struct TableOptions {
    /// 行高策略（`Variable` = 单元格换行的变高表格）。
    pub row_height: RowHeight,
    /// 选中策略（默认多选）。
    pub selection: SelectionMode,
    /// 是否画行体滚动条。
    pub scrollbar: bool,
}

impl TableOptions {
    /// 固定行高的表格。
    #[must_use]
    pub fn fixed(row_height: f32) -> Self {
        Self {
            row_height: RowHeight::Fixed(row_height),
            selection: SelectionMode::Multi,
            scrollbar: true,
        }
    }

    /// 变高表格（单元格换行时用；行高由内容实测）。
    #[must_use]
    pub fn variable(estimate: f32) -> Self {
        Self {
            row_height: RowHeight::Variable { estimate },
            ..Self::fixed(estimate)
        }
    }

    /// 选中策略。
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
    list: List,
    sort: Signal<SortState>,
    on_sort: RefCell<Option<Rc<dyn Fn(&mut App, SortState)>>>,
    empty_text: Option<SmolStr>,
    row_count: Rc<Cell<usize>>,
    sort_action: Action,
    geometry: RefCell<TableGeometry>,
    header_height: f32,
    padding_x: f32,
}

#[derive(Default)]
struct TableGeometry {
    window: Option<WindowId>,
    outer: Option<NodeId>,
    body: Option<NodeId>,
    header_row: Option<NodeId>,
    empty: Option<NodeId>,
}

/// 表格句柄（`Rc` 浅拷贝）。
#[derive(Clone)]
pub struct Table {
    inner: Rc<TableInner>,
}

impl Table {
    /// 建表格。`cells` 把第 `(row, column)` 个单元格的内容建进给定父节点。
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
        let table_key = key.clone();
        let list_columns = columns.clone();
        let list_theme = theme.clone();
        let count_for_rows = row_count.clone();
        let cells: Rc<dyn Fn(&mut NodeTree, NodeId, usize, usize)> = Rc::new(cells);
        let cells_for_rows = cells.clone();
        let fixed_rows = matches!(options.row_height, RowHeight::Fixed(_));
        let empty_text: Option<SmolStr> =
            Some(empty_text.into()).filter(|text: &SmolStr| !text.is_empty());
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
                padding_x: 0.0,
                padding_y: 0.0,
                row_stretch: fixed_rows,
                scrollbar: options.scrollbar,
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
                let cells_row_style = StyleBuilder::new()
                    .flex_row()
                    .width_full()
                    .min_width_0()
                    .flex_1()
                    .build();
                let cells_row = NodeBuilder::boxed()
                    .style(cells_row_style)
                    .build(tree, column)?;
                for (column_index, column) in list_columns.iter().enumerate() {
                    let style = column.cell_style(tokens.padding_x, 4.0);
                    let cell_theme = list_theme.clone();
                    let cell = NodeBuilder::boxed()
                        .id(format_smolstr!("{table_key}:cell:{row}:{column_index}"))
                        .style(style)
                        .bind_style(move |cx| StylePatch {
                            foreground: Some(cell_theme.role(cx, Role::FgDefault)),
                            ..StylePatch::default()
                        })
                        .build(tree, cells_row)?;
                    (cells_for_rows)(tree, cell, row, column_index);
                }
                if row + 1 < count {
                    let divider_theme = list_theme.clone();
                    NodeBuilder::boxed()
                        .style(
                            StyleBuilder::new()
                                .width_full()
                                .height(1.0)
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
        Self {
            inner: Rc::new(TableInner {
                sort: app.signal(None),
                on_sort: RefCell::new(None),
                list,
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

    /// 表格键（调试 id / Action key 前缀）。
    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    /// 行体的列表内核（组合 / 测试用）。
    #[must_use]
    pub fn list(&self) -> &List {
        &self.inner.list
    }

    /// 设置初始行数（`build` 之前）。
    pub fn set_initial_rows(&self, app: &mut App, count: usize) {
        self.inner.row_count.set(count);
        self.inner.list.set_initial_count(app, count);
    }

    /// 行数变化（保留滚动位置与选中）。
    ///
    /// # Errors
    /// 引擎侧错误。
    pub fn set_rows(&self, app: &mut App, count: usize) -> Result<(), EngineError> {
        self.inner.row_count.set(count);
        self.inner.list.set_count(app, count)
    }

    /// 行数。
    #[must_use]
    pub fn rows(&self) -> usize {
        self.inner.row_count.get()
    }

    /// 选中集。
    #[must_use]
    pub fn selection(&self, app: &App) -> Selection {
        self.inner.list.selection(app)
    }

    /// 选中一行（`None` = 清空）。
    pub fn select(&self, app: &mut App, index: Option<usize>) -> bool {
        self.inner.list.select(app, index)
    }

    /// 当前排序状态。
    #[must_use]
    pub fn sort(&self, app: &App) -> SortState {
        app.read_with(&self.inner.sort, |sort| *sort)
    }

    /// 排序状态信号。
    #[must_use]
    pub fn sort_signal(&self) -> Signal<SortState> {
        self.inner.sort
    }

    /// 排序回调（表头点击后调用；应用按它重排行数据）。
    pub fn on_sort(&self, callback: impl Fn(&mut App, SortState) + 'static) {
        *self.inner.on_sort.borrow_mut() = Some(Rc::new(callback));
    }

    /// 行激活回调（双击行 / 键盘 Enter）。
    pub fn on_row_activate(&self, callback: impl Fn(&mut App, usize) + 'static) {
        self.inner.list.on_activate(callback);
    }

    /// 列定义。
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.inner.columns
    }

    /// 只建节点：表头 + 行体（列表）+ 空态。
    ///
    /// # Errors
    /// `StaleNode`。
    pub fn build(&self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError> {
        let inner = &self.inner;
        let outer = NodeBuilder::boxed()
            .id(format_smolstr!("{}:table", inner.key))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .overflow_hidden()
                    .build(),
            )
            .build(tree, parent)?;
        // 表头：单元格行 + 底部分隔线。
        let header_theme = inner.theme.clone();
        let header = NodeBuilder::boxed()
            .id(format_smolstr!("{}:header", inner.key))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .width_full()
                    .flex_none()
                    .build(),
            )
            .bind_style(move |cx| StylePatch {
                background: Some(header_theme.role(cx, Role::SurfaceRaised)),
                ..StylePatch::default()
            })
            .build(tree, outer)?;
        let header_row = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .width_full()
                    .height(inner.header_height)
                    .build(),
            )
            .build(tree, header)?;
        let sort = inner.sort;
        for (index, column) in inner.columns.iter().enumerate() {
            let theme = inner.theme.clone();
            let style = column.cell_style(inner.padding_x, 0.0);
            let header_theme = theme.clone();
            let cell = NodeBuilder::boxed()
                .id(format_smolstr!("{}:header:{}", inner.key, index))
                .style(style)
                .bind_style(move |cx| StylePatch {
                    foreground: Some(header_theme.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                });
            let cell = if column.sortable {
                cell.on_click(inner.sort_action.clone())
            } else {
                cell
            };
            let cell = cell.build(tree, header_row)?;
            let label_theme = theme.clone();
            NodeBuilder::text(
                column.header.clone(),
                TextStyle::new(inner.theme.tokens().medium.font_size),
            )
            .bind_style(move |cx| StylePatch {
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
        }
        let divider_theme = inner.theme.clone();
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .width_full()
                    .height(1.0)
                    .flex_none()
                    .build(),
            )
            .bind_style(move |cx| StylePatch {
                background: Some(divider_theme.role(cx, Role::LineDivider)),
                ..StylePatch::default()
            })
            .build(tree, header)?;
        // 行体：列表内核的落点。
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
        // 空态浮层（行数为 0 时可见）。
        if let Some(empty_text) = inner.empty_text.clone() {
            let count = inner.list.count_signal();
            let empty_theme = inner.theme.clone();
            let empty = NodeBuilder::boxed()
                .id(format_smolstr!("{}:empty", inner.key))
                .style(
                    StyleBuilder::new()
                        .absolute()
                        .left(0.0)
                        .right(0.0)
                        .top(0.0)
                        .bottom(0.0)
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
            geometry.body = Some(body);
            geometry.header_row = Some(header_row);
        }
        Ok(outer)
    }

    /// 接引擎：行体列表 + 排序 Action。
    ///
    /// # Errors
    /// 同 [`List::attach`]。
    pub fn attach(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        self.inner.list.attach(app, window)?;
        self.inner.geometry.borrow_mut().window = Some(window);
        let table = self.clone();
        app.on_action(self.inner.sort_action.clone(), move |ctx| {
            table.on_header_click(ctx);
        });
        Ok(())
    }

    /// 一步完成 `build` + `attach`。
    ///
    /// # Errors
    /// 同 [`Table::build`] / [`Table::attach`]。
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

    /// 帧同步（行内容 / 行高 / 悬停 / 滚动条）。
    ///
    /// # Errors
    /// 同 [`List::sync`]。
    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        self.inner.list.sync(app)
    }

    /// 空态浮层节点（测试用；没有配置空态文案时为 `None`）。
    #[must_use]
    pub fn empty_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().empty
    }

    /// 表头单元格节点（测试用）。
    #[must_use]
    pub fn header_node(&self, app: &App, column: usize) -> Option<NodeId> {
        let geometry = self.inner.geometry.borrow();
        let window = geometry.window?;
        let row = geometry.header_row?;
        drop(geometry);
        app.window(window)?
            .tree()
            .get(row)
            .ok()?
            .children()
            .get(column)
            .copied()
    }

    /// 单元格节点（测试用；行走列表内核）。
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
        tree.get(cells_row).ok()?.children().get(column).copied()
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
    use ui_core::{
        event::{InputEvent, Modifiers, PointerButton},
        node::builder::NodeBuilder,
        test_support::app::TestApp,
        text::TextStyle,
    };

    use super::{
        Column, ColumnAlign, ColumnWidth, SortOrder, Table, TableOptions, next_sort_order,
    };
    use crate::kit::list::{RowHeight, SelectionMode};

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
        assert_eq!(ColumnWidth::Flex(2.0).weight(), 2.0);

        let fixed = Column::fixed("price", 90.0);
        assert_eq!(fixed.width, ColumnWidth::Fixed(90.0));
        assert!(!fixed.sortable);
        let flex = Column::flex("name", 2.0).align(ColumnAlign::End).sortable();
        assert_eq!(flex.width, ColumnWidth::Flex(2.0));
        assert_eq!(flex.align, ColumnAlign::End);
        assert!(flex.sortable);
    }

    #[test]
    fn table_options_default_to_multi_select() {
        let options = TableOptions::fixed(28.0);
        assert_eq!(options.selection, SelectionMode::Multi);
        assert_eq!(options.row_height, RowHeight::Fixed(28.0));
        assert!(options.scrollbar);

        let variable = TableOptions::variable(40.0);
        assert_eq!(variable.row_height, RowHeight::Variable { estimate: 40.0 });
    }

    fn text_cell(
        tree: &mut ui_core::node::tree::NodeTree,
        parent: ui_core::node::id::NodeId,
        row: usize,
        column: usize,
    ) {
        let label = smol_str::format_smolstr!("{row}:{column}");
        NodeBuilder::text(label, TextStyle::new(12.0))
            .build(tree, parent)
            .expect("cell");
    }

    fn table_with_rows(app: &mut TestApp, rows: usize) -> Table {
        let theme = crate::theme::handle::ThemeHandle::new(
            app.app_mut(),
            crate::theme::appearance::Appearance::Light,
        );
        let table = Table::new(
            app.app_mut(),
            theme,
            "table",
            vec![
                Column::fixed("name", 120.0).sortable(),
                Column::flex("value", 1.0),
            ],
            TableOptions::fixed(28.0),
            text_cell,
            "empty",
        );
        table.set_initial_rows(app.app_mut(), rows);
        table
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

    fn center(app: &TestApp, node: ui_core::node::id::NodeId) -> ui_core::math::Point {
        let bounds = app.probe().bounds(node).expect("bounds");
        ui_core::math::Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        )
    }

    fn click(app: &mut TestApp, point: ui_core::math::Point) {
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

    /// 行虚拟化：只挂可见行；单元格按列宽摆放（固定列 120、弹性列占剩余）。
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
            (name.size.width - 120.0).abs() < 0.5,
            "固定列宽 120：{name:?}"
        );
        let value = table.cell_node(app.app(), 0, 1).expect("second cell");
        let value_bounds = app.probe().bounds(value).expect("cell bounds");
        assert!(
            value_bounds.size.width > 120.0,
            "弹性列占剩余宽度：{value_bounds:?}"
        );
        assert_eq!(
            value_bounds.origin.x,
            name.origin.x + name.size.width,
            "列在行内依次排开"
        );
    }

    /// 多选：Ctrl 点击加选、Shift 点击区间；选中集写进列表内核的信号。
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

    /// 排序：点击可排序表头写信号 + 触发回调（应用据此重排数据）。
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

    /// 空态：行数为 0 时提示可见、行体没有行；加行后提示消失。
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
}
