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
//! # 列跨度（合并单元格）
//!
//! [`Column::span`] 让列头 / 数据行的默认单元格横跨多列；[`Table::set_column_span`] 按行覆盖
//! （`financial_calendar` 的「日期标签跨整表」、`watch_list` 的「未加载市场跨列」）。宽度 =
//! 被跨列宽度之和（固定列相加、弹性权重相加），被覆盖的列不再单独绘制单元格；跨度在运行期
//! 夹到 `[1, 剩余列数]`（超出剩余列按剩余列合并，不 panic、不报错）。跨列表头的排序按**起始
//! 列**的 `sortable`，指示符画在这个合并表头里；行命中 / 选中 / hover 语义不变（命中链落到
//! 行容器）。
//!
//! # a11y（E9 集合语义）
//!
//! 采纳引擎 E9 的表格集合语义：容器静态声明 [`A11yRole::Table`]、表头行与数据行 `Row`、表头
//! `ColumnHeader`、单元格 `Cell`。引擎把**行窗口**（行体虚拟列表的 `viewport`）里声明了 `Row`
//! 的已挂载行挂到最近的 `Table` 祖先下、把单元格挂到最近的 `Row` 下 —— 行体 `List` 与单元格
//! 排板盒只是布局包裹层，`Table → Row → Cell` 的 AT 结构成立。
//!
//! **行列语义**（E9）：`Row::row_index` 由引擎推导 —— 0 = 表头行、数据行 = 行窗口基址 + 条目
//! **逻辑下标**；`Cell::row_index` / `column_index` 来自所在行。行**总数**同样由引擎推导
//! （行窗口 `slot.count` + 表头行，含未挂载条目）。例外是表头行 / 表头单元格：它们不随虚拟行
//! 重建，而引擎在 `slot.count` 变化时不会把集合节点标 A11Y 脏 —— 两者显式声明 `row_count`
//! （读行数信号）保证增量交付里的总数是新鲜的（数据行 / 表格容器会在 `set_count` 重建行时
//! 随新行或被重发的行窗口祖先链更新；增量 vs 全树 oracle 会抓到漏更新）。另两项是推导不出来
//! 的，必须声明：
//! - `column_index`（`A11yOverride`）：跨列合并时被覆盖的列不单独绘制，列号必须按**起始列**
//!   声明（声明值优先于「行内单元格序」的推导）；
//! - `Table::column_count`：推导只能数画出来的单元格，跨列合并时小于逻辑列数，故在表上声明。
//!
//! 多选状态由列表内核的行基线进 `A11yPatch::selected`（每行一个布尔）；可排序表头宣告
//! `A11ySort::{Ascending, Descending, None}`（`aria-sort`），▲ / ▼ 装饰字形显式 hidden。
//! 空态文案是行体浮层里的 `Label` 节点：行数为 0 时可见，有行后随浮层 `visible = false`
//! 变成 a11y `hidden`（AccessKit 沿 a11y 子树排除）。
//!
//! **残余**（`.docs/gui-migration/spec-deltas/kit-a11y.md`）：
//! - 行窗口（`List`）自身仍是 `Table` 的 a11y 子节点（E9 R2：集合内虚拟窗口未透明化；滚动
//!   状态仍挂在窗口上）；
//! - AccessKit 没有 `colspan` 等价字段：跨列表头只暴露起始列与逻辑列数，AT 的列导航会跳过
//!   被跨列；
//! - UIA / AT-SPI 的表格接口受上游 adapter 限制（E9 R1）；`TreeGrid` 未加（E9 R3）。
//!
//! # 迁移映射（widget 批 P3）
//!
//! 旧 `TableDataSet::column_span` 的两种用法对应新 API：`financial_calendar` 的「日期标签
//! 跨整表」用 [`Table::set_column_span`] 在 `CalendarRow::Day` 行从第 0 列返回整表列数
//! （或给该行起始列声明 [`Column::span`]）；`watch_list` 的「未加载市场跨中间列」同样用
//! `set_column_span` 按行返回 `COLUMN_COUNT - MISSING_MARKET_COLUMN - 1`。widget 迁移前不需要
//! 改 widget crate。
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
    a11y::{A11yOverride, A11yPatch, A11yRole, A11ySort},
    app::App,
    error::EngineError,
    event::Action,
    interaction::ActionCtx,
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
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
    /// 列跨度（≥ 1）：本列的表头 / 数据行单元格横跨多少列。
    ///
    /// 表头直接用这个值；数据行可以被 [`Table::set_column_span`] 的逐行回调覆盖。运行期
    /// 夹到 `[1, 剩余列数]`（超出剩余列不报错，按剩余列合并；文档化语义，见 [`Table`]）。
    pub column_span: usize,
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
            column_span: 1,
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
            column_span: 1,
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

    /// 跨列（表头 + 数据行的默认跨度；`0` 视为 `1`，超出剩余列时运行期夹到剩余列）。
    #[must_use]
    pub fn span(mut self, span: usize) -> Self {
        self.column_span = span.max(1);
        self
    }
}

