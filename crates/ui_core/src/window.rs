//! 窗口：句柄、建窗参数、节点树 + 绘制场景的持有者。
//!
//! 窗口参数（[`WindowOptions`]）在 headless 测试里只保存；真机上由平台层在创建原生窗口
//! 时消费（初始位置 / 最小最大尺寸），运行时变化经 `Event::Resized` / `Event::Moved`
//! 回流到应用。

mod a11y;
mod focus;
mod frame;
mod frame_budget;
mod paint;
mod record;
mod scale;
mod surface;
#[cfg(test)]
mod tests;

use std::{
    cell::Cell,
    collections::VecDeque,
    rc::Rc,
    time::{Duration, Instant},
};

use smol_str::SmolStr;

use self::{
    frame::{FrameScheduler, FrameState},
    surface::covering_surface,
};
#[cfg(feature = "test-support")]
use crate::paint::instance::InstanceBuffer;
#[cfg(any(target_os = "linux", test))]
use crate::render::present_evidence::clamp_presented_at;
#[cfg(feature = "test-support")]
use crate::test_support::record::{FrameRecord, FrameStats};
use crate::{
    error::EngineError,
    interaction::{FocusScope, PointerState},
    layout::LayoutReport,
    math::{PhysicalRect, Point, Rect, Scale, Size},
    node::{id::NodeId, tree::NodeTree},
    paint::{
        atlas::{AtlasKind, MAX_IMAGE_PAGES},
        color::{Color, ColorResolver},
        pass::PaintReport,
        scene::Scene,
    },
    render::{
        backend::Renderer, compose::WindowComposer, compositor::Plan, damage::DamageAccumulator,
        delta::SceneDelta, timer::FrameTimings,
    },
};

/// 窗口句柄：单调递增、**永不复用**（陈旧 id 落回 `None`，因此不需要代际）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowId(u32);

impl WindowId {
    /// crate 内构造（消费者：`App::create_window`）。
    ///
    /// **0 不是合法句柄**：`App::create_window` 从 1 起单调递增，`WindowId(0)` 永不产生
    /// （`Option<WindowId>` 因此可以靠 `get` 返回 `None` 表达「窗口不存在」，无需哨兵值）。
    pub(crate) fn from_raw(value: u32) -> Self {
        debug_assert!(value != 0, "WindowId 0 是非法句柄");
        Self(value)
    }

    /// 原始值（诊断/日志）。
    #[must_use]
    pub const fn to_bits(self) -> u32 {
        self.0
    }

    /// 仅测试：直接构造。
    #[cfg(test)]
    pub(crate) const fn for_test(value: u32) -> Self {
        Self(value)
    }
}

/// Linux/wayland 客户端装饰（自绘标题栏）的窗口控制请求。
///
/// 其它平台有原生标题栏，不需要应用发这些请求；本类型只在 Linux 构建里存在。
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowControl {
    /// 最小化（`xdg_toplevel.set_minimized`）。
    Minimize,
    /// 在最大化与还原之间切换（`set_maximized` / `unset_maximized`）。
    ToggleMaximize,
    /// 进入 / 退出全屏（`set_fullscreen` / `unset_fullscreen`）。
    ///
    /// 最终状态以 compositor 的 `configure` 为准（`xdg_toplevel` 的 `Fullscreen` 状态位）：
    /// 只发请求不假设结果，两者之间可能被策略拒绝（compositor 忽略请求）。
    ToggleFullscreen,
    /// 开始交互式移动（`xdg_toplevel._move(seat, serial)`）。
    BeginMove,
    /// 开始交互式缩放（`xdg_toplevel.resize(seat, serial, edge)`）。
    BeginResize(WindowResizeEdge),
}

/// Linux：compositor 侧的窗口状态镜像（应用自绘标题栏的唯一数据源）。
///
/// Wayland 的窗口状态**只由 compositor 决定**（`configure` 的状态位与 `wm_capabilities`），
/// 应用不能直接假设：请求了最大化不等于真的最大化了。本结构由平台层每轮泵写入，应用在画
/// 标题栏时读 [`Window::state`]：
///
/// - `client_decoration == false` ⇒ compositor 要求服务端装饰（SSD）：**不要画标题栏**，
///   否则屏幕上出现两条（R10-4）；此时客户区就是整个 surface。
/// - `can_maximize` / `can_minimize` / `can_fullscreen` ⇒ 对应按钮是否可用（不可用时隐藏或
///   禁用，否则用户点了没反应，R10-15）。
/// - `maximized` / `fullscreen` ⇒ 当前真实状态（按钮应显示「还原 / 退出全屏」）。
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowState {
    /// compositor 是否要求**客户端**装饰（`true` = 自绘标题栏）。
    pub client_decoration: bool,
    /// 当前是否最大化。
    pub maximized: bool,
    /// 当前是否全屏。
    pub fullscreen: bool,
    /// compositor 是否支持最大化请求。
    pub can_maximize: bool,
    /// compositor 是否支持最小化请求。
    pub can_minimize: bool,
    /// compositor 是否支持全屏请求。
    pub can_fullscreen: bool,
}

#[cfg(target_os = "linux")]
impl Default for WindowState {
    fn default() -> Self {
        // 未收到 `wm_capabilities` 时的保守默认：假定全部支持（与旧行为一致，不会凭空禁用
        // 按钮）；装饰默认为客户端自绘（引擎的一贯行为）。
        Self {
            client_decoration: true,
            maximized: false,
            fullscreen: false,
            can_maximize: true,
            can_minimize: true,
            can_fullscreen: true,
        }
    }
}

/// Linux/Wayland 文本输入的语义类型（`zwp_text_input_v3.set_content_type`）。
///
/// 只有 Linux 需要显式声明：text-input-v3 的默认值是 `hint = none` / `purpose = normal`，
/// 输入法据此对**密码框**做拼写检查、候选联想与**用户词库学习** —— 对交易终端的 API key /
/// 密码输入框而言这是安全问题（密钥会被写进用户词库、出现在候选里）。声明为
/// [`Self::Password`] / [`Self::Pin`] 会同时打开 `sensitive_data` 与 `hidden_text` 两个 hint。
///
/// Windows（IMM32）与 macOS（`NSTextInputClient`）由原生控件语义表达，平台层没有对应请求，
/// 因此本类型只在 Linux 上有消费者（[`Window::set_ime_content_type`]，`#[cfg(target_os =
/// "linux")]`）。类型本身三平台可见：它同时是平台层映射表（`platform::ime`）的输入，而映射表
/// 的纯逻辑必须在开发机（Windows）上可单测。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ImeContentType {
    /// 普通文本（`purpose = normal`，无 hint）。
    #[default]
    Text,
    /// 密码 / API key（`purpose = password` + `sensitive_data | hidden_text`）。
    Password,
    /// 纯数字密码（`purpose = pin` + `sensitive_data | hidden_text`）。
    Pin,
    /// 金额 / 数量（`purpose = number`，不敏感，允许小数与符号）。
    Number,
}

/// 自绘标题栏的缩放边（与 `xdg_toplevel.resize` 的边一一对应）。
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowResizeEdge {
    /// 上边。
    Top,
    /// 下边。
    Bottom,
    /// 左边。
    Left,
    /// 右边。
    Right,
    /// 左上角。
    TopLeft,
    /// 右上角。
    TopRight,
    /// 左下角。
    BottomLeft,
    /// 右下角。
    BottomRight,
}

