//! 树（P2-2）：展开 / 折叠、缩进、层级线、选择、键盘导航。
//!
//! 纯逻辑在 [`TreeState`]（扁平化、展开集、按上下左右 / 回车导航，可单测）；节点层复用
//! [`List`] 内核（行虚拟化 / 滚动 / 选中 / hover），每行画「缩进 + 层级线 + 折叠标 + 文本」。
//!
//! # 选择
//!
//! 选中集只有一份：列表内核的 `Signal<Selection>`（行高亮由它驱动）。树不另存「选中 id」；
//! 需要 id 时用 [`Tree::selected`] 从行表反查。
//!
//! # 键盘
//!
//! [`Tree::bind_keys`] 注册 `Up/Down/Left/Right` 四个**焦点作用域化**绑定（引擎
//! `App::bind_key_scoped`：只有焦点落在本树 viewport 子树内时才消费；同屏多个树互不干扰，
//! 其它滚动容器的方向键默认滚动也不受影响）。`Enter/Space` 走引擎的「焦点节点激活」通道
//! （列表容器的 Action），由 [`List::on_activate`] 回调触发折叠。
//!
//! # 禁用行
//!
//! `TreeItem::disabled` 的行不可点击 / 不可键盘导航 / 不可程序化选中（列表内核的 disabled
//! 集 + [`TreeState::apply_key`] 跳过）；渲染仍是弱化色（旧 gpui `Button::disabled` 语义）。
//!
//! # a11y（E9 集合语义）
//!
//! 采纳引擎 E9 的树集合语义：容器静态声明 [`A11yRole::Tree`]（`A11yOverride`）并动态宣告集合
//! 大小（`size_of_set` = 可见行数）；每个可见行在列表行基线之上叠加 [`A11yRole::TreeItem`]、
//! 行标签、展开态、禁用态（`set_row_a11y` 的 `A11yPatch`，选中态来自列表基线）。引擎把树项
//! 挂到**最近的** `Tree` / `TreeItem` 祖先下 —— 缩进槽 / 折叠标 / 标签这些渲染包裹层不参与
//! a11y 父子（角色始终落在逻辑树项那一行）。
//!
//! **层级**：本组件是扁平化虚拟树（可见行是虚拟列表的兄弟行，物理节点树里没有嵌套）。因此
//! a11y 父子是「所有可见树项 = `Tree` 的子节点」，嵌套深度用 `aria-level` 语义
//! （`A11yPatch::level`，0-based = [`FlatRow::depth`]）表达 —— ARIA 的 flattened-tree 模式。
//! 不要为了 a11y 层级在物理树里造嵌套：行窗口是虚拟列表，嵌套会和窗口化冲突。
//!
//! 折叠标（`▸` / `▾`）与行标签文本节点都显式 `hidden`：树项的**可访问名在逻辑行上**
//! （`A11yPatch::label`），装饰字形与重复文本不应再作为独立 Label 被读屏朗读。
//!
//! **残余**（`.docs/gui-migration/spec-deltas/kit-a11y.md`）：
//! - 行窗口（列表 `viewport`，角色 `List`）仍是 `Tree` 的 a11y 子节点（E9 R2：集合内虚拟窗口
//!   未透明化，滚动状态仍挂在窗口上）；`Tree` 自身没有滚动动作/滚动值（AT 通过窗口滚动）。
//! - AT 的展开 / 折叠动作未接入：`TreeItem` 默认不宣告 `Expand` / `Collapse`（E9 R9 的操作
//!   路径要求 `on_a11y_action` handler），展开态目前只由状态表达。
//!
//! # 折叠标
//!
//! 折叠标是 `bind_text` 节点，读展开状态 `Signal<TreeState>`：展开 / 折叠后只有该节点的文本
//! 变化；行内容本身在 `set_count` 时按新行表重建（展开会改变行数）。

use std::{cell::RefCell, collections::BTreeSet, rc::Rc};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, KeyBinding, KeyCode, Modifiers},
    interaction::{ActionCtx, ItemAlign},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::style::StylePatch,
    text::TextStyle,
    window::WindowId,
};

use crate::{
    kit::list::{List, ListOptions, RowHeight, SelectionMode},
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};

/// 一条树数据（根或子节点）。
#[derive(Clone, Debug, PartialEq)]
pub struct TreeItem {
    /// 稳定 id（展开集 / 选择身份）。
    pub id: SmolStr,
    /// 显示文本。
    pub label: SmolStr,
    /// 是否禁用（不可交互，画成弱化色）。
    pub disabled: bool,
    /// 子节点（非空即文件夹）。
    pub children: Vec<TreeItem>,
}

impl TreeItem {
    /// 一个叶子 / 文件夹（子节点后续用 [`TreeItem::child`] 加）。
    #[must_use]
    pub fn new(id: impl Into<SmolStr>, label: impl Into<SmolStr>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            disabled: false,
            children: Vec::new(),
        }
    }

    /// 加一个子节点。
    #[must_use]
    pub fn child(mut self, child: Self) -> Self {
        self.children.push(child);
        self
    }

    /// 禁用。
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.disabled = true;
        self
    }

    /// 是否文件夹。
    #[must_use]
    pub fn is_folder(&self) -> bool {
        !self.children.is_empty()
    }
}

/// 扁平化后的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlatRow {
    /// 节点 id。
    pub id: SmolStr,
    /// 显示文本。
    pub label: SmolStr,
    /// 层级（根的 `depth = 0`）。
    pub depth: usize,
    /// 是否文件夹。
    pub folder: bool,
    /// 是否展开（`folder == false` 时恒 `false`）。
    pub expanded: bool,
    /// 是否禁用。
    pub disabled: bool,
    /// 父节点 id（根为 `None`）。
    pub parent: Option<SmolStr>,
}

impl FlatRow {
    /// 缩进层级：叶子比同级文件夹多缩进一格（与旧 `tree.rs` 的显示规则一致）。
    #[must_use]
    pub fn indent_depth(&self) -> usize {
        if self.folder {
            self.depth
        } else {
            self.depth + 1
        }
    }
}

/// 树的模型：根数据 + 展开集合（选中在列表内核里）。
#[derive(Clone, Debug, PartialEq)]
pub struct TreeState {
    roots: Vec<TreeItem>,
    expanded: BTreeSet<SmolStr>,
}

impl TreeState {
    /// 以根数据建树；默认全部折叠。
    #[must_use]
    pub fn new(roots: impl IntoIterator<Item = TreeItem>) -> Self {
        Self {
            roots: roots.into_iter().collect(),
            expanded: BTreeSet::new(),
        }
    }

    /// 根数据。
    #[must_use]
    pub fn roots(&self) -> &[TreeItem] {
        &self.roots
    }

    /// 展开的 id 集合。
    #[must_use]
    pub fn expanded(&self) -> &BTreeSet<SmolStr> {
        &self.expanded
    }

    /// 某节点是否展开。
    #[must_use]
    pub fn is_expanded(&self, id: &str) -> bool {
        self.expanded.contains(id)
    }

    /// 展开 / 折叠（非文件夹 / 禁用返回 `false`）。
    pub fn toggle(&mut self, id: &str) -> bool {
        if self
            .find(id)
            .is_none_or(|item| !item.is_folder() || item.disabled)
        {
            return false;
        }
        if !self.expanded.remove(id) {
            self.expanded.insert(SmolStr::new(id));
        }
        true
    }

    /// 该 id 是否禁用（不存在的 id / 非禁用返回 `false`）。
    #[must_use]
    pub fn is_disabled(&self, id: &str) -> bool {
        self.find(id).is_some_and(|item| item.disabled)
    }

    /// 展开（非文件夹 / 已展开 / 禁用返回 `false`）。
    pub fn expand(&mut self, id: &str) -> bool {
        if self
            .find(id)
            .is_none_or(|item| !item.is_folder() || item.disabled)
        {
            return false;
        }
        self.expanded.insert(SmolStr::new(id))
    }

    /// 折叠（非文件夹 / 未展开返回 `false`）。
    pub fn collapse(&mut self, id: &str) -> bool {
        self.expanded.remove(id)
    }

    /// 全部展开 / 收起（测试与「展开全部」按钮用）。
    pub fn set_all_expanded(&mut self, expanded: bool) {
        self.expanded.clear();
        if expanded {
            let mut ids = Vec::new();
            collect_folder_ids(&self.roots, &mut ids);
            self.expanded.extend(ids);
        }
    }