/// 从 `start` 列起横跨 `span` 列的单元格样式。
///
/// 宽度 = 被跨列宽度之和：固定列相加成 `width`；含弹性列时固定部分进 `flex_basis` /
/// `min_width`、弹性权重相加进 `flex_grow`（对齐取**最后一个**被跨列的声明，与旧实现一致）。
fn span_cell_style(
    columns: &[Column],
    start: usize,
    span: usize,
    padding_x: f32,
    padding_y: f32,
) -> Style {
    let covered = &columns[start..start + span];
    let align = covered
        .last()
        .map_or(ColumnAlign::Start, |column| column.align);
    let fixed: f32 = covered
        .iter()
        .filter_map(|column| match column.width {
            ColumnWidth::Fixed(width) => Some(width),
            ColumnWidth::Flex(_) => None,
        })
        .sum();
    let flex: f32 = covered
        .iter()
        .filter_map(|column| match column.width {
            ColumnWidth::Fixed(_) => None,
            ColumnWidth::Flex(weight) => Some(weight.max(0.0)),
        })
        .sum();
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
        style.inner.flex_grow = flex;
        style.inner.flex_shrink = 1.0;
        style.inner.flex_basis = Dimension::length(fixed);
        style.inner.min_size.width = taffy::style::LengthPercentageAuto::length(fixed);
        style
    } else {
        builder.width(fixed).flex_none().build()
    }
}