/// 建窗参数（平台层在创建原生窗口时消费）。
///
/// 三个平台创建的窗口默认都是**可关闭 / 最小化 / 最大化 / 可缩放**的：Windows 用
/// `WS_OVERLAPPEDWINDOW` 的系统标题栏，macOS 保留三个原生交通灯，Linux/Wayland 请求
/// 客户端装饰并由应用自绘标题栏（`WindowControl` 提供按钮/拖动所需的平台请求）。
///
/// # 坐标语义（三端统一）
///
/// [`Self::position`] / [`Self::size`] / [`Self::min_size`] / [`Self::max_size`] 全部是
/// **逻辑像素**（与 [`Self::scale`] 同域，DPI 换算由平台层完成）：
///
/// - 原点 = **虚拟桌面内容区左上角**，x 向右、y 向下；
/// - Windows：`(虚拟屏幕左上角为原点的物理坐标) / (窗口所在显示器的 scale)`；混合 DPI
///   跨屏恢复时按窗口最终落点显示器的 scale 解释（误差 ≤1 逻辑像素）；
/// - macOS：AppKit 屏幕坐标的原点在主屏**左下角**、y 向上，平台层换算成上面的一致语义
///   （应用无需感知 AppKit 的翻转）；
/// - Linux/Wayland：`xdg_toplevel` **没有定位请求**，[`Self::position`] 被忽略（窗口位置
///   完全由 compositor 决定，`Event::Moved` 在该端永不产生）；`min_size`/`max_size` 走
///   `xdg_toplevel.set_min_size/set_max_size` 的 compositor 提示语义（不保证被强制执行）。
///
/// # 窗口层级（X1）
///
/// [`Self::owner`] / [`Self::modal`] / [`Self::always_on_top`] 表达窗口之间的层级关系
/// （owned 窗口、owner 的模态窗口、置顶窗口）。三端能力不同（Wayland 没有 owner/模态/
/// 置顶的跨合成器协议），每个字段的文档写明**原生路径与降级语义** —— 引擎不得静默忽略。
///
/// # Windows 标题栏 / 非客户区交互
///
/// **非客户区全部交系统**（R07-2）：`WM_NCCALCSIZE` / `WM_NCHITTEST` / `WM_NCLBUTTONDOWN`
/// 默认都不改写 —— 拖动标题栏、双击最大化/还原、右键系统菜单、`Alt+Space`、**Aero Snap /
/// Win11 Snap Layouts** 与边框拖拽都由系统的 move / size 模态循环提供（Chrome / GPUI / Qt
/// 同样不接管它们）。唯一的例外是应用显式声明的自绘标题栏拖动区（X2
/// [`Window::set_drag_regions`]）：命中区域的 `WM_NCHITTEST` 返回 `HTCAPTION`，让自绘标题栏
/// 复用系统标题栏的同一套拖动语义；未声明时非客户区行为完全不变。
///
/// 系统 size 模态循环会占住本线程消息泵，但**重排 + 重绘不会停**：模态循环把 `WM_SIZE`
/// 同步分派到 `WndProc`，平台层在每条 `WM_SIZE` 上直接更新客户区、对齐 DirectComposition
/// 几何并跑一帧完整帧（`platform::windows::event_loop::ResizeFrameDriver`）—— 拖动/缩放中
/// 每一步都是**真实帧**（不是拉伸旧帧），且不引入定时器或额外帧。系统 move 模态循环
/// （拖动标题栏）**不产生 `WM_SIZE`**，由 `WM_APP_FRAME_WAKE` 驱动的模态 tick 出帧
/// （`platform::windows::event_loop::ModalTick`），两者共用同一套「任务 → 钩子 → 全窗口帧」
/// 纪律。
#[derive(Clone, Debug, PartialEq)]
pub struct WindowOptions {
    /// 标题（`SmolStr`，AGENTS §2.2）。
    pub title: SmolStr,
    /// 初始逻辑尺寸。
    pub size: Size,
    /// 初始客户区左上角位置（逻辑像素，坐标语义见 [`WindowOptions`] 的「坐标语义」一节）。
    ///
    /// `None` = 由平台决定（Windows 保留历史默认 `(100,100)`；macOS 由平台按 26 point 级联摆放；
    /// Wayland 由 compositor 决定）。应用若需要稳定的多窗口摆放，应显式传 `position` 并自己
    /// 持久化（placement 归应用层）。
    ///
    /// 平台层会把位置 **clamp 到最近显示器的工作区**（至少保留标题栏/一条边可见，避免
    /// 显示器拔掉后窗口恢复到不可达位置）。placement 的**持久化归属应用层**：应用从
    /// [`crate::event::Event::Moved`] / [`crate::event::Event::Resized`] 记录，下次启动填回
    /// 这里；引擎不落盘（AGENTS §1.2）。
    pub position: Option<Point>,
    /// 客户区最小逻辑尺寸（`None` = 平台默认/不额外约束）。
    ///
    /// - Windows：`WM_GETMINMAXINFO` 的 `ptMinTrackSize`（系统再补上边框/标题栏）；
    /// - macOS：`NSWindow.contentMinSize`；
    /// - Linux：`xdg_toplevel.set_min_size` —— compositor 提示，不保证被执行。
    ///
    /// 同时设置 `min_size`/`max_size` 且 `min > max` 时引擎先 clamp 成 `min = max`
    /// （与 Qt 的 `setMinimumSize/setMaximumSize` 互相钳制同义），平台层按钳制后的值应用。
    pub min_size: Option<Size>,
    /// 客户区最大逻辑尺寸（`None` = 平台默认/不额外约束）；平台对应关系同 [`Self::min_size`]。
    pub max_size: Option<Size>,
    /// 父/属主窗口（owner，X1）。`None` = 无属主的顶层独立窗口（默认）。
    ///
    /// owner 不是「客户区父子关系」：owned 窗口仍是顶层窗口，只是**永远位于 owner 之上**
    /// （系统 z 序）、owner 最小化时一并隐藏/最小化、owner 销毁时 owned 窗口一并销毁。
    /// 模态（[`Self::modal`]）要求 owner 存在。
    ///
    /// 三端语义（用户裁决 X1：无原生能力的端**必须写清降级**，不得静默忽略）：
    /// - **Windows**：`CreateWindowExW` 的 `hWndParent`（owner 语义；不设 `WS_CHILD`）。
    /// - **macOS**：`addChildWindow:ordered:`（child window / transient parent 语义）。
    /// - **Linux/Wayland**：`xdg_toplevel.set_parent` 只是「transient for」提示（compositor
    ///   可忽略、且无法表达「随 owner 最小化/销毁」），**降级**：仅提交 set_parent 提示，
    ///   不保证 z 序或生命周期联动；文档与 `Event` 不假设 owner 语义成立。
    ///
    /// 生命周期：owner 必须已存在（同一 `App` 内先创建的窗口）。创建时 owner 不存在/已关闭
    /// ⇒ 按 `None` 处理并写诊断（不 panic；与「窗口句柄永不复用」一致，陈旧 id 不会误绑新窗口）。
    pub owner: Option<WindowId>,
    /// 是否为 owner 的**模态**窗口（X1）。
    ///
    /// 模态窗口打开期间，**直接 owner**（`owner` 字段指向的窗口）**不接收输入**，直到模态窗口
    /// 关闭。拦截面与 Windows 原生 `EnableWindow(owner, FALSE)` 一致：只作用于 owner 本身，
    /// **不**连带 owner 拥有的其它 owned 窗口（它们不是 `WS_CHILD`）。模态必须是 owned 窗口
    /// （[`Self::owner`] 为 `Some`）；`modal == true` 而 `owner == None`
    /// ⇒ 降级为普通窗口并写诊断（不 panic）。
    ///
    /// 平台语义（用户裁决 X1）：
    /// - **Windows**：原生路径 —— 打开时 `EnableWindow(owner, FALSE)` 禁用 owner（系统级输入
    ///   拦截 + 灰显），关闭/销毁时恢复；同时引擎层按同一规则过滤（UI 线程兜底，防止
    ///   DComp/自定义消息绕过）。
    /// - **macOS**：原生路径（RB-07，用户批准方案 A）—— 打开时对 owner 的 `NSWindow` 施加
    ///   原生拦截：`setIgnoresMouseEvents:YES`（owner 的内容区与红点/标题栏都不再响应鼠标）、
    ///   禁用红点/最小化/缩放、关闭背景拖动；模态窗口用 `NSModalPanelWindowLevel` 且作为
    ///   owner 的 child window。模态窗口显示时 `makeKeyAndOrderFront:` 接管键盘焦点，之后
    ///   若 owner 通过 AppKit 窗口循环抢回 key，泵每轮把焦点拿回模态窗口（否则键盘事件进
    ///   owner 会被模态过滤丢弃，模态对话框收不到键）。关闭时按打开前的**快照**恢复 owner
    ///   的 chrome 并把键盘焦点还给仍可用的 owner。**不用** `beginSheet:`（会改变窗口形态）
    ///   与 `runModalForWindow:`（嵌套 run loop 与引擎事件泵冲突）。引擎分发层同时按 owner
    ///   关系过滤输入（原生拦截被绕过时的兜底）。**已知语义差异（RBM-10）**：AppKit 的
    ///   `ignoresMouseEvents` 只承诺「窗口对鼠标透明」，不承诺「吞掉事件」—— 点击被拦截的
    ///   owner 会**穿透到下层窗口**（可能是别的应用/桌面），这与 Windows `EnableWindow`
    ///   吃掉点击不同；若产品不接受该差异需改方案（overlay/sheet，均不在当前范围）。
    /// - **Linux/Wayland**：协议没有模态；引擎层模态（同 macOS 的兜底路径），并 `set_parent`
    ///   提示 compositor 置于 owner 之上。
    ///
    /// 同一 owner 上可以同时存在多个模态窗口：owner 的拦截在**第一个**模态窗口打开时生效、
    /// 在**最后一个**模态窗口关闭时解除（判据是「该 owner 还有没有活着的模态窗口」，不是
    /// 「关闭了一个模态窗口」）；关闭其中一个时键盘焦点交给另一个仍活着的模态窗口，不还给
    /// owner。macOS 的原生拦截与 Windows 的 `EnableWindow` 都按这条语义执行（Windows 侧
    /// 的计数缺陷见 RB-13 修复）。
    ///
    /// 模态窗口被关闭/否决关闭时语义：否决 = 保持模态；真正关闭 = 恢复 owner 输入（三端
    /// 一致；macOS 的原生拦截在关闭 / 外部关闭 / 级联关闭 / App 关闭 / Drop 每条收尾路径上
    /// 都恢复，且恢复幂等 —— 不会把 owner 永久留在不可交互状态）。
    pub modal: bool,
    /// 是否置顶（always-on-top，X1）。
    ///
    /// - **Windows**：`WS_EX_TOPMOST`（`SetWindowPos(HWND_TOPMOST)`）；
    /// - **macOS**：`NSFloatingWindowLevel` 窗口层级；
    /// - **Linux/Wayland**：**没有跨合成器协议**（KDE plasma-shell / wlr-layer-shell 都不是
    ///   xdg-shell 标准能力）⇒ 降级为无操作并在文档写明（应用不得假设置顶成立）。
    pub always_on_top: bool,
    /// 初始 DPI 比例（由平台在 P12 传入；headless 默认 `Scale::ONE`）。
    pub scale: Scale,
    /// 窗口背景色：内容未覆盖区域的底色（持久目标 clear；Windows 另有独立背景层）。
    ///
    /// 平台差异：
    /// - Windows：persist clear 的**未覆盖像素**（linear，与 macOS/Linux 同一条路径），也是
    ///   独立 1×1 背景层的颜色（DirectComposition 的组合交换链**没有**可用的
    ///   `SetBackgroundColor`，probe 实测返回 OK 但惰性）。背景层的角色是**≤1 帧接缝**
    ///   （几何已提交、真实帧还在飞）、首帧、DPI 切换与平台尚未出帧的路径：它是纯色填充，
    ///   既不拉伸旧内容，**也不是**拖动/缩放期的画面来源 —— 拖动期间每条 `WM_SIZE` 都由
    ///   `ResizeFrameDriver` 驱动一帧真实帧（见上）。旧内容保持 1:1（DEVIATIONS §36 F-205 /
    ///   S-152）。
    /// - macOS：既填旧 drawable **之外**的 `CAMetalLayer.backgroundColor`（sRGB `CGColor`），
    ///   也用于 persist clear 的**未覆盖像素**（linear，`render::color::background_linear`）。
    /// - Linux：只用于 persist clear 的**未覆盖像素**（linear，Vulkan sRGB attachment，
    ///   `render::color::background_linear`）；旧 buffer 不会被缩放，规则与 Windows 的背景层一致。
    ///
    /// alpha 恒为 1（先置 1 再解析，半透明颜色不会变灰）。
    /// 默认 `Color::BLACK`（**不是** `Color::default()`，后者是透明）。
    ///
    /// **局部帧的行为**（复审 P2-5，文档化设计前提）：引擎在 persist 目标上**保留上一帧
    /// 内容**，damage 帧只重画 damage 区域、**不**先清成背景色。因此 damage 区域内的像素
    /// 必须由应用内容覆盖 —— 若某块区域本帧没有任何图元，它会保留旧像素。这与 Chrome 的
    /// layer 重绘语义一致（不是缺陷），但应用若希望「擦除」某块内容，必须显式画一个覆盖
    /// 它的背景 `Quad`（demo 用铺满窗口的 `demo.background` 节点满足这一点）。
    pub background: Color,
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            title: SmolStr::new_static(""),
            size: Size::new(800.0, 600.0),
            position: None,
            min_size: None,
            max_size: None,
            owner: None,
            modal: false,
            always_on_top: false,
            scale: Scale::ONE,
            background: Color::BLACK,
        }
    }
}

