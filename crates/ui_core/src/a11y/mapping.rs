//! 节点 → a11y 的映射表与判据。
//!
//! 全部是**纯函数**（只读 `&NodeTree` / `&Node`），没有状态、没有走帧。`accesskit` 的类型只在这里
//! 与 `a11y::adapter` 出现（角色 / 动作的枚举对译）。

use smol_str::SmolStr;

use crate::{
    a11y::{A11yAction, A11yLabelSource, A11yOverride, A11yPatch, A11yRole},
    interaction::focus::focusable,
    layout::hit_test::HitShape,
    math::{PhysicalRect, Scale},
    node::{
        id::NodeId,
        tree::{Node, NodeKind, NodeTree},
    },
};

/// `A11yRole` → `accesskit::Role`（穷举；**没有** `_` 分支）。
pub(crate) const fn role_to_accesskit(role: A11yRole) -> accesskit::Role {
    use A11yRole as R;
    use accesskit::Role;

    match role {
        R::Window => Role::Window,
        R::Application => Role::Application,
        R::GenericContainer => Role::GenericContainer,
        R::Group => Role::Group,
        R::Label => Role::Label,
        R::Paragraph => Role::Paragraph,
        R::Heading => Role::Heading,
        R::Button => Role::Button,
        R::TextInput => Role::TextInput,
        R::MultilineTextInput => Role::MultilineTextInput,
        R::CheckBox => Role::CheckBox,
        R::Switch => Role::Switch,
        R::RadioButton => Role::RadioButton,
        R::Slider => Role::Slider,
        R::ProgressIndicator => Role::ProgressIndicator,
        R::List => Role::List,
        R::ListBoxOption => Role::ListBoxOption,
        R::Table => Role::Table,
        R::Grid => Role::Grid,
        // 树（E9）：三端 adapter 都直接按角色映射（Windows `UIA_TreeControlTypeId` /
        // macOS `NSAccessibilityOutlineRole` + outline row subrole / AT-SPI `tree`/`tree item`），
        // 平台层不需要额外接线。
        R::Tree => Role::Tree,
        R::Row => Role::Row,
        R::TreeItem => Role::TreeItem,
        R::Cell => Role::Cell,
        // 表头（R13-3）：UIA 的 `ITableItemProvider::GetColumnHeaderItems` / AT-SPI 的
        // `getColumnHeader` 由角色推导，缺这两个角色时交易表格的列名永远到不了 AT。
        R::ColumnHeader => Role::ColumnHeader,
        R::RowHeader => Role::RowHeader,
        // 文本运行：`character_lengths` / `word_starts` / `text_selection` 的合法宿主。
        R::TextRun => Role::TextRun,
        R::ScrollView => Role::ScrollView,
        R::ScrollBar => Role::ScrollBar,
        R::Image => Role::Image,
        R::Menu => Role::Menu,
        R::MenuBar => Role::MenuBar,
        R::MenuItem => Role::MenuItem,
        R::Dialog => Role::Dialog,
        // E12：警告对话框 / 标签页 / 分隔符（AccessKit 0.25 同名同义）。
        R::AlertDialog => Role::AlertDialog,
        R::Tooltip => Role::Tooltip,
        R::TabList => Role::TabList,
        R::Tab => Role::Tab,
        // AccessKit 0.25 **没有** `Role::Separator`：ARIA `separator` 的等价角色是 `Splitter`
        // （`accesskit_windows` 映射 `UIA_SeparatorControlTypeId`、`accesskit_atspi_common`
        // 映射 `AtspiRole::Separator`、`accesskit_macos` 映射 `NSAccessibilitySplitterRole`）。
        R::Separator => Role::Splitter,
        // E15：组合控件 / 颜色控件。AccessKit 0.25 有同名角色，三端 adapter 直接按角色映射
        // （见 `A11yRole` 各变体的文档；`ColorWell` 在 UIA/AT-SPI 上退化为按钮语义）。
        R::ComboBox => Role::ComboBox,
        R::ListBox => Role::ListBox,
        // AccessKit 0.25 的拼写是 `MenuItemCheckBox`（大写 B，与本项目既有 `CheckBox` 一致）。
        R::MenuItemCheckBox => Role::MenuItemCheckBox,
        R::MenuItemRadio => Role::MenuItemRadio,
        R::ColorWell => Role::ColorWell,
    }
}

/// `A11yAction` → `accesskit::Action`（穷举）。
pub(crate) const fn action_to_accesskit(action: A11yAction) -> accesskit::Action {
    use A11yAction as A;
    use accesskit::Action;

    match action {
        A::Click => Action::Click,
        A::Focus => Action::Focus,
        A::SetValue => Action::SetValue,
        A::Increment => Action::Increment,
        A::Decrement => Action::Decrement,
        A::SetTextSelection => Action::SetTextSelection,
        A::ScrollUp => Action::ScrollUp,
        A::ScrollDown => Action::ScrollDown,
        A::ScrollLeft => Action::ScrollLeft,
        A::ScrollRight => Action::ScrollRight,
        A::ScrollIntoView => Action::ScrollIntoView,
        A::SetScrollOffset => Action::SetScrollOffset,
        A::Expand => Action::Expand,
        A::Collapse => Action::Collapse,
        A::ShowContextMenu => Action::ShowContextMenu,
        A::Custom(_) => Action::CustomAction,
    }
}

/// `accesskit::Action` → `A11yAction`（`CustomAction` 需要 payload，单独在 `a11y::action` 处理）。
pub(crate) const fn action_from_accesskit(action: accesskit::Action) -> Option<A11yAction> {
    use A11yAction as A;
    use accesskit::Action;

    match action {
        Action::Click => Some(A::Click),
        Action::Focus => Some(A::Focus),
        Action::SetValue => Some(A::SetValue),
        Action::SetTextSelection => Some(A::SetTextSelection),
        Action::Increment => Some(A::Increment),
        Action::Decrement => Some(A::Decrement),
        Action::ScrollUp => Some(A::ScrollUp),
        Action::ScrollDown => Some(A::ScrollDown),
        Action::ScrollLeft => Some(A::ScrollLeft),
        Action::ScrollRight => Some(A::ScrollRight),
        Action::ScrollIntoView => Some(A::ScrollIntoView),
        Action::SetScrollOffset => Some(A::SetScrollOffset),
        Action::Expand => Some(A::Expand),
        Action::Collapse => Some(A::Collapse),
        Action::ShowContextMenu => Some(A::ShowContextMenu),
        // 需要 `ActionData`（id）或本阶段不映射的动作。
        Action::CustomAction
        | Action::Blur
        | Action::ReplaceSelectedText
        | Action::HideTooltip
        | Action::ShowTooltip
        | Action::ScrollToPoint
        | Action::SetSequentialFocusNavigationStartingPoint => None,
    }
}

/// 实时区域播报策略 → accesskit::Live。
pub(crate) const fn live_to_accesskit(live: crate::a11y::A11yLive) -> accesskit::Live {
    match live {
        crate::a11y::A11yLive::Polite => accesskit::Live::Polite,
        crate::a11y::A11yLive::Assertive => accesskit::Live::Assertive,
    }
}

/// 表头排序状态 → `accesskit::SortDirection`（`aria-sort` 的等价物）。
///
/// `aria-sort="none"`（可排序、当前未排序）映射到 `SortDirection::Other`：AccessKit 没有
/// `None` 变体，`Other` 是它表达"未排序"的方式。
pub(crate) const fn sort_to_accesskit(sort: crate::a11y::A11ySort) -> accesskit::SortDirection {
    match sort {
        crate::a11y::A11ySort::Ascending => accesskit::SortDirection::Ascending,
        crate::a11y::A11ySort::Descending => accesskit::SortDirection::Descending,
        crate::a11y::A11ySort::None => accesskit::SortDirection::Other,
    }
}