    fn find(&self, id: &str) -> Option<&TreeItem> {
        fn walk<'a>(items: &'a [TreeItem], id: &str) -> Option<&'a TreeItem> {
            for item in items {
                if item.id == id {
                    return Some(item);
                }
                if let Some(found) = walk(&item.children, id) {
                    return Some(found);
                }
            }
            None
        }
        walk(&self.roots, id)
    }

    /// 扁平化（只含可见行：折叠的子树不展开）。
    #[must_use]
    pub fn rows(&self) -> Vec<FlatRow> {
        let mut rows = Vec::new();
        fn walk(
            items: &[TreeItem],
            depth: usize,
            parent: Option<&SmolStr>,
            expanded: &BTreeSet<SmolStr>,
            rows: &mut Vec<FlatRow>,
        ) {
            for item in items {
                let expanded_item = item.is_folder() && expanded.contains(&item.id);
                rows.push(FlatRow {
                    id: item.id.clone(),
                    label: item.label.clone(),
                    depth,
                    folder: item.is_folder(),
                    expanded: expanded_item,
                    disabled: item.disabled,
                    parent: parent.cloned(),
                });
                if expanded_item {
                    walk(&item.children, depth + 1, Some(&item.id), expanded, rows);
                }
            }
        }
        walk(&self.roots, 0, None, &self.expanded, &mut rows);
        rows
    }

    /// 键盘导航：返回要执行的结果；`selected` 是当前选中行。
    ///
    /// 不可交互行（[`TreeItem::disabled`]）不展开 / 不折叠 / 不激活；上下移动会跳过它们。
    ///
    /// - `Up/Down`：在可见行里移动（跳过禁用行）；
    /// - `Right`：折叠的文件夹展开；已展开的文件夹选到第一个子节点；叶子无动作；
    /// - `Left`：展开的文件夹折叠；否则选到父节点；
    /// - `Enter`：文件夹折叠 / 展开；叶子无动作。
    pub fn apply_key(&mut self, selected: Option<usize>, key: TreeKey) -> KeyOutcome {
        let rows = self.rows();
        if rows.is_empty() {
            return KeyOutcome::None;
        }
        let last = rows.len() - 1;
        let current = selected.unwrap_or(0).min(last);
        let row = &rows[current];
        if row.disabled && matches!(key, TreeKey::Left | TreeKey::Right | TreeKey::Enter) {
            return KeyOutcome::None;
        }
        match key {
            TreeKey::Up => KeyOutcome::Select(step_over_disabled(&rows, current, false)),
            TreeKey::Down => KeyOutcome::Select(step_over_disabled(&rows, current, true)),
            TreeKey::Right => {
                if row.folder {
                    if row.expanded {
                        KeyOutcome::Select(step_over_disabled(&rows, current, true))
                    } else if self.expand(&row.id) {
                        KeyOutcome::Toggle { row: current }
                    } else {
                        KeyOutcome::None
                    }
                } else {
                    // 叶子上右键是 no-op（常见树语义；旧实现的「跳到下一行」会意外移动选中）。
                    KeyOutcome::None
                }
            }
            TreeKey::Left => {
                if row.folder && row.expanded {
                    if self.collapse(&row.id) {
                        KeyOutcome::Toggle { row: current }
                    } else {
                        KeyOutcome::None
                    }
                } else if let Some(parent) = row.parent.as_deref() {
                    let parent_row = rows.iter().position(|candidate| candidate.id == parent);
                    parent_row.map_or(KeyOutcome::None, KeyOutcome::Select)
                } else {
                    KeyOutcome::Select(current)
                }
            }
            TreeKey::Enter => {
                if row.folder && self.toggle(&row.id) {
                    KeyOutcome::Toggle { row: current }
                } else {
                    KeyOutcome::None
                }
            }
        }
    }
}

/// 朝 `forward` 方向找下一行可交互行（跳过禁用行；没有则停在原地）。
fn step_over_disabled(rows: &[FlatRow], current: usize, forward: bool) -> usize {
    if forward {
        (current + 1..rows.len())
            .find(|index| !rows[*index].disabled)
            .unwrap_or(current)
    } else {
        (0..current)
            .rev()
            .find(|index| !rows[*index].disabled)
            .unwrap_or(current)
    }
}

fn collect_folder_ids(items: &[TreeItem], ids: &mut Vec<SmolStr>) {
    for item in items {
        if item.is_folder() {
            ids.push(item.id.clone());
            collect_folder_ids(&item.children, ids);
        }
    }
}

/// 树键盘动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeKey {
    /// 上移一行。
    Up,
    /// 下移一行。
    Down,
    /// 折叠 / 选父节点。
    Left,
    /// 展开 / 选首个子节点。
    Right,
    /// 折叠切换（叶子无动作）。
    Enter,
}

/// 键盘导航结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyOutcome {
    /// 选中新行（行表未变）。
    Select(usize),
    /// 展开 / 折叠生效（行表变了，调用方要重建行数）。
    Toggle {
        /// 发生切换的行（行表重建后仍是同一行）。
        row: usize,
    },
    /// 无动作。
    None,
}

/// 树参数。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TreeOptions {
    /// 行高（逻辑像素；树是等高行列表）。
    pub row_height: f32,
    /// 每层缩进宽度。
    pub indent: f32,
    /// 选中策略（树默认单选）。
    pub selection: SelectionMode,
    /// 是否画滚动条。
    pub scrollbar: bool,
}

impl Default for TreeOptions {
    fn default() -> Self {
        Self {
            row_height: 30.0,
            indent: 12.0,
            selection: SelectionMode::Single,
            scrollbar: true,
        }
    }
}

impl TreeOptions {
    /// 行高。
    #[must_use]
    pub fn row_height(mut self, row_height: f32) -> Self {
        self.row_height = row_height.max(1.0);
        self
    }

    /// 缩进宽度。
    #[must_use]
    pub fn indent(mut self, indent: f32) -> Self {
        self.indent = indent.max(0.0);
        self
    }
}

struct TreeInner {
    key: SmolStr,
    scope: SignalScope,
    state: Signal<TreeState>,
    flat: Rc<RefCell<Vec<FlatRow>>>,
    list: List,
    geometry: RefCell<TreeGeometry>,
    window: RefCell<Option<WindowId>>,
    actions: TreeActions,
}

struct TreeActions {
    up: Action,
    down: Action,
    left: Action,
    right: Action,
}

#[derive(Default)]
struct TreeGeometry {
    outer: Option<NodeId>,
}

/// 树句柄（`Rc` 浅拷贝）。
#[derive(Clone)]
pub struct Tree {
    inner: Rc<TreeInner>,
}

