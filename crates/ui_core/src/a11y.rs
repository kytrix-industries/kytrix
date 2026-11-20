//! 无障碍（AccessKit）契约：角色、节点、补丁、动作与快照。
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
/// 只包含引擎**真实会产生或应用会显式声明**的角色（表头 / 文本运行 / 表格三级都在内）；
/// 不含 `Unknown`，也不含 130+ 个 HTML/Doc 角色 —— 加变体必须同时给 `a11y::mapping` 的穷举
/// `match` 补分支（编译期强制，不会静默落到 `Unknown`），并把变体加进 [`A11yRole::ALL`]
/// （`every_role_is_included_exactly_once_in_all` 用它做编译期锚定；不要在任何地方写死角色个数）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11yRole {
    /// 窗口根（`TreeInfo.root`）。
    Window,
    /// 应用本身（少见，显式声明时用）。
    Application,
    /// 纯布局容器。AccessKit 消费端会把它当作 `none`/`presentation` 语义忽略，但引擎仍会为
    /// 它发节点（带宽成本）—— 这条注释以前写成"子节点上提到祖父"，实现里从未做过（R13-15），
    /// 现在按事实描述：**引擎不做上提**，`children` 只过滤 `hidden`，不折叠容器。
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
    ///
    /// 表格语境下的 `row_index` 由引擎从"在父节点 children 里的序号"**自动推导**（应用显式
    /// 声明优先），`row_count` 由表节点的行数推导 —— 交易表格不必逐行手填。
    Row,
    /// 单元格（表格 / 网格里的格子，配合 `row_index` / `column_index`）。
    ///
    /// `column_index` / `row_index` 同样自动推导：列 = 在父行 children 里的序号，行 = 父行的行号。
    Cell,
    /// 列头单元格（表格的 `<th>`；`aria-sort` 挂在它上面）。
    ///
    /// UIA 的 `ITableItemProvider::GetColumnHeaderItems` / AT-SPI 的 `getColumnHeader` 由角色
    /// 推导出来，没有这个角色时列名永远到不了 AT（R13-3）。
    ColumnHeader,
    /// 行头单元格（表格最左列的行标题）。
    RowHeader,
    /// 文本运行（一段可被逐字符/逐词选择的文本；`character_lengths` / `word_starts` /
    /// `text_selection` 的唯一合法宿主，见 AccessKit 的 `TextPosition.node` 约束）。
    ///
    /// 引擎**自动**把"父节点角色是 `TextInput`/`MultilineTextInput` 的文本节点"推导成它。
    TextRun,
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

impl A11yRole {
    /// 全部角色（**唯一**的"角色全集"来源）。
    ///
    /// 这个常量是穷举测试的锚点：`every_role_is_included_exactly_once_in_all`
    /// （`a11y::mapping`）用一个**穷举 `match`** 给每个变体计数并与本数组求和比对 ——
    /// 少列一个变体 ⇒ 求和偏小 ⇒ 测试红；加一个变体忘了加进 `ALL` ⇒ 编译期
    /// `non-exhaustive patterns` 先报错。这正是 R13-11 要求的"手写数组不再假绿"。
    pub const ALL: [Self; 32] = [
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
        Self::Row,
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
        Self::Tooltip,
    ];
}

/// 表格表头的排序状态（`aria-sort` 的等价物；`accesskit::SortDirection` 的子集）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum A11ySort {
    /// 升序。
    Ascending,
    /// 降序。
    Descending,
    /// 可排序但当前未排序（`aria-sort="none"`）。
    None,
}

/// 文本选区（**UTF-8 字节偏移**，落在文本自身；引擎在 a11y 层换算成 grapheme 下标）。
///
/// 退化选区（`anchor == focus`）就是 caret。偏移不必落在 char boundary：换算时向下吸附到
/// 所在 grapheme 的起点（AT 传进来的位置来自它自己维护的字符表，可能是旧快照）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct A11yTextSelection {
    /// 选区起点（不随扩展/收缩改变）。
    pub anchor: usize,
    /// 活动端（无选区时 = caret 位置）。
    pub focus: usize,
}