/// 方向 → `accesskit::Orientation`（E15；滑条 / 色盘等轴向控件用）。
pub(crate) const fn orientation_to_accesskit(
    orientation: crate::a11y::A11yOrientation,
) -> accesskit::Orientation {
    match orientation {
        crate::a11y::A11yOrientation::Horizontal => accesskit::Orientation::Horizontal,
        crate::a11y::A11yOrientation::Vertical => accesskit::Orientation::Vertical,
    }
}

/// 窗口客户区相对、**物理像素**的边界盒（逻辑 × scale，逐边 round；含祖先滚动偏移的屏幕化）。
///
/// 三个平台桥接都要求这个坐标系（逐条核对过上游实现）：
/// - Windows UIA（`accesskit_windows`）用 `ClientToScreen(hwnd)` 自己加上屏幕原点 ⇒ 这里必须是
///   客户区相对、物理像素（PMv2 感知进程里 `ClientToScreen` 返回物理像素）。
/// - macOS NSAccessibility（`accesskit_macos`）除以 `backingScaleFactor` 换算到逻辑点，
///   再经 view/window 转换到屏幕 ⇒ 这里必须是 view 相对、物理像素、y 向下。
/// - Linux AT-SPI（`accesskit_unix` + `atspi_common`）把节点 bounds 原样加 `inner.origin()`
///   （见 `a11y::platform` 文档段）⇒ 节点 bounds 是**物理像素/客户区相对**，屏幕原点是
///   `set_root_window_bounds` 的职责，不在本函数里。
pub(crate) fn bounds_of(tree: &NodeTree, node: NodeId, scale: Scale) -> PhysicalRect {
    let content = tree
        .get(node)
        .map_or(crate::math::Rect::default(), |target| {
            let bounds = target.layout().bounds;
            // R04-8：a11y 几何与命中测试**共用同一份命中形状数据**。形状可以超出布局盒
            // （自绘内容平移/缩放的可见范围），AT 的高亮框/命中区域必须覆盖它，否则
            // "点得到、读不到"（与 `hit_test` 的 `bounds ∪ shape.bounds()` 同源）。
            match target.hit_shape().and_then(HitShape::bounds) {
                Some(local) => bounds.union(crate::math::Rect::from_origin_size(
                    bounds.origin + local.origin,
                    local.size,
                )),
                None => bounds,
            }
        });
    let screen = crate::interaction::scroll::to_screen(tree, node, content);
    screen.to_physical(scale)
}

/// 角色默认动作表。
pub(crate) fn default_actions(role: A11yRole) -> Vec<A11yAction> {
    use A11yAction as A;
    use A11yRole as R;

    match role {
        R::Button => vec![A::Click],
        // 可勾选控件**不**在默认表里声明 `Click`：`ListBoxOption` 这类角色是引擎按节点
        // 种类**自动推导**的（虚拟列表的每一行），无脑声明 Click 会让没有任何点击处理的
        // 行也暴露 Invoke 模式 —— 用户按下去什么都不会发生（复审 P2-4）。
        // 真实能力由 `effective_actions` 统一补：节点声明了 `on_click`/`on_action` 就会得到
        // `Click`，与角色无关。E15 的可勾选菜单项（`MenuItemCheckBox` / `MenuItemRadio`）
        // 与 `MenuItem` 同纪律：勾选态走 `checked`，激活能力由真实接线决定。
        R::CheckBox
        | R::Switch
        | R::RadioButton
        | R::MenuItem
        | R::MenuItemCheckBox
        | R::MenuItemRadio
        | R::ListBoxOption => Vec::new(),
        // 滑块：数值通过 `numeric_value` 通道暴露，写值走 `SetValue`（UIA 的
        // `RangeValuePattern.SetValue` 带 `NumericValue` payload），步进走
        // Increment/Decrement（P2-8 / review P2-5）。
        R::Slider => vec![A::SetValue, A::Increment, A::Decrement, A::Focus],
        // 文本输入声明 `SetValue`（整体替换）。`SetTextSelection` 挂在 **TextRun** 上（E4a）：
        // AccessKit 的 `TextPosition.node` 必须是 `Role::TextRun`，且 `character_index` 是
        // `character_lengths` 的 grapheme 下标；引擎在分发时换算成应用层的 UTF-8 字节偏移。
        // 输入框自身仍可由 `action_allowed` 的固定例外接收 `SetTextSelection`（macOS/AT-SPI
        // 把动作打在输入框上，Windows 打在 TextRun 上 —— 入口统一归一为输入框交给应用）。
        R::TextInput | R::MultilineTextInput => vec![A::SetValue],
        // 文本运行：选区是它唯一的原生动作（选择不改变数据，`read_only` 不剔除）。
        R::TextRun => vec![A::SetTextSelection],
        // E12：`Dialog` / `Menu` **不再**默认宣告 `Expand`/`Collapse`。
        //
        // `aria-expanded` 属于「展开触发器」（combo box / menu button / tree item），不属于
        // 被展开的对话框或菜单本身；旧表让每个浮层包装节点都暴露 Expand/Collapse，AT 会为
        // 任何浮层读出「可展开」的死动作（P2-4b R12）。真实的展开能力由节点声明
        // `on_click`/`on_action` 或显式 `A11yOverride::actions` 给出。
        R::Dialog | R::AlertDialog | R::Menu => Vec::new(),
        R::ScrollView => vec![
            A::ScrollUp,
            A::ScrollDown,
            A::ScrollLeft,
            A::ScrollRight,
            A::ScrollIntoView,
            A::SetScrollOffset,
        ],
        // 表格/网格的 `ScrollIntoView` 由 `effective_actions` 按「是否真的有滚动容器」补
        // （无滚动祖先时该动作必然失败）。
        R::Table | R::Grid => Vec::new(),
        // E15：组合框 / 列表框 / 色块也没有「无接线就成立」的默认动作 —— 展开/收起是真实状态机
        // （`on_click` + `A11yPatch::expanded`），色块打开取色器同样是普通 Action；能力由
        // `effective_actions` 按真实接线补（与 `Dialog` / `Menu` 同纪律，避免死 Expand）。
        R::ComboBox | R::ListBox | R::ColorWell => Vec::new(),
        _ => Vec::new(),
    }
}

/// 由节点种类推导角色（`by_kind` 的判定顺序：特殊种类 → 虚拟列表 → 滚动 → 浮层 → 可交互 → 兜底）。
pub(crate) fn role_by_kind(tree: &NodeTree, node: NodeId, target: &Node) -> A11yRole {
    use A11yRole as R;

    if node == tree.root() {
        return R::Window;
    }
    match target.kind() {
        NodeKind::Canvas(_) | NodeKind::Geometry(_) | NodeKind::Image(_) => return R::Image,
        // 文本节点的角色取决于父节点：文本输入框里的文本是 **`TextRun`**（AccessKit 的
        // `TextPosition.node` 硬性要求 `Role::TextRun` 才承认 `text_selection`），其余场合
        // 是 `Label`（可访问名走 `value`）。
        NodeKind::Text(_) => {
            return if is_text_run_in_input(tree, node) {
                R::TextRun
            } else {
                R::Label
            };
        }
        NodeKind::Box => {}
    }
    // 虚拟列表**先判**：它也带滚动槽，但语义是 `List`（滚动动作由 `effective_actions` 补）。
    if tree.virtual_slot(node).is_some() {
        // E15：集合语义容器（Table/Grid/Tree/ListBox/Menu）内部的**虚拟窗口**不再自动声明
        // `List`（E9 残余 R2）。窗口只是布局实现细节；`GenericContainer` 会被平台的
        // `common_filter` 透明上提，AT 因此看到干净的 `Table → Row` / `Tree → TreeItem` /
        // `ListBox → ListBoxOption` / `Menu → MenuItem`。
        if crate::a11y::tree::is_suppressed_list_window(tree, node) {
            return R::GenericContainer;
        }
        return R::List;
    }
    if tree.scroll_slot(node).is_some() {
        return R::ScrollView;
    }
    // 浮层包装节点**不推导语义角色**（E12）：它只是布局/命中容器，真实语义由应用在创建时
    // 显式声明（`OverlayOptions::a11y`）或落在子树的语义节点上。旧实现把所有浮层包装推导成
    // `Dialog`，连菜单 / tooltip 都被 AT 读成对话框（P2-4b R12）。无声明 ⇒ `GenericContainer`
    // （平台 `common_filter` 会把纯泛型容器透明上提，不会多出一层噪声）。
    //
    // 注意 `hidden` 是另一条通道：包装节点若显式 `hidden = true` 会沿 a11y 子树隐藏**整个
    // 浮层内容**（AccessKit 的 hidden 向下继承）——「透明容器」不等于「隐藏容器」。
    if tree.overlay_slot(node).is_some() {
        return R::GenericContainer;
    }
    if is_virtual_row(tree, node) {
        return R::ListBoxOption;
    }
    if focusable(tree, node)
        || tree
            .interaction(node)
            .is_some_and(|slot| slot.action.is_some())
    {
        return R::Button;
    }
    R::GenericContainer
}