impl Tree {
    /// 建树（根数据 + 参数）。之后 [`Tree::build`] + [`Tree::attach`]（或 [`Tree::mount`]）。
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        key: impl Into<SmolStr>,
        roots: impl IntoIterator<Item = TreeItem>,
        options: TreeOptions,
    ) -> Self {
        let key = key.into();
        let scope = app.signal_scope();
        let state = TreeState::new(roots);
        let flat = Rc::new(RefCell::new(state.rows()));
        let signal = app.signal_scoped(scope, state);
        let key_list = key.clone();
        let theme_list = theme.clone();
        let flat_rows = flat.clone();
        let row_state = signal;
        let list = List::new(
            app,
            theme,
            format_smolstr!("{key_list}:rows"),
            ListOptions {
                row_height: RowHeight::Fixed(options.row_height),
                overscan: 2,
                selection: options.selection,
                capacity: None,
                reverse: false,
                keep_on_newest: false,
                padding_x: 6.0,
                padding_y: 2.0,
                row_stretch: false,
                scrollbar: options.scrollbar,
            },
            move |tree, parent, index| {
                let Some(row) = flat_rows.borrow().get(index).cloned() else {
                    return Ok(());
                };
                build_row(tree, parent, &row, &theme_list, row_state, options)?;
                Ok(())
            },
        );
        let count = flat.borrow().len();
        list.set_initial_count(app, count);
        let disabled = disabled_rows(&flat.borrow());
        list.set_disabled_rows(disabled);
        // 行级 a11y（E9）：逻辑树项 = 可见行。角色 / 标签 / 展开态 / 禁用态 / 层级在这里声明；
        // 选中与扁平集合位置来自列表行基线。层级用 `level`（0-based depth）表达 —— 行是虚拟列表
        // 的兄弟行，物理树里没有嵌套，引擎的父子推导因此给出「全部挂在 Tree 下」。
        let flat_a11y = flat.clone();
        list.set_row_a11y(move |index| {
            let flat = flat_a11y.borrow();
            let Some(row) = flat.get(index) else {
                return A11yPatch::default();
            };
            A11yPatch {
                role: Some(A11yRole::TreeItem),
                label: Some(row.label.clone()),
                level: Some(row.depth),
                expanded: row.folder.then_some(row.expanded),
                disabled: Some(row.disabled),
                ..A11yPatch::default()
            }
        });
        let inner = TreeInner {
            list,
            scope,
            state: signal,
            flat,
            geometry: RefCell::new(TreeGeometry::default()),
            window: RefCell::new(None),
            actions: TreeActions {
                up: Action::Custom(format_smolstr!("{key}.up")),
                down: Action::Custom(format_smolstr!("{key}.down")),
                left: Action::Custom(format_smolstr!("{key}.left")),
                right: Action::Custom(format_smolstr!("{key}.right")),
            },
            key,
        };
        let this = Self {
            inner: Rc::new(inner),
        };
        // 列表的行激活（双击 / Enter）→ 文件夹折叠切换。
        let tree = this.clone();
        this.inner.list.on_activate(move |app, row| {
            let _ = tree.toggle_row(app, row);
        });
        this
    }

    /// 树键（调试 id 前缀）。
    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    /// 行体的列表内核（组合 / 测试用）。
    #[must_use]
    pub fn list(&self) -> &List {
        &self.inner.list
    }

    /// 只建节点（容器 + 列表）。
    ///
    /// # Errors
    /// `StaleNode`。
    pub fn build(&self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError> {
        let count = self.inner.list.count_signal();
        let outer = NodeBuilder::boxed()
            .id(format_smolstr!("{}:tree", self.inner.key))
            .style(
                StyleBuilder::new()
                    .relative()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .overflow_hidden()
                    .build(),
            )
            // a11y（E9）：树的集合根。角色是静态语义；集合大小随行数（含展开/折叠后的可见行）
            // 动态更新，屏幕阅读器才能播报「第 N 项，共 M 项」。
            .a11y(A11yOverride {
                role: Some(A11yRole::Tree),
                ..A11yOverride::default()
            })
            .bind_a11y(move |cx| A11yPatch {
                size_of_set: Some(*cx.read(&count)),
                ..A11yPatch::default()
            })
            .build(tree, parent)?;
        self.inner.list.build(tree, outer)?;
        self.inner.geometry.borrow_mut().outer = Some(outer);
        Ok(outer)
    }

    /// 接引擎：列表 + 键盘绑定。
    ///
    /// # Errors
    /// 同 [`List::attach`]（含 `make_virtual_list` / 行工厂建树 / 首帧 `compute_layout` 的
    /// 错误，都会原样传播）；方向键作用域绑定也要求 viewport 已挂载（`bind_key_scoped` 的
    /// `StaleNode` 在这里只可能是引擎不变量被破坏）。
    pub fn attach(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        self.inner.list.attach(app, window)?;
        *self.inner.window.borrow_mut() = Some(window);
        self.bind_keys(app)?;
        Ok(())
    }

    /// 一步完成 `build` + `attach`。
    ///
    /// # Errors
    /// 同 [`Tree::build`] / [`Tree::attach`]。
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

    /// 注册方向键绑定（**实例级**：只有本树的列表容器内有焦点时才消费）。
    ///
    /// 引擎的 [`App::bind_key_scoped`] 在焦点不在本树 viewport 子树内时**不消费**按键，因此
    /// 同屏多个树互不干扰，其它滚动容器的方向键默认滚动也不受影响（P2-2 审查 P2）。
    /// 必须在 [`Tree::attach`] 之后调用（需要窗口与 viewport 节点）；`attach` 会自动调用。
    ///
    /// 绑定的生命周期归 [`App`] 管：`release` 用 [`App::unbind_scope_keys`] 显式摘除，
    /// 即使漏掉也会在节点卸载 / 关窗时自动清理（见 `App::bind_key_scoped`）。
    ///
    /// # Errors
    /// [`EngineError::UnknownWindow`] / [`EngineError::StaleNode`]：窗口或 viewport 已失效。
    pub fn bind_keys(&self, app: &mut App) -> Result<(), EngineError> {
        let (Some(window), Some(viewport)) =
            (*self.inner.window.borrow(), self.inner.list.viewport_node())
        else {
            return Ok(());
        };
        let keys = [
            (KeyCode::ArrowUp, self.inner.actions.up.clone(), TreeKey::Up),
            (
                KeyCode::ArrowDown,
                self.inner.actions.down.clone(),
                TreeKey::Down,
            ),
            (
                KeyCode::ArrowLeft,
                self.inner.actions.left.clone(),
                TreeKey::Left,
            ),
            (
                KeyCode::ArrowRight,
                self.inner.actions.right.clone(),
                TreeKey::Right,
            ),
        ];
        for (code, action, key) in keys {
            app.bind_key_scoped(
                window,
                viewport,
                KeyBinding {
                    key: code,
                    modifiers: Modifiers::empty(),
                    action: action.clone(),
                },
            )?;
            let tree = self.clone();
            app.on_action(action, move |ctx| {
                tree.on_key(ctx, key);
            });
        }
        Ok(())
    }

    /// 释放本组件的信号作用域与引擎侧注册（动态创建 / 销毁的树必须调用；先卸载节点再释放）。
    ///
    /// 清理面：4 条方向键的 scoped 绑定（[`App::unbind_scope_keys`]）+ 4 个方向键 Action
    /// handler（[`App::off_action`]）+ 行体列表（含滚动条）的 handler / 信号作用域。只释放信号
    /// 不摘 handler 会让 `App` 永久持有 `Rc<Tree>` 闭包与绑定表项（P2-2 复核 R2.2 的泄漏）。
    ///
    /// 返回被释放的槽位数（展开状态 + 行体列表 / 滚动条的状态）；重复调用返回 0（幂等）。
    pub fn release(&self, app: &mut App) -> usize {
        if let (Some(window), Some(viewport)) =
            (*self.inner.window.borrow(), self.inner.list.viewport_node())
        {
            app.unbind_scope_keys(window, viewport);
        }
        for action in [
            &self.inner.actions.up,
            &self.inner.actions.down,
            &self.inner.actions.left,
            &self.inner.actions.right,
        ] {
            app.off_action(action);
        }
        self.inner.list.release(app) + app.release_scope(self.inner.scope)
    }

    /// 展开 / 折叠一行；返回行表是否变了。
    ///
    /// # Errors
    /// `List::set_count` 的引擎错误。
    pub fn toggle_row(&self, app: &mut App, row: usize) -> Result<bool, EngineError> {
        let Some(flat) = self.inner.flat.borrow().get(row).cloned() else {
            return Ok(false);
        };
        if flat.disabled || !flat.folder {
            return Ok(false);
        }
        let mut state = app.read_with(&self.inner.state, TreeState::clone);
        if !state.toggle(&flat.id) {
            return Ok(false);
        }
        self.rebuild_rows(app, state)
    }

    /// 展开 / 折叠指定 id；返回行表是否变了。
    ///
    /// # Errors
    /// `List::set_count` 的引擎错误。
    pub fn toggle(&self, app: &mut App, id: &str) -> Result<bool, EngineError> {
        let mut state = app.read_with(&self.inner.state, TreeState::clone);
        if state.is_disabled(id) || !state.toggle(id) {
            return Ok(false);
        }
        self.rebuild_rows(app, state)
    }

    /// 换根数据（行表 / 展开集全部替换，**滚回顶部**，选中清空）。
    ///
    /// # Errors
    /// `List::set_items` 的引擎错误。
    pub fn set_roots(
        &self,
        app: &mut App,
        roots: impl IntoIterator<Item = TreeItem>,
    ) -> Result<(), EngineError> {
        let state = TreeState::new(roots);
        let rows = state.rows();
        let count = rows.len();
        let disabled = disabled_rows(&rows);
        *self.inner.flat.borrow_mut() = rows;
        self.inner.list.set_disabled_rows(disabled);
        app.write(&self.inner.state, state);
        // 换根 = 换数据：重置滚动位置与选中（`set_count` 会保留旧偏移，语义不符）。
        self.inner.list.set_items(app, count)?;
        Ok(())
    }

    /// 把模型写回信号并重建行表（保留选中行）。
    fn rebuild_rows(&self, app: &mut App, state: TreeState) -> Result<bool, EngineError> {
        let rows = state.rows();
        let count = rows.len();
        let disabled = disabled_rows(&rows);
        *self.inner.flat.borrow_mut() = rows;
        self.inner.list.set_disabled_rows(disabled);
        app.write(&self.inner.state, state);
        self.inner.list.set_count(app, count)?;
        Ok(true)
    }

    /// 读模型。
    #[must_use]
    pub fn state(&self, app: &App) -> TreeState {
        app.read_with(&self.inner.state, TreeState::clone)
    }

    /// 可见行表。
    #[must_use]
    pub fn rows(&self) -> Vec<FlatRow> {
        self.inner.flat.borrow().clone()
    }

    /// 某 id 是否展开。
    #[must_use]
    pub fn is_expanded(&self, app: &App, id: &str) -> bool {
        app.read_with(&self.inner.state, |state| state.is_expanded(id))
    }

    /// 选中行。
    #[must_use]
    pub fn selected_row(&self, app: &App) -> Option<usize> {
        self.inner.list.selection(app).first()
    }

    /// 选中行的 id。
    #[must_use]
    pub fn selected(&self, app: &App) -> Option<SmolStr> {
        let row = self.selected_row(app)?;
        self.inner.flat.borrow().get(row).map(|row| row.id.clone())
    }

    /// 选中一行并滚到可见。
    ///
    /// # Errors
    /// `scroll_to_item` 的引擎错误。
    pub fn select_row(&self, app: &mut App, row: usize) -> Result<(), EngineError> {
        self.inner.list.select(app, Some(row));
        self.inner.list.scroll_to_item(app, row, ItemAlign::Nearest)
    }

    /// 键盘一步（`Up/Down/Left/Right`；`Enter` 由列表激活回调处理）。
    ///
    /// # Errors
    /// 行表重建时的引擎错误。
    pub fn handle_key(&self, app: &mut App, key: TreeKey) -> Result<(), EngineError> {
        let selected = self.selected_row(app);
        let mut state = app.read_with(&self.inner.state, TreeState::clone);
        let outcome = state.apply_key(selected, key);
        match outcome {
            KeyOutcome::None => Ok(()),
            KeyOutcome::Select(row) => self.select_row(app, row),
            KeyOutcome::Toggle { row } => {
                self.rebuild_rows(app, state)?;
                self.inner.list.select(app, Some(row));
                self.inner.list.scroll_to_item(app, row, ItemAlign::Nearest)
            }
        }
    }

    /// 帧同步（行内容 / 悬停 / 滚动条）。
    ///
    /// # Errors
    /// 同 [`List::sync`]。
    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        self.inner.list.sync(app)
    }

    /// 外层容器节点（`build` 之后）。
    #[must_use]
    pub fn outer_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().outer
    }

    fn on_key(&self, ctx: &mut ActionCtx<'_>, key: TreeKey) {
        // 绑定是实例作用域的；这里再校验一次「焦点在本树 viewport 子树内」，因为多窗口 / 陈旧
        // 焦点的时序可能让绑定求值与事件分发之间焦点变化。`ctx.window()` 是**事件**所属窗口
        // （旧实现读树的建窗窗口，多窗口下会用另一窗口的陈旧焦点误操作本树）。
        let window = ctx.window();
        let Some(viewport) = self.inner.list.viewport_node() else {
            return;
        };
        let Some(focused) = ctx.app_mut().focused(window) else {
            return;
        };
        let in_scope = ctx
            .app_mut()
            .window(window)
            .is_some_and(|target| node_in_subtree(target.tree(), focused, viewport));
        if !in_scope {
            return;
        }
        let _ = self.handle_key(ctx.app_mut(), key);
    }
}

