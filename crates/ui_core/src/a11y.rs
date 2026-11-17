//! 无障碍（AccessKit）契约：角色、节点、补丁、动作与快照（`design/phase-11-a11y.md`）。
//!
//! 本文件是**公共模块根**：全部公开契约类型定义在这里，实现放子模块
//! （`tree` 快照构建 / `mapping` 映射表 / `adapter` `TreeUpdate` 组装 / `action` 动作分发）。
//! 每个类型只有**一个** inherent `impl` 块（`multiple_inherent_impl = "forbid"`）。
//!
//! **依赖方向**：本文件不出现 `accesskit` 类型——平台无关的引擎侧用 [`A11yRole`] / [`A11yAction`]
//! 表达，`accesskit` 只出现在 `a11y::adapter` 与 `platform/*/a11y.rs`（I9 的同型纪律）。

use smol_str::SmolStr;

use crate::{
    math::{PhysicalRect, Point},
    node::id::NodeId,
    window::WindowId,
};

/// a11y 角色（引擎侧，`accesskit::Role` 的子集）。
///
/// 只包含引擎**真实会产生**的角色（27 个）；不含 `Unknown`，也不含 130+ 个 HTML/Doc 角色——
/// 加变体必须同时给 `a11y::mapping` 的穷举 `match` 补分支（编译期强制，不会静默落到 `Unknown`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11yRole {
    /// 窗口根（`TreeInfo.root`）。
    Window,
    /// 应用本身（少见，显式声明时用）。
    Application,
    /// 纯布局容器（AccessKit 侧透明：子节点上提到祖父）。
    GenericContainer,
    /// 语义分组（容器要「出现」在 AT 树里时用）。
    Group,
    /// 文本叶子（可访问名走 `value`，见 `A11yLabelSource`）。
    Label,
    /// 段落。
    Paragraph,
    /// 标题。
    Heading,
    /// 按钮（点击 / 激活）。
    Button,
    /// 单行输入框。
    TextInput,
    /// 多行输入框。
    MultilineTextInput,
    /// 复选框。
    CheckBox,
    /// 开关。
    Switch,
    /// 单选按钮。
    RadioButton,
    /// 滑块。
    Slider,
    /// 进度指示。
    ProgressIndicator,
    /// 列表容器。
    List,
    /// 列表项（虚拟列表的行）。
    ListBoxOption,
    /// 表格。
    Table,
    /// 网格。
    Grid,
    /// 表格行 / 网格行（**仅**真实表格：`Row` 必须位于 `Table`/`Grid`/`TreeGrid` 内）。
    /// 虚拟列表的行仍用 `ListBoxOption`，位置走 `pos_in_set` / `size_of_set`。
    Row,
    /// 单元格（表格 / 网格里的格子，配合 `row_index` / `column_index`）。
    Cell,
    /// 滚动容器。
    ScrollView,
    /// 滚动条。
    ScrollBar,
    /// 有替代文本的图形（`Canvas` / `Geometry` 的唯一落点）。
    Image,
    /// 菜单。
    Menu,
    /// 菜单栏。
    MenuBar,
    /// 菜单项。
    MenuItem,
    /// 对话框（浮层）。
    Dialog,
    /// 工具提示。
    Tooltip,
}

/// 可访问名的**写入通道**：决定 `accesskit::Node` 侧写 `set_label` 还是 `set_value`。
///
/// 这不是实现细节：`Role::Label` 的可访问名来自 `value`（`accesskit_consumer` 的
/// `label_comes_from_value()`，probe ① 实测），写错通道 = AT 读不到名字。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11yLabelSource {
    /// 应用显式给出的名字（写 `set_label`）。
    Override,
    /// 来自文本节点自身的文本（写 `set_value`）。
    TextContent,
}

/// 可访问名 + 它的来源。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct A11yLabel {
    text: SmolStr,
    source: A11yLabelSource,
}

impl A11yLabel {
    /// 构造（crate 内：`a11y::tree` 与 `a11y::adapter` 是唯一生产者）。
    pub(crate) fn new(text: SmolStr, source: A11yLabelSource) -> Self {
        Self { text, source }
    }

    /// 名字文本。
    #[must_use]
    pub fn text(&self) -> &str {
        self.text.as_str()
    }

    /// 名字来源（决定写 `set_label` 还是 `set_value`）。
    #[must_use]
    pub fn source(&self) -> A11yLabelSource {
        self.source
    }
}