/// 该节点是否是某个虚拟列表容器的**直接**子节点（= 一行）。
fn is_virtual_row(tree: &NodeTree, node: NodeId) -> bool {
    tree.get(node)
        .ok()
        .and_then(crate::node::tree::Node::parent)
        .is_some_and(|parent| tree.virtual_slot(parent).is_some())
}

/// 该文本节点是否位于文本输入框内（⇒ 角色 `TextRun`）。
///
/// 只看**直接父节点**：引擎没有内联文本子树模型，输入框 = 「声明 `TextInput` 角色的盒子 +
/// 一个文本子节点」。父角色用同一套合并规则求（`patch.role` > `over.role` > `by_kind`），
/// 但**不再向下递归**文本分支（父不是文本节点），所以深度恒为 2。
fn is_text_run_in_input(tree: &NodeTree, node: NodeId) -> bool {
    let Some(parent) = tree.get(node).ok().and_then(|target| target.parent()) else {
        return false;
    };
    let Ok(parent_node) = tree.get(parent) else {
        return false;
    };
    let slot = tree.a11y_of(parent);
    let role = slot
        .and_then(|slot| slot.patch.role)
        .or_else(|| slot.and_then(|slot| slot.over.as_ref().and_then(|over| over.role)))
        .unwrap_or_else(|| role_by_kind(tree, parent, parent_node));
    matches!(role, A11yRole::TextInput | A11yRole::MultilineTextInput)
}

/// 合并后的角色（`patch.role` > `override.role` > `by_kind`）。
pub(crate) fn effective_role(
    tree: &NodeTree,
    node: NodeId,
    target: &Node,
    over: Option<&A11yOverride>,
    patch: Option<&A11yPatch>,
) -> A11yRole {
    patch
        .and_then(|patch| patch.role)
        .or_else(|| over.and_then(|over| over.role))
        .unwrap_or_else(|| role_by_kind(tree, node, target))
}

/// 合并后的动作表（`patch.actions` > `override.actions` > `by_role`，再按可聚焦性补 `Focus`）。
///
/// `disabled` 为真时剔除**改变状态**的动作（`Click` / `Focus` / `SetValue` / `Increment` /
/// `Decrement` / `SetTextSelection` / `Expand` / `Collapse`）：a11y 级 `disabled` 与动作能力
/// 必须同源，否则 AT 读到 `IsEnabled == false` 却仍能通过"激活"触发下单（R13-16）。
/// 滚动动作保留 —— 只改视口、不改被禁用控件的数据，且阅读长列表本身不该被禁用阻断。
///
/// `read_only` 是**独立状态**（不折叠进 `disabled`，E3）：只剔除值变更类能力
/// （[`is_value_mutation`]：`SetValue` / `Increment` / `Decrement`），保留 `Focus` / `Click` /
/// `Scroll*` / `SetTextSelection` —— 只读文本仍要能被 AT 选中/朗读（Qt
/// `QAccessible::State::readOnly` 与 UIA `ValuePattern.IsReadOnly` 都独立于 enabled）。
pub(crate) fn effective_actions(
    tree: &NodeTree,
    node: NodeId,
    role: A11yRole,
    over: Option<&A11yOverride>,
    patch: Option<&A11yPatch>,
    disabled: bool,
    read_only: bool,
) -> Vec<A11yAction> {
    let mut actions = patch
        .and_then(|patch| patch.actions.clone())
        .or_else(|| over.and_then(|over| over.actions.clone()))
        .unwrap_or_else(|| default_actions(role));
    // 带滚动槽的节点（含虚拟列表容器）总能接受滚动动作，与角色无关。E15b/F4：集合根在
    // 「自己的虚拟行窗口被平台过滤」时接管窗口的滚动语义 ⇒ 它也必须宣告同一套滚动动作，
    // 否则 AT 读到 `scroll` 属性却没有任何可执行动作。
    let lifted_scroll = crate::a11y::tree::lifted_scroll_window(tree, node);
    if tree.scroll_slot(node).is_some() || lifted_scroll.is_some() {
        for action in default_actions(A11yRole::ScrollView) {
            if !actions.contains(&action) {
                actions.push(action);
            }
        }
    }
    // 表格/网格只有在**真的有滚动容器**（自身滚动槽 / 上提的虚拟窗口 / 滚动祖先）时才提供
    // `ScrollIntoView`：没有滚动容器时该动作必然失败，暴露出去等于给 AT 一个死按钮
    // （复审 P2-4）。
    if matches!(role, A11yRole::Table | A11yRole::Grid)
        && (tree.scroll_slot(node).is_some()
            || lifted_scroll.is_some()
            || crate::interaction::scroll::scroll_ancestor(tree, node).is_some())
        && !actions.contains(&A11yAction::ScrollIntoView)
    {
        actions.push(A11yAction::ScrollIntoView);
    }
    // 能被 AT 激活的节点：**按实际派发语义**判断 —— `focus::action_target` 沿祖先链找
    // `on_click`/`on_action`（指针点击、键盘 Enter、a11y on_click 都走它）。只查本节点会
    // 漏掉「容器委托点击、子节点无 action」的常见结构，AT 因此失去 Invoke（第二轮复审 P2-4）。
    if crate::interaction::focus::action_target(tree, node).is_some()
        && !actions.contains(&A11yAction::Click)
    {
        actions.push(A11yAction::Click);
    }
    let wants_focus = focusable(tree, node);
    if wants_focus && !actions.contains(&A11yAction::Focus) {
        actions.push(A11yAction::Focus);
    }
    if !wants_focus {
        actions.retain(|action| *action != A11yAction::Focus);
    }
    if disabled {
        actions.retain(|action| {
            !matches!(
                action,
                A11yAction::Click
                    | A11yAction::Focus
                    | A11yAction::SetValue
                    | A11yAction::Increment
                    | A11yAction::Decrement
                    | A11yAction::SetTextSelection
                    | A11yAction::Expand
                    | A11yAction::Collapse
            )
        });
    }
    if read_only {
        actions.retain(|action| !is_value_mutation(*action));
    }
    actions
}

/// 该动作是否会**改变节点的数据**（`read_only` 的剔除名单）。
///
/// 只含 `SetValue` / `Increment` / `Decrement`：`SetTextSelection` 不算（只读文本仍可被选择/
/// 朗读，Qt 的只读行编辑也允许移动选区），`Expand` / `Collapse` / `Click` 只改 UI 状态，
/// 滚动动作只改视口。`ReplaceSelectedText` 在 `a11y::action` 已归一为 `SetValue`。
pub(crate) const fn is_value_mutation(action: A11yAction) -> bool {
    matches!(
        action,
        A11yAction::SetValue | A11yAction::Increment | A11yAction::Decrement
    )
}

