//! 列表内核（P2-2）：等高 / 变高、静态 / 双端列表的统一实现。
//!
//! # 结构
//!
//! 外层容器（`outer`，裁剪 + `relative`）包一个**虚拟列表容器**（`viewport`：`Clip::Bounds` +
//! `position: relative`，引擎 `make_virtual_list` 的落点）和一个自绘滚动条
//! （[`crate::kit::scroll_bar`]）。行由引擎按可见窗口挂载 / 回收（`VirtualListConfig` /
//! `rewindow` / `set_virtual_extents` / `scroll_to_item`，E3）。
//!
//! # 行内容的挂载时机（引擎约束）
//!
//! 引擎的行工厂是 `FnMut(usize) -> NodeBuilder`，一次只能返回**单个**节点；列表行通常是复合
//! 子树（表格单元格、树缩进 + 折叠标 + 文本）。因此内核把一行拆成两层：
//!
//! 1. 引擎挂载的**行容器**（背景 / padding / 选中态绑定样式，调试 id `"{key}:row:{index}"`）；
//! 2. 内核在 [`List::sync`] 里给还没有子节点的行容器补一个**内容容器**
//!    （`"{key}:row:{index}:content"`），再调用应用的行内容回调把子树建进内容容器。
//!
//! `sync` 与重建路径（`set_items` / `set_count` / `push`）都**同步**补内容；`attach` 末尾还会
//! 主动跑一次布局并按真实视口重挂（[`List::attach`] 的「首帧同步建行」），因此**首帧与重建
//! 路径首帧都有行内容**。唯一残留：`scroll_to_item` / 树选中滚动在**帧内**挂载的新行，内容在
//! 下一帧 `sync` 补（滚动时 overscan 的新行多在视口外，影响小）。定义「行工厂返回单个节点」
//! 的引擎形态决定了内容子树只能由调用方在行容器挂载后补建。
//!
//! # 生命周期
//!
//! 组件的信号（选中 / 悬停 / 条目数，以及滚动条的状态）都归属**组件自有** `SignalScope`
//! （[`List::release`] 释放，槽位立即回收）。动态创建 / 销毁列表（对话框、选择面板）必须在
//! 卸载节点后调用 `release`；常驻组件可以不调（与根作用域等价）。
//!
//! # 行高
//!
//! - [`RowHeight::Fixed`]：所有行同高，引擎按 `index * extent` 布局；
//! - [`RowHeight::Variable`]：先用 `estimate` 挂载，`sync` 逐行量内容容器的实测高度并
//!   `set_virtual_extents` 回写（引擎重算前缀和并按锚点补偿偏移，E3 语义）。未挂载的行保持
//!   估计值，滚动到那里后再量。
//!
//! 应用层的 `RowHeights` 只服务双端列表锚点换算与测量回写，滚动帧不重建它（前缀和只在行高
//! 变更时重建）。
//!
//! # 双端列表（deque）
//!
//! [`ListOptions::capacity`] + [`List::push`]：容量满时丢最旧一条。数据在应用侧（行内容回调按
//! 展示下标取值），内核只维护「条目数 + 行高 + 滚动锚点」：
//!
//! - 非倒序（最新在末尾）：满时丢首项，读者读的那条下标 −1；
//! - 倒序（最新在开头）：新项插到头部，读者读的那条下标 +1；
//! - `keep_on_newest` 只对「已经在最新一条上」的读者生效：非倒序跟随尾部，倒序回到顶部。
//!
//! 引擎没有「同条目数下强制重建已挂载行」的入口，双端 push 走
//! `set_virtual_items(0)` → `set_virtual_items(n)` 强制重建可见窗口（行内容按新下标重新生成）；
//! 代价是每次 push 重挂可见行，与旧 gpui `splice` 保测量不同，登记为已知差异。

use std::{cell::RefCell, collections::BTreeSet, rc::Rc};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::A11yPatch,
    app::App,
    error::EngineError,
    event::{Action, Modifiers},
    interaction::{
        ActionCtx, ItemAlign, ScrollHandle, VirtualList, VirtualListConfig, VirtualListState,
    },
    math::Point,
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::style::StylePatch,
    window::WindowId,
};

use crate::{
    kit::scroll_bar::{ScrollBar, ScrollbarAxis, ScrollbarOptions},
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};

/// 行高策略。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RowHeight {
    /// 所有行同高（逻辑像素，`> 0`）。
    Fixed(f32),
    /// 逐行实测：挂载时用 `estimate` 估高，`sync` 量到真实高度后回写。
    Variable {
        /// 初始 / 未挂载行的估计高度（逻辑像素，`> 0`）。
        estimate: f32,
    },
}

impl RowHeight {
    /// 挂载期使用的行高（固定值或估计值）。
    #[must_use]
    pub fn extent(self) -> f32 {
        match self {
            Self::Fixed(extent) | Self::Variable { estimate: extent } => extent.max(1.0),
        }
    }

    /// 是否需要实测回写。
    #[must_use]
    pub fn is_variable(self) -> bool {
        matches!(self, Self::Variable { .. })
    }
}

/// 选中策略。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SelectionMode {
    /// 不选中（行仍可点击，用于纯展示列表）。
    #[default]
    None,
    /// 单选。
    Single,
    /// 多选：Ctrl 切换、Shift 区间、普通点击单选。
    Multi,
}

/// 列表参数。
#[derive(Clone, Debug, PartialEq)]
pub struct ListOptions {
    /// 行高策略。
    pub row_height: RowHeight,
    /// 视口外上下各多挂几行。
    pub overscan: usize,
    /// 选中策略。
    pub selection: SelectionMode,
    /// 双端列表容量（`None` = 静态列表，条目数由 [`List::set_items`] / [`List::set_count`] 给）。
    pub capacity: Option<usize>,
    /// 倒序：最新条目画在第一个位置（双端列表）。
    pub reverse: bool,
    /// 读者在最新一条上时，push 后跟着它走（双端列表）。
    pub keep_on_newest: bool,
    /// 行左 / 右内边距。
    pub padding_x: f32,
    /// 行上 / 下内边距（变高列表的测量会把两侧加回去）。
    pub padding_y: f32,
    /// 行内容是否拉满行高（表格单元格用 `true`；普通列表 `false` = 内容垂直居中）。
    pub row_stretch: bool,
    /// 是否画自绘滚动条。
    pub scrollbar: bool,
}

impl ListOptions {
    /// 固定行高的静态列表参数。
    #[must_use]
    pub fn fixed(extent: f32) -> Self {
        Self {
            row_height: RowHeight::Fixed(extent),
            overscan: 2,
            selection: SelectionMode::Single,
            capacity: None,
            reverse: false,
            keep_on_newest: false,
            padding_x: 6.0,
            padding_y: 2.0,
            row_stretch: false,
            scrollbar: true,
        }
    }

    /// 变高静态列表参数。
    #[must_use]
    pub fn variable(estimate: f32) -> Self {
        Self {
            row_height: RowHeight::Variable { estimate },
            ..Self::fixed(estimate)
        }
    }

    /// 变成双端列表（容量 = 保留的最新条目数）。
    #[must_use]
    pub fn deque(mut self, capacity: usize) -> Self {
        self.capacity = Some(capacity.max(1));
        self
    }

    /// 倒序（最新条目在第一个位置）。
    #[must_use]
    pub fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }

    /// 读者在最新一条上时跟随新条目。
    #[must_use]
    pub fn keep_on_newest(mut self) -> Self {
        self.keep_on_newest = true;
        self
    }

    /// 选中策略。
    #[must_use]
    pub fn selection(mut self, mode: SelectionMode) -> Self {
        self.selection = mode;
        self
    }

    /// 行内容拉满行高（表格用；行内容不再垂直居中）。
    #[must_use]
    pub fn row_stretch(mut self) -> Self {
        self.row_stretch = true;
        self
    }
}

/// 选中集（纯逻辑；单选用 `len() <= 1`）。
///
/// `anchor` 是 Shift 区间选择的起点，也是最近一次单击的落点。
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Selection {
    indices: BTreeSet<usize>,
    anchor: Option<usize>,
}

impl std::fmt::Debug for Selection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Selection")
            .field("indices", &self.indices)
            .field("anchor", &self.anchor)
            .finish()
    }
}

impl Selection {
    /// 空选中。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否没有选中项。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// 选中项个数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.indices.len()
    }

    /// 是否选中 `index`。
    #[must_use]
    pub fn contains(&self, index: usize) -> bool {
        self.indices.contains(&index)
    }

    /// 最小下标选中项（单选语义的直接消费面）。
    #[must_use]
    pub fn first(&self) -> Option<usize> {
        self.indices.first().copied()
    }

    /// 选中项迭代（升序）。
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.indices.iter().copied()
    }

    /// 清空。
    pub fn clear(&mut self) {
        self.indices.clear();
        self.anchor = None;
    }

    /// 丢弃 `>= count` 的下标（条目数变小时调用）；返回是否变化。
    pub fn retain_below(&mut self, count: usize) -> bool {
        let before = self.indices.len();
        self.indices.retain(|index| *index < count);
        if self.anchor.is_some_and(|anchor| anchor >= count) {
            self.anchor = None;
        }
        self.indices.len() != before
    }

    /// 单选 `index`。
    pub fn select(&mut self, index: usize) {
        self.indices.clear();
        self.indices.insert(index);
        self.anchor = Some(index);
    }

    /// Ctrl 点击：切换一项（锚点跟随到最后点击的那一项）。
    pub fn toggle(&mut self, index: usize) {
        if !self.indices.remove(&index) {
            self.indices.insert(index);
        }
        self.anchor = Some(index);
    }

    /// Shift 点击：从锚点选到 `index`（含两端）。
    pub fn select_range(&mut self, index: usize) {
        let Some(anchor) = self.anchor else {
            self.select(index);
            return;
        };
        let (low, high) = if anchor <= index {
            (anchor, index)
        } else {
            (index, anchor)
        };
        self.indices.clear();
        self.indices.extend(low..=high);
    }
}

/// 一次点击对选中集的作用（纯函数：`Ctrl` 切换、`Shift` 区间、普通单选）。
pub(crate) fn apply_click(
    selection: &mut Selection,
    index: usize,
    mode: SelectionMode,
    ctrl: bool,
    shift: bool,
) {
    match mode {
        SelectionMode::None => {}
        SelectionMode::Single => selection.select(index),
        SelectionMode::Multi => {
            if shift {
                selection.select_range(index);
            } else if ctrl {
                selection.toggle(index);
            } else {
                selection.select(index);
            }
        }
    }
}

