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
//! [`Tree::bind_keys`] 注册 `Up/Down/Left/Right` 四个全局绑定（引擎按键绑定是**全应用一份**
//! 表，同一 App 里只能有一个树实例绑定方向键；作用域化的绑定属 P2-4 机制）；`Enter/Space` 走
//! 引擎的「焦点节点激活」通道（列表容器的 Action），由 [`List::on_activate`] 回调触发折叠。
//! 绑定只在焦点落在本树的列表容器上时生效。
//!
//! # 折叠标
//!
//! 折叠标是 `bind_text` 节点，读展开状态 `Signal<TreeState>`：展开 / 折叠后只有该节点的文本
//! 变化；行内容本身在 `set_count` 时按新行表重建（展开会改变行数）。

use std::{cell::RefCell, collections::BTreeSet, rc::Rc};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    error::EngineError,
    event::{Action, KeyBinding, KeyCode, Modifiers},
    interaction::ItemAlign,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
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

    /// 展开 / 折叠（非文件夹返回 `false`）。
    pub fn toggle(&mut self, id: &str) -> bool {
        if self.find(id).is_none_or(|item| !item.is_folder()) {
            return false;
        }
        if !self.expanded.remove(id) {
            self.expanded.insert(SmolStr::new(id));
        }
        true
    }

    /// 展开（非文件夹 / 已展开返回 `false`）。
    pub fn expand(&mut self, id: &str) -> bool {
        if self.find(id).is_none_or(|item| !item.is_folder()) {
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
    /// - `Up/Down`：在可见行里移动；
    /// - `Right`：折叠的文件夹展开；已展开的文件夹选到第一个子节点；叶子上移到下一行；
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
        match key {
            TreeKey::Up => KeyOutcome::Select(current.saturating_sub(1)),
            TreeKey::Down => KeyOutcome::Select((current + 1).min(last)),
            TreeKey::Right => {
                if row.folder && !row.expanded {
                    if self.expand(&row.id) {
                        KeyOutcome::Toggle { row: current }
                    } else {
                        KeyOutcome::None
                    }
                } else if self.collapse_if_leaf_selected(&rows, current) {
                    KeyOutcome::None
                } else {
                    KeyOutcome::Select((current + 1).min(last))
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

    /// `Right` 在「已展开文件夹的最后一个子节点」上不越到父节点之后——由调用方的行表决定；
    /// 这里只处理「选中的不是文件夹」的情况（返回 `false` 表示继续走默认右移）。
    fn collapse_if_leaf_selected(&mut self, rows: &[FlatRow], current: usize) -> bool {
        let _ = (rows, current);
        false
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
        let state = TreeState::new(roots);
        let flat = Rc::new(RefCell::new(state.rows()));
        let signal = app.signal(state);
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
        let inner = TreeInner {
            list,
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
            .build(tree, parent)?;
        self.inner.list.build(tree, outer)?;
        self.inner.geometry.borrow_mut().outer = Some(outer);
        Ok(outer)
    }

    /// 接引擎：列表 + 键盘绑定。
    ///
    /// # Errors
    /// 同 [`List::attach`]。
    pub fn attach(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        self.inner.list.attach(app, window)?;
        *self.inner.window.borrow_mut() = Some(window);
        self.bind_keys(app);
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

    /// 注册方向键绑定（全应用一份绑定表：同一 App 只应有一个树实例绑定方向键）。
    pub fn bind_keys(&self, app: &mut App) {
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
            app.bind_key(KeyBinding {
                key: code,
                modifiers: Modifiers::empty(),
                action: action.clone(),
            });
            let tree = self.clone();
            app.on_action(action, move |ctx| {
                tree.on_key(ctx, key);
            });
        }
    }

    /// 展开 / 折叠一行；返回行表是否变了。
    ///
    /// # Errors
    /// `List::set_count` 的引擎错误。
    pub fn toggle_row(&self, app: &mut App, row: usize) -> Result<bool, EngineError> {
        let Some(flat) = self.inner.flat.borrow().get(row).cloned() else {
            return Ok(false);
        };
        if !flat.folder {
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
        if !state.toggle(id) {
            return Ok(false);
        }
        self.rebuild_rows(app, state)
    }

    /// 换根数据（行表 / 展开集全部替换，选中清空）。
    ///
    /// # Errors
    /// `List::set_count` 的引擎错误。
    pub fn set_roots(
        &self,
        app: &mut App,
        roots: impl IntoIterator<Item = TreeItem>,
    ) -> Result<(), EngineError> {
        let state = TreeState::new(roots);
        self.rebuild_rows(app, state)?;
        self.inner.list.clear_selection(app);
        Ok(())
    }

    /// 把模型写回信号并重建行表（保留选中行）。
    fn rebuild_rows(&self, app: &mut App, state: TreeState) -> Result<bool, EngineError> {
        let rows = state.rows();
        let count = rows.len();
        *self.inner.flat.borrow_mut() = rows;
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

    fn on_key(&self, ctx: &mut ui_core::interaction::ActionCtx<'_>, key: TreeKey) {
        let (Some(window), Some(viewport)) =
            (*self.inner.window.borrow(), self.inner.list.viewport_node())
        else {
            return;
        };
        if ctx.app_mut().focused(window) != Some(viewport) {
            return;
        }
        let _ = self.handle_key(ctx.app_mut(), key);
    }
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
    use smol_str::SmolStr;
    use ui_core::{
        event::{InputEvent, KeyCode, Modifiers, PointerButton},
        node::{builder::NodeBuilder, tree::NodeKind},
        test_support::app::TestApp,
    };

    use super::{Tree, TreeItem, TreeOptions};
    use crate::{
        style_builder::StyleBuilder,
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    struct Harness {
        app: TestApp,
        tree: Tree,
    }

    fn items() -> Vec<TreeItem> {
        vec![
            TreeItem::new("first", "first").child(TreeItem::new("first/a", "a")),
            TreeItem::new("leaf", "leaf"),
            TreeItem::new("last", "last").child(TreeItem::new("last/a", "a")),
        ]
    }

    fn harness() -> Harness {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let tree = Tree::new(
            app.app_mut(),
            theme,
            "tree",
            items(),
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
        let mut harness = Harness { app, tree };
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

    fn click_row(harness: &mut Harness, row: usize) {
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
        let _ = harness.app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
    }

    fn key(harness: &mut Harness, code: KeyCode) {
        let _ = harness
            .app
            .inject(InputEvent::key_down(code, Modifiers::empty()));
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
}