/// 节点的 a11y「只读」状态（`node_a11y` 与动作分发**共用同一判据**，E3）。
///
/// 合并方向与 `node_a11y` 一致：动态 `patch.read_only` > 静态 `override.read_only` > false
/// （`Some(false)` 必须能覆盖静态 `true`，与 `is_disabled` 的 patch-first 约定相同）。
pub(crate) fn a11y_read_only(tree: &NodeTree, node: NodeId) -> bool {
    let Some(slot) = tree.a11y_of(node) else {
        return false;
    };
    slot.patch
        .read_only
        .or_else(|| slot.over.as_ref().map(|over| over.read_only))
        .unwrap_or(false)
}

/// 该动作是否被节点的**能力集**允许（能力集与动作分发同源，E3 的信任边界）。
///
/// 规则：`effective_actions` 里有该动作 ⇒ 允许；否则只有三个**固定例外**：
/// 1. 滚动动作（`ScrollUp/Down/Left/Right` / `ScrollIntoView` / `SetScrollOffset`）——
///    对列表行/单元格这类子节点发出时，收敛到可解析的滚动容器再判能力（与
///    `a11y::action` 的 `scroll_container` 同一语义）；否则 AT 对行发 `ScrollIntoView`
///    会被误拒；
/// 2. `SetTextSelection` —— 目标是 `TextRun`，或输入框且子树里有 `TextRun`（AccessKit
///    consumer 的 `supports_text_ranges` 同款判据）；
/// 3. `Custom(id)` —— 已登记进 `custom_actions` 注册表的 id（注册表本身就是能力声明，
///    `to_accesskit` 也会因此宣告 `CustomAction` 类别）。
///
/// `Blur` 没有对应的 `A11yAction`（`a11y::action` 在门禁之前处理），不在此列。
pub(crate) fn action_allowed(tree: &NodeTree, node: NodeId, action: A11yAction) -> bool {
    if effective_actions_of(tree, node).contains(&action) {
        return true;
    }
    match action {
        A11yAction::ScrollUp
        | A11yAction::ScrollDown
        | A11yAction::ScrollLeft
        | A11yAction::ScrollRight
        | A11yAction::ScrollIntoView
        | A11yAction::SetScrollOffset => {
            // 与派发共用同一份目标解析（`a11y_scroll_target`）：不能出现「能力判据解析到
            // 窗口、派发却解析到别的容器」的分叉。
            let Some(container) = crate::a11y::tree::a11y_scroll_target(tree, node) else {
                return false;
            };
            effective_actions_of(tree, container).contains(&action)
        }
        A11yAction::SetTextSelection => {
            let role = effective_role_of(tree, node);
            matches!(role, A11yRole::TextRun)
                || (is_text_input_role(role) && subtree_has_text_run(tree, node))
        }
        A11yAction::Custom(id) => custom_action_registered(tree, node, id),
        _ => false,
    }
}

/// 节点当前的完整能力集（`action_allowed` 的判据；结构变化时 O(角色默认表)）。
fn effective_actions_of(tree: &NodeTree, node: NodeId) -> Vec<A11yAction> {
    let Ok(target) = tree.get(node) else {
        return Vec::new();
    };
    let slot = tree.a11y_of(node);
    let over = slot.and_then(|slot| slot.over.as_ref());
    let patch = slot.map(|slot| &slot.patch);
    let role = effective_role(tree, node, target, over, patch);
    effective_actions(
        tree,
        node,
        role,
        over,
        patch,
        a11y_disabled(tree, node),
        a11y_read_only(tree, node),
    )
}

/// 节点的合并角色（`action_allowed` / 文本选区归一 / 动作分发共用；只读，不分配）。
pub(crate) fn effective_role_of(tree: &NodeTree, node: NodeId) -> A11yRole {
    let Ok(target) = tree.get(node) else {
        return A11yRole::GenericContainer;
    };
    let slot = tree.a11y_of(node);
    effective_role(
        tree,
        node,
        target,
        slot.and_then(|slot| slot.over.as_ref()),
        slot.map(|slot| &slot.patch),
    )
}

fn is_text_input_role(role: A11yRole) -> bool {
    matches!(role, A11yRole::TextInput | A11yRole::MultilineTextInput)
}

/// 子树里是否存在 `TextRun`（输入框的字符语义由子文本运行承担）。
fn subtree_has_text_run(tree: &NodeTree, node: NodeId) -> bool {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if effective_role_of(tree, current) == A11yRole::TextRun {
            return true;
        }
        if let Ok(target) = tree.get(current) {
            stack.extend(target.children().iter().copied());
        }
    }
    false
}

/// 登记过的自定义动作（`patch.custom_actions` > `override.custom_actions`，与 `node_a11y` 同序）。
fn custom_action_registered(tree: &NodeTree, node: NodeId, id: i32) -> bool {
    let Some(slot) = tree.a11y_of(node) else {
        return false;
    };
    let registered = match &slot.patch.custom_actions {
        Some(actions) => actions,
        None => slot
            .over
            .as_ref()
            .map_or(&[][..], |over| over.custom_actions.as_slice()),
    };
    registered.iter().any(|custom| custom.id == id)
}

/// 节点的 a11y "禁用"状态（`node_a11y` 与动作分发**共用同一判据**）。
///
/// 两条来源（任一为真即禁用）：
/// 1. 本地判据（`is_disabled`）：`A11yPatch/A11yOverride::disabled` 或 `StylePatch::interactive
///    == Some(false)`；
/// 2. **祖先继承**（C12）：`NodeTree::is_effectively_interactive` —— 任一祖先 `interactive:
///    Some(false)` ⇒ 子树不可交互。命中测试与 `focus::focusable` 已经是这个语义；a11y 若只看
///    本地，就会出现"指针/键盘点不动、AT 却宣告可点击"的死按钮（R13-16 的同一类不一致）。
///
/// 动作分发（`a11y::action::dispatch`）必须在执行任何 handler / 内置映射之前用它挡一道。
pub(crate) fn a11y_disabled(tree: &NodeTree, node: NodeId) -> bool {
    let Ok(target) = tree.get(node) else {
        return false;
    };
    if !tree.is_effectively_interactive(node) {
        return true;
    }
    let slot = tree.a11y_of(node);
    let over = slot.and_then(|slot| slot.over.as_ref());
    let patch = slot.map(|slot| &slot.patch);
    is_disabled(target, over, patch)
}

/// 合并后的可访问名 + 写入通道。
///
/// 文本节点（`NodeKind::Text`）的名字来自文本内容——除非角色被强制成非 `Label`，那时按
/// `Override` 通道写 `set_label`（否则 AT 读不到名字）。
pub(crate) fn effective_label(
    target: &Node,
    role: A11yRole,
    over: Option<&A11yOverride>,
    patch: Option<&A11yPatch>,
) -> Option<crate::a11y::A11yLabel> {
    let explicit = patch
        .and_then(|patch| patch.label.clone())
        .or_else(|| over.and_then(|over| over.label.clone()));
    if let Some(text) = explicit {
        return Some(crate::a11y::A11yLabel::new(text, A11yLabelSource::Override));
    }
    match target.kind() {
        // `TextRun` 的文本走 `value`（AccessKit 的 `character_lengths` 以 `value` 为基准）；
        // 再写一份 `label` 会让 AT 把同一段文字读两遍。
        NodeKind::Text(content) if role == A11yRole::Label && !content.text.is_empty() => Some(
            crate::a11y::A11yLabel::new(content.text.clone(), A11yLabelSource::TextContent),
        ),
        NodeKind::Text(content)
            if role != A11yRole::Label && role != A11yRole::TextRun && !content.text.is_empty() =>
        {
            Some(crate::a11y::A11yLabel::new(
                content.text.clone(),
                A11yLabelSource::Override,
            ))
        }
        _ => None,
    }
}