/// 应用层的逐项高度表 + 前缀和缓存。
///
/// 引擎 `VirtualSlot` 里已有一份同语义的前缀和（布局真相）；这里再存一份是给应用层的
/// **双端列表锚点换算**与测量回写用（需要在 `&mut App` 之外做纯计算、且可单测）。
/// 只有测量 / 双端 push 会改它，滚动帧不碰（I3 的预算前提）。
///
/// 实现细节（`pub(crate)`）：没有下游消费者，公开面只保留组件本身。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct RowHeights {
    heights: Vec<f32>,
    prefix: Vec<f32>,
}

impl RowHeights {
    /// 以 `count` 项、每项 `height` 构建（前缀和立即建立）。
    #[must_use]
    pub fn filled(count: usize, height: f32) -> Self {
        let height = height.max(0.0);
        let mut this = Self {
            heights: vec![height; count],
            prefix: Vec::with_capacity(count + 1),
        };
        this.rebuild_prefix();
        this
    }

    fn rebuild_prefix(&mut self) {
        self.prefix.clear();
        self.prefix.resize(self.heights.len() + 1, 0.0);
        let mut total = 0.0_f32;
        for (index, height) in self.heights.iter().copied().enumerate() {
            total += height;
            self.prefix[index + 1] = total;
        }
    }

    /// 当前条目数。
    #[cfg(test)]
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.heights.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heights.is_empty()
    }

    /// 逐项高度。
    #[must_use]
    pub fn as_slice(&self) -> &[f32] {
        &self.heights
    }

    /// 第 `index` 项的高度。
    #[cfg(test)]
    #[must_use]
    pub(crate) fn get(&self, index: usize) -> Option<f32> {
        self.heights.get(index).copied()
    }

    /// 第 `index` 项的顶边（前缀和）。
    #[must_use]
    pub fn top(&self, index: usize) -> f32 {
        self.prefix
            .get(index)
            .copied()
            .unwrap_or_else(|| self.total())
    }

    /// 内容总高度。
    #[must_use]
    pub fn total(&self) -> f32 {
        self.prefix.last().copied().unwrap_or(0.0)
    }

    /// 偏移处的 `(首个可见条目下标, 行内偏移)`（零内容 ⇒ `(0, offset)`）。
    #[must_use]
    pub fn index_at(&self, offset: f32) -> (usize, f32) {
        if self.heights.is_empty() || self.total() <= 0.0 {
            return (0, offset.max(0.0));
        }
        let tops = &self.prefix[..self.heights.len()];
        let index = tops
            .partition_point(|top| *top <= offset)
            .saturating_sub(1)
            .min(self.heights.len() - 1);
        (index, (offset - tops[index]).max(0.0))
    }

    /// 改一项的高度；返回是否真的变了（`false` ⇒ 不重建前缀和）。
    pub fn set(&mut self, index: usize, height: f32) -> bool {
        let height = height.max(0.0);
        let Some(slot) = self.heights.get_mut(index) else {
            return false;
        };
        if (*slot - height).abs() <= f32::EPSILON {
            return false;
        }
        *slot = height;
        self.rebuild_prefix();
        true
    }

    /// 头部插入一项（倒序双端列表 push）：所有下标 +1。
    pub fn insert_front(&mut self, height: f32) {
        self.heights.insert(0, height.max(0.0));
        self.rebuild_prefix();
    }

    /// 尾部追加一项。
    pub fn push_back(&mut self, height: f32) {
        self.heights.push(height.max(0.0));
        self.rebuild_prefix();
    }

    /// 丢掉头部一项（非倒序双端列表满容量时）。
    pub fn pop_front(&mut self) -> Option<f32> {
        if self.heights.is_empty() {
            return None;
        }
        let removed = self.heights.remove(0);
        self.rebuild_prefix();
        Some(removed)
    }

    /// 丢掉尾部一项（倒序双端列表满容量时）。
    pub fn pop_back(&mut self) -> Option<f32> {
        let removed = self.heights.pop();
        if removed.is_some() {
            self.rebuild_prefix();
        }
        removed
    }

    /// 调整为 `count` 项：多退（尾部）/ 少补（`fallback`）。
    pub fn resize(&mut self, count: usize, fallback: f32) {
        if self.heights.len() == count {
            return;
        }
        self.heights.resize(count, fallback.max(0.0));
        self.rebuild_prefix();
    }
}

/// 双端 push 之前的读者位置。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PushAnchor {
    /// 读者读的条目在旧下标里的位置。
    pub index: usize,
    /// 该条目内的偏移。
    pub inner: f32,
    /// 读者是否已经在最新一条上（`keep_on_newest` 的判定输入）。
    pub at_newest: bool,
}

/// 双端 push 之后的锚点换算（纯函数）。
///
/// 返回 `None` = 没有可保持的位置（列表此前为空），调用方按 0 处理；否则是
/// `(新下标, 行内偏移)`。`full` 表示 push 前容量已满（会丢最旧一条）。
#[must_use]
pub(crate) fn push_anchor_after(
    anchor: PushAnchor,
    full: bool,
    reverse: bool,
    new_count: usize,
) -> Option<(usize, f32)> {
    if new_count == 0 {
        return None;
    }
    let new_index = if reverse {
        anchor.index.saturating_add(1)
    } else if full {
        anchor.index.saturating_sub(1)
    } else {
        anchor.index
    };
    Some((new_index.min(new_count - 1), anchor.inner))
}

/// 行内容回调：把一行（或单元格）的子树建进 `parent`（应用侧建树，内核不解释内容）。
pub type RowContent = dyn Fn(&mut NodeTree, NodeId, usize) -> Result<(), EngineError>;

/// 把 `over` 里 `Some` 的 a11y 字段覆盖到 `base`（`None` 保留基线）。
///
/// `A11yPatch` 是值语义的「本帧补丁」：两条来源（列表行基线 + 组合控件的行补丁）必须合并成
/// 一份，`bind_a11y` 一个节点只能有一条通道。
fn merge_a11y(base: &mut A11yPatch, over: A11yPatch) {
    macro_rules! cover {
        ($($field:ident),+ $(,)?) => {
            $(
                if over.$field.is_some() {
                    base.$field = over.$field;
                }
            )+
        };
    }
    cover!(
        role,
        label,
        value,
        description,
        disabled,
        hidden,
        row_index,
        column_index,
        row_count,
        column_count,
        checked,
        selected,
        expanded,
        live,
        pos_in_set,
        size_of_set,
        numeric_value,
        min_numeric_value,
        max_numeric_value,
        numeric_value_step,
        numeric_value_jump,
        actions,
        key,
        sort,
        selection,
        custom_actions,
        read_only,
        modal,
        busy,
        invalid,
        error_message,
        placeholder,
        level,
    );
}

/// 列表的内核句柄（`Rc` 浅拷贝：`bind_style` / Action handler 里各持一份）。
#[derive(Clone)]
pub struct List {
    inner: Rc<ListInner>,
}

struct ListInner {
    key: SmolStr,
    theme: ThemeHandle,
    options: ListOptions,
    content: Rc<RowContent>,
    /// 本组件自有的信号作用域（动态创建 / 销毁的列表释放它回收槽位）。
    scope: SignalScope,
    selection: Signal<Selection>,
    hovered: Signal<Option<usize>>,
    count: Signal<usize>,
    /// 不可交互行（树用它表达 `TreeItem::disabled`）：命中 / 选中 / 激活都跳过。
    disabled: RefCell<BTreeSet<usize>>,
    /// 行级 a11y 补丁（表 / 树在列表基线上叠加角色 / 标签 / 展开态）。
    row_a11y: RefCell<Option<Rc<dyn Fn(usize) -> A11yPatch>>>,
    heights: RefCell<RowHeights>,
    geometry: RefCell<Geometry>,
    select_action: Action,
    activate: RefCell<Option<Rc<dyn Fn(&mut App, usize)>>>,
    scrollbar: ScrollBar,
}

#[derive(Default)]
struct Geometry {
    window: Option<WindowId>,
    outer: Option<NodeId>,
    viewport: Option<NodeId>,
    list: Option<VirtualList>,
    handle: Option<ScrollHandle>,
}