/// `node` 是否是 `ancestor` 或它的后代（焦点作用域判定）。
fn node_in_subtree(tree: &NodeTree, node: NodeId, ancestor: NodeId) -> bool {
    let mut cursor = Some(node);
    while let Some(current) = cursor {
        if current == ancestor {
            return true;
        }
        cursor = tree.get(current).ok().and_then(|target| target.parent());
    }
    false
}

/// 行表里不可交互行的下标。
fn disabled_rows(rows: &[FlatRow]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| row.disabled)
        .map(|(index, _)| index)
        .collect()
}

/// 一行内容：缩进 + 层级线 + 折叠标 + 文本。
fn build_row(
    tree: &mut NodeTree,
    parent: NodeId,
    row: &FlatRow,
    theme: &ThemeHandle,
    state: Signal<TreeState>,
    options: TreeOptions,
) -> Result<(), EngineError> {
    let line_height = (options.row_height - 4.0).max(1.0);
    for _ in 0..row.indent_depth() {
        let slot = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .width(options.indent)
                    .flex_none()
                    .build(),
            )
            .build(tree, parent)?;
        let line_theme = theme.clone();
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .width(1.0)
                    .height(line_height)
                    .flex_none()
                    .build(),
            )
            .bind_style(move |cx| StylePatch {
                background: Some(line_theme.role(cx, Role::LineDivider)),
                ..StylePatch::default()
            })
            .build(tree, slot)?;
    }
    if row.folder {
        let caret_theme = theme.clone();
        let id = row.id.clone();
        NodeBuilder::text(
            SmolStr::new_static("▸"),
            TextStyle::new(theme.tokens().small.font_size),
        )
        // 折叠标是纯装饰：树项的可访问名在逻辑行上（`A11yPatch::label`），读屏不应读到
        //「▸ / ▾」这个字形。
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .bind_text(move |cx| {
            let state = cx.read(&state);
            let expanded = state.is_expanded(&id);
            if expanded {
                SmolStr::new_static("▾")
            } else {
                SmolStr::new_static("▸")
            }
        })
        .bind_style(move |cx| StylePatch {
            foreground: Some(caret_theme.role(cx, Role::FgMuted)),
            ..StylePatch::default()
        })
        .build(tree, parent)?;
    } else {
        NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .width(theme.tokens().small.icon_size)
                    .flex_none()
                    .build(),
            )
            .build(tree, parent)?;
    }
    let label_theme = theme.clone();
    let disabled = row.disabled;
    NodeBuilder::text(
        row.label.clone(),
        TextStyle::new(theme.tokens().medium.font_size),
    )
    // 行的可访问名已经由逻辑树项的 `A11yPatch::label` 承载；这份文本只是绘制用的内容，
    // 再从 a11y 暴露一次会让读屏把同一个名字读两遍（树的条目是叶子语义）。
    .a11y(A11yOverride {
        hidden: true,
        ..A11yOverride::default()
    })
    .bind_style(move |cx| StylePatch {
        foreground: Some(label_theme.role(
            cx,
            if disabled {
                Role::FgDisabled
            } else {
                Role::FgDefault
            },
        )),
        interactive: Some(!disabled),
        ..StylePatch::default()
    })
    .build(tree, parent)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{FlatRow, KeyOutcome, TreeItem, TreeKey, TreeOptions, TreeState};

    fn folders() -> Vec<TreeItem> {
        vec![
            TreeItem::new("first", "first").child(TreeItem::new("first/a", "a")),
            TreeItem::new("leaf", "leaf"),
            TreeItem::new("last", "last").child(TreeItem::new("last/a", "a")),
        ]
    }

    fn labels(state: &TreeState) -> Vec<String> {
        state
            .rows()
            .into_iter()
            .map(|row| row.label.to_string())
            .collect()
    }

    #[test]
    fn a_folded_tree_shows_only_its_roots() {
        let state = TreeState::new(folders());
        assert_eq!(labels(&state), ["first", "leaf", "last"]);
        let rows = state.rows();
        assert_eq!(rows[0].depth, 0);
        assert!(rows[0].folder);
        assert!(!rows[0].expanded);
        assert_eq!(rows[0].indent_depth(), 0);
        assert_eq!(rows[1].indent_depth(), 1, "叶子比同级文件夹多缩进一格");
    }

    #[test]
    fn expanding_and_collapsing_changes_the_visible_rows() {
        let mut state = TreeState::new(folders());
        assert!(state.expand("first"));
        assert_eq!(labels(&state), ["first", "a", "leaf", "last"]);
        assert_eq!(state.rows()[1].depth, 1);
        assert_eq!(state.rows()[1].parent.as_deref(), Some("first"));
        assert!(state.collapse("first"));
        assert_eq!(labels(&state), ["first", "leaf", "last"]);
        assert!(!state.collapse("first"), "已折叠再折叠是 no-op");
        assert!(!state.expand("leaf"), "叶子不能展开");
        assert!(!state.toggle("leaf"), "叶子不能切换");
        assert!(state.toggle("last"));
        assert_eq!(labels(&state), ["first", "leaf", "last", "a"]);
    }

    #[test]
    fn keyboard_navigation_moves_expands_and_collapses() {
        let mut state = TreeState::new(folders());
        assert_eq!(
            state.apply_key(None, TreeKey::Down),
            KeyOutcome::Select(1),
            "无选中时从第一行开始"
        );
        assert_eq!(
            state.apply_key(Some(0), TreeKey::Down),
            KeyOutcome::Select(1)
        );
        assert_eq!(
            state.apply_key(Some(2), TreeKey::Down),
            KeyOutcome::Select(2),
            "末行不越界"
        );
        assert_eq!(state.apply_key(Some(1), TreeKey::Up), KeyOutcome::Select(0));

        assert_eq!(
            state.apply_key(Some(0), TreeKey::Right),
            KeyOutcome::Toggle { row: 0 },
            "折叠的文件夹右键展开"
        );
        let rows = state.rows();
        assert_eq!(rows.len(), 4);
        assert_eq!(
            state.apply_key(Some(0), TreeKey::Right),
            KeyOutcome::Select(1),
            "已展开的文件夹右键选到第一个子节点"
        );
        assert_eq!(
            state.apply_key(Some(1), TreeKey::Left),
            KeyOutcome::Select(0),
            "子节点左键选父节点"
        );
        assert_eq!(
            state.apply_key(Some(0), TreeKey::Left),
            KeyOutcome::Toggle { row: 0 },
            "展开的文件夹左键折叠"
        );
        assert_eq!(
            state.apply_key(Some(0), TreeKey::Enter),
            KeyOutcome::Toggle { row: 0 }
        );
        assert_eq!(
            state.apply_key(Some(1), TreeKey::Enter),
            KeyOutcome::None,
            "叶子回车无动作"
        );
    }

    #[test]
    fn right_on_a_leaf_is_a_no_op() {
        let mut state = TreeState::new(folders());
        assert_eq!(
            state.apply_key(Some(1), TreeKey::Right),
            KeyOutcome::None,
            "叶子上的右键不移动选中（常见树语义）"
        );
        assert_eq!(state.rows().len(), 3, "行表不变");
    }

    #[test]
    fn disabled_rows_are_skipped_and_do_not_toggle() {
        let mut state = TreeState::new(vec![
            TreeItem::new("first", "first").child(TreeItem::new("first/a", "a")),
            TreeItem::new("disabled", "disabled").disabled(),
            TreeItem::new("disabled-folder", "disabled-folder")
                .child(TreeItem::new("disabled-folder/a", "a"))
                .disabled(),
            TreeItem::new("leaf", "leaf"),
        ]);
        // 0 → 1 是禁用行：下移动跳过，落到 3。
        assert_eq!(
            state.apply_key(Some(0), TreeKey::Down),
            KeyOutcome::Select(3),
            "下移跳过禁用行"
        );
        assert_eq!(
            state.apply_key(Some(3), TreeKey::Up),
            KeyOutcome::Select(0),
            "上移跳过禁用行"
        );
        assert_eq!(
            state.apply_key(Some(2), TreeKey::Enter),
            KeyOutcome::None,
            "禁用文件夹不能展开"
        );
        assert_eq!(
            state.apply_key(Some(2), TreeKey::Right),
            KeyOutcome::None,
            "禁用行不响应右键"
        );
        assert!(!state.toggle("disabled-folder"), "禁用文件夹不能切换");
        assert!(!state.is_expanded("disabled-folder"));
        assert!(state.is_disabled("disabled"));
        assert!(!state.is_disabled("leaf"));
    }

    #[test]
    fn indent_depth_and_folder_flags_survive_flattening() {
        let mut state =
            TreeState::new(vec![TreeItem::new("root", "root").child(
                TreeItem::new("mid", "mid").child(TreeItem::new("leaf", "leaf")),
            )]);
        state.set_all_expanded(true);
        let rows: Vec<FlatRow> = state.rows();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].depth, 2);
        assert_eq!(rows[2].parent.as_deref(), Some("mid"));
        assert!(!rows[2].folder);
        assert_eq!(rows[2].indent_depth(), 3);
        state.set_all_expanded(false);
        assert_eq!(state.rows().len(), 1);
    }

    #[test]
    fn options_default_to_a_compact_single_selection_tree() {
        let options = TreeOptions::default();
        assert_eq!(options.row_height, 30.0);
        assert_eq!(options.indent, 12.0);
        assert!(options.scrollbar);
        assert_eq!(options.selection, crate::kit::list::SelectionMode::Single);
    }
}