/// 合并后的值。
pub(crate) fn effective_value(
    target: &Node,
    role: A11yRole,
    over: Option<&A11yOverride>,
    patch: Option<&A11yPatch>,
) -> Option<SmolStr> {
    patch
        .and_then(|patch| patch.value.clone())
        .or_else(|| over.and_then(|over| over.value.clone()))
        .or_else(|| match target.kind() {
            // `Role::Label` 的可访问名走 value（`accesskit_consumer` 的 `label_comes_from_value`）。
            // `Role::TextRun` 的文本也走 value —— `character_lengths` 的求和基准就是它。
            NodeKind::Text(content)
                if matches!(role, A11yRole::Label | A11yRole::TextRun)
                    && !content.text.is_empty() =>
            {
                Some(content.text.clone())
            }
            _ => None,
        })
}

/// 合并后的说明。
pub(crate) fn effective_description(
    over: Option<&A11yOverride>,
    patch: Option<&A11yPatch>,
) -> Option<SmolStr> {
    patch
        .and_then(|patch| patch.description.clone())
        .or_else(|| over.and_then(|over| over.description.clone()))
}

/// 隐藏判定：`patch.hidden` > `over.hidden` > `StylePatch::visible` > `display:none` > 自绘缺替代文本。
pub(crate) fn is_hidden(
    target: &Node,
    role: A11yRole,
    label: Option<&crate::a11y::A11yLabel>,
    over: Option<&A11yOverride>,
    patch: Option<&A11yPatch>,
) -> bool {
    // `patch.hidden = Some(false)` 必须能覆盖静态声明（与 `is_disabled` 的 patch-first
    // 约定一致；静态 `hidden` / `visible = false` / `display: none` 仍各自生效）。
    if let Some(hidden) = patch.and_then(|patch| patch.hidden) {
        if hidden {
            return true;
        }
    } else if over.is_some_and(|over| over.hidden) {
        return true;
    }
    if target.style_patch().visible == Some(false) {
        return true;
    }
    if matches!(target.style().inner.display, taffy::style::Display::None) {
        return true;
    }
    // 自绘 / 图片节点必须有替代文本，否则 hidden（§7.4 / §5.4）。E7：图片节点的可访问名只能
    // 来自 `A11yOverride::label`（`ImageContent` 不承载 alt），没有则视为装饰图。注意这是
    // **兜底** hidden：显式 `A11yOverride { hidden: true }` 才是装饰声明（免 `MissingAltText`，
    // 见 `a11y::tree` 的诊断判定），两者对 `hidden` 的结论相同、对诊断的结论不同。
    matches!(
        target.kind(),
        NodeKind::Canvas(_) | NodeKind::Geometry(_) | NodeKind::Image(_)
    ) && label.is_none()
        && role == A11yRole::Image
}

/// 图片节点的**显式装饰声明**（P2-1b3 裁决）：`A11yOverride { hidden: true }`。
///
/// 与 [`is_hidden`] 的兜底区分：没有 label 的图片本来就会被兜底标 `hidden`，但只有**显式**
/// hidden 才是「这是装饰图、不需要可访问名」的语义声明 —— 免 `MissingAltText` 诊断；兜底
/// hidden 的图片仍记诊断（应用应给 label 或显式声明装饰）。`Canvas` / `Geometry` 不适用
/// （它们的替代文本是必需语义，hidden 不是装饰声明）。
pub(crate) fn is_decorative_image(target: &Node, over: Option<&A11yOverride>) -> bool {
    over.is_some_and(|over| over.hidden) && matches!(target.kind(), NodeKind::Image(_))
}

/// 禁用判定：动态/静态声明，或 `interactive == Some(false)`（与 `focusable` 同源）。
pub(crate) fn is_disabled(
    target: &Node,
    over: Option<&A11yOverride>,
    patch: Option<&A11yPatch>,
) -> bool {
    patch.and_then(|patch| patch.disabled).unwrap_or_else(|| {
        over.is_some_and(|over| over.disabled) || target.style_patch().interactive == Some(false)
    })
}

#[cfg(test)]
mod tests {
    use super::{
        a11y_disabled, a11y_read_only, action_allowed, action_from_accesskit, action_to_accesskit,
        bounds_of, default_actions, effective_actions, is_disabled, is_hidden, is_value_mutation,
        orientation_to_accesskit, role_by_kind, role_to_accesskit, sort_to_accesskit,
    };
    use crate::{
        a11y::{
            A11yAction, A11yLabel, A11yLabelSource, A11yOrientation, A11yOverride, A11yPatch,
            A11yRole, A11ySort,
        },
        layout::hit_test::HitShape,
        math::{PhysicalRect, Scale},
        node::{builder::NodeBuilder, tree::NodeTree},
        paint::{
            canvas::Canvas,
            style::{Clip, Style, StylePatch},
        },
        text::{TextContent, TextStyle},
    };