/// 引擎对外暴露（并接受）的 a11y 动作子集。
///
/// `Copy + Eq + Hash`：它是 `App::on_a11y_action` 的 handler 表键（payload 不进键，与 P10 的
/// `Action` 同纪律）。未列入的 `accesskit::Action` 由 `a11y::action` 显式忽略并记诊断。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11yAction {
    /// 激活节点声明的动作（落到 P10 的 `Action` 表）。
    Click,
    /// 把焦点给该节点。
    Focus,
    /// 写输入框的值。
    SetValue,
    /// 数值步进（`Slider` 等；与 `SetValue` 的文本值区分开）。
    Increment,
    Decrement,
    /// 设置文本选区。
    SetTextSelection,
    /// 向上滚动。
    ScrollUp,
    /// 向下滚动。
    ScrollDown,
    /// 向左滚动。
    ScrollLeft,
    /// 向右滚动。
    ScrollRight,
    /// 把节点滚入视口。
    ScrollIntoView,
    /// 直接设置滚动偏移。
    SetScrollOffset,
    /// 展开。
    Expand,
    /// 收起。
    Collapse,
    /// 弹出上下文菜单。
    ShowContextMenu,
    /// 应用自定义动作（`accesskit::CustomAction` 的 id）。
    Custom(i32),
}

/// 挂载时一次写入的**静态** a11y 声明（`NodeBuilder::a11y`）。
///
/// 与 [`A11yPatch`] 同构：静态给默认值、动态覆盖（合并规则见 `design/phase-11-a11y.md` §4.3）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct A11yOverride {
    /// 覆盖由节点种类推导出的角色（唯一的「强制角色」入口）。
    pub role: Option<A11yRole>,
    /// 替代文本 / 可访问名（`Canvas` / `Geometry` **必须**给）。
    pub label: Option<SmolStr>,
    /// 值（输入框内容等）。
    pub value: Option<SmolStr>,
    /// 行/列位置与总数（表格语义；`None` = 不表态，P2-7）。
    pub row_index: Option<usize>,
    pub column_index: Option<usize>,
    pub row_count: Option<usize>,
    pub column_count: Option<usize>,
    /// 实时区域播报策略（`None` = 不表态，P2-6）。
    pub live: Option<A11yLive>,
    /// 集合内位置与总数（`None` = 不表态）。
    ///
    /// **遵循 AccessKit 语义：`pos_in_set` 是 0-based**（其文档明确 aria-posinset
    /// 是 1-based、本属性是 0-based；Windows/AT-SPI 适配层自己 +1）。`size_of_set`
    /// 应挂在集合**容器**上（consumer 只从祖先上读取）。
    ///
    /// 列表项 / 虚拟列表行用它表达「第 N 项，共 M 项」；表格行仍用 `row_index`。
    pub pos_in_set: Option<usize>,
    pub size_of_set: Option<usize>,
    /// 数值通道（`Slider` / `ProgressIndicator` / `ScrollBar` 等，P2-8）。
    ///
    /// 屏幕阅读器播报的是**数值**（"50，最大 100"），UIA 的 `RangeValuePattern` 直接读这几个
    /// 字段；只给字符串 `value` 时滑块既不可调节也不播报数值。
    pub numeric_value: Option<f64>,
    pub min_numeric_value: Option<f64>,
    pub max_numeric_value: Option<f64>,
    /// 步进/跳转幅度（AT 的「加一步 / 加大步」用；缺省时 UIA 的 `SmallChange` 为 0）。
    pub numeric_value_step: Option<f64>,
    pub numeric_value_jump: Option<f64>,
    /// 补充说明（`aria-description` 语义）。
    pub description: Option<SmolStr>,
    /// 显式动作表；`None` ⇒ 按角色默认。
    pub actions: Option<Vec<A11yAction>>,
    /// 应用强制隐藏。
    pub hidden: bool,
    /// 应用强制禁用。
    pub disabled: bool,
}