/// 运行期跨度：`[1, 剩余列数]`（`columns` 非空；调用方保证 `start < len`）。
fn clamp_span(requested: usize, columns: &[Column], start: usize) -> usize {
    requested.max(1).min(columns.len() - start)
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
    /// 逐行列跨度回调（`(row, 起始列) -> span`；`None` = 用 `Column::column_span`）。
    ///
    /// `Rc<RefCell<_>>`：行工厂闭包与表格句柄共享同一份回调（`set_column_span` 可后置设置）。
    column_span: Rc<RefCell<Option<Rc<dyn Fn(usize, usize) -> usize>>>>,
    list: List,
    /// 本表格自有的信号作用域（排序状态；动态表格卸载时释放）。
    scope: SignalScope,
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
                separator: false,
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
                // 变高表格：`cells_row` 必须是**自动高度**（内容实测），不能再 `flex_1` ——
                // 在自动高度的 `flex_col` 里，`flex: 1 1 0` 的 basis 0 不参与主轴尺寸计算，
                // 行内容塌成 0、只剩分隔线 1 px（P2-2 审查 P1）。固定行高表格仍用 `flex_1`
                // 把单元格拉满行高。
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
                    let style =
                        span_cell_style(&list_columns, column_index, span, tokens.padding_x, 4.0);
                    let cell_theme = list_theme.clone();
                    let cell = NodeBuilder::boxed()
                        .id(format_smolstr!("{table_key}:cell:{row}:{column_index}"))
                        .style(style)
                        .bind_style(move |cx| StylePatch {
                            foreground: Some(cell_theme.role(cx, Role::FgDefault)),
                            ..StylePatch::default()
                        })
                        // 单元格 = 表格第 `row` 行、起始列 `column_index` 的 `Cell`。行号 / 行总数 /
                        // 列总数由引擎按集合结构推导（E9：行窗口基址 + 条目逻辑下标、逻辑总行数含
                        // 未挂载行、列总数取表上声明值）；这里只声明**推导不出来的**列起点 —— 跨列
                        // 合并时被覆盖的列不单独绘制，列号必须按起始列声明（声明值优先于行内单元格序）。
                        // 角色与列号是静态语义，用 `A11yOverride`：行内容重建时立刻生效，不依赖
                        // `bind_a11y` 的下一帧求值。
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
        // 行级 a11y（E9）：表格行用 `Row`（不是默认的 `ListBoxOption`）。行号 / 行总数由引擎
        // 从集合结构推导：0 = 表头行、数据行 = 行窗口基址 + 条目逻辑下标、总数含未挂载条目。
        list.set_row_a11y(|_| A11yPatch {
            role: Some(A11yRole::Row),
            ..A11yPatch::default()
        });
        Self {
            inner: Rc::new(TableInner {
                sort,
                scope,
                column_span,
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

    /// 释放本组件的信号作用域与引擎侧注册（动态创建 / 销毁的表格必须调用；先卸载节点再释放）。
    ///
    /// 清理面：排序 Action handler（[`App::off_action`]）+ 行体列表（含滚动条）的 handler /
    /// 信号作用域 + 本表格的信号作用域。只释放信号不摘 handler 会让 `App` 永久持有
    /// `Rc<Table>` 闭包（P2-2 复核 R2.2 的泄漏）。
    /// 返回被释放的槽位数（排序状态 + 行体列表 / 滚动条的状态）；重复调用返回 0（幂等）。
    pub fn release(&self, app: &mut App) -> usize {
        app.off_action(&self.inner.sort_action);
        self.inner.list.release(app) + app.release_scope(self.inner.scope)
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

    /// 逐行列跨度：`(行下标, 起始列) -> span`；`None`（默认）= 用 [`Column::column_span`]。
    ///
    /// 表头只用 [`Column::column_span`]；数据行用本回调（覆盖列声明）。调用方返回的跨度在
    /// 运行期夹到 `[1, 剩余列数]`。**在 `build` / `attach` 之前调用**，或在之后调用
    /// `set_rows(当前行数)` 重建已挂载行（行内容在重建时才按新跨度生成）。
    pub fn set_column_span(&self, span: impl Fn(usize, usize) -> usize + 'static) {
        *self.inner.column_span.borrow_mut() = Some(Rc::new(span));
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
        let column_count = inner.columns.len();
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
            // a11y（E9）：表格容器。行号 / 行总数由引擎按集合结构推导（行号 = 行窗口基址 +
            // 条目逻辑下标；行总数 = 行窗口 `slot.count` + 窗口之外的行，含未挂载条目）。
            // 推导只能数「画出来的单元格」，跨列合并时列总数会偏小，所以在表上静态声明逻辑
            // 列数（`Row` / `Cell` 的 `column_count` 由它推导）。
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                column_count: Some(column_count),
                ..A11yOverride::default()
            })
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
        let header_row_count = inner.list.count_signal();
        let header_row = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .width_full()
                    .height(inner.header_height)
                    .build(),
            )
            // 表头行 = 表格的第 0 行（`AccessKit` 的 `Row`）。行号由引擎推导（0）；行总数显式
            // 读行数信号（见表格容器的注释：集合节点不会因 `slot.count` 变化自动标脏）。
            .a11y(A11yOverride {
                role: Some(A11yRole::Row),
                ..A11yOverride::default()
            })
            .bind_a11y(move |cx| A11yPatch {
                row_count: Some(*cx.read(&header_row_count) + 1),
                ..A11yPatch::default()
            })
            .build(tree, header)?;
        let sort = inner.sort;
        let header_count = inner.list.count_signal();
        let mut index = 0;
        while index < inner.columns.len() {
            let column = &inner.columns[index];
            let span = clamp_span(column.column_span, &inner.columns, index);
            let theme = inner.theme.clone();
            let style = span_cell_style(&inner.columns, index, span, inner.padding_x, 0.0);
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
                // 列头：`ColumnHeader` + 列号（跨列时 = 起始列，声明值优先于行内单元格序）+
                // 排序状态（`aria-sort`）；行号 / 行列总数由引擎从集合结构推导。
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
                // ▲ / ▼ 是纯装饰：排序语义已经在列头上用 `aria-sort` 表达，读屏不应再读字形。
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

    /// 表格外层节点（`build` 之后；测试 / 组合用）。
    #[must_use]
    pub fn outer_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().outer
    }

    /// 表头单元格节点（测试用；按列下标找，跨列时被覆盖的列返回 `None`）。
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

    /// 单元格节点（测试用；按 `(行, 起始列)` 找，被跨列覆盖的列返回 `None`）。
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
    use ui_core::{
        a11y::{A11yAction, A11yRole, A11ySort},
        event::{InputEvent, Modifiers, PointerButton},
        interaction::ItemAlign,
        node::{
            builder::NodeBuilder,
            id::NodeId,
            tree::{NodeKind, NodeTree},
        },
        test_support::app::TestApp,
        text::TextStyle,
    };

    use super::{
        Column, ColumnAlign, ColumnWidth, SortOrder, Table, TableOptions, next_sort_order,
    };
    use crate::{
        kit::{
            list::{RowHeight, SelectionMode},
            test_support::{a11y_of, assert_single_parent_per_node, frame_a11y, full_a11y},
        },
        style_builder::StyleBuilder,
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
        table_with(
            app,
            vec![
                Column::fixed("name", 120.0).sortable(),
                Column::flex("value", 1.0),
            ],
            TableOptions::fixed(28.0),
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
                .style(StyleBuilder::new().width(width).height(height).build())
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

    /// 点击并返回抬起事件的 outcome（`action = Some(..)` ⇔ 点击真的调用了 handler）。
    fn click_outcome(
        app: &mut TestApp,
        point: ui_core::math::Point,
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

    /// `release` 必须摘掉表头排序 handler。
    ///
    /// 两条证据：handler 表项直接消失（`App::has_action_handler`）+ 重放点击的 `InputOutcome`
    /// 不再带 action（没有 handler 可调用）。泄漏的 handler 会在释放后读已回收的排序信号 ⇒ panic。
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
        // 不卸载节点直接释放：重放点击不得再调用已摘除的 handler（否则读死信号 panic）。
        let click1 = click_outcome(&mut app, point);
        assert_eq!(click1.action, None, "释放后重放点击不得再命中排序 handler");
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

    /// 变高表格：行高按单元格内容实测回写（`cells_row` 不再 `flex_1` 塌成 0），
    /// 行位置跟前缀和、锚点偏移与滚动都成立（P2-2 审查 P1）。
    #[test]
    fn variable_table_rows_are_measured_from_cell_content() {
        let mut app = TestApp::new();
        let table = table_with(
            &mut app,
            vec![Column::fixed("name", 120.0), Column::flex("value", 1.0)],
            TableOptions::variable(20.0),
            8,
            |tree, parent, row, _column| {
                let height = if row % 2 == 0 { 40.0 } else { 16.0 };
                NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .width_full()
                            .height(height)
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
            Some(49.0),
            "内容 40 + 单元格上下 padding 4*2 + 行分隔线 1"
        );
        assert_eq!(heights.get(1), Some(25.0), "内容 16 + padding + 分隔线");
        let row0 = table.list().row_node(app.app(), 0).expect("row 0");
        let row1 = table.list().row_node(app.app(), 1).expect("row 1");
        let first = app.probe().bounds(row0).expect("row 0 bounds");
        let second = app.probe().bounds(row1).expect("row 1 bounds");
        assert!(
            (first.size.height - 49.0).abs() < 0.5,
            "行高按实测内容：{first:?}"
        );
        assert!(
            (second.origin.y - first.origin.y - 49.0).abs() < 0.5,
            "行位置跟前缀和走：{first:?} {second:?}"
        );
        // 锚点 / 滚动：第 2 行贴顶 = 49 + 25。
        table
            .list()
            .scroll_to_item(app.app_mut(), 2, ItemAlign::Start)
            .expect("scroll to item");
        let _ = app.drain().expect("frame");
        let _ = table.sync(app.app_mut()).expect("sync");
        assert_eq!(
            table.list().offset(app.app()),
            Some(74.0),
            "变高前缀和上的滚动锚点"
        );
        let cell = table.cell_node(app.app(), 2, 0).expect("row 2 cell");
        let bounds = app.probe().bounds(cell).expect("cell bounds");
        assert!(
            (bounds.size.height - 48.0).abs() < 0.5,
            "单元格高度 = 内容 40 + padding 8：{bounds:?}"
        );
    }

    /// 跨列：宽度 = 被跨列宽度之和（固定 + 弹性）、被覆盖列不单独绘制、表头同样合并、
    /// 跨列表头可排序、命中选择仍归属整行。
    #[test]
    fn merged_cells_span_the_columns_they_cover() {
        let mut app = TestApp::new();
        let table = table_with(
            &mut app,
            vec![
                Column::fixed("name", 100.0).span(2).sortable(),
                Column::flex("value", 1.0),
                Column::fixed("size", 50.0),
                Column::flex("change", 1.0),
            ],
            TableOptions::fixed(28.0),
            4,
            text_cell,
        );
        mount_in(&mut app, &table, 400.0, 240.0);
        // 400 宽：固定列 100 + 50，弹性列按权重分剩余（弹性列 basis 0 但含左右 padding 12，
        // 因此可分配 238、每个权重得 119）：跨两列的表头 = 100 + 119 = 219。
        let header0 = table.header_node(app.app(), 0).expect("header 0");
        let header0 = app.probe().bounds(header0).expect("header 0 bounds");
        assert!(
            (header0.size.width - 219.0).abs() < 0.5,
            "跨列表头 = 固定列 100 + 弹性列份额 119：{header0:?}"
        );
        assert!(
            table.header_node(app.app(), 1).is_none(),
            "被跨列的表头不单独绘制"
        );
        let cell0 = table.cell_node(app.app(), 0, 0).expect("cell 0/0");
        let cell0 = app.probe().bounds(cell0).expect("cell bounds");
        assert!(
            (cell0.size.width - 219.0).abs() < 0.5,
            "跨列单元格 = 被跨列宽度之和：{cell0:?}"
        );
        assert!(
            table.cell_node(app.app(), 0, 1).is_none(),
            "被跨列覆盖的单元格不绘制"
        );
        let cell2 = table.cell_node(app.app(), 0, 2).expect("cell 0/2");
        let cell2 = app.probe().bounds(cell2).expect("cell bounds");
        assert!((cell2.size.width - 50.0).abs() < 0.5, "{cell2:?}");
        assert!(
            (cell2.origin.x - cell0.origin.x - 219.0).abs() < 0.5,
            "后续列紧接合并单元格排开：{cell0:?} {cell2:?}"
        );
        // 跨列表头的排序指示：点击合并表头写排序状态并把 ▲ 画在这个表头里。
        let header0_node = table.header_node(app.app(), 0).expect("header 0");
        let point = center(&app, header0_node);
        click(&mut app, point);
        let _ = app.drain().expect("frame");
        assert_eq!(table.sort(app.app()), Some((0, SortOrder::Ascending)));
        assert!(
            texts(&app, header0_node).contains(&SmolStr::new_static("▲")),
            "排序指示画在跨列表头里"
        );
        // 命中：点击跨列单元格中心选中整行。
        let cell0 = table.cell_node(app.app(), 0, 0).expect("cell 0/0");
        let point = center(&app, cell0);
        click(&mut app, point);
        assert_eq!(table.selection(app.app()).first(), Some(0));
    }

    /// 逐行跨度回调：只影响该行；跨度超出剩余列时夹到剩余列（不 panic / 不报错）。
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
            TableOptions::fixed(24.0),
            2,
            text_cell,
        );
        table.set_column_span(|row, column| if row == 0 && column == 1 { 99 } else { 1 });
        mount_in(&mut app, &table, 400.0, 200.0);
        let cell = table.cell_node(app.app(), 0, 1).expect("row 0 span start");
        let bounds = app.probe().bounds(cell).expect("cell bounds");
        assert!(
            (bounds.size.width - 150.0).abs() < 0.5,
            "99 列夹到剩余 3 列 = 50 + 75 + 25：{bounds:?}"
        );
        assert!(table.cell_node(app.app(), 0, 2).is_none());
        assert!(table.cell_node(app.app(), 0, 3).is_none());
        // 其它行不受影响。
        let cell = table.cell_node(app.app(), 1, 1).expect("row 1 col 1");
        let bounds = app.probe().bounds(cell).expect("cell bounds");
        assert!((bounds.size.width - 50.0).abs() < 0.5, "{bounds:?}");
    }

    /// E9：虚拟化表格的行被领养到 `Table` 下（父不是 `List`），行号 / 总行数按**逻辑集合**
    /// 推导（0 = 表头行；总数含未挂载行），单元格父 = 行、列号来自声明。
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
        // E15b/F4（E9 R2 后半）：行窗口被平台 `common_filter` 透明上提后，它承载的滚动语义
        // 必须由 `Table` 接管 —— 否则读屏用户无法滚动虚拟化表格。
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
        assert!(scroll.vertical);
        assert!(
            scroll.y_max > 0.0,
            "滚动范围来自行窗口的内容高度：{scroll:?}"
        );
        for action in [
            A11yAction::ScrollUp,
            A11yAction::ScrollDown,
            A11yAction::ScrollIntoView,
        ] {
            assert!(
                table_a11y.actions.contains(&action),
                "Table 必须宣告 {action:?}（上提的滚动动作）"
            );
        }
        // 表头行 = 第 0 行，父 = Table；列头父 = 表头行。
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
        // 行窗口被 E9 领养规则跳过：不再列出行；E15b 起也不再以 `List` 暴露（集合行窗口抑制，
        // 平台 `common_filter` 会把 `GenericContainer` 透明上提，AT 看到干净的 Table → Row）。
        let window = a11y_of(&snapshot, viewport);
        assert_eq!(
            window.role,
            A11yRole::GenericContainer,
            "集合行窗口不得再自动声明 List（否则 AT 读到「表格里套列表」）"
        );
        assert!(window.children.is_empty(), "行窗口不得再列行（E9 领养）");
        // 数据行 / 单元格：父链 Table → Row → Cell；行号 = 1（表头）+ 条目逻辑下标。
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
        // 滚动到第 60 行：行窗口换页，新行仍被领养，行号是逻辑下标（不是已挂载序号）。
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

    /// 跨列表头 / 跨列单元格：列号按起始列声明（声明优先于「行内单元格序」推导），列总数是
    /// **逻辑列数**（不是画出来的单元格数）。
    #[test]
    fn merged_columns_keep_logical_column_semantics() {
        let mut app = TestApp::new();
        let table = table_with(
            &mut app,
            vec![
                Column::fixed("name", 100.0).span(2).sortable(),
                Column::flex("value", 1.0),
                Column::fixed("size", 50.0),
                Column::flex("change", 1.0),
            ],
            TableOptions::fixed(28.0),
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
        // 跨列表头：column_index = 起始列 0；被覆盖的列 1 没有节点。
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
        // 数据行：第 2 个画出来的单元格列号是 2（不是文档序 1）。
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

    /// 多选 / 排序指示进 a11y：行 `selected` 是集合状态，可排序列头宣告 `aria-sort`、
    /// 不可排序列头不宣告。
    #[test]
    fn multi_selection_and_sort_reach_a11y() {
        let mut app = TestApp::new();
        let table = table_with_rows(&mut app, 10);
        mount(&mut app, &table);
        app.enable_a11y_capture();
        let _ = app.drain().expect("frame");
        // 普通点击选一行 + Ctrl 点击加选一行。
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
        // 排序：可排序表头宣告 aria-sort，不可排序列头不宣告。
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

    /// 空态：行数为 0 时提示文本作为 `Label` 进 a11y；有行后浮层 hidden（AT 不再读到）。
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
        // 有行之后：浮层 hidden，表行出现；空态文案随 hidden 子树被 AT 排除。
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
        // 行数变化后，表头行 / 表头单元格 / 数据行的总行数都必须刷新（它们读行数信号；
        // 引擎不会因 `slot.count` 变化自动标脏集合节点 —— 见模块文档〈a11y〉）。
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

    /// 组件自有信号作用域：`release` 回收排序 + 行体列表 / 滚动条槽位，重复调用幂等。
    #[test]
    fn releasing_the_table_scope_reclaims_its_signal_slots() {
        let mut app = TestApp::new();
        let table = table_with_rows(&mut app, 3);
        let released = table.release(app.app_mut());
        assert!(
            released >= 6,
            "排序状态 + 列表三条信号 + 滚动条两条信号必须被回收（实际 {released}）"
        );
        assert_eq!(table.release(app.app_mut()), 0, "重复释放幂等");
    }
}