    /// 角色映射：**每个** `A11yRole` 变体必须恰好出现一次，且映射到互不相同的
    /// `accesskit::Role`。
    ///
    /// 旧实现手写 27 项数组、而枚举有 29 个变体（漏了 `Row`/`Cell` —— 恰好是表格语义最关键
    /// 的两个），却因为 `unique.len() == roles.len()` 而永远绿（R13-3/§7.2 自查）。
    /// 现在两道防线：
    /// 1. `count_role` 是**穷举 `match`**：加变体不补分支 ⇒ 编译失败；
    /// 2. `A11yRole::ALL` 漏列变体 ⇒ 求和 != `ALL.len()` ⇒ 测试红。
    #[test]
    fn every_role_is_included_exactly_once_in_all() {
        /// 穷举计数：每个变体返回 1。`match` 没有 `_` 分支，所以漏一个变体编译失败。
        fn count_role(role: A11yRole) -> usize {
            match role {
                A11yRole::Window
                | A11yRole::Application
                | A11yRole::GenericContainer
                | A11yRole::Group
                | A11yRole::Label
                | A11yRole::Paragraph
                | A11yRole::Heading
                | A11yRole::Button
                | A11yRole::TextInput
                | A11yRole::MultilineTextInput
                | A11yRole::CheckBox
                | A11yRole::Switch
                | A11yRole::RadioButton
                | A11yRole::Slider
                | A11yRole::ProgressIndicator
                | A11yRole::List
                | A11yRole::ListBoxOption
                | A11yRole::Table
                | A11yRole::Grid
                | A11yRole::Tree
                | A11yRole::Row
                | A11yRole::TreeItem
                | A11yRole::Cell
                | A11yRole::ColumnHeader
                | A11yRole::RowHeader
                | A11yRole::TextRun
                | A11yRole::ScrollView
                | A11yRole::ScrollBar
                | A11yRole::Image
                | A11yRole::Menu
                | A11yRole::MenuBar
                | A11yRole::MenuItem
                | A11yRole::Dialog
                | A11yRole::AlertDialog
                | A11yRole::Tooltip
                | A11yRole::TabList
                | A11yRole::Tab
                | A11yRole::Separator
                // E15：组合控件 / 颜色控件。
                | A11yRole::ComboBox
                | A11yRole::ListBox
                | A11yRole::MenuItemCheckBox
                | A11yRole::MenuItemRadio
                | A11yRole::ColorWell => 1,
            }
        }

        let counted: usize = A11yRole::ALL.iter().copied().map(count_role).sum();
        assert_eq!(
            counted,
            A11yRole::ALL.len(),
            "A11yRole::ALL 必须覆盖每个变体恰好一次（漏列 ⇒ 求和偏小 ⇒ 恒红）"
        );

        let mut mapped = Vec::with_capacity(A11yRole::ALL.len());
        for role in A11yRole::ALL {
            mapped.push(role_to_accesskit(role));
        }
        let unique: std::collections::BTreeSet<_> = mapped.iter().collect();
        assert_eq!(
            unique.len(),
            mapped.len(),
            "两个 A11yRole 不得映射到同一个 accesskit::Role"
        );
        assert_eq!(role_to_accesskit(A11yRole::Image), accesskit::Role::Image);
        assert_eq!(
            role_to_accesskit(A11yRole::ScrollView),
            accesskit::Role::ScrollView,
            "不是 ScrollArea（不存在）"
        );
        // 表格语义（R13-3）：这三个角色是交易表格读屏路径的核心。
        assert_eq!(
            role_to_accesskit(A11yRole::ColumnHeader),
            accesskit::Role::ColumnHeader
        );
        assert_eq!(
            role_to_accesskit(A11yRole::RowHeader),
            accesskit::Role::RowHeader
        );
        assert_eq!(
            role_to_accesskit(A11yRole::TextRun),
            accesskit::Role::TextRun
        );
        // 树（E9）：`Tree` / `TreeItem` 与 AccessKit 0.25 同名同义；三端 adapter 都按角色
        // 直接映射（Windows UIA tree/tree item、macOS outline/outline row、AT-SPI tree/tree item）。
        assert_eq!(role_to_accesskit(A11yRole::Tree), accesskit::Role::Tree);
        assert_eq!(
            role_to_accesskit(A11yRole::TreeItem),
            accesskit::Role::TreeItem
        );
        // `ALL` 必须真的收录树角色（漏列 ⇒ 上面的求和断言红，这里给出更直接的失败面）。
        assert!(
            A11yRole::ALL.contains(&A11yRole::Tree),
            "A11yRole::ALL 必须包含 Tree"
        );
        assert!(
            A11yRole::ALL.contains(&A11yRole::TreeItem),
            "A11yRole::ALL 必须包含 TreeItem"
        );
        // E12 浮层 / 标签页角色：AccessKit 0.25 同名映射（`Separator` 对应 0.25 的
        // `Splitter` —— 见 `role_to_accesskit` 的注释）。
        assert_eq!(
            role_to_accesskit(A11yRole::AlertDialog),
            accesskit::Role::AlertDialog
        );
        assert_eq!(
            role_to_accesskit(A11yRole::TabList),
            accesskit::Role::TabList
        );
        assert_eq!(role_to_accesskit(A11yRole::Tab), accesskit::Role::Tab);
        assert_eq!(
            role_to_accesskit(A11yRole::Separator),
            accesskit::Role::Splitter,
            "ARIA separator → AccessKit 0.25 Splitter（UIA Separator/AT-SPI separator）"
        );
        for role in [
            A11yRole::AlertDialog,
            A11yRole::TabList,
            A11yRole::Tab,
            A11yRole::Separator,
        ] {
            assert!(
                A11yRole::ALL.contains(&role),
                "A11yRole::ALL 必须包含 E12 新增角色 {role:?}"
            );
        }
        // E15：组合控件 / 颜色控件 —— AccessKit 0.25 同名映射（`MenuItemCheckBox` 是 0.25 的
        // 精确拼写；`ColorWell` 在 UIA/AT-SPI 上退化为按钮语义，见 `A11yRole::ColorWell` 文档）。
        assert_eq!(
            role_to_accesskit(A11yRole::ComboBox),
            accesskit::Role::ComboBox,
            "下拉/选择器触发器（UIA ComboBox / macOS PopUpButton / AT-SPI comboBox）"
        );
        assert_eq!(
            role_to_accesskit(A11yRole::ListBox),
            accesskit::Role::ListBox,
            "列表框容器（UIA List / macOS List / AT-SPI list box）"
        );
        assert_eq!(
            role_to_accesskit(A11yRole::MenuItemCheckBox),
            accesskit::Role::MenuItemCheckBox,
            "可勾选菜单项（UIA CheckBox ControlType + menuitemcheckbox / AT-SPI check menu item）"
        );
        assert_eq!(
            role_to_accesskit(A11yRole::MenuItemRadio),
            accesskit::Role::MenuItemRadio,
            "单选菜单项（UIA RadioButton ControlType + menuitemradio / AT-SPI radio menu item）"
        );
        assert_eq!(
            role_to_accesskit(A11yRole::ColorWell),
            accesskit::Role::ColorWell,
            "色块（macOS 有专用 ColorWell；UIA/AT-SPI 退化为按钮 + 值）"
        );
        for role in [
            A11yRole::ComboBox,
            A11yRole::ListBox,
            A11yRole::MenuItemCheckBox,
            A11yRole::MenuItemRadio,
            A11yRole::ColorWell,
        ] {
            assert!(
                A11yRole::ALL.contains(&role),
                "A11yRole::ALL 必须包含 E15 新增角色 {role:?}"
            );
        }
    }

    /// `aria-sort` 的三个状态都必须有映射（UIA/AT-SPI 靠它播报"按价格升序"）。
    #[test]
    fn sort_directions_map_to_accesskit() {
        assert_eq!(
            sort_to_accesskit(A11ySort::Ascending),
            accesskit::SortDirection::Ascending
        );
        assert_eq!(
            sort_to_accesskit(A11ySort::Descending),
            accesskit::SortDirection::Descending
        );
        assert_eq!(
            sort_to_accesskit(A11ySort::None),
            accesskit::SortDirection::Other,
            "AccessKit 没有 None 变体：未排序 = Other"
        );
    }

    /// E15：方向的两个取值都必须有映射（滑条 / 色盘的轴向语义；三端读同一属性）。
    #[test]
    fn orientations_map_to_accesskit() {
        assert_eq!(
            orientation_to_accesskit(A11yOrientation::Horizontal),
            accesskit::Orientation::Horizontal
        );
        assert_eq!(
            orientation_to_accesskit(A11yOrientation::Vertical),
            accesskit::Orientation::Vertical
        );
    }

    #[test]
    fn actions_round_trip_through_accesskit() {
        for action in [
            A11yAction::Click,
            A11yAction::Focus,
            A11yAction::SetValue,
            A11yAction::SetTextSelection,
            A11yAction::ScrollUp,
            A11yAction::ScrollDown,
            A11yAction::ScrollLeft,
            A11yAction::ScrollRight,
            A11yAction::ScrollIntoView,
            A11yAction::SetScrollOffset,
            A11yAction::Expand,
            A11yAction::Collapse,
            A11yAction::ShowContextMenu,
        ] {
            assert_eq!(
                action_from_accesskit(action_to_accesskit(action)),
                Some(action)
            );
        }
        assert_eq!(
            action_to_accesskit(A11yAction::Custom(7)),
            accesskit::Action::CustomAction
        );
        assert_eq!(
            action_from_accesskit(accesskit::Action::CustomAction),
            None,
            "CustomAction 需要 payload，由 action.rs 单独处理"
        );
    }

    #[test]
    fn default_actions_follow_the_role_table() {
        assert_eq!(default_actions(A11yRole::Button), vec![A11yAction::Click]);
        assert!(default_actions(A11yRole::TextInput).contains(&A11yAction::SetValue));
        assert!(default_actions(A11yRole::ScrollView).contains(&A11yAction::ScrollIntoView));
        // E12：对话框 / 菜单 / 警告对话框**不再**默认宣告 Expand/Collapse —— `aria-expanded`
        // 属于展开触发器，不属于被展开的容器本身（P2-4b R12）。真实能力由节点声明补。
        for role in [
            A11yRole::Dialog,
            A11yRole::AlertDialog,
            A11yRole::Menu,
            A11yRole::TabList,
            A11yRole::Tab,
            A11yRole::Separator,
        ] {
            assert!(
                default_actions(role).is_empty(),
                "{role:?} 的默认动作表必须为空（不暴露死 Expand/Collapse）"
            );
        }
        assert!(default_actions(A11yRole::Label).is_empty());
        // E15：组合框 / 列表框 / 色块 / 可勾选菜单项也没有「无接线就成立」的默认动作（展开态是
        // 真实状态机、激活能力由 on_click 接线决定；勾选态走 checked 通道）—— 避免死 Expand /
        // 死按钮（与 Dialog / Menu 同纪律）。
        for role in [
            A11yRole::ComboBox,
            A11yRole::ListBox,
            A11yRole::ColorWell,
            A11yRole::MenuItemCheckBox,
            A11yRole::MenuItemRadio,
        ] {
            assert!(
                default_actions(role).is_empty(),
                "{role:?} 的默认动作表必须为空（能力由 effective_actions 按真实接线补）"
            );
        }
    }