impl List {
    /// 建内核：状态信号 + 自绘滚动条。之后用 [`List::build`] + [`List::attach`]（或
    /// [`List::mount`]）接进引擎。
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        key: impl Into<SmolStr>,
        options: ListOptions,
        content: impl Fn(&mut NodeTree, NodeId, usize) -> Result<(), EngineError> + 'static,
    ) -> Self {
        let key = key.into();
        let scrollbar = ScrollBar::new(
            app,
            theme.clone(),
            format_smolstr!("{key}:scrollbar"),
            ScrollbarAxis::Vertical,
            ScrollbarOptions::default(),
        );
        let scope = app.signal_scope();
        Self {
            inner: Rc::new(ListInner {
                scope,
                selection: app.signal_scoped(scope, Selection::new()),
                hovered: app.signal_scoped(scope, None),
                count: app.signal_scoped(scope, 0usize),
                disabled: RefCell::new(BTreeSet::new()),
                row_a11y: RefCell::new(None),
                heights: RefCell::new(RowHeights::default()),
                geometry: RefCell::new(Geometry::default()),
                content: Rc::new(content),
                select_action: Action::Custom(format_smolstr!("{key}.select")),
                activate: RefCell::new(None),
                scrollbar,
                theme,
                options,
                key,
            }),
        }
    }

    /// 列表键（调试 id / Action key 的前缀）。
    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    /// 释放本组件的信号作用域与引擎侧注册（动态创建 / 销毁的列表必须调用一次）。
    ///
    /// 清理面：点击 Action handler（[`App::off_action`]）+ 滚动条的 handler / 信号作用域 + 本
    /// 列表的信号作用域。只释放信号不摘 handler 会让 `App` 永久持有 `Rc<List>` 闭包（P2-2
    /// 复核 R2.2 的泄漏）。**先卸载节点再释放**：`bind_style` / `bind_a11y` 闭包捕获了这些
    /// 句柄，释放后一次求值就会读死句柄 panic。重复调用返回 0（幂等）。
    pub fn release(&self, app: &mut App) -> usize {
        app.off_action(&self.inner.select_action);
        self.inner.scrollbar.release(app) + app.release_scope(self.inner.scope)
    }

    /// 登记不可交互行（组合控件用：树的 `TreeItem::disabled`）。
    ///
    /// 内核在点击 / 程序化选中 / 键盘激活入口过滤这些行；行内容本身仍会渲染（弱化色由调用方
    /// 的 `bind_style` 决定）。
    pub(crate) fn set_disabled_rows(&self, rows: impl IntoIterator<Item = usize>) {
        *self.inner.disabled.borrow_mut() = rows.into_iter().collect();
    }

    /// 该行是否不可交互。
    #[must_use]
    pub(crate) fn is_disabled(&self, index: usize) -> bool {
        self.inner.disabled.borrow().contains(&index)
    }

    /// 行级 a11y 补丁（组合控件用：表格在列表基线上叠加 `Row` / 单元格语义，树叠加标签 /
    /// 展开态）。补丁里 `Some` 的字段覆盖列表基线（`selected` / `pos_in_set` /
    /// `size_of_set` / `disabled`），`None` 保留基线。
    pub(crate) fn set_row_a11y(&self, row_a11y: impl Fn(usize) -> A11yPatch + 'static) {
        *self.inner.row_a11y.borrow_mut() = Some(Rc::new(row_a11y));
    }

    /// 只建节点（外层 + 虚拟列表容器 + 滚动条），不接引擎。
    ///
    /// 调用方随后必须调 [`List::attach`]（或用 [`List::mount`] 一步完成）。
    ///
    /// # Errors
    /// `StaleNode`（`parent` 陈旧）。
    pub fn build(&self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError> {
        let key = &self.inner.key;
        let outer_style = StyleBuilder::new()
            .relative()
            .size_full()
            .min_width_0()
            .min_height_0()
            .overflow_hidden()
            .build();
        let outer = NodeBuilder::boxed()
            .id(format_smolstr!("{key}:outer"))
            .style(outer_style)
            .build(tree, parent)?;
        let viewport_style = StyleBuilder::new()
            .relative()
            .size_full()
            .min_width_0()
            .min_height_0()
            .overflow_hidden()
            .build();
        let count = self.inner.count;
        let viewport = NodeBuilder::boxed()
            .id(format_smolstr!("{key}:viewport"))
            .style(viewport_style)
            .on_click(self.inner.select_action.clone())
            // a11y：虚拟列表容器的角色由引擎按种类推导成 `List`；这里补集合大小，
            // 屏幕阅读器才能播报「第 N 项，共 M 项」（§7.4）。
            .bind_a11y(move |cx| A11yPatch {
                size_of_set: Some(*cx.read(&count)),
                ..A11yPatch::default()
            })
            .build(tree, outer)?;
        if self.inner.options.scrollbar {
            self.inner.scrollbar.build(tree, outer)?;
        }
        let mut geometry = self.inner.geometry.borrow_mut();
        geometry.outer = Some(outer);
        geometry.viewport = Some(viewport);
        Ok(outer)
    }

    /// 接进引擎：`make_virtual_list` + 行工厂 + 点击 Action + 滚动条。
    ///
    /// 必须先 [`List::build`]；未 build 时是 no-op（`Ok(())`）。成功返回时首帧的可见行内容
    /// 已经建好（见 [`List::populate_first_frame`]）。
    ///
    /// # Errors
    ///
    /// - [`EngineError::UnknownWindow`] / [`EngineError::StaleNode`]：窗口已关闭，或 `build`
    ///   时的节点已不在该窗口（`make_virtual_list` / `set_virtual_items` / `fill_row_contents`
    ///   的引擎校验）；
    /// - [`EngineError::NotVirtualList`] / [`EngineError::InvalidVirtualExtent`]：`viewport`
    ///   不是可虚拟化容器 / 行高预置非法；
    /// - [`EngineError::InvalidMeasure`]：首帧同步建行时跑一次
    ///   `Window::compute_layout`，measure 闭包返回非有限 / 负尺寸时从这里冒泡；
    /// - 行工厂（`content`）建树失败时原样传播（通常是 `StaleNode`）。
    pub fn attach(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let (Some(viewport), Some(outer)) = (
            self.inner.geometry.borrow().viewport,
            self.inner.geometry.borrow().outer,
        ) else {
            return Ok(());
        };
        let _ = outer;
        let options = &self.inner.options;
        let list = app.make_virtual_list(
            window,
            viewport,
            VirtualListConfig {
                extent: options.row_height.extent(),
                extents: None,
                overscan: options.overscan,
            },
        )?;
        let count = self.count(app);
        self.inner.ensure_heights(count);
        app.set_virtual_items(&list, count, self.row_builder())?;
        let handle = app.scroll_handle(window, viewport);
        {
            let mut geometry = self.inner.geometry.borrow_mut();
            geometry.window = Some(window);
            geometry.list = Some(list);
            geometry.handle = handle;
        }
        if options.scrollbar {
            self.inner.scrollbar.attach(app, window, viewport);
        }
        let select = self.clone();
        app.on_action(self.inner.select_action.clone(), move |ctx| {
            select.on_click(ctx);
        });
        // 首帧同步建行（P2-2 复核遗留）：让首帧就有行内容，而不是空行容器。
        self.populate_first_frame(app, window)?;
        Ok(())
    }

    /// 一步完成 `build` + `attach`，返回外层容器节点。
    ///
    /// # Errors
    /// 同 [`List::build`] / [`List::attach`]。
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

    /// 外层容器节点（`build` 之后可用）。
    #[must_use]
    pub fn outer_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().outer
    }

    /// 虚拟列表容器节点（`build` 之后可用）。
    #[must_use]
    pub fn viewport_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().viewport
    }

    /// 当前条目数。
    #[must_use]
    pub fn count(&self, app: &App) -> usize {
        app.read_with(&self.inner.count, |count| *count)
    }

    /// 条目数信号（组合控件用：表格空态、树的行数派生）。
    #[must_use]
    pub fn count_signal(&self) -> Signal<usize> {
        self.inner.count
    }

    /// 初始条目数（`build` 之前由应用设定；`attach` 用它建首窗）。
    pub fn set_initial_count(&self, app: &mut App, count: usize) {
        self.inner.ensure_heights(count);
        app.write(&self.inner.count, count);
    }

    /// 选中集快照。
    #[must_use]
    pub fn selection(&self, app: &App) -> Selection {
        app.read_with(&self.inner.selection, Selection::clone)
    }

    /// 选中集信号（组合控件用）。
    #[must_use]
    pub fn selection_signal(&self) -> Signal<Selection> {
        self.inner.selection
    }

    /// 悬停行信号（`sync` 镜像引擎 hover；组合控件可读）。
    #[must_use]
    pub fn hovered_signal(&self) -> Signal<Option<usize>> {
        self.inner.hovered
    }

    /// 行高表快照（变高列表的测量结果；固定行高时为空表）。测试专用：没有下游消费者。
    #[cfg(test)]
    #[must_use]
    pub(crate) fn row_heights(&self) -> RowHeights {
        self.inner.heights.borrow().clone()
    }

    /// 引擎虚拟列表状态（可见窗口 / 偏移 / 视口）。
    #[must_use]
    pub fn state(&self, app: &App) -> Option<VirtualListState> {
        let list = self.inner.geometry.borrow().list?;
        app.virtual_list_state(&list)
    }

    /// 当前滚动偏移（逻辑像素，垂直）。
    #[must_use]
    pub fn offset(&self, app: &App) -> Option<f32> {
        let handle = self.inner.geometry.borrow().handle?;
        app.scroll_state(&handle).map(|state| state.offset.y)
    }

    /// 已挂载的 `(下标, 行容器节点)`（按升序；测试与组合控件用）。
    #[must_use]
    pub fn mounted_rows(&self, app: &App) -> Vec<(usize, NodeId)> {
        let geometry = self.inner.geometry.borrow();
        let (Some(window), Some(viewport)) = (geometry.window, geometry.viewport) else {
            return Vec::new();
        };
        drop(geometry);
        let Some(target) = app.window(window) else {
            return Vec::new();
        };
        let tree = target.tree();
        let Ok(container) = tree.get(viewport) else {
            return Vec::new();
        };
        let mut rows: Vec<(usize, NodeId)> = container
            .children()
            .iter()
            .filter_map(|row| {
                let id = tree.debug_id(*row)?;
                parse_row_index(&self.inner.key, id).map(|index| (index, *row))
            })
            .collect();
        rows.sort_unstable_by_key(|(index, _)| *index);
        rows
    }

    /// 选中 `index`（单选语义；`None` = 清空）。
    ///
    /// 不可交互行（[`List::set_disabled_rows`]）不接受程序化选中：返回 `false` 且不改选中集。
    pub fn select(&self, app: &mut App, index: Option<usize>) -> bool {
        if index.is_some_and(|index| self.is_disabled(index)) {
            return false;
        }
        let mut selection = Selection::new();
        if let Some(index) = index {
            selection.select(index);
        }
        app.write(&self.inner.selection, selection)
    }

    /// 清空选中。
    pub fn clear_selection(&self, app: &mut App) -> bool {
        app.write(&self.inner.selection, Selection::new())
    }

    /// 行激活回调（双击行 / 键盘 Enter 时调用；表格的行激活、树的折叠切换用）。
    pub fn on_activate(&self, callback: impl Fn(&mut App, usize) + 'static) {
        *self.inner.activate.borrow_mut() = Some(Rc::new(callback));
    }

    /// 设置条目数（静态列表的「换一批数据」）：重建可见窗口，**滚回顶部**并清空选中。
    ///
    /// # Errors
    /// 引擎侧错误（虚拟列表失效等）。
    pub fn set_items(&self, app: &mut App, count: usize) -> Result<(), EngineError> {
        self.inner.ensure_heights(count);
        self.rebuild(app, count, Some(0.0))?;
        app.write(&self.inner.selection, Selection::new());
        Ok(())
    }

    /// 设置条目数但**保留滚动位置与选中**（活数据刷新：表格行数变化、树展开/折叠）。
    ///
    /// # Errors
    /// 引擎侧错误。
    pub fn set_count(&self, app: &mut App, count: usize) -> Result<(), EngineError> {
        self.inner.ensure_heights(count);
        let offset = self.offset(app).unwrap_or(0.0);
        self.rebuild(app, count, Some(offset))?;
        app.update(&self.inner.selection, |selection| {
            selection.retain_below(count);
        });
        Ok(())
    }

    /// 双端列表 push 一条：条目数 +1（容量满则丢最旧一条），并按锚点保持读者位置。
    ///
    /// 应用的 deque 数据必须**先**按同样的 `capacity` / `reverse` 语义更新（追加或头插），
    /// 内核只维护条目数 / 行高 / 滚动位置。
    ///
    /// # Errors
    /// 引擎侧错误。
    pub fn push(&self, app: &mut App) -> Result<(), EngineError> {
        let capacity = self.inner.options.capacity.unwrap_or(usize::MAX).max(1);
        let old_count = self.count(app);
        let full = old_count >= capacity;
        let new_count = (old_count + 1).min(capacity);
        let Some(handle) = self.inner.geometry.borrow().handle else {
            return Ok(());
        };
        let Some(state) = app.scroll_state(&handle) else {
            return Ok(());
        };
        let viewport = state.viewport.height;
        let offset = state.offset.y;
        let (first, inner) = self.inner.index_at(offset, old_count);
        let max = (self.inner.content_height(old_count) - viewport).max(0.0);
        let at_newest = if self.inner.options.reverse {
            offset <= 0.5
        } else {
            max <= 0.5 || offset >= max - 0.5
        };
        let anchor = PushAnchor {
            index: first,
            inner,
            at_newest,
        };
        let estimate = self.inner.options.row_height.extent();
        if self.inner.options.row_height.is_variable() {
            let mut heights = self.inner.heights.borrow_mut();
            if self.inner.options.reverse {
                heights.insert_front(estimate);
                if full {
                    heights.pop_back();
                }
            } else {
                if full {
                    heights.pop_front();
                }
                heights.push_back(estimate);
            }
            heights.resize(new_count, estimate);
        }
        let follow = self.inner.options.keep_on_newest && at_newest;
        let target = if follow {
            if self.inner.options.reverse {
                0.0
            } else {
                (self.inner.content_height(new_count) - viewport).max(0.0)
            }
        } else if let Some((index, inner)) =
            push_anchor_after(anchor, full, self.inner.options.reverse, new_count)
        {
            self.inner.row_top(index, new_count) + inner
        } else {
            0.0
        };
        self.rebuild(app, new_count, Some(target))
    }

    /// 把第 `index` 行按 `align` 对齐到视口（E3 `scroll_to_item`）。
    ///
    /// # Errors
    /// `NotVirtualList` / `VirtualListExtents`。
    pub fn scroll_to_item(
        &self,
        app: &mut App,
        index: usize,
        align: ItemAlign,
    ) -> Result<(), EngineError> {
        let Some(list) = self.inner.geometry.borrow().list else {
            return Ok(());
        };
        app.scroll_to_item(&list, index, align)
    }

    /// 帧驱动同步：补行内容 → 镜像悬停 → 实测行高回写 → 滚动条几何。
    ///
    /// 返回 `true` = 本次真的改了东西（写信号 / 挂内容 / 行高变化）。无变化时不写状态、
    /// 不请求帧（I3）。组合根应在 `App::on_frame` 钩子里调用（测试里手动调用）。
    ///
    /// # Errors
    /// 行内容建树或引擎状态错误。
    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let Some(window) = self.inner.geometry.borrow().window else {
            return Ok(false);
        };
        let mut changed = false;
        let rows = self.mounted_rows(app);
        // ① 补行内容（引擎只挂了行容器）。
        let populated_now = self.fill_row_contents(app, window)?;
        if !populated_now.is_empty() {
            changed = true;
        }
        // ② 悬停行镜像（引擎的 hover 是全局 Event，ReadCtx 读不到；P2-4 的机制前移到本批的
        //    最小镜像：sync 时比对一次，变了才写信号）。
        let hovered = self.row_index_at_node(app, app.hovered(window));
        if app.write(&self.inner.hovered, hovered) {
            changed = true;
        }
        // ③ 变高列表：实测内容容器高度并回写（引擎按锚点补偿偏移）。
        if self.inner.options.row_height.is_variable() {
            let mut measured = false;
            {
                let Some(target) = app.window(window) else {
                    return Ok(changed);
                };
                let tree = target.tree();
                let padding = self.inner.options.padding_y;
                let mut heights = self.inner.heights.borrow_mut();
                for &(index, row) in &rows {
                    if populated_now.contains(&index) {
                        continue;
                    }
                    let Some(content) = tree
                        .get(row)
                        .ok()
                        .and_then(|node| node.children().first().copied())
                    else {
                        continue;
                    };
                    let height = tree
                        .get(content)
                        .map(|node| node.layout().bounds.size.height)
                        .unwrap_or(0.0)
                        + padding * 2.0;
                    if heights.set(index, height) {
                        measured = true;
                    }
                }
            }
            if measured {
                let heights = self.inner.heights.borrow().as_slice().to_vec();
                if let Some(list) = self.inner.geometry.borrow().list {
                    app.set_virtual_extents(&list, &heights)?;
                }
                changed = true;
            }
        }
        // ④ 滚动条（几何 / 淡出 / 悬停色）。
        if self.inner.scrollbar.sync(app)? {
            changed = true;
        }
        Ok(changed)
    }

    /// 给「已挂载但还没有子节点」的行补内容容器 + 应用行内容；返回本次补了内容的下标。
    ///
    /// 同步建树（不是帧驱动）：`sync`、`rebuild`（`set_items` / `set_count` / `push`）与
    /// `attach` 的首帧建行（[`List::populate_first_frame`]）都调它，因此首帧与重建路径首帧
    /// 就有行内容。只有**引擎在帧内挂载**的新行（`scroll_to_item` 跳跃）要等下一帧的 `sync` 才
    /// 补 —— 见模块文档〈行内容的挂载时机〉。
    fn fill_row_contents(
        &self,
        app: &mut App,
        window: WindowId,
    ) -> Result<Vec<usize>, EngineError> {
        let rows = self.mounted_rows(app);
        let mut populated: Vec<usize> = Vec::new();
        for &(index, row) in &rows {
            let needs_content = app
                .window(window)
                .and_then(|target| target.tree().get(row).ok())
                .is_some_and(|node| node.children().is_empty());
            if !needs_content {
                continue;
            }
            let content = self.inner.content.clone();
            let content_id = format_smolstr!("{}:row:{}:content", self.inner.key, index);
            let content_style = StyleBuilder::new()
                .flex_row()
                .width_full()
                .min_width_0()
                .min_height_0()
                .build();
            let Some(target) = app.window_mut(window) else {
                break;
            };
            let tree = target.tree_mut();
            let parent = NodeBuilder::boxed()
                .id(content_id)
                .style(content_style)
                .build(tree, row)?;
            content(tree, parent, index)?;
            populated.push(index);
        }
        // P3-11：只标本次真的补了内容的行（旧实现把整个可见窗口都标脏）。
        for &(index, row) in &rows {
            if populated.contains(&index) {
                app.request_node_repaint(window, row)?;
            }
        }
        Ok(populated)
    }

    /// 行容器节点（已挂载时）。
    #[must_use]
    pub fn row_node(&self, app: &App, index: usize) -> Option<NodeId> {
        self.mounted_rows(app)
            .into_iter()
            .find_map(|(mounted, node)| (mounted == index).then_some(node))
    }

    /// 行内容容器节点（行已填充内容时）。
    #[must_use]
    pub fn row_content_node(&self, app: &App, index: usize) -> Option<NodeId> {
        let row = self.row_node(app, index)?;
        let window = self.inner.geometry.borrow().window?;
        let tree = app.window(window)?.tree();
        tree.get(row).ok()?.children().first().copied()
    }

    /// 送达点击 Action 的落点行下标（命中链 → 已挂载行）。
    fn row_index_at_node(&self, app: &App, start: Option<NodeId>) -> Option<usize> {
        let geometry = self.inner.geometry.borrow();
        let (Some(window), Some(viewport), Some(list)) =
            (geometry.window, geometry.viewport, geometry.list)
        else {
            return None;
        };
        drop(geometry);
        let mut cursor = start;
        let tree = app.window(window)?.tree();
        while let Some(node) = cursor {
            if let Some(index) = app.virtual_row_index(&list, node) {
                return Some(index);
            }
            if node == viewport {
                return None;
            }
            cursor = tree.get(node).ok().and_then(|target| target.parent());
        }
        None
    }

    fn on_click(&self, ctx: &mut ActionCtx<'_>) {
        // 键盘 Enter / Space 是对焦点节点的「激活」：引擎把它按 Click 派发到这同一个动作上
        // （`ActionSource::Click` 且没有指针命中）。此时作用在选中行上，不改选中集。
        let hit_node = ctx.hit().map(|hit| hit.node);
        if hit_node.is_none() {
            let selected = self.selection(ctx.app_mut()).first();
            if let Some(index) = selected
                && !self.is_disabled(index)
            {
                let callback = self.inner.activate.borrow().clone();
                if let Some(callback) = callback {
                    callback(ctx.app_mut(), index);
                }
            }
            return;
        }
        let Some(index) = self.row_index_at_node(ctx.app_mut(), hit_node) else {
            return;
        };
        // 不可交互行：点击既不选中也不激活（旧 gpui `Button::disabled` 语义）。
        if self.is_disabled(index) {
            return;
        }
        let mode = self.inner.options.selection;
        if mode != SelectionMode::None {
            let ctrl = ctx.modifiers().contains(Modifiers::CONTROL);
            let shift = ctx.modifiers().contains(Modifiers::SHIFT);
            ctx.update(&self.inner.selection, |selection| {
                apply_click(selection, index, mode, ctrl, shift);
            });
        }
        let click_count = match ctx.source() {
            ui_core::event::ActionSource::Click { click_count, .. } => click_count,
            _ => 1,
        };
        if click_count >= 2 {
            let callback = self.inner.activate.borrow().clone();
            if let Some(callback) = callback {
                callback(ctx.app_mut(), index);
            }
        }
    }

    fn row_builder(&self) -> impl FnMut(usize) -> NodeBuilder + 'static {
        let key = self.inner.key.clone();
        let theme = self.inner.theme.clone();
        let selection = self.inner.selection;
        let hovered = self.inner.hovered;
        let count = self.inner.count;
        let options = self.inner.options.clone();
        let inner = self.inner.clone();
        move |index| {
            let row_style = StyleBuilder::new()
                .flex_row()
                .width_full()
                .min_width_0()
                .min_height_0()
                .padding_sides(
                    options.padding_y,
                    options.padding_x,
                    options.padding_y,
                    options.padding_x,
                )
                .overflow_hidden();
            let row_style = if options.row_stretch {
                row_style.build()
            } else {
                row_style.items_center().build()
            };
            let row_theme = theme.clone();
            let a11y_inner = inner.clone();
            NodeBuilder::boxed()
                .id(format_smolstr!("{key}:row:{index}"))
                .style(row_style)
                .bind_style(move |cx| {
                    let selected = cx.read(&selection).contains(index);
                    let hovered = *cx.read(&hovered) == Some(index);
                    let background = if selected {
                        Some(row_theme.role(cx, Role::Selection))
                    } else if hovered {
                        Some(row_theme.role(cx, Role::VeilHover))
                    } else {
                        None
                    };
                    StylePatch {
                        background,
                        ..StylePatch::default()
                    }
                })
                // a11y 行基线（§7.4：虚拟列表行 = `ListBoxOption`，位置走 `pos_in_set` /
                // `size_of_set`）：选中 / 禁用 / 集合位置由内核声明；组合控件（表格 / 树）的
                // `set_row_a11y` 补丁在此基础上叠加角色 / 标签 / 展开态。
                .bind_a11y(move |cx| {
                    let mut patch = A11yPatch {
                        selected: Some(cx.read(&selection).contains(index)),
                        pos_in_set: Some(index),
                        size_of_set: Some(*cx.read(&count)),
                        disabled: Some(a11y_inner.disabled.borrow().contains(&index)),
                        ..A11yPatch::default()
                    };
                    let row_a11y = a11y_inner.row_a11y.borrow().clone();
                    if let Some(row_a11y) = row_a11y {
                        merge_a11y(&mut patch, row_a11y(index));
                    }
                    patch
                })
        }
    }

    /// 重建可见窗口（`set_items` / `set_count` / `push` 的共同路径）。
    fn rebuild(
        &self,
        app: &mut App,
        count: usize,
        target_offset: Option<f32>,
    ) -> Result<(), EngineError> {
        let window = self.inner.geometry.borrow().window;
        let (Some(list), Some(handle)) = (
            self.inner.geometry.borrow().list,
            self.inner.geometry.borrow().handle,
        ) else {
            app.write(&self.inner.count, count);
            return Ok(());
        };
        let heights = if self.inner.options.row_height.is_variable() {
            Some(self.inner.heights.borrow().as_slice().to_vec())
        } else {
            None
        };
        app.batch(|app| {
            // 强制重建：条目内容按新下标重排后，引擎必须丢掉旧行（否则已挂载行还是旧内容）。
            app.set_virtual_items(&list, 0, self.row_builder())?;
            app.set_virtual_items(&list, count, self.row_builder())?;
            if let Some(heights) = heights.as_deref()
                && count > 0
            {
                app.set_virtual_extents(&list, heights)?;
            }
            if let Some(offset) = target_offset {
                app.scroll_to(&handle, Point::new(0.0, offset))?;
            }
            Ok(())
        })?;
        app.write(&self.inner.count, count);
        // 同步补行内容（P2-2b）：`set_virtual_items` 已把行容器挂上，这里立刻建内容子树，
        // 重建路径的首帧就不是空行。`scroll_to_item` 在帧内挂载的新行仍走下一帧 `sync`。
        if let Some(window) = window {
            let _ = self.fill_row_contents(app, window)?;
        }
        Ok(())
    }

    /// 首帧同步建行（P2-2 复核遗留）：让**首帧**就有行内容，而不是空行容器。
    ///
    /// # 前置条件依赖（重要）
    ///
    /// 本方法在帧外调用 [`Window::compute_layout`]，而该 API 的文档要求「文本节点必须先在本帧
    /// 调用 `App::refresh_text`（shape）后再测量，否则文本测量为 0×0」。这里的正确性**不是**
    /// 靠满足那个前置条件，而是靠两个已成立的性质，改动其中任一个都可能把未 shape 的文本冻结
    /// 成 0 高度：
    ///
    /// 1. `set_text_shaped`（`node::tree::content`）无条件清掉 `node..root` 的 taffy 缓存 ——
    ///    首帧 `refresh_text` 把真实文本 shape 完之后，缓存已失效，布局会重新测量；
    /// 2. 本次 `set_virtual_items` 新挂的行容器 / 内容子树带 `LAYOUT` 脏位 —— 首帧一定重跑
    ///    布局，不会沿用这次冷路径布局的 0 高度结果。
    ///
    /// 若未来把本路径改成引擎级 `App::settle_layout(window)`（E8.md §9.3 的候选），这些依赖由
    /// 引擎入口承担，这里的注释随之更新。
    ///
    /// 引擎在视口未知（首次布局前）时按「挂首行 + overscan」处理可见窗口
    /// （`interaction::virtual_list` 的「视口未知」分支），其余可见行要等首帧
    /// `rewindow_virtuals`（布局之后）才挂 —— 而列表只能在 `sync` 里补内容，于是首帧画出来
    /// 是空行。`attach` 是冷路径，这里主动 measure-then-place：
    ///
    /// 1. `Window::compute_layout` 跑一次布局拿到真实视口尺寸（幂等；尚未 shape 的文本测量为 0，
    ///    但只影响文本自身，不影响外层 / 行容器几何）；
    /// 2. 再 `set_virtual_items` 让引擎按真实视口重挂可见窗口（**同步**挂载行容器）；
    /// 3. `fill_row_contents` 同步建内容子树。
    ///
    /// 新挂的内容子树是 `LAYOUT` 脏节点：首帧的 `refresh_text` + 布局会正常处理它们（内容文本
    /// 在强制布局时还不存在，因此不会被 0 测量冻结）。程序化 `scroll_to_item` 在**帧内**挂载的
    /// 新行仍要等下一帧 `sync`（引擎行工厂形态的残余，见模块文档〈行内容的挂载时机〉）。
    fn populate_first_frame(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        {
            let Some(target) = app.window_mut(window) else {
                return Ok(());
            };
            target.compute_layout()?;
        }
        let count = self.count(app);
        let Some(list) = self.inner.geometry.borrow().list else {
            return Ok(());
        };
        app.set_virtual_items(&list, count, self.row_builder())?;
        let _ = self.fill_row_contents(app, window)?;
        Ok(())
    }
}