/// `bind_a11y` 每帧求值的**动态**补丁（值语义；`None` = 本帧不覆盖）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct A11yPatch {
    /// 覆盖角色。
    pub role: Option<A11yRole>,
    /// 覆盖可访问名。
    pub label: Option<SmolStr>,
    /// 覆盖值。
    pub value: Option<SmolStr>,
    /// 覆盖说明。
    pub description: Option<SmolStr>,
    /// 禁用。
    pub disabled: Option<bool>,
    /// 隐藏。
    pub hidden: Option<bool>,
    /// 行/列位置与总数（虚拟列表 / 表格的语义，P2-7）。`None` = 不表态。
    ///
    /// 屏幕阅读器靠这些字段播报「第 N 行，共 M 行」，并支持按行列导航；虚拟列表的
    /// `Row` 必须给 `row_index`，否则整棵树会被当成一堆无名容器。
    pub row_index: Option<usize>,
    pub column_index: Option<usize>,
    pub row_count: Option<usize>,
    pub column_count: Option<usize>,
    /// 勾选/开关状态（`accesskit::Toggled`）。
    ///
    /// `RadioButton` / `CheckBox` / `Switch` 的勾选态都走这个通道：UIA 的 RadioButton 只有
    /// `toggled()` 有值时才暴露 `SelectionItemPattern`（`accesskit_windows` 的
    /// `is_selection_item_pattern_supported`），CheckBox / Switch 映射到 `TogglePattern`；
    /// 这几个角色都不读 `selected`。
    pub checked: Option<bool>,
    /// 选中（`None` = 不表态）。
    pub selected: Option<bool>,
    /// 展开（`None` = 不表态）。
    pub expanded: Option<bool>,
    /// 实时区域播报策略（`None` = 不表态）。
    pub live: Option<A11yLive>,
    /// 集合内位置与总数（`None` = 不表态）。
    ///
    /// **遵循 AccessKit 语义：`pos_in_set` 是 0-based**（其文档明确 aria-posinset
    /// 是 1-based、本属性是 0-based；Windows/AT-SPI 适配层自己 +1）。`size_of_set`
    /// 应挂在集合**容器**上（consumer 只从祖先上读取）。
    ///
    /// 列表项 / 虚拟列表行用它表达「第 N 项，共 M 项」；表格行仍用 `row_index`。
    pub pos_in_set: Option<usize>,
    pub size_of_set: Option<usize>,
    /// 数值通道（滑块 / 进度 / 滚动条的数值与范围，P2-8）。
    pub numeric_value: Option<f64>,
    pub min_numeric_value: Option<f64>,
    pub max_numeric_value: Option<f64>,
    /// 步进/跳转幅度（AT 的「加一步 / 加大步」用；缺省时 UIA 的 `SmallChange` 为 0）。
    pub numeric_value_step: Option<f64>,
    pub numeric_value_jump: Option<f64>,
    /// 覆盖动作表。
    pub actions: Option<Vec<A11yAction>>,
}

/// 实时区域（live region）的播报策略。
///
/// 行情数字这类**持续变化**的内容必须声明 live region，否则屏幕阅读器只会把它当静态
/// 文本、用户根本不知道数值变了（P2-6）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum A11yLive {
    /// 排队播报（不打断用户当前朗读）：适合价格、进度这类高频更新。
    Polite,
    /// 立即打断当前朗读：只用于告警级变化（如触发阈值）。
    Assertive,
}

/// a11y 侧的节点身份：编码 = 引擎 `NodeId::to_bits()`（`(generation << 32) | index`）。
///
/// 与 `accesskit::NodeId(pub u64)` **同构**（后者字段是 pub），因此不需要 node→NodeId 的映射表，
/// 并且天然带代际：陈旧句柄解码后 `tree.get(..)` 失败 ⇒ 该 `ActionRequest` 被忽略。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct A11yNodeId(u64);

impl A11yNodeId {
    /// 从引擎句柄构造。
    #[must_use]
    pub const fn from_engine(node: NodeId) -> Self {
        Self(node.to_bits())
    }

    /// 原始位模式（逐字节与 `accesskit::NodeId` 相同）。
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// 从原始位模式构造（crate 内：解码 `accesskit::NodeId` 时用）。
    pub(crate) const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// 解码回引擎句柄（代际 0 的编码非法 ⇒ `None`）。
    pub(crate) const fn to_engine(self) -> Option<NodeId> {
        NodeId::from_bits(self.0)
    }
}

