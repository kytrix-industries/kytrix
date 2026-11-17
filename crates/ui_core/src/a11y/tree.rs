//! a11y 树的侧表载荷与「节点 → `A11yNode`」的构建。
//!
//! 侧表（`NodeTree::a11y_slots`）与 `layout_cache` / `clip_keys` / `text_slots` / `debug_ids` 同型：
//! 按 arena 槽位索引、槽位复用时清空、`remove` 时逐槽清空。`Node` 因此保持 ≤ 320 B（B5 之后为 312 B）。
//!
//! 本模块的三条职责（都在 [`node_a11y`] 一个入口里）：
//! 1. **语义判定**：角色 / 名字 / 值 / 隐藏 / 禁用 / 动作，判据来自 `a11y::mapping` 的纯函数；
//! 2. **自动语义**：虚拟列表位置、表格的行列索引与总数、文本的 grapheme 长度与词边界、
//!    滚动状态 —— 这些是引擎**已经知道**的事实，不该逼应用逐节点手填；
//! 3. **稳定身份**：一切 id（自身 / children / parent）都经 `a11y::identity` 的分配器发放。

use smallvec::SmallVec;

use crate::{
    a11y::{
        A11yDiagnostic, A11yLabel, A11yLabelSource, A11yNode, A11yOverride, A11yPatch, A11yRole,
        A11yScroll,
        identity::{A11yIds, A11yKey},
        mapping::{
            a11y_disabled, bounds_of, effective_actions, effective_description, effective_label,
            effective_role, effective_value, is_hidden,
        },
        textrun,
    },
    interaction::ScrollAxis,
    math::Scale,
    node::{
        dependency::ReadCtx,
        id::NodeId,
        tree::{Node, NodeKind, NodeTree},
    },
    paint::style::Clip,
};

/// 一个节点的 a11y 载荷（侧表元素）。
///
/// `pub(crate)` 结构 + `pub` 字段（workspace lint 禁止字段级 `pub(crate)`；与 `NodeInteraction` 同款）。
pub(crate) struct NodeA11y {
    /// 静态声明（挂载时写入）。
    pub over: Option<A11yOverride>,
    /// 动态补丁闭包（`bind_a11y`）。
    pub bind: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> A11yPatch>>,
    /// 上次求值的动态补丁（diff 用）。
    pub patch: A11yPatch,
}

impl NodeA11y {
    pub(crate) fn new(
        over: Option<A11yOverride>,
        bind: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> A11yPatch>>,
    ) -> Self {
        Self {
            over,
            bind,
            patch: A11yPatch::default(),
        }
    }
}

impl std::fmt::Debug for NodeA11y {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeA11y")
            .field("over", &self.over)
            .field("bind", &self.bind.as_ref().map(|_| "<closure>"))
            .field("patch", &self.patch)
            .finish()
    }
}