impl ListInner {
    fn ensure_heights(&self, count: usize) {
        if !self.options.row_height.is_variable() {
            return;
        }
        let estimate = self.options.row_height.extent();
        let mut heights = self.heights.borrow_mut();
        if heights.is_empty() && count > 0 {
            *heights = RowHeights::filled(count, estimate);
        } else {
            heights.resize(count, estimate);
        }
    }

    fn row_top(&self, index: usize, count: usize) -> f32 {
        match self.options.row_height {
            RowHeight::Fixed(extent) => index.min(count) as f32 * extent,
            RowHeight::Variable { estimate } => {
                let heights = self.heights.borrow();
                if heights.is_empty() {
                    index as f32 * estimate
                } else {
                    heights.top(index)
                }
            }
        }
    }

    fn content_height(&self, count: usize) -> f32 {
        match self.options.row_height {
            RowHeight::Fixed(extent) => count as f32 * extent,
            RowHeight::Variable { estimate } => {
                let heights = self.heights.borrow();
                if heights.is_empty() {
                    count as f32 * estimate
                } else {
                    heights.total()
                }
            }
        }
    }

    fn index_at(&self, offset: f32, count: usize) -> (usize, f32) {
        if count == 0 {
            return (0, 0.0);
        }
        match self.options.row_height {
            RowHeight::Fixed(extent) => {
                let index = ((offset / extent).floor().max(0.0) as usize).min(count - 1);
                (index, offset - index as f32 * extent)
            }
            RowHeight::Variable { estimate } => {
                let heights = self.heights.borrow();
                if heights.is_empty() {
                    let index = ((offset / estimate).floor().max(0.0) as usize).min(count - 1);
                    (index, offset - index as f32 * estimate)
                } else {
                    heights.index_at(offset)
                }
            }
        }
    }
}