/// 单调递增的提交序号。**0 = 从未提交**（`Default`），首帧 = 1。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameIndex(u32);

impl FrameIndex {
    /// 原始值（`0` 只在 `Default` 里出现；`commit` 保证提交序号 ≥ 1）。
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// crate 内构造（渲染器的**一次性**帧，如快照；不参与窗口的 `FrameScheduler`）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) const fn from_raw(value: u32) -> Self {
        debug_assert!(value >= 1, "FrameIndex 0 从未提交");
        Self(value)
    }
}

/// 一帧的**呈现快照**（A4 的公开消费面：`commit → presented` 的 e2e 观测）。
///
/// 三端统一的数据面（每轮泵在 [`Window::poll_renderer`] 里从 [`crate::render::frame::FrameFeedback`]
/// 更新；Linux 的 `wp_presentation` 事件在到达后回填 `presented_at`）：
///
/// - [`Self::published_at`]：UI 提交本帧的时刻（= `completed_at − latency`，两者同源，无额外误差）；
/// - [`Self::completed_at`]：GPU 线程观察到 GPU 工作完成的时刻；
/// - [`Self::presented_at`]：真实上屏时刻的**下界**（`None` = 该端/该帧没有可信值；偏差方向恒为
///   偏早，**不是**「用 completed_at 顶替」的占位，契约见 `render::frame::FrameFeedback`）。
///
/// 这是本 crate 交给应用 / 验收 harness 的**唯一**呈现观测面：`FrameFeedback` 本身是 crate 私有。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FramePresentation {
    /// 本帧的提交序号（与 `FrameOutcome::Committed` 同源）。
    pub index: FrameIndex,
    /// UI 提交本帧的时刻（`completed_at − latency`）。
    pub published_at: Instant,
    /// GPU 线程观察到本帧 GPU 工作完成的时刻。
    pub completed_at: Instant,
    /// 本帧真实上屏时刻的下界（`None` = 该端/该帧没有可信值）。
    pub presented_at: Option<Instant>,
    /// `completed_at − published_at`（三端同义；HUD 的 `Latency` 行同为这个口径）。
    pub latency: Duration,
}

impl FramePresentation {
    /// `published_at → presented_at` 的 e2e 时长（`presented_at` 为上屏时刻的下界 ⇒ 结果是
    /// e2e 时长的**下界**）。`None` = 本帧没有可信的上屏时刻。
    #[must_use]
    pub fn publish_to_presented(&self) -> Option<Duration> {
        self.presented_at
            .map(|presented| presented.saturating_duration_since(self.published_at))
    }

    /// `completed_at → presented_at` 的残留（呈现链在 GPU 完成之后还要等多久；`None` 同上）。
    #[must_use]
    pub fn completion_to_presented(&self) -> Option<Duration> {
        self.presented_at
            .map(|presented| presented.saturating_duration_since(self.completed_at))
    }
}

/// 每窗口保留的最近呈现快照条数（A4）。
///
/// 为什么不是只留最后一条：Linux 的 `wp_presentation` 事件在帧反馈**之后**才到达（GPU 完成时
/// 还没上屏），需要按 `FrameIndex` 回填；验收 harness 也按 index 增量消费（不能因为新一轮
/// 反馈覆盖旧条目而漏采）。8 条覆盖「事件晚到 + 采样节拍错开」的正常范围。
const FRAME_PRESENTATION_HISTORY: usize = 8;

/// 一帧的最终结果（`FrameRecord` 只存在于 `test-support`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameOutcome {
    /// 本帧不提交：没有像素要画，或有像素但不会产生 GPU 工作（V-122 ⑧）。`FrameIndex` 不变（I3）。
    Idle,
    /// 已把帧发布给 GPU 线程（**不等于**已呈现）：有 renderer 时本帧恰好 `publish` 一次，
    /// Linux 帧门禁（F-183）依赖这一点 —— GPU 线程 `commit` 后 `wl_callback` 才会回来。
    /// 序号 = 本次提交后的 `FrameIndex`。
    Committed(FrameIndex),
}

/// `Draining` 的凭证：只能由 [`FrameScheduler::begin`] 铸出，`commit`/`abort` 各消费一次。
///
/// **Drop 是 abort**（P1-2.4）：`App::frame` 里的样式/文本/布局/paint 都会执行用户闭包，
/// 任一 panic 都会让本值被 drop 而不是被 `commit`/`abort` 消费。旧实现没有 `Drop`，状态就永久
/// 停在 `Draining`：之后 `request_frame()` 恒返回 false → `begin_frame` 返回 `None` →
/// `App::frame` 返回 `Ok(Idle)`，窗口再也不出帧，且没有 API 能复位。
///
/// 与 wgpu 的 surface texture guard、GPUI 的 frame guard、Qt 的 `QPainter::begin/end` 同一思路：
/// 帧状态的回滚由 RAII 保证，不依赖调用方记得写 `abort`。
pub(crate) struct FrameTicket {
    /// 共享的帧状态（与 `FrameScheduler` 同一份）。`Drop` 用它把 `Draining` 复位成 `Idle`。
    state: Rc<Cell<FrameState>>,
    /// 已被 `commit`/`abort` 消费：`Drop` 不再改状态（`commit` 可能把状态设成别的）。
    consumed: bool,
}

impl Drop for FrameTicket {
    fn drop(&mut self) {
        if self.consumed {
            return;
        }
        // panic 展开路径：把 `Draining` 复位，让下一帧仍能开始。
        if self.state.get() == FrameState::Draining {
            self.state.set(FrameState::Idle);
        }
    }
}

/// IME caret hint：`node` 是文本节点，`byte_index` 是该文本内容的 UTF-8 字节偏移。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ImeCursorRequest {
    node: NodeId,
    byte_index: usize,
}