/// 入向文本选区的**一个端点**：宿主 `TextRun` 节点 + 该节点文本内的 **UTF-8 字节偏移**。
///
/// 语义对齐 `accesskit::TextPosition { node: NodeId, character_index: usize }`：AccessKit 要求
/// `TextPosition.node` 指向 `Role::TextRun`（`accesskit_consumer::text` 会丢弃节点不是 TextRun
/// 的选区），所以「位置属于哪个 run」是 AT 契约的一部分，引擎在应用边界必须保留它 ——
/// 输入框有多个 `TextRun` 子节点时，应用无法从裸字节区间还原「从 run A 的哪个字节到 run B 的
/// 哪个字节」。单 `TextRun` 输入框里两个端点指向同一个 run，调用方无需额外分支。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct A11yTextPosition {
    /// 位置所属的 `TextRun` 节点（AT 侧 `TextPosition.node` 的引擎口径）。
    ///
    /// **只在本次 handler 调用内有效**：它是引擎句柄（含 generation/scope），跨帧或跨快照保存后
    /// 会失效（查询得到 `StaleNode`/查不到，不会 UB 但会静默错）。**当前没有公开的跨帧映射
    /// API**，应用不要跨帧保存它；需要稳定身份时按自己的业务 id 记录。
    pub node: NodeId,
    /// 该 `TextRun` 文本内的 UTF-8 字节偏移（落在 char boundary；越界夹紧到该 run 的文本末尾）。
    pub byte_index: usize,
}

/// 一个自定义动作（`accesskit::CustomAction` 的 id + 给 AT 读的名字）。
///
/// `A11yAction::Custom(id)` 只是"能力"（`Copy + Eq + Hash` 的 handler 键）；要真正出现在
/// 读屏器的动作列表里，必须用本结构注册 id 与人类可读名字（R13-4）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct A11yCustomAction {
    /// 动作 id（与 `A11yAction::Custom(id)` 对应）。
    pub id: i32,
    /// 给 AT 读的动作名（"加入自选" / "取消订单"）。
    pub label: SmolStr,
}

/// 滚动容器的 a11y 状态（`accesskit::Node` 的 `scroll_*` 属性组）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct A11yScroll {
    /// 水平偏移（逻辑像素，向 AT 报告的是**物理像素**）。
    pub x: f32,
    /// 垂直偏移。
    pub y: f32,
    /// 水平可用范围上界（`content - viewport`，`>= 0`）。
    pub x_max: f32,
    /// 垂直可用范围上界。
    pub y_max: f32,
    /// 轴向（`true` = 垂直）。
    pub vertical: bool,
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
/// 与 [`A11yPatch`] 同构：静态给默认值、动态覆盖（`patch` > `over` > 由节点种类/结构推导）。
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
    /// **稳定身份键**（与 `A11yPatch::key` 同义）：节点被删掉重建时用它把 a11y id 固定下来。
    ///
    /// 引擎对虚拟列表的行**自动**生成键（容器 + 条目下标）；只有"自己管理节点生命周期、
    /// 又希望 AT 认为它是同一个元素"的应用才需要显式给（例如应用层虚拟化的表格）。
    pub key: Option<u64>,
    /// 表头排序状态（`aria-sort`）。
    pub sort: Option<A11ySort>,
    /// 文本选区 / caret（UTF-8 字节偏移；**只对 `TextRun` 节点有效** —— AccessKit 的
    /// `TextPosition.node` 必须是 `Role::TextRun`）。
    pub selection: Option<A11yTextSelection>,
    /// 自定义动作（id + 名字）：`A11yAction::Custom(id)` 的能力登记。
    pub custom_actions: Vec<A11yCustomAction>,
    /// 只读（`accesskit::Node::set_read_only`）。
    pub read_only: bool,
    /// 模态（`set_modal`；对话框/浮层用）。
    pub modal: bool,
    /// 忙碌（`set_busy`；异步加载中的区域）。
    pub busy: bool,
    /// 非法值（`set_invalid`；表单校验失败的字段）。
    pub invalid: bool,
    /// 错误信息节点（`set_error_message` 收的是**节点关系**：指向展示错误文本的节点）。
    ///
    /// AccessKit 的 `ErrorMessage` 是 `NodeId` 关系（对应 `aria-errormessage`），不是字符串；
    /// 引擎在构建时把引擎句柄换算成稳定 a11y id（目标已卸载则丢弃）。
    pub error_message: Option<NodeId>,
    /// 占位符（`set_placeholder`；输入框的预期输入提示）。
    pub placeholder: Option<SmolStr>,
    /// 标题层级（`set_level`；`Heading` 用）。
    pub level: Option<usize>,
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
    /// **稳定身份键**（与 `A11yOverride::key` 同义；虚拟列表行由引擎自动生成）。
    pub key: Option<u64>,
    /// 表头排序状态（`aria-sort`）。
    pub sort: Option<A11ySort>,
    /// 文本选区 / caret（UTF-8 字节偏移）。
    ///
    /// **只能打在 `TextRun` 节点上**（E4a 契约）：AccessKit 的 `TextPosition.node` 必须是
    /// `Role::TextRun`，打在输入框/普通文本节点上会被 consumer 静默丢弃。输入框的选区由它的子
    /// TextRun 承载。
    pub selection: Option<A11yTextSelection>,
    /// 自定义动作（id + 名字）。
    pub custom_actions: Option<Vec<A11yCustomAction>>,
    /// 只读。
    pub read_only: Option<bool>,
    /// 模态。
    pub modal: Option<bool>,
    /// 忙碌。
    pub busy: Option<bool>,
    /// 非法值。
    pub invalid: Option<bool>,
    /// 错误信息节点（节点关系；目标必须在树里，否则本笔丢弃）。
    pub error_message: Option<NodeId>,
    /// 占位符。
    pub placeholder: Option<SmolStr>,
    /// 标题层级。
    pub level: Option<usize>,
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

