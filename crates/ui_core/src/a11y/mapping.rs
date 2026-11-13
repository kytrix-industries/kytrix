//! 节点 → a11y 的映射表与判据（`design/phase-11-a11y.md` §4.1 / §5）。
//!
//! 全部是**纯函数**（只读 `&NodeTree` / `&Node`），没有状态、没有走帧。`accesskit` 的类型只在这里
//! 与 `a11y::adapter` 出现（角色 / 动作的枚举对译）。

use smol_str::SmolStr;

use crate::{
    a11y::{A11yAction, A11yLabelSource, A11yOverride, A11yPatch, A11yRole},
    interaction::focus::focusable,
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
        R::ScrollView => Role::ScrollView,
        R::ScrollBar => Role::ScrollBar,
        R::Image => Role::Image,
        R::Menu => Role::Menu,
        R::MenuBar => Role::MenuBar,
        R::MenuItem => Role::MenuItem,
        R::Dialog => Role::Dialog,
        R::Tooltip => Role::Tooltip,
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
        | Action::Decrement
        | Action::Increment
        | Action::HideTooltip
        | Action::ShowTooltip
        | Action::ScrollToPoint
        | Action::SetSequentialFocusNavigationStartingPoint => None,
    }
}

/// 窗口相对、**物理像素**的边界盒（逻辑 × scale，逐边 round；含祖先滚动偏移的屏幕化）。
pub(crate) fn bounds_of(tree: &NodeTree, node: NodeId, scale: Scale) -> PhysicalRect {
    let content = tree
        .get(node)
        .map_or(crate::math::Rect::default(), |target| {
            target.layout().bounds
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
        R::TextInput | R::MultilineTextInput => vec![A::SetValue, A::SetTextSelection],
        R::ScrollView => vec![
            A::ScrollUp,
            A::ScrollDown,
            A::ScrollLeft,
            A::ScrollRight,
            A::ScrollIntoView,
            A::SetScrollOffset,
        ],
        R::Dialog | R::Menu => vec![A::Expand, A::Collapse],
        _ => Vec::new(),
    }
}

/// 由节点种类推导角色（`by_kind`；`design §5.2` 的判定顺序）。
pub(crate) fn role_by_kind(tree: &NodeTree, node: NodeId, target: &Node) -> A11yRole {
    use A11yRole as R;

    if node == tree.root() {
        return R::Window;
    }
    match target.kind() {
        NodeKind::Canvas(_) | NodeKind::Geometry(_) => return R::Image,
        NodeKind::Text(_) => return R::Label,
        NodeKind::Box => {}
    }
    // 虚拟列表**先判**：它也带滚动槽，但语义是 `List`（滚动动作由 `effective_actions` 补）。
    if tree.virtual_slot(node).is_some() {
        return R::List;
    }
    if tree.scroll_slot(node).is_some() {
        return R::ScrollView;
    }
    if tree.overlay_slot(node).is_some() {
        return R::Dialog;
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
pub(crate) fn effective_actions(
    tree: &NodeTree,
    node: NodeId,
    role: A11yRole,
    over: Option<&A11yOverride>,
    patch: Option<&A11yPatch>,
) -> Vec<A11yAction> {
    let mut actions = patch
        .and_then(|patch| patch.actions.clone())
        .or_else(|| over.and_then(|over| over.actions.clone()))
        .unwrap_or_else(|| default_actions(role));
    // 带滚动槽的节点（含虚拟列表容器）总能接受滚动动作，与角色无关。
    if tree.scroll_slot(node).is_some() {
        for action in default_actions(A11yRole::ScrollView) {
            if !actions.contains(&action) {
                actions.push(action);
            }
        }
    }
    // 声明了动作（`on_click` / `on_action`）的节点一定能被 AT 激活，与角色无关。
    if tree
        .interaction(node)
        .is_some_and(|slot| slot.action.is_some())
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
    actions
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
        NodeKind::Text(content) if !content.text.is_empty() => {
            let source = if role == A11yRole::Label {
                A11yLabelSource::TextContent
            } else {
                A11yLabelSource::Override
            };
            Some(crate::a11y::A11yLabel::new(content.text.clone(), source))
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
            NodeKind::Text(content) if role == A11yRole::Label && !content.text.is_empty() => {
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

/// 隐藏判定（`design §4.3` 的「与 `StylePatch` 的一致性」表）。
pub(crate) fn is_hidden(
    target: &Node,
    role: A11yRole,
    label: Option<&crate::a11y::A11yLabel>,
    over: Option<&A11yOverride>,
    patch: Option<&A11yPatch>,
) -> bool {
    if patch.is_some_and(|patch| patch.hidden == Some(true)) {
        return true;
    }
    if over.is_some_and(|over| over.hidden) {
        return true;
    }
    if target.style_patch().visible == Some(false) {
        return true;
    }
    if matches!(target.style().inner.display, taffy::style::Display::None) {
        return true;
    }
    // 自绘节点必须有替代文本，否则 hidden + 诊断（§7.4 / §5.4）。
    matches!(target.kind(), NodeKind::Canvas(_) | NodeKind::Geometry(_))
        && label.is_none()
        && role == A11yRole::Image
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
        action_from_accesskit, action_to_accesskit, default_actions, is_disabled, is_hidden,
        role_to_accesskit,
    };
    use crate::{
        a11y::{A11yAction, A11yLabel, A11yLabelSource, A11yOverride, A11yPatch, A11yRole},
        node::{builder::NodeBuilder, tree::NodeTree},
        paint::{
            canvas::Canvas,
            style::{Clip, Style, StylePatch},
        },
    };

    #[test]
    fn every_role_maps_to_a_distinct_accesskit_role() {
        let roles = [
            A11yRole::Window,
            A11yRole::Application,
            A11yRole::GenericContainer,
            A11yRole::Group,
            A11yRole::Label,
            A11yRole::Paragraph,
            A11yRole::Heading,
            A11yRole::Button,
            A11yRole::TextInput,
            A11yRole::MultilineTextInput,
            A11yRole::CheckBox,
            A11yRole::Switch,
            A11yRole::RadioButton,
            A11yRole::Slider,
            A11yRole::ProgressIndicator,
            A11yRole::List,
            A11yRole::ListBoxOption,
            A11yRole::Table,
            A11yRole::Grid,
            A11yRole::ScrollView,
            A11yRole::ScrollBar,
            A11yRole::Image,
            A11yRole::Menu,
            A11yRole::MenuBar,
            A11yRole::MenuItem,
            A11yRole::Dialog,
            A11yRole::Tooltip,
        ];
        let mut mapped = Vec::new();
        for role in roles {
            mapped.push(role_to_accesskit(role));
        }
        let unique: std::collections::BTreeSet<_> = mapped.iter().collect();
        assert_eq!(unique.len(), roles.len(), "角色映射必须一一对应");
        assert_eq!(role_to_accesskit(A11yRole::Image), accesskit::Role::Image);
        assert_eq!(
            role_to_accesskit(A11yRole::ScrollView),
            accesskit::Role::ScrollView,
            "不是 ScrollArea（不存在）"
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
        assert!(default_actions(A11yRole::Dialog).contains(&A11yAction::Expand));
        assert!(default_actions(A11yRole::Label).is_empty());
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
}