/// 一个窗口的全部状态：节点树 + 绘制场景 + DPI + 参数 + 帧调度。
pub struct Window {
    id: WindowId,
    options: WindowOptions,
    tree: NodeTree,
    scene: Scene,
    scale: Scale,
    /// 渲染目标的物理像素尺寸。
    ///
    /// 平台真值由 `set_surface` 写入：Windows 是真实客户区，macOS 是 AppKit 的真实 backing
    /// （`convertSizeToBacking`），Linux 是 Wayland buffer 的真实像素尺寸（`round(逻辑 × scale)`）。
    /// 在平台首次写入之前（headless / 快照 / 首帧前）才跟随 `size × scale` 的 covering。
    surface: (u32, u32),
    /// 平台层是否已经通过 `set_surface` 写过真实渲染目标。
    ///
    /// `false`（构造时、headless / 快照路径）⇒ `set_size` / `set_scale` 让 `surface` 跟随
    /// `size × scale` 的 covering；`true` 之后平台真值是唯一来源，这两个方法**不得**用 covering
    /// 覆盖它（F-186 / F-189 / V-122）。
    surface_from_platform: bool,
    /// Linux：整数回退的 `wl_surface` buffer scale（fractional 路径固定 1）。
    ///
    /// buffer 尺寸按 `round(逻辑 × scale)` 产出；Wayland 的逻辑尺寸已在平台层取整，整数回退下
    /// buffer 因此严格是 `buffer_scale` 的整数倍（否则 `wl_surface.attach` 报 `invalid_size`）。
    /// `buffer_scale` 必须等于整数 scale，surface 的逻辑尺寸才等于 `buffer / buffer_scale`，
    /// 否则 compositor 会把整面放大 N×（V-110 / V-122 ④）。平台层用 `set_buffer_scale` 写入，
    /// 随 [`FrameRequest`] 过线，GPU 线程在 present 前与 attach/commit 同一次生效。
    #[cfg(target_os = "linux")]
    buffer_scale: i32,
    /// DPI 代际：`set_scale` 递增；`App::refresh_text` 用它判定「图集要换代」（P05）。
    scale_epoch: u32,
    /// 每 refresh 周期最多一帧的预算门禁（A5；实现见 `frame_budget` 子模块）。
    frame_budget: frame_budget::FrameBudget,
    /// 帧调度（`Idle / Pending / Draining`；`FrameIndex` 只在提交时递增）。
    scheduler: FrameScheduler,
    /// 合成器（**inline 模式**持有编码器：headless / 离屏 / 单测）。
    ///
    /// 真实窗口的编码器在合成线程里（`render/compositor_thread.rs`），首帧之后这里恒为
    /// `None`：两个位置各自持有**恰好一个** `InstanceEncoder`，不存在双写。
    composer: Option<WindowComposer>,
    /// 本帧待发布的 `SceneDelta`（`paint_frame` 产出；threaded 模式在 `submit_gpu` 补头部后发布）。
    pending_delta: Option<SceneDelta>,
    /// 本帧已合成的帧（inline 模式；`submit_gpu` 消费后发布给渲染器）。
    pending_payload: Option<crate::render::compose::ComposedFrame>,
    /// delta 采集器（lane 版本 / 绘制序 / 几何描述 / 滚动槽）。
    delta_builder: crate::render::delta::DeltaBuilder,
    /// 滚动槽表：槽位 → （逻辑滚动位移，物理视口）。
    ///
    /// **由 paint pass 每帧重写**（`paint/pass.rs` 的 `PaintPass::scroll_slots`）：下标 0 恒是
    /// 「不随滚动偏移」的哨兵，滚动容器按绘制序依次占 1..。lane 的图元烘焙在「不含本槽位移」
    /// 的内容空间里，`DeltaBuilder` 只把偏移变化（O(槽数)）带上总线，合成端更新偏移表 ⇒
    /// **滚动不重建任何 lane、不重编码任何实例**（架构决策 B3B6 §4 / wave-5）。
    scroll_slots: Vec<(crate::math::Point, PhysicalRect)>,
    /// 与 `scroll_slots` 平行的「槽位 → 容器节点」（下标 i ⇒ 槽位 i + 1；槽位 0 是哨兵）。
    ///
    /// 消费者：纯滚动帧的快路径（`paint::paint_frame`）——树结构没变时槽位编号与视口都不变，
    /// 只有偏移变了，于是直接按这张表把偏移刷进槽位表，不必遍历 10k 节点。
    scroll_slot_nodes: Vec<crate::node::id::NodeId>,
    /// 渲染器重建代际的观察门（消费端重置 ⇒ 生产端整帧重发，见 `render::frame::ResyncGate`）。
    ///
    /// 设备恢复（TDR / 驱动更新）之后 GPU 侧的实例镜像被重建为空：若继续只发增量 lane，
    /// 未变化的段会画成退化实例（近似黑/花）。这个门在每帧开始前观察
    /// [`crate::render::frame::FrameSink::reset_epoch`]，一旦变化就强制下一帧整帧发布。
    resync: crate::render::frame::ResyncGate,
    /// 合成线程注册 ticket：**必须声明在 `renderer` 之前**（字段按声明顺序析构）。
    ///
    /// 这样窗口关闭时先注销合成线程（最后一个窗口会 join 它），再析构 `renderer`（join GPU
    /// 线程）—— 「合成线程先于所有 GPU 线程有序退出」的落点。
    compositor_ticket: Option<crate::render::compositor_thread::CompositorTicket>,
    /// 本帧的 damage 累加器（`paint_frame` 重置，`commit_frame` 消费）。
    damage: DamageAccumulator,
    /// 颜色解析器（编码期 `resolve`；P14 主题的注入点）。
    resolver: ColorResolver,
    /// GPU 渲染器（`None` = headless，I7 的默认；P09 的窗口层负责 attach 原生窗口）。
    renderer: Option<Renderer>,
    /// 最近若干帧的呈现快照（A4 的公开消费面；容量 [`FRAME_PRESENTATION_HISTORY`]）。
    ///
    /// 每轮泵在 [`Window::poll_renderer`] 里追加 GPU 线程的反馈；Linux 的 `wp_presentation`
    /// 事件到达后按 `FrameIndex` 回填对应条目的 `presented_at`（事件晚于反馈）。
    presentation: VecDeque<FramePresentation>,
    /// 该窗口的渲染器已收到的**完整**字形图集 extent（`None` = 还没收到 / 渲染器已换）。
    ///
    /// 图集页是进程级共享数据，但纹理是每窗口一份（`AtlasTextures`），因此必须逐窗口记账：
    /// 第一次（或渲染器重建后 / 页尺寸变化后）必须整页上传，之后才允许按脏行增量
    /// （P13 §8.2 / 评审阻塞 ② / DEVIATIONS F-208）。
    glyph_page_received: Option<u32>,
    /// 图片图集 texture array 的 extent（整数组已按该 extent 建立；`None` = 还没收到 / 渲染器已换）。
    image_page_received: Option<u32>,
    /// 图片图集每个 layer 是否已经拿过整层上传（新窗口 / 换渲染器 / 页换代后逐层补齐）。
    ///
    /// texture array 一旦建立就是 `MAX_IMAGE_PAGES` 层，但每层内容独立：某层第一次入队前
    /// 必须整层上传（资源层新页首帧整页脏）。下标 = 层号。
    image_layers_received: [bool; MAX_IMAGE_PAGES as usize],
    /// 字形图集在本窗口上「已写脏、还没入队」的矩形（恒 layer 0）。
    glyph_atlas_rects: crate::render::DirtyRects,
    /// 图片图集每层的「已写脏、还没入队」矩形（下标 = texture array 层号）。
    ///
    /// 共享槽忙（GPU 线程还没录制上一笔上传）的窗口不参与全局等待：新矩形先攒在这里，
    /// 等该窗口的槽空闲再补发。否则一个不再出帧的窗口会永久占住图集上传（DEVIATIONS F-211）。
    /// R06-3 起是**矩形**（AOI）而不是整页宽的行区间。
    image_atlas_rects: [crate::render::DirtyRects; MAX_IMAGE_PAGES as usize],
    /// 本窗口「图集行没赶上帧」的补帧次数（上限见 [`MAX_ATLAS_FOLLOWUPS`]）。
    atlas_deferrals: u8,
    /// 本帧的分段计时（容量跨帧复用）。
    timings: FrameTimings,
    /// 本帧 lane 绘制序的复用缓冲（避免每帧分配）。
    paint_order: Vec<crate::paint::scene::LaneId>,
    /// 尺寸/DPI 变化 ⇒ 必须整帧重绘（S-64）。
    full_damage_pending: bool,
    /// `App::frame` 在提交前写入的帧统计（仅 `test-support`）。
    #[cfg(feature = "test-support")]
    stats: FrameStats,
    /// 最近一帧的调度/绘制记录（仅 `test-support`）。
    #[cfg(feature = "test-support")]
    record: Option<FrameRecord>,
    /// 焦点（P10 的唯一权威源）。
    focus: FocusScope,
    /// 指针状态（hover / 光标 / 按下 / 拖拽 / 捕获）。
    pointer: PointerState,
    /// 下一帧必须重绘的逻辑矩形（滚动视口；`paint_frame` 消费后清空）。
    pending_damage: Option<Rect>,
    /// a11y 状态机（P11：变更集合 / 全树旗标 / 捕获开关）。
    a11y: crate::a11y::adapter::A11yBridge,
    /// IME caret hint（P16：应用声明的文本节点 + UTF-8 字节偏移）。
    ime_cursor: Option<ImeCursorRequest>,
    /// 自绘标题栏的可拖动区域（X2；逻辑窗口坐标，客户区左上角原点）。
    ///
    /// 空 = 没有拖动区（默认）。平台层每轮从 `App` 同步到原生窗口：
    /// Windows 命中区域的 `WM_NCHITTEST` 返回 `HTCAPTION`（系统 move 模态循环，与系统
    /// 标题栏同路径：双击最大化 / Aero Snap / 右键系统菜单都成立）；macOS 命中区域的
    /// 主键按下走 `performWindowDragWithEvent:`（不进引擎指针分发）；Linux/Wayland CSD
    /// 命中区域的主键按下请求 `xdg_toplevel.move(seat, serial)`（compositor 驱动移动）。
    titlebar_drag_regions: Vec<Rect>,
    /// Linux：当前焦点文本域的语义类型（`zwp_text_input_v3.set_content_type` 的数据源）。
    ///
    /// 密码 / API key 输入框必须切到 [`ImeContentType::Password`]：text-input-v3 的默认值
    /// （`hint = none` / `purpose = normal`）会让输入法对其做联想与**用户词库学习**。
    #[cfg(target_os = "linux")]
    ime_content: ImeContentType,
    /// Linux：compositor 侧的窗口状态镜像（最大化 / 全屏 / 装饰模式 / 可用能力）。
    #[cfg(target_os = "linux")]
    state: WindowState,
    /// 最近一帧**完成**帧的实际 GPU 绘制统计（P17 HUD）。
    ///
    /// threaded 路径（真实窗口）由 [`crate::window::record`] 的完成反馈写入；inline 路径
    /// （headless / 离屏）由提交时同步返回的统计写入。两种来源都是后端**录制命令时**的真实计数。
    #[cfg(feature = "hud")]
    render_stats: crate::render::RenderStats,
    /// 最近一次反馈帧的「发布 → GPU 完成」时长（HUD 的 Latency 行；窗口路径落后 ≤1 帧）。
    #[cfg(feature = "hud")]
    last_frame_latency: Option<std::time::Duration>,
    /// 已完成的帧数（HUD 的 FPS / Frame 唯一来源：完成反馈到达次数）。
    #[cfg(feature = "hud")]
    completed_frames: u64,
    /// 最近一帧的 CPU 时间（`begin_frame` → `paint_frame` 结束）。
    #[cfg(feature = "hud")]
    frame_cpu: Option<std::time::Duration>,
    /// 当前帧开始时间（仅 CPU 计时用；中止/空闲时清空）。
    #[cfg(feature = "hud")]
    frame_started_at: Option<std::time::Instant>,
}