/// a11y 侧的节点身份：由 [`identity`] 的分配器发放的**稳定 id**（`1..`，永不复用）。
///
/// 与渲染 `NodeId` **解耦**：`NodeId` 编码的是 arena `(generation, index)`，槽位复用与虚拟列表
/// 回收都会变；AT 依赖"同一语义元素 = 同一 id"，因此 a11y 身份必须跨重建稳定（审查 §5.7 /
/// R13-1）。反查表（稳定 id → 引擎句柄）在 `A11yBridge` 的分配器里，`ActionRequest` 解码走它，
/// 陈旧 / 未挂载的 id 返回 `None` ⇒ `StaleAction` 诊断。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct A11yNodeId(u64);

impl A11yNodeId {
    /// 从分配器发放的稳定 id 构造（`a11y::identity` 是唯一生产者）。
    #[must_use]
    pub(crate) const fn from_stable(id: u64) -> Self {
        Self(id)
    }

    /// 原始位模式（逐字节与 `accesskit::NodeId` 相同）。
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
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
    /// 表头排序状态（`aria-sort`；`None` = 不表态）。
    pub sort: Option<A11ySort>,
    /// 滚动状态（`ScrollView` / 虚拟列表容器；`None` = 不是滚动容器）。
    pub scroll: Option<A11yScroll>,
    /// 该节点裁剪后代（`Clip::Bounds` 的滚动容器 / 裁剪盒）：`set_clips_children`。
    pub clips_children: bool,
    /// 自定义动作注册表（id + 名字；`to_accesskit` 用它写 `push_custom_action`）。
    pub custom_actions: Vec<A11yCustomAction>,
    /// 文本的**逐字符（grapheme）UTF-8 字节长度**（`accesskit::Node::character_lengths`）。
    ///
    /// 仅在该节点的 `value` 就是这段文本时有意义（`Label` / `TextRun` / 文本输入框）：
    /// `Σ character_lengths == value.len()` 是 AccessKit 的硬约束。
    pub character_lengths: Vec<u8>,
    /// 每个词的起始字符下标（`accesskit::Node::word_starts`；升序，`u8` 上限见 AccessKit）。
    pub word_starts: Vec<u8>,
    /// 逐 grapheme 的 caret 位置（**物理像素、节点 bounds 原点起算的 inline 轴坐标**；
    /// `accesskit::Node::character_positions`，E4b）。
    ///
    /// 只在能正确表达时非空：`TextRun` + 已 shape 且与当前文本一致 + **单行 LTR**。空 = AT
    /// 回退（无 caret 高亮），引擎绝不发近似/错行的值。
    pub character_positions: Vec<f32>,
    /// 逐 grapheme 的 advance 宽度（物理像素；`accesskit::Node::character_widths`，E4b）。
    ///
    /// 与 [`Self::character_positions`] 等长；`text_direction` 由 `to_accesskit` 在非空时写
    /// `LeftToRight`。
    pub character_widths: Vec<f32>,
    /// 文本选区 / caret（**grapheme 下标**，已由字节偏移换算；`None` = 不表态）。
    pub text_selection: Option<A11yTextSelection>,
    /// 只读（`set_read_only`）。
    pub read_only: bool,
    /// 模态（`set_modal`）。
    pub modal: bool,
    /// 忙碌（`set_busy`）。
    pub busy: bool,
    /// 非法值（`set_invalid`）。
    pub invalid: bool,
    /// 错误信息节点（`set_error_message` 的 NodeId 关系）。
    pub error_message: Option<A11yNodeId>,
    /// 占位符（`set_placeholder`）。
    pub placeholder: Option<SmolStr>,
    /// 标题层级（`set_level`）。
    pub level: Option<usize>,
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
    /// 窗口根的稳定 id（`TreeInfo.root` 用它；根恒在树里）。
    pub root: A11yNodeId,
    /// 本次发出的节点（祖先先于后代；全树笔是 DFS 先序）。
    pub nodes: Vec<A11yNode>,
    /// 焦点身份（恒存在，见设计 §6.3）。
    pub focus: A11yNodeId,
    /// 焦点节点的记录。
    ///
    /// **独立于 `nodes`**：增量笔里焦点节点通常不在本批（只有它自己变了才会在），
    /// 因此不能靠 `nodes` 里查找（P11 审查 #4）。
    pub focus_node: A11yNode,
    /// 焦点节点的祖先记录（根 → 直接父节点），只在焦点节点不在本批时非空。
    ///
    /// AccessKit 的 `TreeUpdate` 要求每个节点要么已在消费端树里、要么是本笔某个节点的
    /// `children`。焦点节点刚挂载时还必须把父链一起重发（父的 `children` 才会包含它），
    /// 否则 `accesskit_consumer` 会把它归入 `pending_nodes` 并 panic。
    pub focus_ancestors: Vec<A11yNode>,
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