/// 构建一个节点的 a11y 快照记录（角色 → 名字 → 值 → 隐藏 → 禁用 → 动作 → 自动语义 → 稳定身份）。
///
/// 返回 `None` = 句柄陈旧（调用方已过滤，防御性）。诊断直接推进 `diagnostics`。
/// 稳定身份由 `ids` 发放（同一节点跨帧不变、逻辑行重建后不变）。
pub(crate) fn node_a11y(
    tree: &NodeTree,
    node: NodeId,
    scale: Scale,
    title: &str,
    ids: &mut A11yIds,
    diagnostics: &mut Vec<A11yDiagnostic>,
) -> Option<A11yNode> {
    let target = tree.get(node).ok()?;
    let slot = tree.a11y_of(node);
    let over = slot.and_then(|slot| slot.over.as_ref());
    let patch = slot.map(|slot| &slot.patch);

    let role = effective_role(tree, node, target, over, patch);
    let mut label = effective_label(target, role, over, patch);
    if node == tree.root() && label.is_none() && !title.is_empty() {
        label = Some(A11yLabel::new(
            smol_str::SmolStr::new(title),
            A11yLabelSource::Override,
        ));
    }
    let value = effective_value(target, role, over, patch);
    let description = effective_description(over, patch);
    // 与 `selected` / `expanded` 同型：布尔状态只走动态补丁（`A11yOverride` 不承载）。
    let checked = patch.and_then(|patch| patch.checked);
    let hidden = is_hidden(target, role, label.as_ref(), over, patch);
    let disabled = a11y_disabled(tree, node);
    let actions = effective_actions(tree, node, role, over, patch, disabled);

    // 诊断：自绘缺替代文本（§5.4）、交互节点无名（§5.2）。
    if role == A11yRole::Image
        && label.is_none()
        && let Some(kind) = match target.kind() {
            NodeKind::Canvas(_) => Some(crate::a11y::A11yDrawKind::Canvas),
            NodeKind::Geometry(_) => Some(crate::a11y::A11yDrawKind::Geometry),
            _ => None,
        }
    {
        diagnostics.push(A11yDiagnostic::MissingAltText { node, kind });
    }
    if label.is_none()
        && value.is_none()
        && matches!(
            role,
            A11yRole::Button
                | A11yRole::TextInput
                | A11yRole::MultilineTextInput
                | A11yRole::CheckBox
                | A11yRole::Switch
                | A11yRole::RadioButton
                | A11yRole::Slider
                | A11yRole::MenuItem
                | A11yRole::ListBoxOption
                // R13-22 补：没有名字就没有语义的容器 / 控件。
                // 刻意**不含** `Row` / `Cell` / `ColumnHeader` / `RowHeader`：这些角色的
                // 可读内容在子文本节点上（AT 读子节点），要求它们自己有 label 是假诊断。
                | A11yRole::ScrollBar
                | A11yRole::Menu
                | A11yRole::MenuBar
                | A11yRole::Tooltip
        )
    {
        diagnostics.push(A11yDiagnostic::MissingLabel { node });
    }

    // 自定义动作（R13-4）：能力（`A11yAction::Custom`）与注册表（id + 名字）配对；
    // 只有能力没有注册项的节点在 AT 侧完全不可见。
    let custom_actions = match patch.and_then(|patch| patch.custom_actions.clone()) {
        Some(actions) => actions,
        None => over
            .map(|over| over.custom_actions.clone())
            .unwrap_or_default(),
    };
    for action in &actions {
        if let crate::a11y::A11yAction::Custom(id) = action
            && !custom_actions.iter().any(|custom| custom.id == *id)
        {
            diagnostics.push(A11yDiagnostic::UnregisteredCustomAction { node, id: *id });
        }
    }

    // 文本语义（A5）：`character_lengths` / `word_starts` 只对"文本就是 value"的角色有意义。
    let text_metrics = match role {
        A11yRole::Label
        | A11yRole::TextRun
        | A11yRole::TextInput
        | A11yRole::MultilineTextInput => value.as_deref().and_then(textrun::text_metrics),
        _ => None,
    };
    let (character_lengths, word_starts) = text_metrics.unwrap_or_default();
    let text_selection = match role {
        A11yRole::TextRun | A11yRole::TextInput | A11yRole::MultilineTextInput => patch
            .and_then(|patch| patch.selection)
            .or_else(|| over.and_then(|over| over.selection))
            .map(|selection| textrun::selection(&character_lengths, selection)),
        _ => None,
    };

    // 表格语义（A2）：行/列索引与总数在没有显式声明时**自动推导**。
    let table = derive_table_semantics(tree, node, role, over, patch);
    let sort = patch
        .and_then(|patch| patch.sort)
        .or_else(|| over.and_then(|over| over.sort));

    // 滚动状态（R13-13）：`ScrollView` 宣告了 6 个滚动动作却没暴露 `scroll_*`，AT 无从播报。
    let scroll = scroll_state(tree, node);
    let clips_children = tree.scroll_slot(node).is_some()
        || tree
            .get(node)
            .is_ok_and(|state| state.style().clip != Clip::None);

    // 表单 / 状态属性（R13-14）。
    let read_only = patch
        .and_then(|patch| patch.read_only)
        .or_else(|| over.map(|over| over.read_only))
        .unwrap_or(false);
    let modal = patch
        .and_then(|patch| patch.modal)
        .or_else(|| over.map(|over| over.modal))
        // 浮层自动模态：`Dialog` 角色且确实在浮层槽里 ⇒ AT 必须把阅读限制在对话框内。
        .unwrap_or_else(|| role == A11yRole::Dialog && tree.overlay_slot(node).is_some());
    let busy = patch
        .and_then(|patch| patch.busy)
        .or_else(|| over.map(|over| over.busy))
        .unwrap_or(false);
    let invalid = patch
        .and_then(|patch| patch.invalid)
        .or_else(|| over.map(|over| over.invalid))
        .unwrap_or(false);
    // `error_message` 是 NodeId 关系（`aria-errormessage`）：目标必须存活（悬挂引用会让
    // `TreeUpdate` 指向不存在的节点），存活时换算成稳定 id。
    let error_message = patch
        .and_then(|patch| patch.error_message)
        .or_else(|| over.and_then(|over| over.error_message))
        .filter(|target| tree.get(*target).is_ok())
        .map(|target| {
            let key = identity_key(
                tree,
                target,
                over_child(tree, target),
                patch_child(tree, target),
            );
            ids.id_for(tree, target, key, diagnostics)
        });
    let placeholder = patch
        .and_then(|patch| patch.placeholder.clone())
        .or_else(|| over.and_then(|over| over.placeholder.clone()));
    let level = patch
        .and_then(|patch| patch.level)
        .or_else(|| over.and_then(|over| over.level));

    let id = ids.id_for(
        tree,
        node,
        identity_key(tree, node, over, patch),
        diagnostics,
    );
    let children = target
        .children()
        .iter()
        .map(|child| {
            let key = identity_key(
                tree,
                *child,
                over_child(tree, *child),
                patch_child(tree, *child),
            );
            ids.id_for(tree, *child, key, diagnostics)
        })
        .collect();
    Some(A11yNode {
        id,
        engine: node,
        role,
        label,
        value,
        description,
        bounds: bounds_of(tree, node, scale),
        actions,
        hidden,
        disabled,
        checked,
        selected: patch.and_then(|patch| patch.selected),
        expanded: patch.and_then(|patch| patch.expanded),
        // 以下新增的语义字段一律**动态补丁优先、静态声明兜底** —— 与 `effective_role` /
        // `effective_label` / `effective_value` 的合并方向一致（复审发现此前写成 over 优先，
        // 会让静态声明永久压住 `bind_a11y` 的每帧更新，例如滑块数值永远停在初值）。
        live: patch
            .and_then(|patch| patch.live)
            .or_else(|| over.and_then(|over| over.live)),
        // 虚拟列表行：引擎知道「第 N 项 / 共 M 项」，必须自动填 —— 否则应用得自己算，
        // 而 `VirtualSlot` 是内部状态（review P2-6：修复只做了字段管道，没接线）。
        //
        // **AccessKit 的 `pos_in_set` 是 0-based**（其文档明确：aria-posinset 是 1-based，
        // 而本属性是 0-based；Windows/AT-SPI 适配层自己 +1）。此前写成 `index + 1` 会让 AT
        // 播报「第 N+2 项」（第二轮复审 P2-1）。
        pos_in_set: patch
            .and_then(|patch| patch.pos_in_set)
            .or_else(|| over.and_then(|over| over.pos_in_set))
            .or_else(|| virtual_set_position(tree, node).map(|(index, _)| index)),
        // `size_of_set` 必须挂在**容器**上：AccessKit 的 consumer 只在祖先上查它
        // （`size_of_set_from_container`），写在行上不会到达 AT；写在行上还会让嵌套虚拟列表
        // 的内层行读到外层的 count。所以这里对「虚拟列表容器」节点填自己的条目总数。
        size_of_set: patch
            .and_then(|patch| patch.size_of_set)
            .or_else(|| over.and_then(|over| over.size_of_set))
            .or_else(|| tree.virtual_slot(node).map(|slot| slot.count)),
        numeric_value: patch
            .and_then(|patch| patch.numeric_value)
            .or_else(|| over.and_then(|over| over.numeric_value)),
        min_numeric_value: patch
            .and_then(|patch| patch.min_numeric_value)
            .or_else(|| over.and_then(|over| over.min_numeric_value)),
        max_numeric_value: patch
            .and_then(|patch| patch.max_numeric_value)
            .or_else(|| over.and_then(|over| over.max_numeric_value)),
        numeric_value_step: patch
            .and_then(|patch| patch.numeric_value_step)
            .or_else(|| over.and_then(|over| over.numeric_value_step)),
        numeric_value_jump: patch
            .and_then(|patch| patch.numeric_value_jump)
            .or_else(|| over.and_then(|over| over.numeric_value_jump)),
        row_index: table.row_index,
        column_index: table.column_index,
        row_count: table.row_count,
        column_count: table.column_count,
        sort,
        scroll,
        clips_children,
        custom_actions,
        character_lengths,
        word_starts,
        text_selection,
        read_only,
        modal,
        busy,
        invalid,
        error_message,
        placeholder,
        level,
        children,
        parent: target.parent().map(|parent| {
            let key = identity_key(
                tree,
                parent,
                over_child(tree, parent),
                patch_child(tree, parent),
            );
            ids.id_for(tree, parent, key, diagnostics)
        }),
    })
}