impl Window {
    /// **唯一的构造点**：`Scene::empty()` 与 `NodeTree::new()` 只在这里组合。
    pub(crate) fn new(id: WindowId, options: WindowOptions) -> Self {
        let scale = options.scale;
        let surface = covering_surface(options.size, scale);
        let mut tree = NodeTree::new();
        // B7：非整数 DPI 才开启布局提交前的设备像素吸附（见 `NodeTree::set_device_scale`）。
        // 窗口 id 的绑定在 `App::create_window`（`NodeTree::bind_window`）—— 那里才确定
        // 该树归属哪个窗口，而 `Window::new` 也可能被测试直接调用（不绑定 = 通配作用域）。
        tree.set_device_scale((scale.factor() != 1.0).then_some(scale.factor()));
        Self {
            id,
            options,
            tree,
            scene: Scene::empty(),
            scale,
            surface,
            surface_from_platform: false,
            #[cfg(target_os = "linux")]
            buffer_scale: 1,
            scale_epoch: 0,
            frame_budget: frame_budget::FrameBudget::default(),
            scheduler: FrameScheduler::new(),
            composer: Some(WindowComposer::new()),
            pending_delta: None,
            pending_payload: None,
            delta_builder: crate::render::delta::DeltaBuilder::new(),
            scroll_slots: Vec::new(),
            scroll_slot_nodes: Vec::new(),
            resync: crate::render::frame::ResyncGate::default(),
            compositor_ticket: None,
            damage: DamageAccumulator::new(scale),
            resolver: ColorResolver,
            renderer: None,
            presentation: VecDeque::with_capacity(FRAME_PRESENTATION_HISTORY),
            glyph_page_received: None,
            image_page_received: None,
            image_layers_received: [false; MAX_IMAGE_PAGES as usize],
            glyph_atlas_rects: crate::render::DirtyRects::default(),
            image_atlas_rects: std::array::from_fn(|_| crate::render::DirtyRects::default()),
            atlas_deferrals: 0,
            timings: FrameTimings::new(),
            paint_order: Vec::new(),
            full_damage_pending: false,
            #[cfg(feature = "test-support")]
            stats: FrameStats::default(),
            #[cfg(feature = "test-support")]
            record: None,
            focus: FocusScope::default(),
            pointer: PointerState::new(),
            pending_damage: None,
            a11y: crate::a11y::adapter::A11yBridge::new(),
            ime_cursor: None,
            titlebar_drag_regions: Vec::new(),
            #[cfg(target_os = "linux")]
            ime_content: ImeContentType::default(),
            #[cfg(target_os = "linux")]
            state: WindowState::default(),
            #[cfg(feature = "hud")]
            render_stats: crate::render::RenderStats::default(),
            #[cfg(feature = "hud")]
            last_frame_latency: None,
            #[cfg(feature = "hud")]
            completed_frames: 0,
            #[cfg(feature = "hud")]
            frame_cpu: None,
            #[cfg(feature = "hud")]
            frame_started_at: None,
        }
    }

    /// 窗口句柄。
    #[must_use]
    pub fn id(&self) -> WindowId {
        self.id
    }

    // ── A4：呈现快照（`commit → presented` 的公开消费面） ──

    /// 最近一帧的呈现快照（还没观测到任何完成帧时 `None`）。
    ///
    /// 字段语义见 [`FramePresentation`]；`presented_at` 在 Linux 上可能晚一轮才回填（事件异步），
    /// 需要增量采集时用 [`Self::recent_frame_presentations`] 按 `index` 消费。
    #[must_use]
    pub fn last_frame_presentation(&self) -> Option<FramePresentation> {
        self.presentation.back().copied()
    }