    /// 引擎句柄 → 本快照里的稳定 id（查 `nodes` 与独立的 `focus_node`）。
    ///
    /// 测试与诊断从引擎句柄出发断言 a11y 语义时用它，而不是自己编码 id
    /// （身份由 `a11y::identity` 分配，编码细节不是公开契约）。
    #[must_use]
    pub fn id_for_engine(&self, engine: NodeId) -> Option<A11yNodeId> {
        self.nodes
            .iter()
            .find(|node| node.engine == engine)
            .map(|node| node.id)
            .or_else(|| (self.focus_node.engine == engine).then_some(self.focus_node.id))
    }

    /// 本批里某角色的全部节点（语义断言用："所有 `Cell` 的列号必须从 0 连续"）。
    pub fn nodes_with_role(&self, role: A11yRole) -> impl Iterator<Item = &A11yNode> {
        self.nodes.iter().filter(move |node| node.role == role)
    }

    /// 未被 `hidden` 覆盖的节点：**自身与祖先链**在本快照内都没有 `hidden`。
    ///
    /// 名字里的 `in_batch` 是刻意的（R13-28）：增量的快照只含变更节点 + 祖先，因此
    /// 「祖先不在本笔里」的节点按可见处理 —— 这个视图描述的是**本批**，不是整棵树。整棵树的
    /// 可见性断言请用 `full == true` 的那一笔，或直接用平台侧的 `set_hidden` 语义。
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
///
/// `#[non_exhaustive]`：诊断是**观测/日志面**，会随实现补充变体（E3/E4 一次新增了
/// `ReadOnlyAction` / `UnadvertisedAction` / `InvalidActionData` 三个）；外部只应
/// `matches!` / `match ... { .. }` 处理自己关心的变体，不依赖穷举 —— 与 `A11yLive` 同风格。
/// （`EngineError` 刻意保持穷举：调用方要按错误类型决定处理策略，诊断不参与控制流。）
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
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
    /// 两个**同时存活**的节点声明了同一个稳定身份键（`A11yOverride::key` / `A11yPatch::key`）。
    ///
    /// 后到的节点拿新 id 并保留本诊断（不顶替前者：顶替会让 AT 把两行当成同一行）。
    DuplicateKey {
        /// 声明冲突键的节点。
        node: NodeId,
    },
    /// 节点宣告了 `A11yAction::Custom(id)`，但没有登记对应的 `A11yCustomAction { id, label }`。
    ///
    /// 这种"半残动作"在 AT 侧不可见（AccessKit 只暴露 `CustomAction` 类别，动作名来自
    /// `push_custom_action`），旧实现完全静默（R13-4）。
    UnregisteredCustomAction {
        /// 节点。
        node: NodeId,
        /// 未登记的 id。
        id: i32,
    },
    /// 对**禁用**节点发来的会改变状态的动作（已忽略，R13-16）。
    ///
    /// 宣告 `disabled` 后 AT 仍可能发 `Click`（NVDA 的"激活"）；引擎必须在唯一入口挡下，
    /// 而不是让应用 handler 真的下单。
    DisabledAction {
        /// 节点。
        node: NodeId,
        /// 被拒绝的动作。
        action: A11yAction,
    },
    /// 对**只读**节点发来的值变更动作（`SetValue` / `Increment` / `Decrement`；已忽略，E3）。
    ///
    /// `read_only` 是独立于 `disabled` 的状态：只读输入框对 AT 仍宣告可聚焦/可选中，但不宣告
    /// 也不执行值变更。应用要区分"禁用"（播报 unavailable）与"只读"（播报 read-only）时靠它。
    ReadOnlyAction {
        /// 节点。
        node: NodeId,
        /// 被拒绝的动作。
        action: A11yAction,
    },
    /// 动作**不在节点的能力集**里（未在 `A11yOverride/A11yPatch::actions` 声明、角色默认表
    /// 也没有；已忽略，E3）。
    ///
    /// 能力集是 AT 动作的唯一信任边界：能力集里没有的动作，应用 handler 也收不到。
    UnadvertisedAction {
        /// 节点。
        node: NodeId,
        /// 被拒绝的动作。
        action: A11yAction,
    },
    /// 动作的 payload 无法按引擎口径解释（已忽略，E4a）。
    ///
    /// 典型：`SetTextSelection` 的 `TextPosition.node` 既不是存活的 `TextRun`、目标也不能回退
    /// 成 TextRun（AT 拿的是旧快照 / 多 run 定位失败）。此时**不能**把含糊的字符索引交给应用
    /// （CJK/emoji 下会错位改写文本），只能拒绝并留诊断。
    InvalidActionData {
        /// 节点。
        node: NodeId,
        /// 动作。
        action: A11yAction,
    },
}

