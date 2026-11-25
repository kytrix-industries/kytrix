use smol_str::SmolStr;

use crate::{
    a11y::{A11yAction, A11yLabelSource, A11yOverride, A11yPatch, A11yRole},
    interaction::focus::focusable,
    layout::hit_test::HitShape,
    math::{DevicePixelRatio, DeviceRect},
    node::{
        id::NodeId,
        tree::{Node, NodeKind, NodeTree},
    },
};

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
        R::PasswordInput => Role::PasswordInput,
        R::CheckBox => Role::CheckBox,
        R::Switch => Role::Switch,
        R::RadioButton => Role::RadioButton,
        R::Slider => Role::Slider,
        R::ProgressIndicator => Role::ProgressIndicator,
        R::List => Role::List,
        R::ListBoxOption => Role::ListBoxOption,
        R::Table => Role::Table,
        R::Grid => Role::Grid,
        R::Tree => Role::Tree,
        R::Row => Role::Row,
        R::TreeItem => Role::TreeItem,
        R::Cell => Role::Cell,
        R::ColumnHeader => Role::ColumnHeader,
        R::RowHeader => Role::RowHeader,
        R::TextRun => Role::TextRun,
        R::ScrollView => Role::ScrollView,
        R::ScrollBar => Role::ScrollBar,
        R::Image => Role::Image,
        R::Menu => Role::Menu,
        R::MenuBar => Role::MenuBar,
        R::MenuItem => Role::MenuItem,
        R::Dialog => Role::Dialog,
        R::AlertDialog => Role::AlertDialog,
        R::Tooltip => Role::Tooltip,
        R::TabList => Role::TabList,
        R::Tab => Role::Tab,
        R::Separator => Role::Splitter,
        R::ComboBox => Role::ComboBox,
        R::ListBox => Role::ListBox,
        R::MenuItemCheckBox => Role::MenuItemCheckBox,
        R::MenuItemRadio => Role::MenuItemRadio,
        R::ColorWell => Role::ColorWell,
    }
}

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
        Action::CustomAction
        | Action::Blur
        | Action::ReplaceSelectedText
        | Action::HideTooltip
        | Action::ShowTooltip
        | Action::ScrollToPoint
        | Action::SetSequentialFocusNavigationStartingPoint => None,
    }
}

pub(crate) const fn live_to_accesskit(live: crate::a11y::A11yLive) -> accesskit::Live {
    match live {
        crate::a11y::A11yLive::Polite => accesskit::Live::Polite,
        crate::a11y::A11yLive::Assertive => accesskit::Live::Assertive,
    }
}

pub(crate) const fn sort_to_accesskit(sort: crate::a11y::A11ySort) -> accesskit::SortDirection {
    match sort {
        crate::a11y::A11ySort::Ascending => accesskit::SortDirection::Ascending,
        crate::a11y::A11ySort::Descending => accesskit::SortDirection::Descending,
        crate::a11y::A11ySort::None => accesskit::SortDirection::Other,
    }
}

pub(crate) const fn orientation_to_accesskit(
    orientation: crate::a11y::A11yOrientation,
) -> accesskit::Orientation {
    match orientation {
        crate::a11y::A11yOrientation::Horizontal => accesskit::Orientation::Horizontal,
        crate::a11y::A11yOrientation::Vertical => accesskit::Orientation::Vertical,
    }
}

pub(crate) fn bounds_of(tree: &NodeTree, node: NodeId, scale: DevicePixelRatio) -> DeviceRect {
    let content = tree
        .get(node)
        .map_or(crate::math::LogicalRect::default(), |target| {
            let bounds = target.layout().bounds;
            match target.hit_shape().and_then(HitShape::bounds) {
                Some(local) => bounds.union(crate::math::LogicalRect::from_origin_size(
                    bounds.origin + local.origin,
                    local.size,
                )),
                None => bounds,
            }
        });
    let screen = crate::interaction::scroll::to_screen(tree, node, content);
    scale.to_covering_device_rect(screen)
}

pub(crate) fn default_actions(role: A11yRole) -> Vec<A11yAction> {
    use A11yAction as A;
    use A11yRole as R;

    match role {
        R::Button => vec![A::Click],
        R::CheckBox
        | R::Switch
        | R::RadioButton
        | R::MenuItem
        | R::MenuItemCheckBox
        | R::MenuItemRadio
        | R::ListBoxOption => Vec::new(),
        R::Slider => vec![A::SetValue, A::Increment, A::Decrement, A::Focus],
        R::TextInput | R::MultilineTextInput | R::PasswordInput => vec![A::SetValue],
        R::TextRun => vec![A::SetTextSelection],
        R::Dialog | R::AlertDialog | R::Menu => Vec::new(),
        R::ScrollView => vec![
            A::ScrollUp,
            A::ScrollDown,
            A::ScrollLeft,
            A::ScrollRight,
            A::ScrollIntoView,
            A::SetScrollOffset,
        ],
        R::Table | R::Grid => Vec::new(),
        R::ComboBox | R::ListBox | R::ColorWell => Vec::new(),
        _ => Vec::new(),
    }
}