/// 一行容器的调试 id（测试用：生产侧只解析、不拼装）。
#[cfg(test)]
#[must_use]
pub(crate) fn row_debug_id(key: &str, index: usize) -> SmolStr {
    format_smolstr!("{key}:row:{index}")
}

/// 从行容器的调试 id 解析下标（内容容器 / 其他 id 返回 `None`）。
#[must_use]
pub(crate) fn parse_row_index(key: &str, id: &str) -> Option<usize> {
    id.strip_prefix(key)?.strip_prefix(":row:")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_heights_prefix_and_index_at() {
        let mut heights = RowHeights::filled(4, 25.0);
        assert_eq!(heights.total(), 100.0);
        assert_eq!(heights.top(0), 0.0);
        assert_eq!(heights.top(2), 50.0);
        assert_eq!(heights.index_at(0.0), (0, 0.0));
        assert_eq!(heights.index_at(24.0), (0, 24.0));
        assert_eq!(heights.index_at(25.0), (1, 0.0));
        assert_eq!(heights.index_at(74.0), (2, 24.0));
        assert_eq!(
            heights.index_at(200.0),
            (3, 125.0),
            "越界偏移退到末项（行内偏移可超过行高）"
        );

        assert!(heights.set(1, 50.0));
        assert_eq!(heights.total(), 125.0);
        assert_eq!(heights.top(2), 75.0);
        assert_eq!(heights.index_at(75.0), (2, 0.0));
        assert!(!heights.set(1, 50.0), "同值不重建前缀和");

        let empty = RowHeights::default();
        assert_eq!(empty.index_at(12.0), (0, 12.0), "空表无条目");
        assert_eq!(empty.total(), 0.0);
    }

    #[test]
    fn row_heights_mutations_keep_the_prefix_consistent() {
        let mut heights = RowHeights::filled(3, 10.0);
        heights.insert_front(5.0);
        assert_eq!(heights.as_slice(), &[5.0, 10.0, 10.0, 10.0]);
        assert_eq!(heights.top(1), 5.0);
        assert_eq!(heights.total(), 35.0);

        heights.pop_back();
        assert_eq!(heights.total(), 25.0);
        heights.push_back(20.0);
        assert_eq!(heights.total(), 45.0);
        heights.pop_front();
        assert_eq!(heights.top(0), 0.0);
        assert_eq!(heights.total(), 40.0);

        heights.resize(5, 7.0);
        assert_eq!(heights.as_slice(), &[10.0, 10.0, 20.0, 7.0, 7.0]);
        assert_eq!(heights.total(), 54.0);
        heights.resize(2, 7.0);
        assert_eq!(heights.total(), 20.0);
    }

    #[test]
    fn selection_click_modes() {
        let mut selection = Selection::new();
        apply_click(&mut selection, 3, SelectionMode::Single, false, false);
        assert_eq!(selection.iter().collect::<Vec<_>>(), vec![3]);

        apply_click(&mut selection, 5, SelectionMode::Multi, false, false);
        assert_eq!(selection.iter().collect::<Vec<_>>(), vec![5]);
        apply_click(&mut selection, 7, SelectionMode::Multi, true, false);
        assert_eq!(selection.iter().collect::<Vec<_>>(), vec![5, 7]);
        apply_click(&mut selection, 7, SelectionMode::Multi, true, false);
        assert_eq!(
            selection.iter().collect::<Vec<_>>(),
            vec![5],
            "Ctrl 再点切换掉"
        );

        selection.select(2);
        apply_click(&mut selection, 5, SelectionMode::Multi, false, true);
        assert_eq!(selection.iter().collect::<Vec<_>>(), vec![2, 3, 4, 5]);
        assert_eq!(selection.first(), Some(2));

        apply_click(&mut selection, 9, SelectionMode::None, true, true);
        assert_eq!(selection.len(), 4, "None 模式不改选中");

        assert!(selection.retain_below(4));
        assert_eq!(selection.iter().collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn push_anchor_moves_with_the_dropped_or_inserted_item() {
        let anchor = PushAnchor {
            index: 3,
            inner: 4.0,
            at_newest: false,
        };
        assert_eq!(push_anchor_after(anchor, false, false, 5), Some((3, 4.0)));
        assert_eq!(
            push_anchor_after(anchor, true, false, 5),
            Some((2, 4.0)),
            "非倒序满容量丢首项 ⇒ 读者下标 −1"
        );
        assert_eq!(
            push_anchor_after(anchor, true, true, 5),
            Some((4, 4.0)),
            "倒序满容量头插 ⇒ 读者下标 +1"
        );
        assert_eq!(
            push_anchor_after(anchor, false, false, 3),
            Some((2, 4.0)),
            "条目数缩小时 clamp 到末项"
        );
        assert_eq!(push_anchor_after(anchor, false, false, 0), None);
    }

    #[test]
    fn row_ids_roundtrip() {
        assert_eq!(row_debug_id("list", 7), SmolStr::new_static("list:row:7"));
        assert_eq!(parse_row_index("list", "list:row:7"), Some(7));
        assert_eq!(parse_row_index("list", "list:row:7:content"), None);
        assert_eq!(parse_row_index("list", "other:row:7"), None);
    }
}

#[cfg(test)]
mod ui_tests {
    use std::{cell::RefCell, collections::VecDeque, rc::Rc};

    use smol_str::{SmolStr, format_smolstr};
    use ui_core::{
        error::EngineError,
        event::{InputEvent, Modifiers, PointerButton},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        test_support::app::TestApp,
        text::TextStyle,
    };

    use super::{List, ListOptions, RowHeight, SelectionMode};
    use crate::{
        style_builder::StyleBuilder,
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    const BOX_WIDTH: f32 = 200.0;
    const BOX_HEIGHT: f32 = 120.0;
    const ROW_EXTENT: f32 = 30.0;

    struct Harness {
        app: TestApp,
        list: List,
    }

    fn text_row(tree: &mut NodeTree, parent: NodeId, index: usize) -> Result<(), EngineError> {
        NodeBuilder::text(format_smolstr!("row{index}"), TextStyle::new(12.0))
            .build(tree, parent)?;
        Ok(())
    }

    fn harness_with(
        options: ListOptions,
        rows: usize,
        content: impl Fn(&mut NodeTree, NodeId, usize) -> Result<(), EngineError> + 'static,
    ) -> Harness {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let list = List::new(app.app_mut(), theme, "list", options, content);
        list.set_initial_count(app.app_mut(), rows);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let parent = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(BOX_WIDTH)
                        .height(BOX_HEIGHT)
                        .build(),
                )
                .build(tree, root)
                .expect("parent")
        };
        list.mount(app.app_mut(), window, parent).expect("mount");
        let mut harness = Harness { app, list };
        settle(&mut harness);
        harness
    }

    fn harness(rows: usize) -> Harness {
        harness_with(ListOptions::fixed(ROW_EXTENT), rows, text_row)
    }

    fn settle(harness: &mut Harness) {
        for _ in 0..6 {
            let _ = harness.app.drain().expect("frame");
            let _ = harness.list.sync(harness.app.app_mut()).expect("sync");
        }
        let _ = harness.app.drain().expect("frame");
    }

    fn center(app: &TestApp, node: NodeId) -> ui_core::math::Point {
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

    fn collect_texts(tree: &NodeTree, node: NodeId, out: &mut Vec<SmolStr>) {
        let Ok(target) = tree.get(node) else {
            return;
        };
        if let ui_core::node::tree::NodeKind::Text(content) = target.kind() {
            out.push(content.text.clone());
        }
        for child in target.children() {
            collect_texts(tree, *child, out);
        }
    }

    /// 子树里第一个文本节点（首帧测量断言用）。
    fn first_text_node(tree: &NodeTree, node: NodeId) -> Option<NodeId> {
        let target = tree.get(node).ok()?;
        for child in target.children() {
            let Ok(child_target) = tree.get(*child) else {
                continue;
            };
            if matches!(child_target.kind(), ui_core::node::tree::NodeKind::Text(_)) {
                return Some(*child);
            }
            if let Some(found) = first_text_node(tree, *child) {
                return Some(found);
            }
        }
        None
    }

    fn row_texts(
        app: &ui_core::app::App,
        window: ui_core::window::WindowId,
        list: &List,
        row: usize,
    ) -> Vec<SmolStr> {
        let tree = app.window(window).expect("window").tree();
        let Some(row) = list.row_node(app, row) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        collect_texts(tree, row, &mut out);
        out
    }

    /// P2-2 复核遗留：初始 `mount` 的**首帧**就有行内容。
    ///
    /// `attach` 末尾按真实视口同步建行（先 `compute_layout` 拿到视口尺寸，再按视口重挂可见
    /// 窗口并补内容），因此 `mount` 返回时/首帧 `drain` 后都不存在空行容器。
    #[test]
    fn mounting_populates_row_contents_before_the_first_frame() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let list = List::new(
            app.app_mut(),
            theme,
            "first-frame",
            ListOptions::fixed(ROW_EXTENT),
            text_row,
        );
        list.set_initial_count(app.app_mut(), 20);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let parent = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(BOX_WIDTH)
                        .height(BOX_HEIGHT)
                        .build(),
                )
                .build(tree, root)
                .expect("parent")
        };
        list.mount(app.app_mut(), window, parent).expect("mount");

        // mount 返回时（任何帧 / sync 之前）可见窗口已按**真实视口**挂满：视口 120 / 行高 30
        // + overscan 2 ⇒ 6 行；如果 `attach` 不主动布局，引擎只能按「视口未知」挂首行+overscan。
        let mounted = list.mounted_rows(app.app());
        assert_eq!(mounted.len(), 6, "首帧前必须按真实视口挂满可见窗口");
        for (index, _) in &mounted {
            assert_eq!(
                row_texts(app.app(), window, &list, *index),
                vec![format_smolstr!("row{index}")],
                "第 {index} 行内容必须在首帧前同步建好"
            );
        }

        let first = app.drain().expect("frame");
        assert!(first.rendered, "首帧必须渲染");
        assert_eq!(
            list.mounted_rows(app.app()).len(),
            6,
            "首帧不得再靠 rewindow 补挂新行（否则这些行首帧是空的）"
        );
        for (index, _) in list.mounted_rows(app.app()) {
            assert!(
                !row_texts(app.app(), window, &list, index).is_empty(),
                "首帧第 {index} 行不得是空行"
            );
        }

        // 一次渲染即 Idle：首帧之后没有新脏位 / 帧请求，不得持续出帧。
        for frame_number in 0..5 {
            let record = app.drain().expect("frame");
            assert!(
                !record.rendered,
                "首帧之后第 {} 帧不得再渲染（出帧循环）",
                frame_number + 2
            );
        }

        // 首帧行文本必须已 shape + 布局（不是 0×0 的空文本节点）。
        let row0 = list.row_node(app.app(), 0).expect("row 0");
        let text = first_text_node(app.app().window(window).expect("window").tree(), row0)
            .expect("第 0 行必须有文本节点");
        let bounds = app.probe().bounds(text).expect("行文本 bounds");
        assert!(
            bounds.size.width > 0.0 && bounds.size.height > 0.0,
            "首帧行文本必须已布局（bounds 非零）：{bounds:?}"
        );
    }

    /// 组件 release 必须摘掉点击 Action handler。
    ///
    /// 「重放点击不 panic」不足以证明清理生效：`List` 的指针路径对死句柄只做 `ctx.update`
    /// （no-op，不 panic）。这里直接观察 handler 表项，并用 `InputOutcome::action`（= handler
    /// 真的被调用）验证重放点击不再命中。
    #[test]
    fn releasing_the_list_removes_its_click_handler() {
        let mut harness = harness(5);
        let select_action = harness.list.inner.select_action.clone();
        assert!(
            harness.app.app().has_action_handler(&select_action),
            "attach 必须注册点击 handler"
        );
        let row0 = harness.list.row_node(harness.app.app(), 0).expect("row 0");
        let point0 = center(&harness.app, row0);
        let click0 = click_outcome(&mut harness.app, point0);
        assert_eq!(
            click0.action,
            Some(select_action.clone()),
            "释放前点击必须由列表 handler 消费"
        );
        assert_eq!(
            harness.list.selection(harness.app.app()).first(),
            Some(0),
            "释放前点击正常选中"
        );

        assert!(harness.list.release(harness.app.app_mut()) > 0);
        assert!(
            !harness.app.app().has_action_handler(&select_action),
            "release 必须摘掉点击 handler（否则 App 永久持有 Rc<List>）"
        );

        // 不卸载节点直接释放（正常契约是先卸载再 release）：重放点击不得再调用已摘除的 handler。
        let row1 = harness.list.row_node(harness.app.app(), 1).expect("row 1");
        let point1 = center(&harness.app, row1);
        let click1 = click_outcome(&mut harness.app, point1);
        assert_eq!(click1.action, None, "释放后重放点击不得再命中列表 handler");
    }

    /// 重建路径（`set_items` / `set_count` / `push`）同步补行内容：调用返回时行内容已经挂好。
    #[test]
    fn rebuilding_items_populates_row_contents_in_the_same_call() {
        let mut harness = harness(10);
        harness
            .list
            .set_items(harness.app.app_mut(), 5)
            .expect("set items");
        assert_eq!(harness.list.count(harness.app.app()), 5);
        // 不 drain、不 sync：重建路径在 `set_virtual_items` 之后同步建内容容器 + 行内容。
        assert_eq!(
            row_texts(harness.app.app(), harness.app.window_id(), &harness.list, 0),
            vec![SmolStr::new_static("row0")],
            "set_items 返回时首行内容已挂载"
        );
        assert_eq!(
            row_texts(harness.app.app(), harness.app.window_id(), &harness.list, 4),
            vec![SmolStr::new_static("row4")]
        );
        let _ = harness.app.drain().expect("frame");
        let _ = harness.list.sync(harness.app.app_mut()).expect("sync");
        assert_eq!(harness.list.count(harness.app.app()), 5);
    }

    /// 禁用行：点击不选中、程序化选中被拒绝（旧 gpui `Button::disabled` 语义）。
    #[test]
    fn disabled_rows_reject_clicks_and_programmatic_selection() {
        let mut harness = harness(5);
        harness.list.set_disabled_rows([1usize]);
        let row = harness.list.row_node(harness.app.app(), 1).expect("row 1");
        let point = center(&harness.app, row);
        click(&mut harness.app, point);
        assert!(
            harness.list.selection(harness.app.app()).is_empty(),
            "点击禁用行不选中"
        );
        let row = harness.list.row_node(harness.app.app(), 0).expect("row 0");
        let point = center(&harness.app, row);
        click(&mut harness.app, point);
        assert_eq!(harness.list.selection(harness.app.app()).first(), Some(0));
        assert!(
            !harness.list.select(harness.app.app_mut(), Some(1)),
            "程序化选中禁用行被拒绝"
        );
        assert_eq!(
            harness.list.selection(harness.app.app()).first(),
            Some(0),
            "拒绝后选中集不变"
        );
        assert!(harness.list.select(harness.app.app_mut(), Some(2)));
    }

    /// 行信号暴露 a11y 语义（§7.4）：容器 `List`、行 `ListBoxOption`、选中 / 位置 / 集合大小。
    #[test]
    fn rows_declare_list_semantics_to_a11y() {
        let mut harness = harness(4);
        harness.app.enable_a11y_capture();
        let _ = harness.app.drain().expect("frame");
        let viewport = harness.list.viewport_node().expect("viewport");
        let row0 = harness.list.row_node(harness.app.app(), 0).expect("row 0");
        let row1 = harness.list.row_node(harness.app.app(), 1).expect("row 1");
        {
            let snapshot = harness.app.a11y().expect("a11y snapshot");
            let container = snapshot.node_for(viewport).expect("viewport a11y");
            assert_eq!(container.role, ui_core::a11y::A11yRole::List);
            assert_eq!(container.size_of_set, Some(4), "列表宣告集合大小");
            let row = snapshot.node_for(row0).expect("row 0 a11y");
            assert_eq!(row.role, ui_core::a11y::A11yRole::ListBoxOption);
            assert_eq!(row.pos_in_set, Some(0));
            assert_eq!(row.size_of_set, Some(4));
            assert_eq!(row.selected, Some(false));
        }
        let point = center(&harness.app, row1);
        click(&mut harness.app, point);
        let _ = harness.app.drain().expect("frame");
        let snapshot = harness.app.a11y().expect("a11y snapshot");
        let row = snapshot.node_for(row1).expect("row 1 a11y");
        assert_eq!(row.selected, Some(true), "选中态进 a11y");
    }

    /// 组件自有信号作用域：`release` 回收槽位（动态列表不泄漏），重复调用幂等。
    #[test]
    fn releasing_the_list_scope_reclaims_its_signal_slots() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let list = List::new(
            app.app_mut(),
            theme,
            "released",
            ListOptions::fixed(ROW_EXTENT),
            text_row,
        );
        let released = list.release(app.app_mut());
        assert!(
            released >= 5,
            "三条列表信号 + 滚动条两条信号必须被回收（实际 {released}）"
        );
        assert_eq!(list.release(app.app_mut()), 0, "重复释放幂等");
    }

    #[test]
    fn a_fixed_list_mounts_only_the_visible_window() {
        let harness = harness(20);
        let mounted = harness.list.mounted_rows(harness.app.app());
        assert_eq!(mounted.len(), 6, "视口 120 / 行高 30 + overscan 2 ⇒ 6 行");
        let first = harness.list.row_node(harness.app.app(), 0).expect("row 0");
        let second = harness.list.row_node(harness.app.app(), 1).expect("row 1");
        let first_bounds = harness.app.probe().bounds(first).expect("row 0 bounds");
        let second_bounds = harness.app.probe().bounds(second).expect("row 1 bounds");
        assert!(
            (second_bounds.origin.y - first_bounds.origin.y - ROW_EXTENT).abs() < 0.5,
            "行按 extent 紧凑排列：{first_bounds:?} {second_bounds:?}"
        );
        assert_eq!(
            row_texts(harness.app.app(), harness.app.window_id(), &harness.list, 2),
            vec![SmolStr::new_static("row2")]
        );
    }

    #[test]
    fn scrolling_moves_the_window_and_recycles_rows() {
        let mut harness = harness(20);
        harness
            .list
            .scroll_to_item(
                harness.app.app_mut(),
                10,
                ui_core::interaction::ItemAlign::Start,
            )
            .expect("scroll to item");
        let _ = harness.app.drain().expect("frame");
        let _ = harness.list.sync(harness.app.app_mut()).expect("sync");
        assert_eq!(
            harness.list.offset(harness.app.app()),
            Some(300.0),
            "第 10 行贴顶 = 10 * 30"
        );
        let mounted = harness.list.mounted_rows(harness.app.app());
        assert!(mounted.iter().any(|(index, _)| *index == 10));
        assert!(
            !mounted.iter().any(|(index, _)| *index == 0),
            "前缀行被回收"
        );
        assert_eq!(
            row_texts(
                harness.app.app(),
                harness.app.window_id(),
                &harness.list,
                10
            ),
            vec![SmolStr::new_static("row10")]
        );

        // 滚轮：引擎的平滑滚动动画推进后偏移真的变了。
        let viewport = harness.list.viewport_node().expect("viewport");
        let point = center(&harness.app, viewport);
        let _ = harness.app.inject(InputEvent::scroll_lines(
            point,
            ui_core::math::Point::new(0.0, -2.0),
            Modifiers::empty(),
        ));
        for _ in 0..8 {
            harness.app.advance(std::time::Duration::from_millis(40));
            let _ = harness.app.drain().expect("frame");
        }
        let offset = harness.list.offset(harness.app.app()).expect("offset");
        assert!(offset > 300.0, "滚轮下滚后偏移增大：{offset}");
    }

    #[test]
    fn variable_heights_are_measured_and_written_back() {
        let mut harness = harness_with(
            ListOptions::variable(ROW_EXTENT),
            20,
            |tree, parent, index| {
                let height = if index.is_multiple_of(2) { 40.0 } else { 20.0 };
                NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .width_full()
                            .height(height)
                            .flex_none()
                            .build(),
                    )
                    .build(tree, parent)?;
                Ok(())
            },
        );
        settle(&mut harness);
        let heights = harness.list.row_heights();
        assert_eq!(heights.get(0), Some(44.0), "40 + 上下 padding 2*2");
        assert_eq!(heights.get(1), Some(24.0), "20 + padding");
        let first = harness.list.row_node(harness.app.app(), 0).expect("row 0");
        let second = harness.list.row_node(harness.app.app(), 1).expect("row 1");
        let first_bounds = harness.app.probe().bounds(first).expect("bounds");
        let second_bounds = harness.app.probe().bounds(second).expect("bounds");
        assert!(
            (first_bounds.size.height - 44.0).abs() < 0.5,
            "{first_bounds:?}"
        );
        assert!(
            (second_bounds.origin.y - first_bounds.origin.y - 44.0).abs() < 0.5,
            "行位置跟前缀和走：{first_bounds:?} {second_bounds:?}"
        );
    }

    #[test]
    fn replacing_the_items_resets_the_scroll_and_the_selection() {
        let mut harness = harness(20);
        let row = harness.list.row_node(harness.app.app(), 2).expect("row 2");
        let point = center(&harness.app, row);
        click(&mut harness.app, point);
        assert_eq!(harness.list.selection(harness.app.app()).first(), Some(2));
        harness
            .list
            .scroll_to_item(
                harness.app.app_mut(),
                10,
                ui_core::interaction::ItemAlign::Start,
            )
            .expect("scroll");
        let _ = harness.app.drain().expect("frame");
        harness
            .list
            .set_items(harness.app.app_mut(), 5)
            .expect("replace");
        settle(&mut harness);
        assert_eq!(harness.list.count(harness.app.app()), 5);
        assert_eq!(harness.list.offset(harness.app.app()), Some(0.0));
        assert!(harness.list.selection(harness.app.app()).is_empty());
    }

    #[test]
    fn hover_is_mirrored_to_the_row_signal() {
        let mut harness = harness(10);
        let row = harness.list.row_node(harness.app.app(), 2).expect("row 2");
        let point = center(&harness.app, row);
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(point, Modifiers::empty()));
        let _ = harness.app.drain().expect("frame");
        let _ = harness.list.sync(harness.app.app_mut()).expect("sync");
        let hovered = harness
            .app
            .app()
            .read_with(&harness.list.hovered_signal(), |hovered| *hovered);
        assert_eq!(hovered, Some(2), "悬停镜像到行下标");
    }

    #[test]
    fn an_idle_list_requests_no_frames() {
        let mut harness = harness(10);
        let _ = harness.app.drain().expect("frame");
        let record = harness.app.drain().expect("frame").clone();
        assert!(!record.rendered, "静止列表不得再出帧（I3）");
        assert!(
            !harness.list.sync(harness.app.app_mut()).expect("sync"),
            "无变化的 sync 不写状态"
        );
    }

    #[test]
    fn a_deque_push_drops_the_oldest_and_keeps_the_reader() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let data: Rc<RefCell<VecDeque<usize>>> = Rc::new(RefCell::new((0..8).collect()));
        let data_rows = data.clone();
        let list = List::new(
            app.app_mut(),
            theme,
            "deque",
            ListOptions::fixed(ROW_EXTENT).deque(8).keep_on_newest(),
            move |tree, parent, index| {
                let value = data_rows.borrow().get(index).copied().unwrap_or(0);
                NodeBuilder::text(format_smolstr!("row{value}"), TextStyle::new(12.0))
                    .build(tree, parent)?;
                Ok(())
            },
        );
        list.set_initial_count(app.app_mut(), 8);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let parent = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(BOX_WIDTH)
                        .height(BOX_HEIGHT)
                        .build(),
                )
                .build(tree, root)
                .expect("parent")
        };
        list.mount(app.app_mut(), window, parent).expect("mount");
        let mut harness = Harness { app, list };
        settle(&mut harness);
        harness
            .list
            .scroll_to_item(
                harness.app.app_mut(),
                2,
                ui_core::interaction::ItemAlign::Start,
            )
            .expect("scroll");
        let _ = harness.app.drain().expect("frame");
        assert_eq!(harness.list.offset(harness.app.app()), Some(60.0));
        // 应用侧 deque 先更新（容量满 ⇒ 丢最旧），再通知内核。
        {
            let mut data = data.borrow_mut();
            if data.len() == 8 {
                data.pop_front();
            }
            data.push_back(8);
        }
        harness.list.push(harness.app.app_mut()).expect("push");
        assert_eq!(
            row_texts(harness.app.app(), harness.app.window_id(), &harness.list, 0),
            vec![SmolStr::new_static("row1")],
            "push 返回时行内容已按新下标同步重建"
        );
        settle(&mut harness);
        assert_eq!(harness.list.count(harness.app.app()), 8);
        assert_eq!(
            harness.list.offset(harness.app.app()),
            Some(30.0),
            "读者读的条目下标 −1，偏移跟着前缀和回退"
        );
        assert_eq!(
            row_texts(harness.app.app(), harness.app.window_id(), &harness.list, 0),
            vec![SmolStr::new_static("row1")],
            "最旧的一行被丢掉后，第 0 行显示原第 1 条"
        );
    }

    #[test]
    fn a_reversed_deque_push_prepends_and_keeps_the_reader() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let data: Rc<RefCell<VecDeque<usize>>> = Rc::new(RefCell::new((0..8).collect()));
        let data_rows = data.clone();
        let list = List::new(
            app.app_mut(),
            theme,
            "deque-reverse",
            ListOptions::fixed(ROW_EXTENT)
                .deque(8)
                .reverse()
                .keep_on_newest(),
            move |tree, parent, index| {
                let value = data_rows.borrow().get(index).copied().unwrap_or(0);
                NodeBuilder::text(format_smolstr!("row{value}"), TextStyle::new(12.0))
                    .build(tree, parent)?;
                Ok(())
            },
        );
        list.set_initial_count(app.app_mut(), 8);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let parent = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(BOX_WIDTH)
                        .height(BOX_HEIGHT)
                        .build(),
                )
                .build(tree, root)
                .expect("parent")
        };
        list.mount(app.app_mut(), window, parent).expect("mount");
        let mut harness = Harness { app, list };
        settle(&mut harness);
        harness
            .list
            .scroll_to_item(
                harness.app.app_mut(),
                2,
                ui_core::interaction::ItemAlign::Start,
            )
            .expect("scroll");
        let _ = harness.app.drain().expect("frame");
        assert_eq!(harness.list.offset(harness.app.app()), Some(60.0));
        {
            let mut data = data.borrow_mut();
            if data.len() == 8 {
                data.pop_back();
            }
            data.push_front(8);
        }
        harness.list.push(harness.app.app_mut()).expect("push");
        settle(&mut harness);
        assert_eq!(
            harness.list.offset(harness.app.app()),
            Some(90.0),
            "倒序头插 ⇒ 读者下标 +1，偏移前移一行"
        );
        assert_eq!(
            row_texts(harness.app.app(), harness.app.window_id(), &harness.list, 3),
            vec![SmolStr::new_static("row2")],
            "读者读的条目从旧下标 2 移到新下标 3（行内容随数据重排）"
        );
        assert_eq!(harness.list.count(harness.app.app()), 8);
        assert_eq!(
            harness.list.selection(harness.app.app()).len(),
            0,
            "push 不改选中"
        );
        let _ = SelectionMode::Single;
        let _ = RowHeight::Fixed(1.0);
    }
}