fn over_child(tree: &NodeTree, node: NodeId) -> Option<&A11yOverride> {
    tree.a11y_of(node).and_then(|slot| slot.over.as_ref())
}

fn patch_child(tree: &NodeTree, node: NodeId) -> Option<&A11yPatch> {
    tree.a11y_of(node).map(|slot| &slot.patch)
}

/// 节点的**逻辑身份键**（显式声明优先，其次虚拟列表行）。
///
/// 引擎自动为虚拟列表行生成 `(容器, 条目下标)` 键：行滚出视口被回收、再滚回来重新 `build`
/// 之后，a11y id 仍与首次挂载相同（R13-1 的核心场景：订单簿/持仓表的读屏位置不丢）。
fn identity_key(
    tree: &NodeTree,
    node: NodeId,
    over: Option<&A11yOverride>,
    patch: Option<&A11yPatch>,
) -> Option<A11yKey> {
    if let Some(key) = patch
        .and_then(|patch| patch.key)
        .or_else(|| over.and_then(|over| over.key))
    {
        return Some(A11yKey::App(key));
    }
    let parent = tree.get(node).ok()?.parent()?;
    let slot = tree.virtual_slot(parent)?;
    let (index, _) = slot.mounted.iter().find(|(_, row)| *row == node)?;
    Some(A11yKey::VirtualRow {
        list: parent,
        index: *index,
    })
}