#[cfg(test)]
mod ui_tests {
    use smol_str::{SmolStr, format_smolstr};
    use ui_core::{
        a11y::A11yRole,
        event::{Action, InputEvent, KeyCode, Modifiers, PointerButton},
        interaction::ScrollConfig,
        math::Size,
        node::{builder::NodeBuilder, tree::NodeKind},
        test_support::{a11y::assert_incremental_within, app::TestApp},
    };

    use super::{Tree, TreeItem, TreeOptions};
    use crate::{
        kit::test_support::{
            A11yUpdates, a11y_id, a11y_of, assert_single_parent_per_node, frame_a11y, full_a11y,
        },
        style_builder::StyleBuilder,
        theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role},
    };

    struct Harness {
        app: TestApp,
        tree: Tree,
        theme: ThemeHandle,
    }

    fn items() -> Vec<TreeItem> {
        vec![
            TreeItem::new("first", "first").child(TreeItem::new("first/a", "a")),
            TreeItem::new("leaf", "leaf"),
            TreeItem::new("last", "last").child(TreeItem::new("last/a", "a")),
        ]
    }

    fn harness() -> Harness {
        harness_with(items())
    }

    fn harness_with(items: Vec<TreeItem>) -> Harness {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let tree = Tree::new(
            app.app_mut(),
            theme.clone(),
            "tree",
            items,
            TreeOptions::default(),
        );
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let parent = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(StyleBuilder::new().width(240.0).height(180.0).build())
                .build(tree, root)
                .expect("parent")
        };
        tree.mount(app.app_mut(), window, parent).expect("mount");
        let mut harness = Harness { app, tree, theme };
        settle(&mut harness);
        harness
    }

    fn settle(harness: &mut Harness) {
        for _ in 0..6 {
            let _ = harness.app.drain().expect("frame");
            let _ = harness.tree.sync(harness.app.app_mut()).expect("sync");
        }
        let _ = harness.app.drain().expect("frame");
    }

    fn click_row(harness: &mut Harness, row: usize) -> ui_core::event::InputOutcome {
        let node = harness
            .tree
            .list()
            .row_node(harness.app.app(), row)
            .expect("row");
        let bounds = harness.app.probe().bounds(node).expect("bounds");
        let point = ui_core::math::Point::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let _ = harness.app.inject(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        harness.app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ))
    }

    fn key(harness: &mut Harness, code: KeyCode) -> ui_core::event::InputOutcome {
        harness
            .app
            .inject(InputEvent::key_down(code, Modifiers::empty()))
    }

    fn row_texts(harness: &Harness, row: usize) -> Vec<SmolStr> {
        let window = harness.app.window_id();
        let tree = harness.app.app().window(window).expect("window").tree();
        let Some(node) = harness.tree.list().row_node(harness.app.app(), row) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        fn walk(
            tree: &ui_core::node::tree::NodeTree,
            node: ui_core::node::id::NodeId,
            out: &mut Vec<SmolStr>,
        ) {
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
    fn a_tree_mounts_its_roots_with_carets_and_indentation() {
        let harness = harness();
        assert_eq!(harness.tree.rows().len(), 3, "折叠状态只显示根节点");
        let texts = row_texts(&harness, 0);
        assert!(texts.contains(&SmolStr::new_static("first")), "{texts:?}");
        assert!(texts.contains(&SmolStr::new_static("▸")), "{texts:?}");
        let leaf_texts = row_texts(&harness, 1);
        assert!(leaf_texts.contains(&SmolStr::new_static("leaf")));
        assert!(
            !leaf_texts.contains(&SmolStr::new_static("▸")),
            "叶子没有折叠标：{leaf_texts:?}"
        );
    }

    #[test]
    fn clicking_selects_and_enter_folds_a_folder() {
        let mut harness = harness();
        click_row(&mut harness, 0);
        assert_eq!(
            harness.tree.selected_row(harness.app.app()),
            Some(0),
            "点击选中第一行"
        );
        assert_eq!(
            harness.tree.selected(harness.app.app()),
            Some(SmolStr::new_static("first"))
        );
        key(&mut harness, KeyCode::Enter);
        settle(&mut harness);
        assert!(harness.tree.is_expanded(harness.app.app(), "first"));
        assert_eq!(harness.tree.rows().len(), 4, "展开后多出一行");
        assert_eq!(
            row_texts(&harness, 1),
            vec![SmolStr::new_static("a")],
            "子节点缩进后显示在第二行"
        );
        key(&mut harness, KeyCode::Enter);
        settle(&mut harness);
        assert!(!harness.tree.is_expanded(harness.app.app(), "first"));
        assert_eq!(harness.tree.rows().len(), 3);
    }

    #[test]
    fn arrow_keys_move_the_selection_and_left_walks_to_the_parent() {
        let mut harness = harness();
        click_row(&mut harness, 0);
        key(&mut harness, KeyCode::ArrowDown);
        assert_eq!(harness.tree.selected_row(harness.app.app()), Some(1));
        key(&mut harness, KeyCode::ArrowUp);
        assert_eq!(harness.tree.selected_row(harness.app.app()), Some(0));
        // 展开 first，再右键进入子节点，然后左键回父节点。
        key(&mut harness, KeyCode::Enter);
        settle(&mut harness);
        key(&mut harness, KeyCode::ArrowRight);
        assert_eq!(harness.tree.selected_row(harness.app.app()), Some(1));
        assert_eq!(
            harness.tree.selected(harness.app.app()),
            Some(SmolStr::new_static("first/a"))
        );
        key(&mut harness, KeyCode::ArrowLeft);
        assert_eq!(harness.tree.selected_row(harness.app.app()), Some(0));
        key(&mut harness, KeyCode::ArrowLeft);
        settle(&mut harness);
        assert!(!harness.tree.is_expanded(harness.app.app(), "first"));
    }

    #[test]
    fn an_idle_tree_requests_no_frames() {
        let mut harness = harness();
        let mut idle = false;
        for _ in 0..6 {
            harness.app.advance(std::time::Duration::from_millis(200));
            if !harness.app.drain().expect("frame").rendered {
                idle = true;
                break;
            }
        }
        assert!(idle, "静止的树最终不得再出帧（I3）");
        assert!(
            !harness.tree.sync(harness.app.app_mut()).expect("sync"),
            "无变化的 sync 不写状态"
        );
    }

    fn key_on(app: &mut TestApp, code: KeyCode) {
        let _ = app.inject(InputEvent::key_down(code, Modifiers::empty()));
    }

    #[test]
    fn disabled_rows_reject_clicks_selection_and_toggle() {
        let mut harness = harness_with(vec![
            TreeItem::new("first", "first").child(TreeItem::new("first/a", "a")),
            TreeItem::new("disabled", "disabled").disabled(),
            TreeItem::new("disabled-folder", "disabled-folder")
                .child(TreeItem::new("disabled-folder/a", "a"))
                .disabled(),
            TreeItem::new("leaf", "leaf"),
        ]);
        // 点击禁用行不选中。
        click_row(&mut harness, 1);
        assert_eq!(harness.tree.selected_row(harness.app.app()), None);
        // 程序化选中被拒绝。
        harness
            .tree
            .select_row(harness.app.app_mut(), 2)
            .expect("select");
        assert_eq!(
            harness.tree.selected_row(harness.app.app()),
            None,
            "程序化选中禁用行被拒绝"
        );
        // 键盘上下跳过禁用行（0 → 3）。
        click_row(&mut harness, 0);
        assert_eq!(harness.tree.selected_row(harness.app.app()), Some(0));
        key(&mut harness, KeyCode::ArrowDown);
        assert_eq!(
            harness.tree.selected_row(harness.app.app()),
            Some(3),
            "方向键跳过禁用行"
        );
        key(&mut harness, KeyCode::ArrowUp);
        assert_eq!(harness.tree.selected_row(harness.app.app()), Some(0));
        // 禁用文件夹不能程序化展开 / 折叠。
        assert!(
            !harness
                .tree
                .toggle(harness.app.app_mut(), "disabled-folder")
                .expect("toggle")
        );
        assert!(
            !harness
                .tree
                .is_expanded(harness.app.app(), "disabled-folder")
        );
    }

    #[test]
    fn arrow_keys_are_scoped_to_the_focused_tree_and_leave_other_scroll_containers_alone() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let first = Tree::new(
            app.app_mut(),
            theme.clone(),
            "tree-a",
            items(),
            TreeOptions::default(),
        );
        let second = Tree::new(
            app.app_mut(),
            theme,
            "tree-b",
            items(),
            TreeOptions::default(),
        );
        let root = app.app().window(window).expect("window").tree().root();
        for tree in [&first, &second] {
            let parent = {
                let tree_mut = app.app_mut().window_mut(window).expect("window").tree_mut();
                NodeBuilder::boxed()
                    .style(StyleBuilder::new().width(240.0).height(180.0).build())
                    .build(tree_mut, root)
                    .expect("parent")
            };
            tree.mount(app.app_mut(), window, parent).expect("mount");
        }
        let external = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .relative()
                        .width(100.0)
                        .height(100.0)
                        .overflow_hidden()
                        .build(),
                )
                .on_click(Action::custom("external-scroll-focus"))
                .build(tree, root)
                .expect("external")
        };
        let handle = app
            .app_mut()
            .make_scroll(window, external, ScrollConfig::default())
            .expect("scrollable");
        app.app_mut()
            .set_scroll_content(&handle, Size::new(100.0, 400.0))
            .expect("content");
        for _ in 0..4 {
            let _ = app.drain().expect("frame");
            let _ = first.sync(app.app_mut()).expect("sync");
            let _ = second.sync(app.app_mut()).expect("sync");
        }
        let first_viewport = first.list().viewport_node().expect("first viewport");
        let second_viewport = second.list().viewport_node().expect("second viewport");

        // 焦点在第一棵树：只有它消费方向键。
        app.app_mut().set_focus(window, Some(first_viewport));
        key_on(&mut app, KeyCode::ArrowDown);
        assert_eq!(first.selected_row(app.app()), Some(1));
        assert_eq!(second.selected_row(app.app()), None, "未聚焦的树不动");

        // 焦点换到第二棵树：反过来成立。
        app.app_mut().set_focus(window, Some(second_viewport));
        key_on(&mut app, KeyCode::ArrowDown);
        assert_eq!(second.selected_row(app.app()), Some(1));
        assert_eq!(first.selected_row(app.app()), Some(1), "第一棵树保持不动");

        // 焦点在别的滚动容器：方向键的**默认滚动**照常生效（树不再吞键）。
        app.app_mut().set_focus(window, Some(external));
        key_on(&mut app, KeyCode::ArrowDown);
        let offset = app
            .app()
            .scroll_state(&handle)
            .expect("scroll state")
            .offset
            .y;
        assert!(
            offset > 0.0,
            "树的实例级绑定不得吞掉其它控件的默认方向键滚动（offset={offset}）"
        );
        assert_eq!(first.selected_row(app.app()), Some(1));
        assert_eq!(second.selected_row(app.app()), Some(1));
    }

    /// E9：树容器 = `Tree`、可见行 = `TreeItem`（a11y 父 = `Tree`），标签 / 展开 / 层级 / 选中
    /// 状态进 a11y；行窗口（`List`）不再是树项的父；装饰字形与重复文本 hidden。
    #[test]
    fn tree_items_declare_tree_semantics_to_a11y() {
        let mut harness = harness();
        harness.app.enable_a11y_capture();
        let _ = harness.app.drain().expect("frame");
        harness.app.assert_a11y_contract();
        let outer = harness.tree.outer_node().expect("tree node");
        let viewport = harness.tree.list().viewport_node().expect("viewport");
        let row0 = harness
            .tree
            .list()
            .row_node(harness.app.app(), 0)
            .expect("row 0");
        {
            let snapshot = harness.app.a11y().expect("a11y snapshot");
            let tree = a11y_of(snapshot, outer);
            assert_eq!(tree.role, A11yRole::Tree);
            assert_eq!(tree.size_of_set, Some(3), "集合大小 = 可见行数");
            let row = a11y_of(snapshot, row0);
            assert_eq!(row.role, A11yRole::TreeItem);
            assert_eq!(row.label_text(), Some("first"));
            assert_eq!(
                row.level,
                Some(0),
                "根层级 0（AccessKit 的 level 是 0-based）"
            );
            assert_eq!(row.expanded, Some(false), "文件夹行宣告折叠态");
            assert_eq!(
                row.parent,
                Some(tree.id),
                "树项的 a11y 父是 Tree（行窗口只是布局包裹层）"
            );
            // 行窗口被 E9 领养规则跳过：它不再列出树项；E15b 起也不再以 `List` 暴露（集合
            // 行窗口抑制，平台 `common_filter` 会把 `GenericContainer` 透明上提）。
            let window = a11y_of(snapshot, viewport);
            assert_eq!(
                window.role,
                A11yRole::GenericContainer,
                "集合行窗口不得再自动声明 List（否则 AT 读到「树里套列表」）"
            );
            assert!(window.children.is_empty(), "行窗口不得再列树项");
            // 折叠标 / 重复的行标签文本显式 hidden：读屏不应读到「▸」或把名字读两遍。
            assert!(
                snapshot.nodes().iter().any(|node| node.hidden),
                "装饰文本必须 hidden"
            );
            for node in snapshot.visible_nodes_in_batch() {
                if let Some(text) = node.value.as_deref() {
                    assert!(
                        !text.contains('▸') && !text.contains('▾'),
                        "折叠标不得作为可见文本进 a11y：{text:?}"
                    );
                }
            }
        }
        // 选中 + 展开后行语义更新；全树快照上同时校验父子结构（父唯一 / 无孤儿）。
        click_row(&mut harness, 0);
        key(&mut harness, KeyCode::Enter);
        settle(&mut harness);
        let snapshot = full_a11y(&mut harness.app);
        assert_single_parent_per_node(&snapshot);
        let outer_id = a11y_id(&snapshot, outer);
        let row0 = harness
            .tree
            .list()
            .row_node(harness.app.app(), 0)
            .expect("row 0");
        let first = a11y_of(&snapshot, row0);
        assert_eq!(first.selected, Some(true));
        assert_eq!(first.expanded, Some(true), "展开后折叠态更新");
        let child_row = harness
            .tree
            .list()
            .row_node(harness.app.app(), 1)
            .expect("child row");
        let child = a11y_of(&snapshot, child_row);
        assert_eq!(child.role, A11yRole::TreeItem);
        assert_eq!(child.label_text(), Some("a"));
        assert_eq!(child.level, Some(1), "子节点层级 = 1");
        assert_eq!(
            child.parent,
            Some(outer_id),
            "扁平化虚拟树：子项仍是 Tree 的直接子节点（层级走 aria-level）"
        );
        assert_eq!(
            a11y_of(&snapshot, outer).size_of_set,
            Some(4),
            "展开后可见行数 4"
        );
    }

    /// 选中 / 展开 / 禁用三种状态都经 `A11yPatch` 动态更新，且结构变化发增量笔（不是全树）。
    #[test]
    fn tree_state_changes_are_incremental_a11y_updates() {
        let mut harness = harness_with(vec![
            TreeItem::new("first", "first").child(TreeItem::new("first/a", "a")),
            TreeItem::new("disabled", "disabled").disabled(),
            TreeItem::new("leaf", "leaf"),
        ]);
        harness.app.enable_a11y_capture();
        let _ = harness.app.drain().expect("frame");
        harness.app.assert_a11y_contract();
        let mut updates = A11yUpdates::new();
        updates.apply(&harness.app);

        // 点击禁用行：不选中、不产生 a11y 结构变化。
        click_row(&mut harness, 1);
        frame_a11y(&mut harness.app);
        updates.apply(&harness.app);
        assert_eq!(harness.tree.selected_row(harness.app.app()), None);

        // 展开（行表重建）：结构变化必须是增量笔，不能退化成全树。
        assert!(
            harness
                .tree
                .toggle(harness.app.app_mut(), "first")
                .expect("toggle")
        );
        let _ = harness.app.drain().expect("展开帧");
        harness.app.assert_a11y_contract();
        updates.apply(&harness.app);
        let update = harness
            .app
            .a11y_update()
            .expect("展开必须产生 a11y 交付")
            .clone();
        assert!(update.tree.is_none(), "展开只发增量笔（不是全树重置）");

        let snapshot = full_a11y(&mut harness.app);
        let disabled_row = harness
            .tree
            .list()
            .row_node(harness.app.app(), 2)
            .expect("disabled row");
        let disabled = a11y_of(&snapshot, disabled_row);
        assert_eq!(disabled.role, A11yRole::TreeItem);
        assert_eq!(disabled.label_text(), Some("disabled"));
        assert!(disabled.disabled, "禁用态进 a11y");
        assert_eq!(disabled.expanded, None, "叶子不宣告展开态");

        // 选中：只有相关节点进增量笔（挂载行 + 祖先链），且有上界。
        click_row(&mut harness, 0);
        let _ = harness.app.drain().expect("选中帧");
        harness.app.assert_a11y_contract();
        updates.apply(&harness.app);
        let update = harness
            .app
            .a11y_update()
            .expect("选中必须产生 a11y 交付")
            .clone();
        assert!(update.tree.is_none(), "选中只发增量笔");
        let mounted = harness.tree.list().mounted_rows(harness.app.app()).len();
        assert_incremental_within(&update, mounted + 8);
        let snapshot = full_a11y(&mut harness.app);
        let row0 = harness
            .tree
            .list()
            .row_node(harness.app.app(), 0)
            .expect("row 0");
        assert_eq!(a11y_of(&snapshot, row0).selected, Some(true));
        assert!(
            updates.applied() >= 4,
            "每次状态变化都必须逐笔过 ShadowTree 契约（实际 {} 笔）",
            updates.applied()
        );
    }

    /// 虚拟化：a11y 树只含已挂载的可见行，集合大小仍是逻辑总数（可见行数）；滚动换页后
    /// 每笔 `TreeUpdate` 都过 `ShadowTree` 契约，树项始终挂在 `Tree` 下。
    #[test]
    fn a_virtualized_tree_exposes_mounted_items_and_the_logical_set() {
        let roots: Vec<TreeItem> = (0..40)
            .map(|index| {
                TreeItem::new(format_smolstr!("n{index}"), format_smolstr!("node {index}"))
            })
            .collect();
        let mut harness = harness_with(roots);
        harness.app.enable_a11y_capture();
        let _ = harness.app.drain().expect("frame");
        harness.app.assert_a11y_contract();
        let mut updates = A11yUpdates::new();
        updates.apply(&harness.app);
        let outer = harness.tree.outer_node().expect("tree");
        let mounted = harness.tree.list().mounted_rows(harness.app.app());
        assert!(mounted.len() < 40, "40 行只挂可见窗口：{}", mounted.len());
        let snapshot = full_a11y(&mut harness.app);
        assert_single_parent_per_node(&snapshot);
        let tree = a11y_of(&snapshot, outer);
        assert_eq!(tree.size_of_set, Some(40), "集合大小是逻辑总数（可见行数）");
        let visible_items = tree
            .children
            .iter()
            .filter(|id| {
                snapshot
                    .node(**id)
                    .is_some_and(|node| node.role == A11yRole::TreeItem)
            })
            .count();
        assert_eq!(
            visible_items,
            mounted.len(),
            "a11y 树只含已挂载的可见行（未挂载行不产生节点）"
        );
        for (_, row) in &mounted {
            let node = a11y_of(&snapshot, *row);
            assert_eq!(node.role, A11yRole::TreeItem);
            assert_eq!(node.parent, Some(tree.id));
        }

        // 滚动到第 30 行：窗口换页；新行同样是 Tree 下的 TreeItem，集合大小不变。
        harness
            .tree
            .list()
            .scroll_to_item(
                harness.app.app_mut(),
                30,
                ui_core::interaction::ItemAlign::Start,
            )
            .expect("scroll");
        frame_a11y(&mut harness.app);
        updates.apply(&harness.app);
        settle(&mut harness);
        let snapshot = full_a11y(&mut harness.app);
        assert_single_parent_per_node(&snapshot);
        let tree = a11y_of(&snapshot, outer);
        assert_eq!(tree.size_of_set, Some(40), "滚动不改变逻辑集合大小");
        let mounted = harness.tree.list().mounted_rows(harness.app.app());
        assert!(
            mounted.first().is_some_and(|(index, _)| *index > 0),
            "滚动后可见窗口换页"
        );
        for (_, row) in &mounted {
            let node = a11y_of(&snapshot, *row);
            assert_eq!(node.role, A11yRole::TreeItem);
            assert_eq!(node.parent, Some(tree.id), "换页后的行仍被领养到 Tree");
        }
        let visible_items = tree
            .children
            .iter()
            .filter(|id| {
                snapshot
                    .node(**id)
                    .is_some_and(|node| node.role == A11yRole::TreeItem)
            })
            .count();
        assert_eq!(visible_items, mounted.len());
        assert!(updates.applied() >= 2, "滚动前后两笔交付都必须过影子树契约");
    }

    #[test]
    fn replacing_the_roots_resets_scroll_and_selection() {
        let mut harness = harness_with(vec![
            TreeItem::new("a", "a"),
            TreeItem::new("b", "b"),
            TreeItem::new("c", "c"),
            TreeItem::new("d", "d"),
            TreeItem::new("e", "e"),
            TreeItem::new("f", "f"),
            TreeItem::new("g", "g"),
            TreeItem::new("h", "h"),
        ]);
        click_row(&mut harness, 4);
        harness
            .tree
            .list()
            .scroll_to_item(
                harness.app.app_mut(),
                6,
                ui_core::interaction::ItemAlign::Start,
            )
            .expect("scroll");
        let _ = harness.app.drain().expect("frame");
        assert!(harness.tree.list().offset(harness.app.app()).unwrap_or(0.0) > 0.0);
        harness
            .tree
            .set_roots(harness.app.app_mut(), vec![TreeItem::new("only", "only")])
            .expect("roots");
        settle(&mut harness);
        assert_eq!(harness.tree.rows().len(), 1);
        assert_eq!(
            harness.tree.list().offset(harness.app.app()),
            Some(0.0),
            "换根数据回到顶部"
        );
        assert_eq!(harness.tree.selected_row(harness.app.app()), None);
    }

    #[test]
    fn releasing_the_tree_scope_reclaims_its_signal_slots() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let tree = Tree::new(
            app.app_mut(),
            theme,
            "released-tree",
            items(),
            TreeOptions::default(),
        );
        let released = tree.release(app.app_mut());
        assert!(
            released >= 6,
            "展开状态 + 行体列表 / 滚动条信号必须被回收（实际 {released}）"
        );
        assert_eq!(tree.release(app.app_mut()), 0, "重复释放幂等");
    }

    /// `release` 必须解绑 4 条 scoped 方向键绑定并摘掉 4 个方向键 handler。
    ///
    /// 两种遗漏必须分别可观测：
    /// - 只漏 `off_action`：绑定表项归零但 handler 残留 ⇒ `has_action_handler` 抓到；
    /// - 只漏 `unbind_scope_keys`：handler 已摘，重放按键既不 panic 也不消费 ⇒ 只有绑定表
    ///   计数（`scoped_key_binding_count`）能抓到。
    #[test]
    fn releasing_the_tree_unbinds_its_keys_and_removes_the_handlers() {
        let mut harness = harness();
        let window = harness.app.window_id();
        let viewport = harness.tree.list().viewport_node().expect("viewport");
        let actions = [
            harness.tree.inner.actions.up.clone(),
            harness.tree.inner.actions.down.clone(),
            harness.tree.inner.actions.left.clone(),
            harness.tree.inner.actions.right.clone(),
        ];
        for action in &actions {
            assert!(
                harness.app.app().has_action_handler(action),
                "attach 必须注册 4 个方向键 handler"
            );
        }
        assert_eq!(
            harness.app.app().scoped_key_binding_count(window, viewport),
            4,
            "attach 必须注册 4 条焦点作用域化方向键绑定"
        );

        click_row(&mut harness, 0);
        assert_eq!(
            harness.tree.selected_row(harness.app.app()),
            Some(0),
            "释放前方向键/点击正常"
        );

        assert!(harness.tree.release(harness.app.app_mut()) >= 6);
        assert_eq!(
            harness.app.app().scoped_key_binding_count(window, viewport),
            0,
            "release 必须解绑全部 4 条 scoped 方向键（节点未卸载，自动清理不会触发）"
        );
        for action in &actions {
            assert!(
                !harness.app.app().has_action_handler(action),
                "release 必须摘掉方向键 handler"
            );
        }

        // 不卸载节点直接释放：重放按键 / 点击不得再调用已清理的绑定与 handler。
        let key_outcome = key(&mut harness, KeyCode::ArrowDown);
        assert_eq!(
            key_outcome.action, None,
            "释放后重放方向键不得命中已解绑的 scoped 绑定"
        );
        let click_outcome = click_row(&mut harness, 1);
        assert_eq!(
            click_outcome.action, None,
            "释放后重放点击不得命中已摘除的行选择 handler"
        );
        // 解析出的节点仍可查（纯树查询，不读信号）。
        assert!(harness.tree.list().row_node(harness.app.app(), 1).is_some());
    }

    // ── G3：运行时主题响应性（行文字 / 折叠标 / 选中底色） ─────────────────

    fn resolved(color: ui_core::paint::color::Color) -> [f32; 4] {
        ui_core::paint::color::ColorResolver
            .resolve(color)
            .components
    }

    /// 行内文本节点的 `bind_style.foreground`（DFS 顺序：折叠标在前、标签在后）。
    fn row_text_foregrounds(
        harness: &Harness,
        row: usize,
    ) -> Vec<Option<ui_core::paint::color::Color>> {
        let window = harness.app.window_id();
        let tree = harness.app.app().window(window).expect("window").tree();
        let Some(node) = harness.tree.list().row_node(harness.app.app(), row) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut stack = vec![node];
        while let Some(current) = stack.pop() {
            let Ok(entry) = tree.get(current) else {
                continue;
            };
            if matches!(entry.kind(), NodeKind::Text(_)) {
                out.push(entry.style_patch().foreground);
            }
            stack.extend(entry.children().iter().rev().copied());
        }
        out
    }

    /// 外观切换后树行的文字 / 折叠标 / 选中底色换成新主题的角色色；不读主题的静态节点不被失效。
    #[test]
    fn switching_appearance_repaints_tree_row_text_and_selection() {
        let mut harness = harness();
        click_row(&mut harness, 0);
        settle(&mut harness);

        let light = Theme::of(Appearance::Light);
        let window = harness.app.window_id();
        let row_node = harness
            .tree
            .list()
            .row_node(harness.app.app(), 0)
            .expect("row");
        let row_background = |harness: &Harness| {
            harness
                .app
                .app()
                .window(window)
                .expect("window")
                .tree()
                .get(row_node)
                .expect("row node")
                .style_patch()
                .background
        };
        assert_eq!(
            row_background(&harness).map(resolved),
            Some(resolved(light.role(Role::Selection))),
            "浅色选中行底色"
        );
        let texts = row_text_foregrounds(&harness, 0);
        assert_eq!(
            texts.first().copied().flatten().map(resolved),
            Some(resolved(light.role(Role::FgMuted))),
            "浅色折叠标"
        );
        assert_eq!(
            texts.last().copied().flatten().map(resolved),
            Some(resolved(light.role(Role::FgDefault))),
            "浅色行标签"
        );

        // 静态节点（不读任何信号）：主题切换不得失效它。
        let static_node = {
            let parent = harness.tree.outer_node().expect("树的外层");
            let window = harness.app.window_id();
            let tree = harness
                .app
                .app_mut()
                .window_mut(window)
                .expect("window")
                .tree_mut();
            NodeBuilder::boxed()
                .style(StyleBuilder::new().size(8.0, 8.0).build())
                .build(tree, parent)
                .expect("静态兄弟")
        };
        settle(&mut harness);

        assert!(
            harness
                .theme
                .set_appearance(harness.app.app_mut(), Appearance::Dark)
        );
        let record = harness.app.drain().expect("切深色").clone();
        assert!(
            record.dirty_nodes.contains(&row_node),
            "读过主题的行节点必须被失效：{:?}",
            record.dirty_nodes
        );
        assert!(
            !record.dirty_nodes.contains(&static_node),
            "未读主题的静态节点不得被失效：{:?}",
            record.dirty_nodes
        );
        settle(&mut harness);

        let dark = Theme::of(Appearance::Dark);
        assert_eq!(
            row_background(&harness).map(resolved),
            Some(resolved(dark.role(Role::Selection))),
            "深色选中行底色"
        );
        let texts = row_text_foregrounds(&harness, 0);
        assert_eq!(
            texts.first().copied().flatten().map(resolved),
            Some(resolved(dark.role(Role::FgMuted))),
            "深色折叠标"
        );
        assert_eq!(
            texts.last().copied().flatten().map(resolved),
            Some(resolved(dark.role(Role::FgDefault))),
            "深色行标签"
        );
        assert_ne!(
            resolved(light.role(Role::FgDefault)),
            resolved(dark.role(Role::FgDefault)),
            "两套外观的 FgDefault 必须不同（否则上面的断言无意义）"
        );
    }
}
