use smol_str::SmolStr;

use crate::{
    math::{PhysicalRect, Point},
    node::id::NodeId,
    window::WindowId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11yRole {
    Window,
    Application,
    GenericContainer,
    Group,
    Label,
    Paragraph,
    Heading,
    Button,
    TextInput,
    MultilineTextInput,
    CheckBox,
    Switch,
    RadioButton,
    Slider,
    ProgressIndicator,
    List,
    ListBoxOption,
    Table,
    Grid,
    Tree,
    Row,
    TreeItem,
    Cell,
    ColumnHeader,
    RowHeader,
    TextRun,
    ScrollView,
    ScrollBar,
    Image,
    Menu,
    MenuBar,
    MenuItem,
    Dialog,
    AlertDialog,
    Tooltip,
    TabList,
    Tab,
    Separator,
    ComboBox,
    ListBox,
    MenuItemCheckBox,
    MenuItemRadio,
    ColorWell,
}

impl A11yRole {
    pub const ALL: [Self; 43] = [
        Self::Window,
        Self::Application,
        Self::GenericContainer,
        Self::Group,
        Self::Label,
        Self::Paragraph,
        Self::Heading,
        Self::Button,
        Self::TextInput,
        Self::MultilineTextInput,
        Self::CheckBox,
        Self::Switch,
        Self::RadioButton,
        Self::Slider,
        Self::ProgressIndicator,
        Self::List,
        Self::ListBoxOption,
        Self::Table,
        Self::Grid,
        Self::Tree,
        Self::Row,
        Self::TreeItem,
        Self::Cell,
        Self::ColumnHeader,
        Self::RowHeader,
        Self::TextRun,
        Self::ScrollView,
        Self::ScrollBar,
        Self::Image,
        Self::Menu,
        Self::MenuBar,
        Self::MenuItem,
        Self::Dialog,
        Self::AlertDialog,
        Self::Tooltip,
        Self::TabList,
        Self::Tab,
        Self::Separator,
        Self::ComboBox,
        Self::ListBox,
        Self::MenuItemCheckBox,
        Self::MenuItemRadio,
        Self::ColorWell,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11ySort {
    Ascending,
    Descending,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11yOrientation {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct A11yTextSelection {
    pub anchor: usize,
    pub focus: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct A11yTextPosition {
    pub node: NodeId,
    pub byte_index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct A11yCustomAction {
    pub id: i32,
    pub label: SmolStr,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct A11yScroll {
    pub x: f32,
    pub y: f32,
    pub x_max: f32,
    pub y_max: f32,
    pub vertical: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11yLabelSource {
    Override,
    TextContent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct A11yLabel {
    text: SmolStr,
    source: A11yLabelSource,
}

impl A11yLabel {
    pub(crate) fn new(text: SmolStr, source: A11yLabelSource) -> Self {
        Self { text, source }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        self.text.as_str()
    }

    #[must_use]
    pub fn source(&self) -> A11yLabelSource {
        self.source
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11yAction {
    Click,
    Focus,
    SetValue,
    Increment,
    Decrement,
    SetTextSelection,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
    ScrollIntoView,
    SetScrollOffset,
    Expand,
    Collapse,
    ShowContextMenu,
    Custom(i32),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct A11yOverride {
    pub role: Option<A11yRole>,
    pub label: Option<SmolStr>,
    pub value: Option<SmolStr>,
    pub row_index: Option<usize>,
    pub column_index: Option<usize>,
    pub row_count: Option<usize>,
    pub column_count: Option<usize>,
    pub live: Option<A11yLive>,
    pub pos_in_set: Option<usize>,
    pub size_of_set: Option<usize>,
    pub numeric_value: Option<f64>,
    pub min_numeric_value: Option<f64>,
    pub max_numeric_value: Option<f64>,
    pub numeric_value_step: Option<f64>,
    pub numeric_value_jump: Option<f64>,
    pub description: Option<SmolStr>,
    pub actions: Option<Vec<A11yAction>>,
    pub hidden: bool,
    pub disabled: bool,
    pub key: Option<u64>,
    pub sort: Option<A11ySort>,
    pub selection: Option<A11yTextSelection>,
    pub custom_actions: Vec<A11yCustomAction>,
    pub read_only: bool,
    pub modal: bool,
    pub busy: bool,
    pub invalid: bool,
    pub error_message: Option<NodeId>,
    pub placeholder: Option<SmolStr>,
    pub level: Option<usize>,
    pub orientation: Option<A11yOrientation>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct A11yPatch {
    pub role: Option<A11yRole>,
    pub label: Option<SmolStr>,
    pub value: Option<SmolStr>,
    pub description: Option<SmolStr>,
    pub disabled: Option<bool>,
    pub hidden: Option<bool>,
    pub row_index: Option<usize>,
    pub column_index: Option<usize>,
    pub row_count: Option<usize>,
    pub column_count: Option<usize>,
    pub checked: Option<bool>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    pub live: Option<A11yLive>,
    pub pos_in_set: Option<usize>,
    pub size_of_set: Option<usize>,
    pub numeric_value: Option<f64>,
    pub min_numeric_value: Option<f64>,
    pub max_numeric_value: Option<f64>,
    pub numeric_value_step: Option<f64>,
    pub numeric_value_jump: Option<f64>,
    pub actions: Option<Vec<A11yAction>>,
    pub key: Option<u64>,
    pub sort: Option<A11ySort>,
    pub selection: Option<A11yTextSelection>,
    pub custom_actions: Option<Vec<A11yCustomAction>>,
    pub read_only: Option<bool>,
    pub modal: Option<bool>,
    pub busy: Option<bool>,
    pub invalid: Option<bool>,
    pub error_message: Option<NodeId>,
    pub placeholder: Option<SmolStr>,
    pub level: Option<usize>,
    pub orientation: Option<A11yOrientation>,
}

impl A11yPatch {
    #[must_use]
    pub(crate) fn affects_descendants(&self, previous: &Self) -> bool {
        self.role != previous.role || self.hidden != previous.hidden
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum A11yLive {
    Polite,
    Assertive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct A11yNodeId(u64);

impl A11yNodeId {
    #[must_use]
    pub(crate) const fn from_stable(id: u64) -> Self {
        Self(id)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct A11yNode {
    pub id: A11yNodeId,
    pub engine: NodeId,
    pub role: A11yRole,
    pub label: Option<A11yLabel>,
    pub value: Option<SmolStr>,
    pub description: Option<SmolStr>,
    pub bounds: PhysicalRect,
    pub actions: Vec<A11yAction>,
    pub hidden: bool,
    pub disabled: bool,
    pub checked: Option<bool>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    pub row_index: Option<usize>,
    pub column_index: Option<usize>,
    pub row_count: Option<usize>,
    pub column_count: Option<usize>,
    pub live: Option<A11yLive>,
    pub pos_in_set: Option<usize>,
    pub size_of_set: Option<usize>,
    pub numeric_value: Option<f64>,
    pub min_numeric_value: Option<f64>,
    pub max_numeric_value: Option<f64>,
    pub numeric_value_step: Option<f64>,
    pub numeric_value_jump: Option<f64>,
    pub sort: Option<A11ySort>,
    pub scroll: Option<A11yScroll>,
    pub clips_children: bool,
    pub custom_actions: Vec<A11yCustomAction>,
    pub character_lengths: Vec<u8>,
    pub word_starts: Vec<u8>,
    pub character_positions: Vec<f32>,
    pub character_widths: Vec<f32>,
    pub text_selection: Option<A11yTextSelection>,
    pub read_only: bool,
    pub modal: bool,
    pub busy: bool,
    pub invalid: bool,
    pub error_message: Option<A11yNodeId>,
    pub placeholder: Option<SmolStr>,
    pub level: Option<usize>,
    pub orientation: Option<A11yOrientation>,
    pub children: Vec<A11yNodeId>,
    pub parent: Option<A11yNodeId>,
}

impl A11yNode {
    #[must_use]
    pub fn label_text(&self) -> Option<&str> {
        self.label.as_ref().map(A11yLabel::text)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct A11yTreeSnapshot {
    pub full: bool,
    pub root: A11yNodeId,
    pub nodes: Vec<A11yNode>,
    pub focus: A11yNodeId,
    pub focus_node: A11yNode,
    pub focus_ancestors: Vec<A11yNode>,
    pub diagnostics: usize,
}

impl A11yTreeSnapshot {
    #[must_use]
    pub fn nodes(&self) -> &[A11yNode] {
        &self.nodes
    }

    #[must_use]
    pub fn node(&self, id: A11yNodeId) -> Option<&A11yNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    #[must_use]
    pub fn node_for(&self, engine: NodeId) -> Option<&A11yNode> {
        self.nodes.iter().find(|node| node.engine == engine)
    }

    #[must_use]
    pub fn focus(&self) -> A11yNodeId {
        self.focus
    }

    #[must_use]
    pub fn focus_node(&self) -> &A11yNode {
        &self.focus_node
    }

    #[must_use]
    pub fn id_for_engine(&self, engine: NodeId) -> Option<A11yNodeId> {
        self.nodes
            .iter()
            .find(|node| node.engine == engine)
            .map(|node| node.id)
            .or_else(|| (self.focus_node.engine == engine).then_some(self.focus_node.id))
    }

    pub fn nodes_with_role(&self, role: A11yRole) -> impl Iterator<Item = &A11yNode> {
        self.nodes.iter().filter(move |node| node.role == role)
    }

    pub fn visible_nodes_in_batch(&self) -> impl Iterator<Item = &A11yNode> {
        let hidden: ahash::HashSet<A11yNodeId> = self
            .nodes
            .iter()
            .filter(|node| node.hidden)
            .map(|node| node.id)
            .collect();
        let parents: ahash::HashMap<A11yNodeId, Option<A11yNodeId>> = self
            .nodes
            .iter()
            .map(|node| (node.id, node.parent))
            .collect();
        self.nodes.iter().filter(move |node| {
            let mut cursor = Some(node.id);
            while let Some(id) = cursor {
                if hidden.contains(&id) {
                    return false;
                }
                cursor = parents.get(&id).copied().flatten();
            }
            true
        })
    }

    pub fn hidden_nodes(&self) -> impl Iterator<Item = &A11yNode> {
        self.nodes.iter().filter(|node| node.hidden)
    }

    #[must_use]
    pub fn children_of(&self, id: A11yNodeId) -> Vec<A11yNodeId> {
        self.node(id)
            .map(|node| node.children.clone())
            .unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11yDrawKind {
    Canvas,
    Geometry,
    Image,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum A11yDiagnostic {
    MissingAltText {
        node: NodeId,
        kind: A11yDrawKind,
    },
    MissingLabel {
        node: NodeId,
    },
    StaleAction {
        action: Option<A11yAction>,
    },
    UnhandledAction {
        node: NodeId,
        action: Option<A11yAction>,
    },
    MissingFocusTarget {
        node: NodeId,
    },
    DuplicateKey {
        node: NodeId,
    },
    UnregisteredCustomAction {
        node: NodeId,
        id: i32,
    },
    DisabledAction {
        node: NodeId,
        action: A11yAction,
    },
    ReadOnlyAction {
        node: NodeId,
        action: A11yAction,
    },
    UnadvertisedAction {
        node: NodeId,
        action: A11yAction,
    },
    InvalidActionData {
        node: NodeId,
        action: A11yAction,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum A11yActionData<'a> {
    None,
    Value(&'a str),
    Selection {
        anchor: A11yTextPosition,
        focus: A11yTextPosition,
    },
    ScrollOffset(Point),
    NumericValue(f64),
    Custom(i32),
}

pub struct A11yActionCtx<'a> {
    app: &'a mut crate::app::App,
    window: WindowId,
    node: NodeId,
    action: A11yAction,
    data: A11yActionData<'a>,
}

impl<'a> A11yActionCtx<'a> {
    pub(crate) fn new(
        app: &'a mut crate::app::App,
        window: WindowId,
        node: NodeId,
        action: A11yAction,
        data: A11yActionData<'a>,
    ) -> Self {
        Self {
            app,
            window,
            node,
            action,
            data,
        }
    }

    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }

    #[must_use]
    pub fn action(&self) -> A11yAction {
        self.action
    }

    #[must_use]
    pub fn data(&self) -> A11yActionData<'a> {
        self.data
    }

    pub fn read<T: 'static>(
        &self,
        signal: &crate::node::signal::Signal<T>,
    ) -> crate::node::signal::SignalRef<T> {
        self.app.read(signal)
    }

    pub fn write<T: Clone + PartialEq + 'static>(
        &mut self,
        signal: &crate::node::signal::Signal<T>,
        value: T,
    ) -> bool {
        self.app.write(signal, value)
    }

    pub fn update<T: 'static>(
        &mut self,
        signal: &crate::node::signal::Signal<T>,
        update: impl FnOnce(&mut T),
    ) -> bool {
        self.app.update(signal, update)
    }

    pub fn app_mut(&mut self) -> &mut crate::app::App {
        self.app
    }
}

impl std::fmt::Debug for A11yActionCtx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A11yActionCtx")
            .field("window", &self.window)
            .field("node", &self.node)
            .field("action", &self.action)
            .field("data", &self.data)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{A11yPatch, A11yRole};

    #[test]
    fn affects_descendants_only_tracks_scope_fields() {
        let base = A11yPatch::default();
        let role = A11yPatch {
            role: Some(A11yRole::TextInput),
            ..A11yPatch::default()
        };
        let hidden = A11yPatch {
            hidden: Some(true),
            ..A11yPatch::default()
        };
        let label = A11yPatch {
            label: Some(smol_str::SmolStr::new_static("价格")),
            ..A11yPatch::default()
        };
        assert!(
            role.affects_descendants(&base),
            "role 会改变后代的推导角色（Label ↔ TextRun、集合领养）"
        );
        assert!(
            hidden.affects_descendants(&base),
            "被集合领养跳过的物理祖先 hidden 会被后代继承"
        );
        assert!(role.affects_descendants(&hidden));
        assert!(
            !label.affects_descendants(&base),
            "label 只描述本节点，不该触发 O(子树) 标记"
        );
        assert!(!base.affects_descendants(&A11yPatch::default()));
        assert!(
            base.affects_descendants(&role),
            "撤掉覆盖（Some → None）同样改变后代的合并角色"
        );
    }
}

pub(crate) mod action;
pub(crate) mod adapter;
pub(crate) mod identity;
pub(crate) mod mapping;
pub(crate) mod textrun;
pub(crate) mod tree;