    /// 最近若干帧的呈现快照（提交顺序，最多 [`FRAME_PRESENTATION_HISTORY`] 条）。
    ///
    /// 验收 harness 用它按 `index` 增量消费：Linux 的呈现事件在反馈之后到达，只看最后一条会
    /// 漏掉被新一轮反馈挤走的帧。
    pub fn recent_frame_presentations(&self) -> impl Iterator<Item = FramePresentation> + '_ {
        self.presentation.iter().copied()
    }

    /// 追加一条 GPU 线程的完成反馈（`poll_renderer` 的唯一调用点）。
    pub(crate) fn note_frame_presentation(&mut self, presentation: FramePresentation) {
        if self.presentation.len() >= FRAME_PRESENTATION_HISTORY {
            let _ = self.presentation.pop_front();
        }
        self.presentation.push_back(presentation);
    }

    /// Linux：`wp_presentation` 的呈现事件到达，回填对应帧的 `presented_at`。
    ///
    /// 事件由平台泵在 **dispatch 之后**按 `FrameIndex` 从呈现邮箱里取走并调用本方法；反馈本身
    /// 是异步的（presented 可能晚于 GPU 完成反馈若干轮），因此历史里还没有该帧时调用方先不
    /// 取，等下一轮。已被历史窗口挤掉的帧直接忽略（不会伪造；验收侧有各自的「已结算」口径）。
    ///
    /// `presented_at` 仍按**下界**收口：真实上屏只可能发生在 GPU 完成之后，事件时间戳与
    /// `completed_at` 两个下界取较大者（[`clamp_presented_at`]），偏差方向恒为偏早。
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn note_frame_presented(
        &mut self,
        index: FrameIndex,
        presented_at: Option<Instant>,
    ) {
        if let Some(entry) = self
            .presentation
            .iter_mut()
            .find(|entry| entry.index == index)
        {
            entry.presented_at = clamp_presented_at(presented_at, entry.completed_at);
        }
    }

    /// 节点树只读视图。
    #[must_use]
    pub fn tree(&self) -> &NodeTree {
        &self.tree
    }

    /// 应用层建树的唯一入口（`NodeBuilder::build` 的 `&mut NodeTree` 来自这里）。
    pub fn tree_mut(&mut self) -> &mut NodeTree {
        &mut self.tree
    }

    /// 替换 **Canvas 节点**的画布内容（D-6：异步图片解码交付的落地入口）。
    ///
    /// # 语义
    /// - 只接受 `NodeKind::Canvas` 节点：节点不存在 ⇒ [`EngineError::StaleNode`]；节点存在但
    ///   不是 Canvas ⇒ `Ok(false)`（不改变任何状态，调用方按契约错误处理）。
    /// - 替换后标该节点**子树** `PAINT`：下一帧重建 lane，damage 来自新图元；**不**置整窗
    ///   damage，也**不**重算布局 —— Canvas 节点的尺寸仍由布局 / `LayoutOverride` 决定，
    ///   应用换画布前应保证尺寸与内容匹配（否则溢出/留白由应用负责）。
    /// - `AtlasTile` 的生命周期由调用方持有的 [`crate::resources::ResourceImage`] 决定：
    ///   引擎不会因替换而保留/延长旧 tile，也不会自动 `unload` 旧资源；应用负责对不再引用的
    ///   资源调用 `ResourceCache::unload`（或关窗时 `release_window`）。
    /// - 本方法不请求帧（Window 无平台唤醒能力）：在 `on_frame` 钩子里调用时本帧稍后就会重建
    ///   lane；其它时机随后调用 `App::request_node_repaint(window, node)` 或等下一次帧。
    ///
    /// # Errors
    /// [`EngineError::StaleNode`]：节点已卸载（陈旧句柄）。
    pub fn set_canvas(
        &mut self,
        node: NodeId,
        canvas: crate::paint::canvas::Canvas,
    ) -> Result<bool, EngineError> {
        if self.tree.get(node).is_err() {
            return Err(EngineError::StaleNode { node });
        }
        Ok(self.tree.set_canvas(node, canvas))
    }

    /// 指针状态（crate 私有：`App::dispatch_input` 与平台泵是唯一读者）。
    pub(crate) fn pointer(&self) -> &PointerState {
        &self.pointer
    }

    /// 指针状态的可写引用。
    pub(crate) fn pointer_mut(&mut self) -> &mut PointerState {
        &mut self.pointer
    }

    // ── a11y：实现见 `a11y` 子模块的自由函数 ──
    #[must_use]
    pub(crate) fn a11y_pending(&self) -> bool {
        a11y::a11y_pending(self)
    }
    pub(crate) fn a11y_take(
        &mut self,
        diagnostics: &mut Vec<crate::a11y::A11yDiagnostic>,
    ) -> Option<crate::a11y::A11yTreeSnapshot> {
        a11y::a11y_take(self, diagnostics)
    }
    pub(crate) fn a11y_note_scroll(&mut self, container: crate::node::id::NodeId) {
        a11y::a11y_note_scroll(self, container)
    }
    /// 焦点是否变化且尚未交付（`App::a11y_pending` 的不节流旁路）。
    #[must_use]
    pub(crate) fn a11y_focus_pending(&self) -> bool {
        a11y::a11y_focus_pending(self)
    }
    /// 消费端树生命周期代际句柄（E1）：平台 adapter 的激活/停用回调用它标记"下一笔必须全树"。
    #[must_use]
    pub(crate) fn a11y_consumer_epoch(&self) -> crate::a11y::adapter::A11yConsumerEpoch {
        a11y::a11y_consumer_epoch(self)
    }
    /// 消费端是否重置过而全树尚未交付（`App::a11y_pending` 的不节流旁路，E1）。
    #[must_use]
    pub(crate) fn a11y_consumer_epoch_pending(&self) -> bool {
        a11y::a11y_consumer_epoch_pending(self)
    }
    /// 是否还有**已开始交付的** a11y 批次未走完（分批首树/DPI 重建的剩余部分；E5）。
    #[must_use]
    pub(crate) fn a11y_backlog_pending(&self) -> bool {
        a11y::a11y_backlog_pending(self)
    }
    /// 是否有一轮**尚未开始交付**的全量重建（首树 / DPI 代际变化；E5 不节流旁路）。
    #[must_use]
    pub(crate) fn a11y_rebuild_pending(&self) -> bool {
        a11y::a11y_rebuild_pending(self)
    }
    /// 稳定 a11y id → 引擎句柄（`ActionRequest` 解码）。
    #[must_use]
    pub(crate) fn a11y_engine_of(
        &self,
        id: crate::a11y::A11yNodeId,
    ) -> Option<crate::node::id::NodeId> {
        a11y::a11y_engine_of(self, id)
    }
    /// 引擎句柄 → 稳定 a11y id（只查不分配；动作测试用）。
    #[cfg(test)]
    #[must_use]
    pub(crate) fn a11y_stable_id(
        &self,
        node: crate::node::id::NodeId,
    ) -> Option<crate::a11y::A11yNodeId> {
        a11y::a11y_stable_id(self, node)
    }
    /// 强制下一笔 a11y 交付为全树（测试 oracle / 防御兜底）。
    pub(crate) fn a11y_force_full(&mut self) {
        a11y::a11y_force_full(self);
    }
    /// 只有 `bind_a11y` 的节点被写信号：登记待重算（不标 `BIND`/`PAINT`，不产生像素）。
    pub(crate) fn a11y_note_binding(&mut self, node: crate::node::id::NodeId) -> bool {
        a11y::a11y_note_binding(self, node)
    }
    /// 是否有待重算的 a11y 绑定（帧门禁：a11y-only 写仍要出一帧来投递 TreeUpdate）。
    #[must_use]
    pub(crate) fn a11y_binding_work(&self) -> bool {
        a11y::a11y_binding_work(self)
    }
    /// 取走本批待重算的 a11y 绑定节点（`App::refresh_a11y_bindings` 的唯一消费者）。
    pub(crate) fn a11y_take_bindings(&mut self) -> Vec<crate::node::id::NodeId> {
        a11y::a11y_take_bindings(self)
    }
    /// 归还 [`Window::a11y_take_bindings`] 借走的缓冲。
    pub(crate) fn a11y_recycle_bindings(&mut self, buffer: Vec<crate::node::id::NodeId>) {
        a11y::a11y_recycle_bindings(self, buffer)
    }
    /// 本帧是否只剩 a11y 工作（R13-6 的 a11y-only 快速路径判据）。
    #[must_use]
    pub(crate) fn a11y_only_frame(&self) -> bool {
        a11y::a11y_only_frame(self)
    }
    /// 记一笔 a11y-only 帧（指标面；仅 `test-support` 有帧记录）。
    #[cfg(feature = "test-support")]
    pub(crate) fn record_a11y_only_frame(&mut self) {
        a11y::record_a11y_only_frame(self)
    }
    /// 本窗口累计的 a11y-only 帧数（R13-6 指标）。
    #[cfg(feature = "test-support")]
    #[must_use]
    pub(crate) fn a11y_only_frames(&self) -> usize {
        a11y::a11y_only_frames(self)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn set_a11y_capture(&mut self, on: bool) {
        a11y::set_a11y_capture(self, on)
    }

    // ── frame：实现见 `frame` 子模块的自由函数 ──
    pub(crate) fn needs_frame(&self, wake: bool) -> bool {
        frame::needs_frame(self, wake)
    }
    // ── frame_budget：实现见 `frame_budget` 子模块的自由函数 ──

    /// 平台层注入「窗口所在 output」的刷新率提示（A5；`None` / 非法值 ⇒ 门禁禁用）。
    ///
    /// 消费者：三端事件循环（Windows 逐窗口显示器查询 / macOS `NSScreen` 的
    /// `maximumFramesPerSecond` / Linux `wl_output.mode`）。只在提示变化时调用即可
    /// （同值写入是幂等的）。
    pub(crate) fn set_frame_refresh_hz(&mut self, hz: f32) {
        self.frame_budget.set_refresh_hz(hz);
    }
    /// 当前生效的刷新率提示（诊断 / 验收；未知 = `None`）。
    #[must_use]
    pub(crate) fn frame_refresh_hz(&self) -> Option<f32> {
        self.frame_budget.refresh_hz()
    }
    /// 本帧是否放行（`forced` = force 通道，绕过预算）。
    pub(crate) fn frame_budget_admits(&self, now: std::time::Instant, forced: bool) -> bool {
        self.frame_budget.admits(now, forced)
    }
    /// 放行一帧：推进下一拍（`App::frame` 在通过门禁后调用）。
    pub(crate) fn note_frame_start(&mut self, now: std::time::Instant) {
        self.frame_budget.note_start(now);
    }
    /// 记一次预算推迟；返回 `true` = 新的推迟（平台尚未挂一次性唤醒）。
    pub(crate) fn note_frame_budget_deferred(&mut self) -> bool {
        self.frame_budget.note_deferred()
    }
    /// 是否有「已推迟、还没放行」的帧请求。
    #[must_use]
    pub(crate) fn frame_budget_deferred(&self) -> bool {
        self.frame_budget.is_deferred()
    }
    /// 清除「推迟未补」标记（不可见 / 最小化窗口：平台帧循环会跳过它，否则一次性唤醒常驻）。
    pub(crate) fn clear_frame_budget_deferred(&mut self) {
        self.frame_budget.clear_deferred();
    }
    /// 被推迟的帧最早何时可以开始（Linux calloop timeout 的到期时刻）。
    #[cfg(target_os = "linux")]
    #[must_use]
    pub(crate) fn frame_budget_deadline(&self) -> Option<std::time::Instant> {
        self.frame_budget.deadline()
    }
    /// 累计被预算推迟的帧请求数（只增不减的诊断量）。
    #[must_use]
    pub(crate) fn frame_budget_deferrals(&self) -> u64 {
        self.frame_budget.deferrals()
    }
    /// 仅测试：注入确定的预算周期（可注入 200 ms 这类长周期让测试不依赖真实刷新率抖动）。
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn set_frame_budget_test_interval(&mut self, interval: std::time::Duration) {
        self.frame_budget.set_test_interval(Some(interval));
    }
    pub(crate) fn begin_frame(&mut self) -> Option<FrameTicket> {
        frame::begin_frame(self)
    }
    /// 实现见子模块 `frame::force_full_republish`（合成线程重启之后整帧重发）。
    pub(crate) fn force_full_republish(&mut self) {
        frame::force_full_republish(self);
    }
    pub(crate) fn layout_if_dirty(&mut self) -> Result<Option<LayoutReport>, EngineError> {
        frame::layout_if_dirty(self)
    }
    pub(crate) fn commit_frame(
        &mut self,
        ticket: FrameTicket,
        report: PaintReport,
    ) -> FrameOutcome {
        frame::commit_frame(self, ticket, report)
    }
    fn submit_gpu(
        &mut self,
        renderer: &mut Renderer,
        index: FrameIndex,
        plan: Plan,
    ) -> Option<crate::render::RenderStats> {
        frame::submit_gpu(self, renderer, index, plan)
    }
    pub(crate) fn abort_frame(&mut self, ticket: FrameTicket) {
        frame::abort_frame(self, ticket)
    }
    /// 实现见子模块 `frame::frame`（本文件只保留签名）。
    pub fn frame(&mut self) -> Result<FrameOutcome, EngineError> {
        frame::frame(self)
    }
    /// 实现见子模块 `frame::frame_index`（本文件只保留签名）。
    #[must_use]
    pub fn frame_index(&self) -> FrameIndex {
        frame::frame_index(self)
    }
    /// 实现见子模块 `frame::compute_layout`（本文件只保留签名）。
    pub fn compute_layout(&mut self) -> Result<LayoutReport, EngineError> {
        frame::compute_layout(self)
    }

    // ── paint：实现见 `paint` 子模块的自由函数 ──
    pub(crate) fn paint_frame(
        &mut self,
        geometry: &crate::paint::geometry_store::GeometryStore,
        atlas_generation: Option<u32>,
    ) -> Result<PaintReport, EngineError> {
        paint::paint_frame(self, geometry, atlas_generation)
    }
    fn needs_node_repaint(&self) -> bool {
        paint::needs_node_repaint(self)
    }
    /// **只给基准/测试**：摘掉 inline 合成器，模拟「编码在合成线程」时 UI 线程的耗时。
    ///
    /// 摘掉之后 `paint_frame` 只产 `SceneDelta`（不做编码、不构造载荷），因此同一 fixture 下
    /// `frame_incremental_10k_ui_thread` 与 `frame_incremental_10k` 的差值就是「合成工作」的
    /// 成本 —— 这是「编码搬离 UI 线程」的可度量证据。
    #[cfg(feature = "test-support")]
    pub fn detach_composer(&mut self) {
        self.composer = None;
        self.pending_payload = None;
    }

    /// 最近一帧的编码产物：**只有 inline 合成**（headless / 离屏 / 单测）的窗口有。
    ///
    /// 真实窗口的编码器在合成线程里（B6-2），编码产物不回到 UI 线程 —— 这条 API 因此返回
    /// `None`（读它的只有诊断 / 读回 / 测试路径）。
    #[cfg(feature = "test-support")]
    #[must_use]
    pub(crate) fn instance_buffer(&self) -> Option<&InstanceBuffer> {
        paint::instance_buffer(self)
    }
    /// 实现见子模块 `paint::scene`（本文件只保留签名）。
    #[must_use]
    pub fn scene(&self) -> &Scene {
        paint::scene(self)
    }
    /// 仅测试（Windows）：当前保留的 `SceneDelta`（Idle 帧不丢弃 delta 的回归断言面）。
    #[cfg(all(windows, test, feature = "test-support"))]
    #[must_use]
    pub(crate) fn pending_delta(&self) -> Option<&crate::render::delta::SceneDelta> {
        self.pending_delta.as_ref()
    }
    /// 仅测试（Windows）：当前是否有未发布的 inline 合成载荷。
    #[cfg(all(windows, test, feature = "test-support"))]
    #[must_use]
    pub(crate) fn has_pending_payload(&self) -> bool {
        self.pending_payload.is_some()
    }
    #[cfg(test)]
    pub(crate) fn encoded_geometry(&self) -> &[crate::paint::instance::GeometryDrawGpu] {
        paint::encoded_geometry(self)
    }
    /// 最近一帧采到的滚动槽表（槽位 → （逻辑滚动位移，物理视口））。
    ///
    /// 诊断 / 测试专用：它证明「滚动容器真的拿到了槽位、偏移真的进了槽位表而不是烘焙进图元」。
    /// 下标 0 恒是哨兵（不随滚动偏移）。
    #[cfg(test)]
    #[must_use]
    pub(crate) fn scroll_slots(&self) -> &[(crate::math::Point, PhysicalRect)] {
        &self.scroll_slots
    }
    pub(crate) fn request_damage(&mut self, rect: Rect) {
        paint::request_damage(self, rect)
    }
    pub(crate) fn request_repaint(&mut self) {
        paint::request_repaint(self)
    }

    // ── surface：实现见 `surface` 子模块的自由函数 ──
    /// 实现见子模块 `surface::request_composition_rebuild`（C2；Windows 泵的几何 pass 调用）。
    ///
    /// 其它平台没有 DirectComposition，不存在该入口（`cfg` 保证只有 Windows 编译它）。
    #[cfg(target_os = "windows")]
    pub(crate) fn request_composition_rebuild(&mut self) {
        surface::request_composition_rebuild(self)
    }
    /// 实现见子模块 `surface::options`（本文件只保留签名）。
    #[must_use]
    pub fn options(&self) -> &WindowOptions {
        surface::options(self)
    }
    /// 实现见子模块 `surface::set_background_color`（本文件只保留签名）。
    pub fn set_background_color(&mut self, color: Color) {
        surface::set_background_color(self, color)
    }
    /// 实现见子模块 `surface::size`（本文件只保留签名）。
    #[must_use]
    pub fn size(&self) -> Size {
        surface::size(self)
    }
    /// 实现见子模块 `surface::set_size`（本文件只保留签名）。
    pub fn set_size(&mut self, size: Size) {
        surface::set_size(self, size)
    }
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub(crate) fn set_surface(&mut self, surface: (u32, u32)) {
        surface::set_surface(self, surface)
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn set_max_frame_rate(&mut self, max_frame_rate: f32) {
        surface::set_max_frame_rate(self, max_frame_rate)
    }
    #[cfg(target_os = "linux")]
    pub(crate) fn set_buffer_scale(&mut self, scale: i32) {
        surface::set_buffer_scale(self, scale)
    }
    pub(crate) fn has_renderer(&self) -> bool {
        surface::has_renderer(self)
    }
    #[cfg(test)]
    pub(crate) fn glyph_page_received(&self) -> bool {
        surface::glyph_page_received(self)
    }
    #[cfg(test)]
    pub(crate) fn image_page_received(&self) -> bool {
        surface::image_page_received(self)
    }
    /// 图片图集某层是否已经拿过整层上传（D-9-c 多页：新窗口 / 换渲染器 / 新页要逐层补齐）。
    pub(crate) fn image_layer_received(&self, layer: u16) -> bool {
        surface::image_layer_received(self, layer)
    }
    pub(crate) fn atlas_page_matches(&self, kind: AtlasKind, extent: u32) -> bool {
        surface::atlas_page_matches(self, kind, extent)
    }
    pub(crate) fn mark_atlas_page_received(&mut self, kind: AtlasKind, extent: u32) {
        surface::mark_atlas_page_received(self, kind, extent)
    }
    pub(crate) fn mark_image_layer_received(&mut self, layer: u16) {
        surface::mark_image_layer_received(self, layer)
    }
    pub(crate) fn mark_atlas_page_stale(&mut self, kind: AtlasKind) {
        surface::mark_atlas_page_stale(self, kind)
    }
    pub(crate) fn atlas_rects(&self, kind: AtlasKind, layer: u16) -> &crate::render::DirtyRects {
        surface::atlas_rects(self, kind, layer)
    }
    pub(crate) fn atlas_rects_mut(
        &mut self,
        kind: AtlasKind,
        layer: u16,
    ) -> &mut crate::render::DirtyRects {
        surface::atlas_rects_mut(self, kind, layer)
    }
    pub(crate) fn clear_atlas_rects(&mut self, kind: AtlasKind, layer: u16) {
        surface::clear_atlas_rects(self, kind, layer)
    }
    pub(crate) fn clear_all_atlas_rects(&mut self, kind: AtlasKind) {
        surface::clear_all_atlas_rects(self, kind)
    }
    pub(crate) fn atlas_rects_pending(&self) -> bool {
        surface::atlas_rects_pending(self)
    }
    pub(crate) fn note_atlas_deferral(&mut self) -> bool {
        surface::note_atlas_deferral(self)
    }
    pub(crate) fn reset_atlas_deferrals(&mut self) {
        surface::reset_atlas_deferrals(self)
    }
    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        layer: u16,
        extent: u32,
        pixels: &[u8],
        rects: crate::render::DirtyRects,
    ) -> bool {
        surface::queue_atlas_upload(self, kind, layer, extent, pixels, rects)
    }
    pub(crate) fn atlas_idle(&self, kind: AtlasKind) -> bool {
        surface::atlas_idle(self, kind)
    }
    pub(crate) fn set_renderer(&mut self, renderer: Renderer) {
        surface::set_renderer(self, renderer)
    }
    /// 实现见子模块 `surface::take_renderer`（macOS 设备恢复：先摘旧 renderer 再重建）。
    #[cfg(target_os = "macos")]
    pub(crate) fn take_renderer(&mut self) -> Option<Renderer> {
        surface::take_renderer(self)
    }
    /// 实现见子模块 `surface::has_compositor_ticket`（设备恢复据此决定是否覆盖合成线程 sink）。
    #[cfg(target_os = "macos")]
    pub(crate) fn has_compositor_ticket(&self) -> bool {
        surface::has_compositor_ticket(self)
    }
    pub(crate) fn poll_renderer(&mut self) -> Result<(), EngineError> {
        surface::poll_renderer(self)
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn sync_pacing(&mut self, continuous: bool) {
        surface::sync_pacing(self, continuous)
    }
    /// 实现见子模块 `surface::enable_offscreen_renderer`（本文件只保留签名）。
    pub fn enable_offscreen_renderer(&mut self) -> Result<(), EngineError> {
        surface::enable_offscreen_renderer(self)
    }
    fn physical_surface(&self) -> (u32, u32) {
        surface::physical_surface(self)
    }
    #[cfg(windows)]
    pub(crate) fn presented_surface(&self) -> (u32, u32) {
        surface::presented_surface(self)
    }
    #[cfg(windows)]
    pub(crate) fn take_composition_pending(&self) -> bool {
        surface::take_composition_pending(self)
    }
    fn frame_rect(&self) -> PhysicalRect {
        surface::frame_rect(self)
    }

    // ── scale：实现见 `scale` 子模块的自由函数 ──
    /// 实现见子模块 `scale::scale`（本文件只保留签名）。
    #[must_use]
    pub fn scale(&self) -> Scale {
        scale::scale(self)
    }
    /// 实现见子模块 `scale::set_scale`（本文件只保留签名）。
    pub fn set_scale(&mut self, scale: Scale) {
        scale::set_scale(self, scale)
    }
    /// 实现见子模块 `scale::scale_epoch`（本文件只保留签名）。
    #[must_use]
    pub fn scale_epoch(&self) -> u32 {
        scale::scale_epoch(self)
    }

    // ── focus：实现见 `focus` 子模块的自由函数 ──
    /// 实现见子模块 `focus::focus`（本文件只保留签名）。
    #[must_use]
    pub fn focus(&self) -> &FocusScope {
        focus::focus(self)
    }
    pub(crate) fn focus_mut(&mut self) -> &mut FocusScope {
        focus::focus_mut(self)
    }
    /// 实现见子模块 `focus::text_caret_rect`（本文件只保留签名）。
    pub fn text_caret_rect(&self, node: NodeId, byte_index: usize) -> Result<Rect, EngineError> {
        focus::text_caret_rect(self, node, byte_index)
    }
    pub(crate) fn set_ime_cursor_request(&mut self, node: NodeId, byte_index: usize) -> bool {
        focus::set_ime_cursor_request(self, node, byte_index)
    }
    pub(crate) fn clear_ime_cursor_request(&mut self) -> bool {
        focus::clear_ime_cursor_request(self)
    }
    pub(crate) fn ime_cursor_area(&self) -> Result<Option<Rect>, EngineError> {
        focus::ime_cursor_area(self)
    }

    // ── X2：自绘标题栏拖动区（Chrome / Electron `-webkit-app-region: drag` 模型） ──

    /// 设置自绘标题栏的**可拖动区域**（X2；三端原生路径见下方列表）。
    ///
    /// 区域是**逻辑窗口坐标**（客户区左上角为原点，x 右 y 下），可以有多个（顶部条带 + 侧栏
    /// 把手）；传空切片清空。引擎不自动跟随布局：应用在布局/尺寸变化后重新调用（Chrome
    /// `SetDraggableRegions` 同模型）。命中区域**不再投递**指针事件给应用 —— 拖动区是窗口
    /// chrome，不是内容。
    ///
    /// 平台语义（用户裁决 X1/X2「不得静默忽略」）：
    ///
    /// - **Windows**：命中区域的 `WM_NCHITTEST` 返回 `HTCAPTION` ⇒ 走系统 move 模态循环，
    ///   与系统标题栏完全同路径（双击最大化/还原、Aero Snap、`Alt+Space` 系统菜单都成立；
    ///   拖动期间的出帧由 B1 的模态 tick 保证）。
    /// - **macOS**：`GuiView mouseDown:` 命中区域 ⇒ `performWindowDragWithEvent:`（AppKit
    ///   同步拖动），不进入引擎指针分发。用于恢复 `FullSizeContentView` 下被 view 挖掉的
    ///   顶部拖动条（AppKit 的 drag map 只让 `mouseDownCanMoveWindow == YES` 且有内容的视图
    ///   可拖）。
    /// - **Linux/Wayland**：CSD 窗口命中区域的主键按下 ⇒ `xdg_toplevel.move(seat, serial)`
    ///   交给 compositor 驱动（协议没有客户端侧拖动）；不投递 `PointerDown`。没有
    ///   `xdg_toplevel` / compositor 不实现 move 时降级为无操作（应用仍可用
    ///   `WindowControl::BeginMove` 的显式路径）。
    ///
    /// 消费者：`App::set_drag_regions`（平台泵每轮对账到原生窗口）。
    pub fn set_drag_regions(&mut self, regions: &[Rect]) {
        if self.titlebar_drag_regions == regions {
            return;
        }
        self.titlebar_drag_regions.clear();
        self.titlebar_drag_regions.extend_from_slice(regions);
    }

    /// 当前拖动区（平台层读点；X2）。
    #[must_use]
    pub(crate) fn drag_regions(&self) -> &[Rect] {
        &self.titlebar_drag_regions
    }

    // ── Linux：文本输入的语义类型（`zwp_text_input_v3.set_content_type`）──

    /// 声明本窗口当前焦点文本域的语义类型（Linux/Wayland；其它平台由原生控件语义表达）。
    ///
    /// 应用在焦点进入敏感输入框（密码 / API key）时调用 [`ImeContentType::Password`]，离开时
    /// 切回 [`ImeContentType::Text`]。平台层每轮泵读取最新值，变化时下发
    /// `set_content_type` + `commit`（text-input-v3 的请求是双缓冲状态）。
    ///
    /// 语义范围是**整个窗口**（一个 `zwp_text_input_v3` 只描述当前焦点 surface 的上下文），
    /// 所以同一窗口里密码框与普通输入框并存时，以最后声明的为准 —— 宁可让普通输入框少一点
    /// 联想，也不能让密钥进入输入法词库。
    #[cfg(target_os = "linux")]
    pub fn set_ime_content_type(&mut self, content: ImeContentType) {
        self.ime_content = content;
    }

    /// 当前声明的文本输入语义类型（平台层读点）。
    #[cfg(target_os = "linux")]
    #[must_use]
    pub(crate) fn ime_content_type(&self) -> ImeContentType {
        self.ime_content
    }

    // ── Linux：compositor 侧的窗口状态镜像 ──

    /// 平台写入的窗口状态（compositor 说了算；应用画标题栏时读它）。
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn state(&self) -> WindowState {
        self.state
    }

    /// 平台层写入新状态；返回是否真的变化（同值 no-op，避免每轮无意义地失效样式）。
    ///
    /// 消费者：`platform::linux::event_loop` 的逐窗口镜像。
    #[cfg(target_os = "linux")]
    pub(crate) fn set_state(&mut self, state: WindowState) -> bool {
        if self.state == state {
            return false;
        }
        self.state = state;
        true
    }

    // ── record：实现见 `record` 子模块的自由函数 ──
    #[cfg(feature = "test-support")]
    pub(crate) fn record(&self) -> Option<&FrameRecord> {
        record::record(self)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn record_idle(&mut self) {
        record::record_idle(self)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn record_budget_deferred(&mut self) {
        record::record_budget_deferred(self)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn attach_a11y(&mut self, snapshot: crate::a11y::A11yTreeSnapshot) {
        record::attach_a11y(self, snapshot)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn set_frame_stats(&mut self, stats: FrameStats) {
        record::set_frame_stats(self, stats)
    }
    /// 最近一帧的 pass / 编码统计（**仅 `test-support`**：基准与集成测试用它断言增量编码的
    /// 事实 —— `encoded_lanes` / `reused_segments` / `uploaded_bytes`）。
    ///
    /// 来源是**帧记录**（`self.stats` 在提交时被 `capture` 取走，不能直接读）。
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn frame_stats(&self) -> Option<&FrameStats> {
        self.record.as_ref().map(|record| &record.stats)
    }
    #[cfg(feature = "hud")]
    pub(crate) fn reset_debug_hud_timing(&mut self) {
        record::reset_debug_hud_timing(self)
    }
    #[cfg(feature = "hud")]
    pub(crate) fn debug_hud_metrics(&self) -> crate::debug_hud::DebugHudMetrics {
        record::debug_hud_metrics(self)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn force_full_repaint(&mut self) {
        record::force_full_repaint(self)
    }
}