/// 快照里的一个 a11y 节点（值语义）。
#[derive(Clone, Debug, PartialEq)]
pub struct A11yNode {
    /// a11y 身份。
    pub id: A11yNodeId,
    /// 反向定位到引擎节点（测试 / 诊断的唯一桥梁）。
    pub engine: NodeId,
    /// 角色。
    pub role: A11yRole,
    /// 可访问名（`None` = 无名）。
    pub label: Option<A11yLabel>,
    /// 值（输入框内容 / `Role::Label` 的文本 / 数值控件的文本形态）。
    pub value: Option<SmolStr>,
    /// 补充说明。
    pub description: Option<SmolStr>,
    /// **物理像素**、窗口相对（= `LayoutBox.bounds` 屏幕化后 × `scale`，逐边 round）。
    pub bounds: PhysicalRect,
    /// 支持的动作。
    pub actions: Vec<A11yAction>,
    /// 隐藏（`set_hidden`；只表达**直接**原因，子树继承由 AccessKit 处理）。
    pub hidden: bool,
    /// 禁用（`set_disabled`）。
    pub disabled: bool,
    /// 勾选/开关状态（`set_toggled`；`RadioButton` 的选中态走这里，不是 `selected`）。
    pub checked: Option<bool>,
    /// 选中（`None` = 不表态）。
    pub selected: Option<bool>,
    /// 展开（`None` = 不表态）。
    pub expanded: Option<bool>,
    /// 行/列位置与总数（`None` = 不表态；虚拟列表 / 表格的语义，P2-7）。
    pub row_index: Option<usize>,
    pub column_index: Option<usize>,
    pub row_count: Option<usize>,
    pub column_count: Option<usize>,
    /// 实时区域播报策略（`None` = 不表态，P2-6）。
    pub live: Option<A11yLive>,
    /// 集合内位置与总数（`None` = 不表态）。
    ///
    /// **遵循 AccessKit 语义：`pos_in_set` 是 0-based**（其文档明确 aria-posinset
    /// 是 1-based、本属性是 0-based；Windows/AT-SPI 适配层自己 +1）。`size_of_set`
    /// 应挂在集合**容器**上（consumer 只从祖先上读取）。
    ///
    /// 列表项 / 虚拟列表行用它表达「第 N 项，共 M 项」；表格行仍用 `row_index`。
    pub pos_in_set: Option<usize>,
    pub size_of_set: Option<usize>,
    /// 数值通道（`Slider` / `ProgressIndicator` / `ScrollBar` 等，P2-8）。
    ///
    /// 屏幕阅读器播报的是**数值**（"50%，最大 100"）而不是字符串，且 UIA 的
    /// `RangeValuePattern` 直接读这几个字段；只给 `value` 文本时滑块不可调节也不播报。
    pub numeric_value: Option<f64>,
    pub min_numeric_value: Option<f64>,
    pub max_numeric_value: Option<f64>,
    /// 步进/跳转幅度（AT 的「加一步 / 加大步」用；缺省时 UIA 的 `SmallChange` 为 0）。
    pub numeric_value_step: Option<f64>,
    pub numeric_value_jump: Option<f64>,
    /// 引擎侧的**直接**子节点（未过滤）。
    pub children: Vec<A11yNodeId>,
    /// 父节点。
    pub parent: Option<A11yNodeId>,
}

impl A11yNode {
    /// 可访问名的文本（`None` = 无名）。
    #[must_use]
    pub fn label_text(&self) -> Option<&str> {
        self.label.as_ref().map(A11yLabel::text)
    }
}

/// 一次 a11y 交付的只读结果（`FrameRecord.a11y` 的载荷，也是 CPU 测试的断言面）。
///
/// `full == false` 时 `nodes` 只含**变更节点 + 其祖先链**，因此它描述的是"这一笔增量"，
/// 而不是"整棵树"；整棵树的断言请用 `full == true` 的那一笔。
#[derive(Clone, Debug, PartialEq)]
pub struct A11yTreeSnapshot {
    /// 本次是否发出全树。
    pub full: bool,
    /// 本次发出的节点（祖先先于后代；全树笔是 DFS 先序）。
    pub nodes: Vec<A11yNode>,
    /// 焦点身份（恒存在，见设计 §6.3）。
    pub focus: A11yNodeId,
    /// 焦点节点的记录。
    ///
    /// **独立于 `nodes`**：增量笔里焦点节点通常不在本批（只有它自己变了才会在），
    /// 因此不能靠 `nodes` 里查找（P11 审查 #4）。
    pub focus_node: A11yNode,
    /// 本次交付触发的诊断条数（详情在 `App::a11y_diagnostics`）。
    pub diagnostics: usize,
}

impl A11yTreeSnapshot {
    /// 本次发出的节点。
    #[must_use]
    pub fn nodes(&self) -> &[A11yNode] {
        &self.nodes
    }

    /// 按 a11y 身份查。
    #[must_use]
    pub fn node(&self, id: A11yNodeId) -> Option<&A11yNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    /// 按引擎句柄查。
    #[must_use]
    pub fn node_for(&self, engine: NodeId) -> Option<&A11yNode> {
        self.nodes.iter().find(|node| node.engine == engine)
    }

    /// 焦点身份。
    #[must_use]
    pub fn focus(&self) -> A11yNodeId {
        self.focus
    }

    /// 焦点节点（恒存在：回退链的终点是窗口根，根恒在）。
    ///
    /// 不查 `nodes`（增量笔里焦点通常不在本批），因此**不会 panic**。
    #[must_use]
    pub fn focus_node(&self) -> &A11yNode {
        &self.focus_node
    }