/// 该节点是虚拟列表的已挂载行时返回 `(index, count)`（`index` 是 0-based）。
///
/// `VirtualSlot` 是引擎内部状态，应用拿不到 —— 所以「第 N 项，共 M 项」必须由引擎自己
/// 填进 a11y 快照，否则屏幕阅读器只能播报行内容、无法告知位置（复审 P2-6）。
fn virtual_set_position(tree: &NodeTree, node: NodeId) -> Option<(usize, usize)> {
    let mut cursor = tree.get(node).ok().and_then(|target| target.parent());
    while let Some(current) = cursor {
        if let Some(slot) = tree.virtual_slot(current)
            && let Some((index, _)) = slot.mounted.iter().find(|(_, row)| *row == node)
        {
            return Some((*index, slot.count));
        }
        cursor = tree.get(current).ok().and_then(|target| target.parent());
    }
    None
}

/// 滚动状态（逻辑像素 → 物理像素由适配层按 scale 换算；这里保留引擎的布局单位）。
fn scroll_state(tree: &NodeTree, node: NodeId) -> Option<A11yScroll> {
    let slot = tree.scroll_slot(node)?;
    let viewport = crate::interaction::scroll::viewport(tree, node);
    let max = crate::interaction::scroll::max_offset(&slot, viewport.size);
    Some(A11yScroll {
        x: slot.offset.x,
        y: slot.offset.y,
        x_max: max.x,
        y_max: max.y,
        vertical: matches!(slot.axis, ScrollAxis::Vertical),
    })
}

/// 表格行列语义（显式声明优先，缺失时从结构推导）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TableSemantics {
    row_index: Option<usize>,
    column_index: Option<usize>,
    row_count: Option<usize>,
    column_count: Option<usize>,
}

fn derive_table_semantics(
    tree: &NodeTree,
    node: NodeId,
    role: A11yRole,
    over: Option<&A11yOverride>,
    patch: Option<&A11yPatch>,
) -> TableSemantics {
    let mut semantics = TableSemantics {
        row_index: patch
            .and_then(|patch| patch.row_index)
            .or_else(|| over.and_then(|over| over.row_index)),
        column_index: patch
            .and_then(|patch| patch.column_index)
            .or_else(|| over.and_then(|over| over.column_index)),
        row_count: patch
            .and_then(|patch| patch.row_count)
            .or_else(|| over.and_then(|over| over.row_count)),
        column_count: patch
            .and_then(|patch| patch.column_count)
            .or_else(|| over.and_then(|over| over.column_count)),
    };
    let parent = tree.get(node).ok().and_then(Node::parent);
    match role {
        A11yRole::Table | A11yRole::Grid => {
            if semantics.row_count.is_none() {
                semantics.row_count = count_rows(tree, node);
            }
            if semantics.column_count.is_none() {
                semantics.column_count = count_columns(tree, node);
            }
        }
        A11yRole::Row => {
            let Some(parent) = parent else {
                return semantics;
            };
            if is_table_role(tree, parent) {
                if semantics.row_index.is_none() {
                    semantics.row_index = row_sibling_index(tree, node);
                }
                if semantics.row_count.is_none() {
                    semantics.row_count =
                        declared_row_count(tree, parent).or_else(|| count_rows(tree, parent));
                }
            }
        }
        A11yRole::Cell | A11yRole::ColumnHeader | A11yRole::RowHeader => {
            let Some(row) = parent else {
                return semantics;
            };
            if semantics.column_index.is_none() {
                semantics.column_index = cell_sibling_index(tree, node);
            }
            if is_row_role(tree, row) {
                if semantics.row_index.is_none() {
                    semantics.row_index = row_sibling_index(tree, row);
                }
                if let Some(table) = tree.get(row).ok().and_then(Node::parent) {
                    if semantics.row_count.is_none() {
                        semantics.row_count =
                            declared_row_count(tree, table).or_else(|| count_rows(tree, table));
                    }
                    if semantics.column_count.is_none() {
                        semantics.column_count = declared_column_count(tree, table)
                            .or_else(|| count_columns(tree, table));
                    }
                }
            }
        }
        _ => {}
    }
    semantics
}

/// 该节点的**声明**角色（只看 `patch.role` / `over.role`；不做 kind 推导）。
///
/// 表格结构的推导只认显式角色：`Row` / `Cell` 本来就是"应用声明才有"的角色，靠 kind 猜会把
/// 任何虚拟列表行也算进来。
fn declared_role(tree: &NodeTree, node: NodeId) -> Option<A11yRole> {
    let slot = tree.a11y_of(node)?;
    slot.patch
        .role
        .or_else(|| slot.over.as_ref().and_then(|over| over.role))
}