/// a11y 动作的 payload（强类型；替代 `accesskit::ActionData` 的公开面）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum A11yActionData<'a> {
    /// 无 payload。
    None,
    /// 文本值（`SetValue` / `ReplaceSelectedText`）。
    Value(&'a str),
    /// 文本选区（**UTF-8 字节偏移 + 端点宿主**，E4a 字节口径 / E4b 跨 run 身份）。
    ///
    /// 两端各自描述「输入框里哪个 `TextRun` 的哪个字节」（[`A11yTextPosition`]）：AT 的
    /// `TextPosition` 带节点身份，引擎归一化时只把**动作目标**折成输入框（`ctx.node()`），
    /// 位置本身的宿主身份必须保留下来 —— 多 `TextRun` 的输入框（分段样式/富文本）里，
    /// 「anchor 在 run A、focus 在 run B」是应用还原选区的唯一依据。
    ///
    /// 目标语义：Windows 的 `SetTextSelection` 打在 `TextRun` 上、macOS/AT-SPI 打在输入框上；
    /// 引擎把 `ctx.node()` 归一为**输入框节点**（三端一致），应用无需关心平台差异。
    Selection {
        /// 起点（不随扩展/收缩改变）。
        anchor: A11yTextPosition,
        /// 活动端（无选区时 = caret 位置）。
        focus: A11yTextPosition,
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

    /// **非追踪**读（handler 不是渲染 pass，不登记依赖），返回读守卫（零拷贝）。
    ///
    /// 借用规则见 `node::signal` 模块文档：守卫不借用 `A11yActionCtx`，但存活期间对同一信号的
    /// 写会 panic（显式借用冲突，不静默拷贝）。
    pub fn read<T: 'static>(
        &self,
        signal: &crate::node::signal::Signal<T>,
    ) -> crate::node::signal::SignalRef<T> {
        self.app.read(signal)
    }

    /// 写状态（`App::write` 的转调：等值短路 + 依赖反查 + 唤醒）。
    ///
    /// # Panics
    /// 该信号存在活跃读守卫。
    pub fn write<T: Clone + PartialEq + 'static>(
        &mut self,
        signal: &crate::node::signal::Signal<T>,
        value: T,
    ) -> bool {
        self.app.write(signal, value)
    }

    /// 原地更新状态（`App::update` 的转调：无等值短路）。
    ///
    /// # Panics
    /// 该信号存在活跃读守卫。
    pub fn update<T: 'static>(
        &mut self,
        signal: &crate::node::signal::Signal<T>,
        update: impl FnOnce(&mut T),
    ) -> bool {
        self.app.update(signal, update)
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
pub(crate) mod identity;
pub(crate) mod mapping;
pub(crate) mod textrun;
pub(crate) mod tree;