    /// 未被 `hidden` 覆盖的节点：**自身与祖先链**在本快照内都没有 `hidden`。
    ///
    /// 增量的快照只含变更节点 + 祖先，因此「祖先不在本笔里」的节点按可见处理
    /// （平台侧的 `set_hidden` 才是权威；这里给测试一个不撒谎的可见性视图）。
    pub fn visible_nodes(&self) -> impl Iterator<Item = &A11yNode> {
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

    /// 本快照内被标 `hidden` 的节点。
    pub fn hidden_nodes(&self) -> impl Iterator<Item = &A11yNode> {
        self.nodes.iter().filter(|node| node.hidden)
    }

    /// 某个节点的直接子节点（本快照内的记录）。
    #[must_use]
    pub fn children_of(&self, id: A11yNodeId) -> Vec<A11yNodeId> {
        self.node(id)
            .map(|node| node.children.clone())
            .unwrap_or_default()
    }
}

/// 自绘节点的种类（`MissingAltText` 诊断的来源标识）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11yDrawKind {
    /// `NodeKind::Canvas`。
    Canvas,
    /// `NodeKind::Geometry`。
    Geometry,
}

/// a11y 诊断（结构化字段；对齐 `text::TextDiagnostic::FontStackMiss` 的形态）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum A11yDiagnostic {
    /// `Canvas` / `Geometry` 缺替代文本（节点被标 `hidden`，**不**静默丢弃）。
    MissingAltText {
        /// 节点。
        node: NodeId,
        /// 哪一种自绘节点（诊断要能直接指出来源）。
        kind: A11yDrawKind,
    },
    /// 交互节点没有可访问名。
    MissingLabel {
        /// 节点。
        node: NodeId,
    },
    /// `ActionRequest` 的目标陈旧 / 不存在（已忽略）。
    StaleAction {
        /// 映射到的引擎动作（`None` = 该 accesskit 动作本就不映射）。
        action: Option<A11yAction>,
    },
    /// 动作没有映射也没有应用 handler（已忽略）。
    UnhandledAction {
        /// 节点。
        node: NodeId,
        /// 动作（`None` = 该 `accesskit::Action` 本阶段根本没有映射）。
        action: Option<A11yAction>,
    },
    /// 焦点指向未发出的节点（已回退到最近的已发出祖先）。
    MissingFocusTarget {
        /// 原焦点节点。
        node: NodeId,
    },
}

/// a11y 动作的 payload（强类型；替代 `accesskit::ActionData` 的公开面）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum A11yActionData<'a> {
    /// 无 payload。
    None,
    /// 文本值（`SetValue` / `ReplaceSelectedText`）。
    Value(&'a str),
    /// 文本选区（**字符**索引；转为字节/字形归应用）。
    Selection {
        /// 起点。
        anchor: usize,
        /// 活动端。
        focus: usize,
    },
    /// 滚动偏移。
    ScrollOffset(Point),
    /// 数值（UIA `RangeValuePattern.SetValue` 传的就是这个，不是文本值）。
    NumericValue(f64),
    /// 自定义动作 id。
    Custom(i32),
}

/// a11y 动作分发的上下文（handler 的**唯一**入口；形态对齐 `interaction::ActionCtx`）。
pub struct A11yActionCtx<'a> {
    app: &'a mut crate::app::App,
    window: WindowId,
    node: NodeId,
    action: A11yAction,
    data: A11yActionData<'a>,
}

impl<'a> A11yActionCtx<'a> {
    /// 构造（crate 内：`a11y::action` 是唯一生产者）。
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

    /// 所属窗口。
    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    /// 目标节点。
    #[must_use]
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// 动作。
    #[must_use]
    pub fn action(&self) -> A11yAction {
        self.action
    }

    /// payload。
    #[must_use]
    pub fn data(&self) -> A11yActionData<'a> {
        self.data
    }

    /// **非追踪**读（handler 不是渲染 pass，不登记依赖）。
    pub fn read<T: Clone + 'static>(&self, signal: &crate::node::signal::Signal<T>) -> T {
        self.app.read(signal)
    }

    /// 写状态（`App::write` 的转调：等值短路 + 依赖反查 + 唤醒）。
    pub fn write<T: Clone + PartialEq + 'static>(
        &mut self,
        signal: &crate::node::signal::Signal<T>,
        value: T,
    ) -> bool {
        self.app.write(signal, value)
    }

    /// 其余能力（滚动 / 焦点 / 浮层）走 `&mut App`。
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

pub(crate) mod action;
pub(crate) mod adapter;
pub(crate) mod mapping;
pub(crate) mod tree;