pub(crate) fn role_by_kind(tree: &NodeTree, node: NodeId, target: &Node) -> A11yRole {
    use A11yRole as R;

    if node == tree.root() {
        return R::Window;
    }
    match target.kind() {
        NodeKind::Canvas(_) | NodeKind::Geometry(_) | NodeKind::Image(_) => return R::Image,
        NodeKind::Text(_) => {
            return if is_text_run_in_input(tree, node) {
                R::TextRun
            } else {
                R::Label
            };
        }
        NodeKind::Box => {}
    }
    if tree.virtual_slot(node).is_some() {
        if crate::a11y::tree::is_suppressed_list_window(tree, node) {
            return R::GenericContainer;
        }
        return R::List;
    }
    if tree.scroll_slot(node).is_some() {
        return R::ScrollView;
    }
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

fn is_virtual_row(tree: &NodeTree, node: NodeId) -> bool {
    tree.get(node)
        .ok()
        .and_then(crate::node::tree::Node::parent)
        .is_some_and(|parent| tree.virtual_slot(parent).is_some())
}

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
    let lifted_scroll = crate::a11y::tree::lifted_scroll_window(tree, node);
    if tree.scroll_slot(node).is_some() || lifted_scroll.is_some() {
        for action in default_actions(A11yRole::ScrollView) {
            if !actions.contains(&action) {
                actions.push(action);
            }
        }
    }
    if matches!(role, A11yRole::Table | A11yRole::Grid)
        && (tree.scroll_slot(node).is_some()
            || lifted_scroll.is_some()
            || crate::interaction::scroll::scroll_ancestor(tree, node).is_some())
        && !actions.contains(&A11yAction::ScrollIntoView)
    {
        actions.push(A11yAction::ScrollIntoView);
    }
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

pub(crate) const fn is_value_mutation(action: A11yAction) -> bool {
    matches!(
        action,
        A11yAction::SetValue | A11yAction::Increment | A11yAction::Decrement
    )
}

pub(crate) fn a11y_read_only(tree: &NodeTree, node: NodeId) -> bool {
    let Some(slot) = tree.a11y_of(node) else {
        return false;
    };
    slot.patch
        .read_only
        .or_else(|| slot.over.as_ref().map(|over| over.read_only))
        .unwrap_or(false)
}

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
            NodeKind::Text(content)
                if matches!(role, A11yRole::Label | A11yRole::TextRun)
                    && !content.text.is_empty() =>
            {
                Some(content.text.clone())
            }
            _ => None,
        })
}

pub(crate) fn effective_description(
    over: Option<&A11yOverride>,
    patch: Option<&A11yPatch>,
) -> Option<SmolStr> {
    patch
        .and_then(|patch| patch.description.clone())
        .or_else(|| over.and_then(|over| over.description.clone()))
}

pub(crate) fn is_hidden(
    target: &Node,
    role: A11yRole,
    label: Option<&crate::a11y::A11yLabel>,
    over: Option<&A11yOverride>,
    patch: Option<&A11yPatch>,
) -> bool {
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
    if matches!(target.style().inner().display, taffy::style::Display::None) {
        return true;
    }
    matches!(
        target.kind(),
        NodeKind::Canvas(_) | NodeKind::Geometry(_) | NodeKind::Image(_)
    ) && label.is_none()
        && role == A11yRole::Image
}