    /// P2-4：可勾选控件的 `Click` 必须由**真实能力**决定（节点声明了 on_click 才有），
    /// 而不是角色默认表无条件给出 —— 否则自动推导出的虚拟列表行会暴露死按钮。
    #[test]
    fn click_comes_from_capability_not_role() {
        // 默认表不再给可勾选角色声明 Click。
        for role in [
            A11yRole::CheckBox,
            A11yRole::Switch,
            A11yRole::RadioButton,
            A11yRole::MenuItem,
            A11yRole::MenuItemCheckBox,
            A11yRole::MenuItemRadio,
            A11yRole::ListBoxOption,
        ] {
            let actions = default_actions(role);
            assert!(
                !actions.contains(&A11yAction::Click),
                "{role:?} 的默认表不该声明 Click（能力由 effective_actions 补）：{actions:?}"
            );
        }

        // 没有交互声明的行：没有任何动作（不暴露死 Click）。
        let mut tree = NodeTree::new();
        let root = tree.root();
        let plain = NodeBuilder::boxed().build(&mut tree, root).expect("mount");
        let actions = effective_actions(
            &tree,
            plain,
            A11yRole::ListBoxOption,
            None,
            None,
            false,
            false,
        );
        assert!(
            !actions.contains(&A11yAction::Click),
            "无 on_click 的列表项不得暴露 Click：{actions:?}"
        );

        // 声明了 on_click 的节点：Click 由能力规则补上。
        let clickable = NodeBuilder::boxed()
            .on_click(crate::event::Action::custom("pick"))
            .build(&mut tree, root)
            .expect("mount");
        let actions = effective_actions(
            &tree,
            clickable,
            A11yRole::ListBoxOption,
            None,
            None,
            false,
            false,
        );
        assert!(
            actions.contains(&A11yAction::Click),
            "有 on_click 的列表项必须暴露 Click：{actions:?}"
        );

        // 滑块的数值动作不受影响。
        let slider = default_actions(A11yRole::Slider);
        assert!(slider.contains(&A11yAction::SetValue));
        assert!(slider.contains(&A11yAction::Increment));
    }

    #[test]
    fn hidden_follows_visible_and_display() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed().build(&mut tree, root).expect("mount");
        let label = A11yLabel::new("x".into(), A11yLabelSource::Override);
        let target = tree.get(node).expect("exists");

        assert!(!is_hidden(
            target,
            A11yRole::Button,
            Some(&label),
            None,
            None
        ));