#[cfg(test)]
mod deque_tests {
    use std::{cell::RefCell, collections::VecDeque, rc::Rc};

    use smol_str::{SmolStr, format_smolstr};
    use ui_core::{
        error::EngineError,
        interaction::ItemAlign,
        node::{builder::NodeBuilder, tree::NodeTree},
        test_support::app::TestApp,
        text::TextStyle,
    };

    use super::{List, ListOptions};
    use crate::{
        style_builder::StyleBuilder,
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    const BOX_WIDTH: f32 = 200.0;
    const BOX_HEIGHT: f32 = 120.0;
    const ROW_EXTENT: f32 = 30.0;

    fn mount_list(app: &mut TestApp, list: &List, rows: usize, window: ui_core::window::WindowId) {
        list.set_initial_count(app.app_mut(), rows);
        let root = app.app().window(window).expect("window").tree().root();
        let parent = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(BOX_WIDTH)
                        .height(BOX_HEIGHT)
                        .build(),
                )
                .build(tree, root)
                .expect("parent")
        };
        list.mount(app.app_mut(), window, parent).expect("mount");
        settle(app, list);
    }

    fn settle(app: &mut TestApp, list: &List) {
        for _ in 0..6 {
            let _ = app.drain().expect("frame");
            let _ = list.sync(app.app_mut()).expect("sync");
        }
        let _ = app.drain().expect("frame");
    }

    fn fixed_deque(app: &mut TestApp, reverse: bool, data: Rc<RefCell<VecDeque<usize>>>) -> List {
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let mut options = ListOptions::fixed(ROW_EXTENT).deque(8).keep_on_newest();
        if reverse {
            options = options.reverse();
        }
        let rows = data;
        List::new(
            app.app_mut(),
            theme,
            "deque",
            options,
            move |tree, parent, index| {
                let value = rows.borrow().get(index).copied().unwrap_or(0);
                NodeBuilder::text(format_smolstr!("row{value}"), TextStyle::new(12.0))
                    .build(tree, parent)?;
                Ok(())
            },
        )
    }

    #[test]
    fn keeping_on_newest_follows_the_tail_and_leaves_a_scrolled_away_reader() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let data: Rc<RefCell<VecDeque<usize>>> = Rc::new(RefCell::new((0..8).collect()));
        let list = fixed_deque(&mut app, false, data.clone());
        mount_list(&mut app, &list, 8, window);

        // 读在尾部：push 后跟随新条目（内容总高不变 ⇒ 偏移停在最大偏移）。
        list.scroll_to_item(app.app_mut(), 7, ItemAlign::End)
            .expect("scroll end");
        let _ = app.drain().expect("frame");
        let tail = list.offset(app.app()).expect("offset");
        {
            let mut data = data.borrow_mut();
            data.pop_front();
            data.push_back(8);
        }
        list.push(app.app_mut()).expect("push");
        settle(&mut app, &list);
        assert_eq!(
            list.offset(app.app()),
            Some(tail),
            "读者在最新一条上 ⇒ 跟随尾部"
        );

        // 读在中间（滚走了）：push 后仍停在原来读的那一条上（下标 −1、偏移回退一行）。
        list.scroll_to_item(app.app_mut(), 2, ItemAlign::Start)
            .expect("scroll mid");
        let _ = app.drain().expect("frame");
        assert_eq!(list.offset(app.app()), Some(60.0));
        {
            let mut data = data.borrow_mut();
            data.pop_front();
            data.push_back(9);
        }
        list.push(app.app_mut()).expect("push");
        settle(&mut app, &list);
        assert_eq!(
            list.offset(app.app()),
            Some(30.0),
            "滚走的读者不被跟随拖走：原第 2 条移到下标 1"
        );
        assert_eq!(list.count(app.app()), 8);
    }

    #[test]
    fn a_reversed_deque_puts_a_reader_on_the_newest_item_back_on_top() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let data: Rc<RefCell<VecDeque<usize>>> = Rc::new(RefCell::new((0..8).collect()));
        let list = fixed_deque(&mut app, true, data.clone());
        mount_list(&mut app, &list, 8, window);
        assert_eq!(list.offset(app.app()), Some(0.0));
        {
            let mut data = data.borrow_mut();
            data.pop_back();
            data.push_front(8);
        }
        list.push(app.app_mut()).expect("push");
        settle(&mut app, &list);
        assert_eq!(
            list.offset(app.app()),
            Some(0.0),
            "倒序列表的读者在最新一条上 ⇒ 回到顶部（最新一条）"
        );
        let node = list.row_node(app.app(), 0).expect("row 0");
        let window_id = app.window_id();
        let tree = app.app().window(window_id).expect("window").tree();
        let mut texts = Vec::new();
        fn walk(tree: &NodeTree, node: ui_core::node::id::NodeId, out: &mut Vec<SmolStr>) {
            let Ok(target) = tree.get(node) else {
                return;
            };
            if let ui_core::node::tree::NodeKind::Text(content) = target.kind() {
                out.push(content.text.clone());
            }
            for child in target.children() {
                walk(tree, *child, out);
            }
        }
        walk(tree, node, &mut texts);
        assert_eq!(
            texts,
            vec![SmolStr::new_static("row8")],
            "第 0 行是最新一条"
        );
    }

    #[test]
    fn a_variable_height_deque_keeps_the_reader_on_the_measured_prefix() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let list = List::new(
            app.app_mut(),
            theme,
            "deque-variable",
            ListOptions::variable(ROW_EXTENT).deque(8).keep_on_newest(),
            |tree: &mut NodeTree, parent: ui_core::node::id::NodeId, index: usize| {
                let height = if index.is_multiple_of(2) { 40.0 } else { 20.0 };
                NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .width_full()
                            .height(height)
                            .flex_none()
                            .build(),
                    )
                    .build(tree, parent)?;
                Ok::<(), EngineError>(())
            },
        );
        mount_list(&mut app, &list, 8, window);
        let heights = list.row_heights();
        assert_eq!(heights.len(), 8);
        list.scroll_to_item(app.app_mut(), 2, ItemAlign::Start)
            .expect("scroll");
        let _ = app.drain().expect("frame");
        let expected = heights.top(2);
        assert!((list.offset(app.app()).expect("offset") - expected).abs() < 0.5);
        list.push(app.app_mut()).expect("push");
        settle(&mut app, &list);
        // 丢首项后原第 2 条移到下标 1，其顶边 = 旧高度表第 1 项。
        let expected = heights.get(1).expect("height 1");
        let offset = list.offset(app.app()).expect("offset");
        let _ = expected;
        let measured = list.row_heights();
        assert!(
            (offset - measured.top(1)).abs() < 0.5,
            "变高双端列表按测量前缀和保持锚点（原第 2 条移到下标 1 并贴顶）"
        );
        assert!(offset > 0.0);
    }
}