fn is_table_role(tree: &NodeTree, node: NodeId) -> bool {
    matches!(
        declared_role(tree, node),
        Some(A11yRole::Table | A11yRole::Grid)
    )
}

fn is_row_role(tree: &NodeTree, node: NodeId) -> bool {
    matches!(declared_role(tree, node), Some(A11yRole::Row))
}

fn is_cell_role(tree: &NodeTree, node: NodeId) -> bool {
    matches!(
        declared_role(tree, node),
        Some(A11yRole::Cell | A11yRole::ColumnHeader | A11yRole::RowHeader)
    )
}

fn children_of(tree: &NodeTree, node: NodeId) -> &[NodeId] {
    tree.get(node).map_or(&[], |target| target.children())
}

/// 表节点声明的行数（`row_count` 在表上声明时优先用它）。
fn declared_row_count(tree: &NodeTree, table: NodeId) -> Option<usize> {
    let slot = tree.a11y_of(table)?;
    slot.patch
        .row_count
        .or_else(|| slot.over.as_ref().and_then(|over| over.row_count))
}

fn declared_column_count(tree: &NodeTree, table: NodeId) -> Option<usize> {
    let slot = tree.a11y_of(table)?;
    slot.patch
        .column_count
        .or_else(|| slot.over.as_ref().and_then(|over| over.column_count))
}

/// 行数 = 声明为 `Row` 的直接子节点数（没有 ⇒ `None`，不编造 0）。
fn count_rows(tree: &NodeTree, table: NodeId) -> Option<usize> {
    let rows = children_of(tree, table)
        .iter()
        .filter(|child| is_row_role(tree, **child))
        .count();
    (rows > 0).then_some(rows)
}

/// 列数 = 各行里声明为 `Cell`/`ColumnHeader`/`RowHeader` 的子节点数的最大值。
fn count_columns(tree: &NodeTree, table: NodeId) -> Option<usize> {
    let mut widest = 0usize;
    for row in children_of(tree, table) {
        if !is_row_role(tree, *row) {
            continue;
        }
        let cells = children_of(tree, *row)
            .iter()
            .filter(|child| is_cell_role(tree, **child))
            .count();
        widest = widest.max(cells);
    }
    (widest > 0).then_some(widest)
}

/// 节点在"同角色兄弟"里的序号（`Row` 在表里、`Cell` 在行里）。
fn sibling_index_where(
    tree: &NodeTree,
    node: NodeId,
    predicate: fn(&NodeTree, NodeId) -> bool,
) -> Option<usize> {
    let parent = tree.get(node).ok()?.parent()?;
    let mut index = 0usize;
    for child in children_of(tree, parent) {
        if *child == node {
            return Some(index);
        }
        if predicate(tree, *child) {
            index += 1;
        }
    }
    None
}

fn row_sibling_index(tree: &NodeTree, node: NodeId) -> Option<usize> {
    sibling_index_where(tree, node, is_row_role)
}

fn cell_sibling_index(tree: &NodeTree, node: NodeId) -> Option<usize> {
    sibling_index_where(tree, node, is_cell_role)
}

/// 把 `root` 及其整棵子树（DFS 先序）推进 `out`（去重由调用方决定）。
pub(crate) fn collect_subtree(tree: &NodeTree, root: NodeId, out: &mut SmallVec<[NodeId; 16]>) {
    let mut stack: SmallVec<[NodeId; 16]> = SmallVec::new();
    stack.push(root);
    while let Some(current) = stack.pop() {
        if tree.get(current).is_err() {
            continue;
        }
        out.push(current);
        if let Ok(target) = tree.get(current) {
            for child in target.children().iter().rev() {
                stack.push(*child);
            }
        }
    }
}

/// 从 `from` 的**父**开始向上，把祖先链推进 `out`（`from` 自身不加）。
pub(crate) fn push_ancestors(tree: &NodeTree, from: NodeId, out: &mut SmallVec<[NodeId; 16]>) {
    let mut cursor = tree.get(from).ok().and_then(|target| target.parent());
    while let Some(current) = cursor {
        out.push(current);
        cursor = tree.get(current).ok().and_then(|target| target.parent());
    }
}

/// 焦点回退：焦点节点必须**存在**于当前树里（否则 `accesskit_consumer` 的 `validate_global`
/// 会 panic），陈旧/已卸载时回退到窗口根。
///
/// 返回值第二项 = 需要记 `MissingFocusTarget` 诊断的原节点。
pub(crate) fn resolve_focus(tree: &NodeTree, focus: Option<NodeId>) -> (NodeId, Option<NodeId>) {
    match focus {
        Some(node) if tree.get(node).is_ok() => (node, None),
        Some(node) => (tree.root(), Some(node)),
        None => (tree.root(), None),
    }
}