pub(crate) fn is_decorative_image(target: &Node, over: Option<&A11yOverride>) -> bool {
    over.is_some_and(|over| over.hidden) && matches!(target.kind(), NodeKind::Image(_))
}

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
        math::{DevicePixelRatio, DeviceRect, dp, px},
        node::{builder::NodeBuilder, tree::NodeTree},
        paint::{
            canvas::Canvas,
            style::{Clip, Style, StylePatch},
        },
        text::{TextContent, TextStyle},
    };

    #[test]
    fn every_role_is_included_exactly_once_in_all() {
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
                | A11yRole::PasswordInput
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
        assert_eq!(
            role_to_accesskit(A11yRole::PasswordInput),
            accesskit::Role::PasswordInput,
            "密码输入使用 AccessKit 的 PasswordInput 角色"
        );
        assert!(
            A11yRole::ALL.contains(&A11yRole::PasswordInput),
            "A11yRole::ALL 必须包含 PasswordInput"
        );
        assert_eq!(role_to_accesskit(A11yRole::Tree), accesskit::Role::Tree);
        assert_eq!(
            role_to_accesskit(A11yRole::TreeItem),
            accesskit::Role::TreeItem
        );
        assert!(
            A11yRole::ALL.contains(&A11yRole::Tree),
            "A11yRole::ALL 必须包含 Tree"
        );
        assert!(
            A11yRole::ALL.contains(&A11yRole::TreeItem),
            "A11yRole::ALL 必须包含 TreeItem"
        );
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
        assert!(default_actions(A11yRole::PasswordInput).contains(&A11yAction::SetValue));
        assert!(default_actions(A11yRole::ScrollView).contains(&A11yAction::ScrollIntoView));
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

    #[test]
    fn click_comes_from_capability_not_role() {
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
        let canvas = NodeBuilder::canvas(Canvas::new(crate::math::LogicalRect::new(
            px(0.0),
            px(0.0),
            px(4.0),
            px(4.0),
        )))
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

        let enabled = effective_actions(&tree, button, A11yRole::Button, None, None, false, false);
        assert!(enabled.contains(&A11yAction::Click));
        assert!(enabled.contains(&A11yAction::Focus));

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

        let view = effective_actions(&tree, button, A11yRole::ScrollView, None, None, true, false);
        assert!(view.contains(&A11yAction::ScrollDown), "{view:?}");
    }

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
        let inner = NodeBuilder::text("42150", TextStyle::new(px(12.0)))
            .build(&mut tree, input)
            .expect("text");
        let label = NodeBuilder::text("最新价", TextStyle::new(px(12.0)))
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
        let _ = TextContent::new("x", TextStyle::new(px(12.0)));
    }

    #[test]
    fn a11y_bounds_follow_the_hit_shape() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .layout_override(crate::math::LogicalRect::new(
                px(10.0),
                px(20.0),
                px(40.0),
                px(30.0),
            ))
            .hit_shape(HitShape::Rect(crate::math::LogicalRect::new(
                px(-5.0),
                px(-5.0),
                px(60.0),
                px(50.0),
            )))
            .build(&mut tree, root)
            .expect("mount");
        tree.compute_layout(taffy::geometry::Size {
            width: Some(200.0),
            height: Some(200.0),
        })
        .expect("layout");
        let scale = DevicePixelRatio::new(1.0).expect("scale");

        assert_eq!(
            bounds_of(&tree, node, scale),
            DeviceRect::new(dp(5), dp(15), dp(65), dp(65)),
            "a11y bounds = 布局盒 (10,20,40,30) ∪ 形状局部 AABB (-5,-5,60,50)"
        );

        let plain = NodeBuilder::boxed()
            .layout_override(crate::math::LogicalRect::new(
                px(10.0),
                px(20.0),
                px(40.0),
                px(30.0),
            ))
            .build(&mut tree, root)
            .expect("mount");
        tree.compute_layout(taffy::geometry::Size {
            width: Some(200.0),
            height: Some(200.0),
        })
        .expect("layout");
        assert_eq!(
            bounds_of(&tree, plain, scale),
            DeviceRect::new(dp(10), dp(20), dp(50), dp(50)),
            "未设置 HitShape ⇒ 仍用布局盒（旧行为）"
        );
    }

    #[test]
    fn a11y_bounds_cover_the_full_logical_rect() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .layout_override(crate::math::LogicalRect::new(
                px(1.3),
                px(0.0),
                px(0.4),
                px(1.0),
            ))
            .build(&mut tree, root)
            .expect("mount");
        tree.compute_layout(taffy::geometry::Size {
            width: Some(200.0),
            height: Some(200.0),
        })
        .expect("layout");

        let covering = DevicePixelRatio::new(2.0).expect("scale");
        assert_eq!(
            bounds_of(&tree, node, covering),
            DeviceRect::new(dp(2), dp(0), dp(4), dp(2)),
            "a11y bounds 用 floor/ceil 外扩，覆盖整个逻辑矩形"
        );
        assert_eq!(
            covering.to_device_rect(crate::math::LogicalRect::new(
                px(1.3),
                px(0.0),
                px(0.4),
                px(1.0)
            )),
            DeviceRect::new(dp(3), dp(0), dp(3), dp(2)),
            "逐边 round 只保留 0 宽，不能用于 a11y"
        );
    }

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
        let run = NodeBuilder::text("42150", TextStyle::new(px(12.0)))
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