        tree.set_style_patch_for_test(
            node,
            StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            },
        );
        assert!(is_hidden(
            tree.get(node).expect("exists"),
            A11yRole::Button,
            Some(&label),
            None,
            None
        ));

        tree.set_style_patch_for_test(node, StylePatch::default());
        assert!(is_hidden(
            tree.get(node).expect("exists"),
            A11yRole::Button,
            Some(&label),
            None,
            Some(&A11yPatch {
                hidden: Some(true),
                ..A11yPatch::default()
            })
        ));
    }

    #[test]
    fn canvas_without_alt_text_is_hidden() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let canvas = NodeBuilder::canvas(Canvas::new(crate::math::Rect::new(0.0, 0.0, 4.0, 4.0)))
            .build(&mut tree, root)
            .expect("mount");
        let target = tree.get(canvas).expect("exists");
        assert!(
            is_hidden(target, A11yRole::Image, None, None, None),
            "自绘节点缺替代文本必须 hidden（不静默丢弃）"
        );
        let label = A11yLabel::new("K 线".into(), A11yLabelSource::Override);
        assert!(!is_hidden(
            target,
            A11yRole::Image,
            Some(&label),
            None,
            None
        ));
    }

    #[test]
    fn disabled_follows_interactive() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed().build(&mut tree, root).expect("mount");
        tree.set_style_patch_for_test(
            node,
            StylePatch {
                interactive: Some(false),
                ..StylePatch::default()
            },
        );
        assert!(is_disabled(tree.get(node).expect("exists"), None, None));
        assert!(!is_hidden(
            tree.get(node).expect("exists"),
            A11yRole::Button,
            None,
            None,
            None
        ));
        assert!(is_disabled(
            tree.get(node).expect("exists"),
            Some(&A11yOverride {
                disabled: true,
                ..A11yOverride::default()
            }),
            None
        ));
        let _ = (Style::default(), Clip::None);
    }

    /// R13-16：a11y 级 `disabled` 必须与**动作能力**同源。
    ///
    /// 失败面：`bind_a11y(disabled: Some(true))` 宣告"不可用"后，节点仍然宣告 `Click`/`Focus`，
    /// AT 的"激活"会真的执行 `on_click`（下单）。
    #[test]
    fn disabled_nodes_do_not_advertise_state_changing_actions() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let button = NodeBuilder::boxed()
            .on_click(crate::event::Action::custom("buy"))
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(smol_str::SmolStr::new_static("买入")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("mount");

        // 启用态：Click + Focus 都在。
        let enabled = effective_actions(&tree, button, A11yRole::Button, None, None, false, false);
        assert!(enabled.contains(&A11yAction::Click));
        assert!(enabled.contains(&A11yAction::Focus));

        // 动态 `disabled` 补丁（最自然的写法）⇒ 能力必须同步消失。
        tree.set_a11y_patch(
            button,
            A11yPatch {
                disabled: Some(true),
                ..A11yPatch::default()
            },
        );
        assert!(
            a11y_disabled(&tree, button),
            "动作分发与 node_a11y 必须共用同一判据"
        );
        let slot = tree.a11y_of(button).expect("a11y slot");
        let disabled = effective_actions(
            &tree,
            button,
            A11yRole::Button,
            slot.over.as_ref(),
            Some(&slot.patch),
            a11y_disabled(&tree, button),
            false,
        );
        assert!(
            !disabled.contains(&A11yAction::Click),
            "disabled 节点不得宣告 Click：{disabled:?}"
        );
        assert!(
            !disabled.contains(&A11yAction::Focus),
            "disabled 节点不得宣告 Focus：{disabled:?}"
        );

        // 滚动动作保留（只改视口，不改被禁用控件的数据）。
        let view = effective_actions(&tree, button, A11yRole::ScrollView, None, None, true, false);
        assert!(view.contains(&A11yAction::ScrollDown), "{view:?}");
    }

    /// C12 一致化：祖先 `interactive: false` ⇒ 整个子树在 a11y 里也是 disabled（与命中/焦点同一
    /// 语义），后代不得再宣告 `Click`/`Focus`，否则 AT 会看到一个"点不动的可点击按钮"。
    #[test]
    fn a_disabled_ancestor_disables_the_subtree_for_a11y() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let panel = NodeBuilder::boxed().build(&mut tree, root).expect("panel");
        let child = NodeBuilder::boxed()
            .on_click(crate::event::Action::custom("buy"))
            .a11y(A11yOverride {
                role: Some(A11yRole::Button),
                label: Some(smol_str::SmolStr::new_static("买入")),
                ..A11yOverride::default()
            })
            .build(&mut tree, panel)
            .expect("child");
        tree.set_style_patch_for_test(
            panel,
            StylePatch {
                interactive: Some(false),
                ..StylePatch::default()
            },
        );
        assert!(
            a11y_disabled(&tree, child),
            "祖先禁用必须继承到 a11y disabled"
        );
        assert!(
            !crate::interaction::focus::focusable(&tree, child),
            "与焦点系统同源"
        );
        let actions = effective_actions(
            &tree,
            child,
            A11yRole::Button,
            None,
            None,
            a11y_disabled(&tree, child),
            false,
        );
        assert!(
            !actions.contains(&A11yAction::Click) && !actions.contains(&A11yAction::Focus),
            "继承 disabled 的后代不得宣告动作：{actions:?}"
        );
    }

    /// 文本输入框里的文本节点自动成为 `TextRun`（`text_selection` 的合法宿主），
    /// 其它文本节点仍是 `Label`。
    #[test]
    fn text_inside_an_input_becomes_a_text_run() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let input = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::TextInput),
                label: Some(smol_str::SmolStr::new_static("价格")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("input");
        let inner = NodeBuilder::text("42150", TextStyle::new(12.0))
            .build(&mut tree, input)
            .expect("text");
        let label = NodeBuilder::text("最新价", TextStyle::new(12.0))
            .build(&mut tree, root)
            .expect("label");
        assert_eq!(
            role_by_kind(&tree, inner, tree.get(inner).expect("inner")),
            A11yRole::TextRun,
            "输入框内的文本必须是 TextRun"
        );
        assert_eq!(
            role_by_kind(&tree, label, tree.get(label).expect("label")),
            A11yRole::Label,
            "普通文本仍是 Label"
        );
        let _ = TextContent::new("x", TextStyle::new(12.0));
    }

    /// R04-8：a11y 几何与命中测试**共用同一份 HitShape 数据** —— 形状超出布局盒时，
    /// AT 的 bounds（屏幕高亮框 / 命中区域）必须覆盖形状而不是只覆盖布局盒。
    #[test]
    fn a11y_bounds_follow_the_hit_shape() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(10.0, 20.0, 40.0, 30.0))
            .hit_shape(HitShape::Rect(crate::math::Rect::new(
                -5.0, -5.0, 60.0, 50.0,
            )))
            .build(&mut tree, root)
            .expect("mount");
        tree.compute_layout(taffy::geometry::Size {
            width: Some(200.0),
            height: Some(200.0),
        })
        .expect("layout");
        let scale = Scale::new(1.0).expect("scale");

        assert_eq!(
            bounds_of(&tree, node, scale),
            PhysicalRect::new(5, 15, 65, 65),
            "a11y bounds = 布局盒 (10,20,40,30) ∪ 形状局部 AABB (-5,-5,60,50)"
        );

        // 没有命中形状时行为不变（仅布局盒）。
        let plain = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(10.0, 20.0, 40.0, 30.0))
            .build(&mut tree, root)
            .expect("mount");
        tree.compute_layout(taffy::geometry::Size {
            width: Some(200.0),
            height: Some(200.0),
        })
        .expect("layout");
        assert_eq!(
            bounds_of(&tree, plain, scale),
            PhysicalRect::new(10, 20, 50, 50),
            "未设置 HitShape ⇒ 仍用布局盒（旧行为）"
        );
    }

    /// E3：`read_only` 只剔除值变更能力（`SetValue`/`Increment`/`Decrement`），保留
    /// `Focus`/`Click`/`SetTextSelection`；`disabled` 的剔除面更大（证明两者不是同一状态）。
    #[test]
    fn read_only_removes_only_value_mutations() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::TextInput),
                label: Some(smol_str::SmolStr::new_static("搜索")),
                actions: Some(vec![
                    A11yAction::SetValue,
                    A11yAction::Increment,
                    A11yAction::Decrement,
                    A11yAction::SetTextSelection,
                    A11yAction::Click,
                ]),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("input");

        let over = tree.a11y_of(node).and_then(|slot| slot.over.as_ref());
        let patch = tree.a11y_of(node).map(|slot| &slot.patch);
        let writable =
            effective_actions(&tree, node, A11yRole::TextInput, over, patch, false, false);
        for action in [
            A11yAction::SetValue,
            A11yAction::Increment,
            A11yAction::Decrement,
            A11yAction::SetTextSelection,
            A11yAction::Click,
        ] {
            assert!(writable.contains(&action), "可写节点必须保留 {action:?}");
        }

        let read_only =
            effective_actions(&tree, node, A11yRole::TextInput, over, patch, false, true);
        for action in [
            A11yAction::SetValue,
            A11yAction::Increment,
            A11yAction::Decrement,
        ] {
            assert!(
                !read_only.contains(&action),
                "只读节点不得宣告值变更动作 {action:?}"
            );
        }
        assert!(read_only.contains(&A11yAction::SetTextSelection));
        assert!(read_only.contains(&A11yAction::Click));

        let disabled =
            effective_actions(&tree, node, A11yRole::TextInput, over, patch, true, false);
        assert!(
            !disabled.contains(&A11yAction::SetTextSelection)
                && !disabled.contains(&A11yAction::Click),
            "disabled 必须比 read_only 剔除更多（两者语义不同）"
        );
    }

    /// E3：`read_only` 的合并方向与 `node_a11y` 一致（patch 优先，`Some(false)` 能解静态只读）。
    #[test]
    fn a11y_read_only_follows_the_patch_first_merge() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .a11y(A11yOverride {
                read_only: true,
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("mount");
        assert!(a11y_read_only(&tree, node), "静态 read_only 必须生效");
        tree.set_a11y_patch(
            node,
            A11yPatch {
                read_only: Some(false),
                ..A11yPatch::default()
            },
        );
        assert!(
            !a11y_read_only(&tree, node),
            "动态补丁 Some(false) 必须能解掉静态只读（patch-first）"
        );
    }

    /// E3：`is_value_mutation` 的名单固定（`SetTextSelection` 不属于值变更）。
    #[test]
    fn is_value_mutation_covers_exactly_the_data_actions() {
        assert!(is_value_mutation(A11yAction::SetValue));
        assert!(is_value_mutation(A11yAction::Increment));
        assert!(is_value_mutation(A11yAction::Decrement));
        for action in [
            A11yAction::Click,
            A11yAction::Focus,
            A11yAction::SetTextSelection,
            A11yAction::ScrollUp,
            A11yAction::ScrollDown,
            A11yAction::ScrollLeft,
            A11yAction::ScrollRight,
            A11yAction::ScrollIntoView,
            A11yAction::SetScrollOffset,
            A11yAction::Expand,
            A11yAction::Collapse,
            A11yAction::ShowContextMenu,
            A11yAction::Custom(1),
        ] {
            assert!(
                !is_value_mutation(action),
                "{action:?} 不属于值变更（read_only 必须保留它）"
            );
        }
    }

    /// E3：`action_allowed` 的固定例外 —— `SetTextSelection`（TextRun / 含 TextRun 的输入框）
    /// 与已登记的 `Custom(id)`；其余未宣告动作一律拒绝。
    #[test]
    fn action_allowed_honours_the_fixed_exceptions() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let input = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::TextInput),
                label: Some(smol_str::SmolStr::new_static("搜索")),
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("input");
        assert!(
            !action_allowed(&tree, input, A11yAction::SetTextSelection),
            "没有 TextRun 子节点的输入框不默认宣告 SetTextSelection"
        );
        // 输入框内的文本自动推导为 TextRun（`role_by_kind`）⇒ 选区能力成立。
        let run = NodeBuilder::text("42150", TextStyle::new(12.0))
            .build(&mut tree, input)
            .expect("text run");
        assert!(
            action_allowed(&tree, input, A11yAction::SetTextSelection),
            "含 TextRun 的输入框必须允许 SetTextSelection（consumer 同款判据）"
        );
        assert!(
            action_allowed(&tree, run, A11yAction::SetTextSelection),
            "TextRun 自身必须允许 SetTextSelection"
        );
        assert!(
            !action_allowed(&tree, input, A11yAction::Increment),
            "未声明/角色默认表不含的动作必须被拒"
        );

        // Custom：注册表本身就是能力声明。
        let custom = NodeBuilder::boxed()
            .a11y(A11yOverride {
                label: Some(smol_str::SmolStr::new_static("自选")),
                custom_actions: vec![crate::a11y::A11yCustomAction {
                    id: 3,
                    label: smol_str::SmolStr::new_static("加入自选"),
                }],
                ..A11yOverride::default()
            })
            .build(&mut tree, root)
            .expect("custom");
        assert!(action_allowed(&tree, custom, A11yAction::Custom(3)));
        assert!(!action_allowed(&tree, custom, A11yAction::Custom(4)));
    }
}