#[cfg(test)]
mod tests {
    use smallvec::SmallVec;

    use super::{collect_subtree, push_ancestors, resolve_focus};
    use crate::{
        a11y::{A11yOverride, A11yRole, A11ySort, identity::A11yIds},
        math::Scale,
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        text::TextStyle,
    };

    fn chain(tree: &mut NodeTree) -> (NodeId, NodeId, NodeId) {
        let root = tree.root();
        let parent = NodeBuilder::boxed().build(tree, root).expect("parent");
        let child = NodeBuilder::boxed().build(tree, parent).expect("child");
        (root, parent, child)
    }

    /// P2-6：live region 必须落进 accesskit 节点（AT 靠它决定是否主动播报）。
    #[test]
    fn live_region_reaches_the_accesskit_node() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let price = NodeBuilder::boxed()
            .a11y(crate::a11y::A11yOverride {
                role: Some(crate::a11y::A11yRole::Label),
                label: Some(smol_str::SmolStr::new_static("BTC 42000")),
                live: Some(crate::a11y::A11yLive::Polite),
                ..crate::a11y::A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("price");
        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let node = super::node_a11y(
            &tree,
            price,
            crate::math::Scale::ONE,
            "",
            &mut ids,
            &mut diagnostics,
        )
        .expect("snapshot");
        assert_eq!(node.live, Some(crate::a11y::A11yLive::Polite));
        let ak = crate::a11y::adapter::to_accesskit(&node);
        assert_eq!(ak.live(), Some(accesskit::Live::Polite));
    }
    /// P2-8：滑块的数值通道必须同时进快照与 accesskit 节点。
    ///
    /// UIA 的 `RangeValuePattern` 读的是 accesskit 那一层；只给字符串值时不播报也不行调节。
    #[test]
    fn numeric_value_reaches_the_accesskit_node() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let slider = NodeBuilder::boxed()
            .a11y(crate::a11y::A11yOverride {
                role: Some(crate::a11y::A11yRole::Slider),
                label: Some(smol_str::SmolStr::new_static("volume")),
                numeric_value: Some(37.0),
                min_numeric_value: Some(0.0),
                max_numeric_value: Some(100.0),
                numeric_value_step: Some(1.0),
                numeric_value_jump: Some(10.0),
                ..crate::a11y::A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("slider");
        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let node = super::node_a11y(
            &tree,
            slider,
            crate::math::Scale::ONE,
            "",
            &mut ids,
            &mut diagnostics,
        )
        .expect("snapshot");
        assert_eq!(node.numeric_value, Some(37.0));
        assert_eq!(node.max_numeric_value, Some(100.0));
        let ak = crate::a11y::adapter::to_accesskit(&node);
        assert_eq!(ak.numeric_value(), Some(37.0));
        assert_eq!(ak.min_numeric_value(), Some(0.0));
        assert_eq!(ak.max_numeric_value(), Some(100.0));
        assert_eq!(ak.numeric_value_step(), Some(1.0));
        assert_eq!(ak.numeric_value_jump(), Some(10.0));
    }
    /// P2-7：静态声明的行/列语义必须出现在快照里（虚拟列表 / 表格播报依赖它）。
    #[test]
    fn row_and_column_semantics_come_from_the_static_override() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let row = NodeBuilder::boxed()
            .a11y(crate::a11y::A11yOverride {
                role: Some(crate::a11y::A11yRole::Row),
                row_index: Some(3),
                row_count: Some(100),
                ..crate::a11y::A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("row");
        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let node = super::node_a11y(
            &tree,
            row,
            crate::math::Scale::ONE,
            "",
            &mut ids,
            &mut diagnostics,
        )
        .expect("snapshot");
        assert_eq!(node.row_index, Some(3));
        assert_eq!(node.row_count, Some(100));
        assert_eq!(node.column_index, None, "未声明列语义时不得凭空编造");
        // 快照转 accesskit 后必须真的写到节点上（屏幕阅读器读的是这一层）。
        let ak = crate::a11y::adapter::to_accesskit(&node);
        assert_eq!(ak.row_index(), Some(3));
        assert_eq!(ak.row_count(), Some(100));
    }

    #[test]
    fn collect_subtree_is_dfs_preorder() {
        let mut tree = NodeTree::new();
        let (root, parent, child) = chain(&mut tree);
        let sibling = NodeBuilder::boxed()
            .build(&mut tree, root)
            .expect("sibling");
        let mut out = SmallVec::new();
        collect_subtree(&tree, root, &mut out);
        assert_eq!(out.as_slice(), &[root, parent, child, sibling]);
    }

    #[test]
    fn push_ancestors_walks_to_root() {
        let mut tree = NodeTree::new();
        let (root, parent, child) = chain(&mut tree);
        let mut out = SmallVec::new();
        push_ancestors(&tree, child, &mut out);
        assert_eq!(out.as_slice(), &[parent, root]);
    }

    #[test]
    fn resolve_focus_falls_back_to_root() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        assert_eq!(resolve_focus(&tree, None), (root, None));
        let node = NodeBuilder::boxed().build(&mut tree, root).expect("node");
        assert_eq!(resolve_focus(&tree, Some(node)), (node, None));
    }

    #[test]
    fn resolve_focus_reports_a_removed_target() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let doomed = NodeBuilder::boxed().build(&mut tree, root).expect("node");
        tree.remove(doomed).expect("remove");
        let (focus, missing) = resolve_focus(&tree, Some(doomed));
        assert_eq!(focus, root, "陈旧焦点必须回退到根");
        assert_eq!(missing, Some(doomed), "并记一条 MissingFocusTarget");
    }

    /// A2：表格的 3 列表头 + 5 行 —— 表头角色、行列索引、行列数都必须自动正确。
    #[test]
    fn table_semantics_are_derived_from_structure() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let table = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                label: Some(smol_str::SmolStr::new_static("行情")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("table");

        fn snapshot(tree: &NodeTree, node: NodeId) -> crate::a11y::A11yNode {
            let mut diagnostics = Vec::new();
            let mut ids = A11yIds::new();
            super::node_a11y(tree, node, Scale::ONE, "", &mut ids, &mut diagnostics)
                .expect("snapshot")
        }

        let mut rows = Vec::new();
        let mut cells = Vec::new();
        for index in 0..5 {
            let row = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(A11yRole::Row),
                    ..A11yOverride::default()
                })
                .build(&mut tree, table)
                .expect("row");
            rows.push(row);
            for column in 0..3 {
                let role = if index == 0 {
                    A11yRole::ColumnHeader
                } else {
                    A11yRole::Cell
                };
                let cell = NodeBuilder::boxed()
                    .a11y(A11yOverride {
                        role: Some(role),
                        ..A11yOverride::default()
                    })
                    .build(&mut tree, row)
                    .expect("cell");
                cells.push((cell, index, column));
            }
        }

        // 表容器：行数 / 列数从结构推导（`Grid` 的 `row_count` / `column_count` 是同一路径）。
        let table_snapshot = snapshot(&tree, table);
        assert_eq!(table_snapshot.row_count, Some(5), "行数从 Row 子节点数推导");
        assert_eq!(
            table_snapshot.column_count,
            Some(3),
            "列数从行里的 Cell 数最大值推导"
        );

        for (cell, row_index, column_index) in cells {
            let cell_snapshot = snapshot(&tree, cell);
            assert_eq!(cell_snapshot.row_index, Some(row_index), "行号自动推导");
            assert_eq!(
                cell_snapshot.column_index,
                Some(column_index),
                "列号自动推导（含表头行）"
            );
            assert_eq!(cell_snapshot.row_count, Some(5));
            assert_eq!(cell_snapshot.column_count, Some(3));
        }
        // 行自身的行号同样自动推导。
        assert_eq!(snapshot(&tree, rows[3]).row_index, Some(3));
        // 表头单元格的角色 + 排序状态（`aria-sort`）必须到达 accesskit 层。
        let header = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::ColumnHeader),
                label: Some(smol_str::SmolStr::new_static("价格")),
                sort: Some(A11ySort::Descending),
                ..A11yOverride::default()
            })
            .build(&mut tree, rows[0])
            .expect("header");
        let header_snapshot = snapshot(&tree, header);
        let ak = crate::a11y::adapter::to_accesskit(&header_snapshot);
        assert_eq!(ak.role(), accesskit::Role::ColumnHeader);
        assert_eq!(
            ak.sort_direction(),
            Some(accesskit::SortDirection::Descending)
        );
        // 加了一个表头单元格 ⇒ 列数变成 4（自动推导必须跟着结构走）。
        assert_eq!(snapshot(&tree, table).column_count, Some(4));
    }

    /// A5：文本节点的 `character_lengths` 必须与真实文本一致（CJK / emoji / ZWJ），
    /// 且 `Σ lengths == value.len()`；`TextRun` 的 caret 走 `text_selection`。
    #[test]
    fn text_runs_expose_grapheme_lengths_and_caret() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let input = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::TextInput),
                label: Some(smol_str::SmolStr::new_static("数量")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("input");
        let text = NodeBuilder::text("价🌍ab", TextStyle::new(12.0))
            .build(&mut tree, input)
            .expect("text");
        // caret 放在 emoji 之后（字节 7）：订阅通道是字节偏移，a11y 层换算成 grapheme 下标 2。
        tree.set_a11y_patch(
            text,
            crate::a11y::A11yPatch {
                selection: Some(crate::a11y::A11yTextSelection {
                    anchor: 7,
                    focus: 7,
                }),
                ..crate::a11y::A11yPatch::default()
            },
        );
        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let snapshot = super::node_a11y(&tree, text, Scale::ONE, "", &mut ids, &mut diagnostics)
            .expect("snapshot");
        assert_eq!(snapshot.role, A11yRole::TextRun, "输入框内的文本是 TextRun");
        assert_eq!(
            snapshot.character_lengths,
            vec![3, 4, 1, 1],
            "grapheme 字节长度（CJK 3 / emoji 4）"
        );
        assert_eq!(
            snapshot.text_selection,
            Some(crate::a11y::A11yTextSelection {
                anchor: 2,
                focus: 2
            }),
            "字节偏移 7 → grapheme 下标 2"
        );
        let ak = crate::a11y::adapter::to_accesskit(&snapshot);
        assert_eq!(ak.character_lengths(), &[3, 4, 1, 1]);
        assert_eq!(ak.value(), Some("价🌍ab"));
        let selection = ak.text_selection().expect("text_selection");
        assert_eq!(selection.focus.character_index, 2);
        assert_eq!(
            selection.focus.node,
            accesskit::NodeId(snapshot.id.get()),
            "TextPosition.node 必须是 TextRun 自己"
        );
    }

    /// 空文本：不给字符长度（`Σ == value.len()` 依旧成立），也不 panic。
    #[test]
    fn empty_text_has_no_grapheme_lengths() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let text = NodeBuilder::text("", TextStyle::new(12.0))
            .build(&mut tree, root)
            .expect("text");
        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let snapshot = super::node_a11y(&tree, text, Scale::ONE, "", &mut ids, &mut diagnostics)
            .expect("snapshot");
        assert!(snapshot.character_lengths.is_empty());
        assert!(snapshot.text_selection.is_none());
    }

    /// R13-4：自定义动作必须把 id + 名字注册进 accesskit（`custom_actions()` 非空）。
    #[test]
    fn custom_actions_are_registered_on_the_accesskit_node() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let row = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(smol_str::SmolStr::new_static("BTC/USDT")),
                actions: Some(vec![
                    crate::a11y::A11yAction::Click,
                    crate::a11y::A11yAction::Custom(7),
                ]),
                custom_actions: vec![crate::a11y::A11yCustomAction {
                    id: 7,
                    label: smol_str::SmolStr::new_static("加入自选"),
                }],
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("row");
        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let snapshot = super::node_a11y(&tree, row, Scale::ONE, "", &mut ids, &mut diagnostics)
            .expect("snapshot");
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let ak = crate::a11y::adapter::to_accesskit(&snapshot);
        assert!(ak.supports_action(accesskit::Action::CustomAction));
        assert_eq!(ak.custom_actions().len(), 1);
        assert_eq!(ak.custom_actions()[0].id, 7);
        assert_eq!(ak.custom_actions()[0].description, "加入自选");
    }

    /// R13-4：声明了 `Custom(id)` 却没有注册项 ⇒ 必须留诊断（旧实现完全静默）。
    #[test]
    fn unregistered_custom_actions_are_diagnosed() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(smol_str::SmolStr::new_static("x")),
                actions: Some(vec![crate::a11y::A11yAction::Custom(3)]),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("node");
        let mut diagnostics = Vec::new();
        let mut ids = A11yIds::new();
        let _ = super::node_a11y(&tree, node, Scale::ONE, "", &mut ids, &mut diagnostics)
            .expect("snapshot");
        assert!(
            diagnostics.iter().any(|diagnostic| matches!(
                diagnostic,
                crate::a11y::A11yDiagnostic::UnregisteredCustomAction { id: 3, .. }
            )),
            "{diagnostics:?}"
        );
    }

    /// A1：同一节点跨两笔快照 id 不变；不同节点不冲突（分配器是窗口级状态）。
    #[test]
    fn snapshot_ids_are_stable_across_calls() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let first = NodeBuilder::boxed().build(&mut tree, root).expect("a");
        let second = NodeBuilder::boxed().build(&mut tree, root).expect("b");
        let mut ids = A11yIds::new();
        let mut diagnostics = Vec::new();
        let a = super::node_a11y(&tree, first, Scale::ONE, "", &mut ids, &mut diagnostics)
            .expect("a snapshot");
        let b = super::node_a11y(&tree, second, Scale::ONE, "", &mut ids, &mut diagnostics)
            .expect("b snapshot");
        let a2 = super::node_a11y(&tree, first, Scale::ONE, "", &mut ids, &mut diagnostics)
            .expect("a again");
        assert_eq!(a.id, a2.id, "同一节点跨帧 id 必须不变");
        assert_ne!(a.id, b.id, "不同节点不得共享 id");
        assert_eq!(a.parent, ids.stable_of(root), "父 id 来自同一分配器");
        assert_eq!(ids.sizes().0, 3, "root + first + second 各一个引擎绑定");
    }
}
