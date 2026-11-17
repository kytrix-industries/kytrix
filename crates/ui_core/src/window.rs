//! 窗口：句柄、建窗参数、节点树 + 绘制场景的持有者。
//!
//! 设计见 `design/phase-03-node-state.md` §5.2 / §15。P03 是 headless 的：只保存参数，不驱动平台
//! （平台窗口句柄、DPI 事件、标题栏属 P12）。

mod a11y;
mod focus;
mod frame;
mod paint;
mod record;
mod scale;
mod surface;
#[cfg(test)]
mod tests;

use std::{cell::Cell, rc::Rc};

use smol_str::SmolStr;

use self::{
    frame::{FrameScheduler, FrameState},
    surface::covering_surface,
};
#[cfg(feature = "test-support")]
use crate::paint::instance::InstanceBuffer;
#[cfg(feature = "test-support")]
use crate::test_support::record::{FrameRecord, FrameStats};
use crate::{
    error::EngineError,
    interaction::{FocusScope, PointerState},
    layout::LayoutReport,
    math::{PhysicalRect, Rect, Scale, Size},
    node::{id::NodeId, tree::NodeTree},
    paint::{
        atlas::AtlasKind,
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

/// 建窗参数。P03 只保存，不驱动平台（P12 才消费）。
///
/// 三个平台创建的窗口默认都是**可关闭 / 最小化 / 最大化 / 可缩放**的：Windows 用
/// `WS_OVERLAPPEDWINDOW` 的系统标题栏，macOS 保留三个原生交通灯，Linux/Wayland 请求
/// 客户端装饰并由应用自绘标题栏（`WindowControl` 提供按钮/拖动所需的平台请求）。
///
/// # Windows 标题栏 / 非客户区交互
///
/// **完全交系统**（R07-2）：引擎不接管 `WM_NCCALCSIZE` / `WM_NCHITTEST` /
/// `WM_NCLBUTTONDOWN`。拖动标题栏、拖边框缩放、双击标题栏最大化/还原、右键标题栏与图标区的
/// 系统菜单、`Alt+Space`、以及 **Aero Snap / Win11 Snap Layouts** 都由 `DefWindowProcW` 进入
/// 的原生 move/size 模态循环提供（Chrome / GPUI / Qt 的选择相同 —— 这些手势只存在于系统循环
/// 内部，应用自己 `SetWindowPos` 永远无法复现）。
///
/// 代价（有意取舍）：模态循环期间本线程的消息泵被系统占用，窗口内容在拖动/缩放中**不重绘**
/// （渲染线程仍在跑，旧内容保持 1:1 呈现）；拖动中推进的动画/HUD 会在松手后补上。
#[derive(Clone, Debug, PartialEq)]
pub struct WindowOptions {
    /// 标题（`SmolStr`，AGENTS §2.2）。
    pub title: SmolStr,
    /// 初始逻辑尺寸。
    pub size: Size,
    /// 初始 DPI 比例（由平台在 P12 传入；headless 默认 `Scale::ONE`）。
    pub scale: Scale,
    /// 窗口背景色：内容未覆盖区域的底色（持久目标 clear；Windows 另有独立背景层）。
    ///
    /// 平台差异：
    /// - Windows：persist clear 的**未覆盖像素**（linear，与 macOS/Linux 同一条路径），也是
    ///   独立 1×1 背景层的颜色（DirectComposition 的组合交换链**没有**可用的
    ///   `SetBackgroundColor`，probe 实测返回 OK 但惰性）。窗口放大时露出的新区域就是这个
    ///   背景层；旧内容保持 1:1，不会被拉伸（DEVIATIONS §36 F-205 / S-152）。
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
    /// 滚动槽表：槽位 → （逻辑偏移，物理视口）。
    ///
    /// **接线点**：paint pass 为滚动容器分配槽位后写入这里（`Pass` 侧一行），此后滚动只改偏移，
    /// 不重建 lane（架构决策 B3B6 §4）。当前为空 ⇒ 所有 lane 的滚动槽恒 0，行为与旧实现一致。
    scroll_slots: Vec<(crate::math::Point, PhysicalRect)>,
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
    /// 该窗口的渲染器已收到的**完整**字形图集页尺寸（`None` = 还没收到 / 渲染器已换）。
    ///
    /// 图集页是进程级共享数据，但纹理是每窗口一份（`AtlasTextures`），因此必须逐窗口记账：
    /// 第一次（或渲染器重建后 / 页尺寸变化后）必须整页上传，之后才允许按脏行增量
    /// （P13 §8.2 / 评审阻塞 ② / DEVIATIONS F-208）。
    glyph_page_received: Option<u32>,
    /// 同上，图片图集页。
    image_page_received: Option<u32>,
    /// 每个图集在本窗口上「已写脏、还没入队」的行。
    ///
    /// 共享槽忙（GPU 线程还没录制上一笔上传）的窗口不参与全局等待：新行先攒在这里，
    /// 等该窗口的槽空闲再补发。否则一个不再出帧的窗口会永久占住图集上传（DEVIATIONS F-211）。
    atlas_rows: [crate::render::DirtyRows; 2],
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
    /// Linux：当前焦点文本域的语义类型（`zwp_text_input_v3.set_content_type` 的数据源）。
    ///
    /// 密码 / API key 输入框必须切到 [`ImeContentType::Password`]：text-input-v3 的默认值
    /// （`hint = none` / `purpose = normal`）会让输入法对其做联想与**用户词库学习**。
    #[cfg(target_os = "linux")]
    ime_content: ImeContentType,
    /// Linux：compositor 侧的窗口状态镜像（最大化 / 全屏 / 装饰模式 / 可用能力）。
    #[cfg(target_os = "linux")]
    state: WindowState,
    /// 最近一帧的实际 GPU 绘制统计（P17 HUD）。
    #[cfg(feature = "hud")]
    render_stats: crate::render::RenderStats,
    /// 最近一次反馈帧的「发布 → GPU 完成」时长（HUD 的 Latency 行；窗口路径落后 ≤1 帧）。
    #[cfg(feature = "hud")]
    last_frame_latency: Option<std::time::Duration>,
    /// 最近一帧的 CPU 时间（`begin_frame` → `paint_frame` 结束）。
    #[cfg(feature = "hud")]
    frame_cpu: Option<std::time::Duration>,
    /// 最近两次提交帧入口的间隔（首帧为 `None`）。
    #[cfg(feature = "hud")]
    frame_interval: Option<std::time::Duration>,
    /// 上一次提交帧的入口时间戳（App 时钟；用于 HUD 帧间隔）。
    #[cfg(feature = "hud")]
    last_commit_at: Option<std::time::Instant>,
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
            scheduler: FrameScheduler::new(),
            composer: Some(WindowComposer::new()),
            pending_delta: None,
            pending_payload: None,
            delta_builder: crate::render::delta::DeltaBuilder::new(),
            scroll_slots: Vec::new(),
            compositor_ticket: None,
            damage: DamageAccumulator::new(scale),
            resolver: ColorResolver,
            renderer: None,
            glyph_page_received: None,
            image_page_received: None,
            atlas_rows: std::array::from_fn(|_| crate::render::DirtyRows::default()),
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
            #[cfg(target_os = "linux")]
            ime_content: ImeContentType::default(),
            #[cfg(target_os = "linux")]
            state: WindowState::default(),
            #[cfg(feature = "hud")]
            render_stats: crate::render::RenderStats::default(),
            #[cfg(feature = "hud")]
            last_frame_latency: None,
            #[cfg(feature = "hud")]
            frame_cpu: None,
            #[cfg(feature = "hud")]
            frame_interval: None,
            #[cfg(feature = "hud")]
            last_commit_at: None,
            #[cfg(feature = "hud")]
            frame_started_at: None,
        }
    }

    /// 窗口句柄。
    #[must_use]
    pub fn id(&self) -> WindowId {
        self.id
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
    pub(crate) fn a11y_note_bind_work(&mut self) {
        a11y::a11y_note_bind_work(self)
    }
    pub(crate) fn a11y_clear_bind_work(&mut self) {
        a11y::a11y_clear_bind_work(self)
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn set_a11y_capture(&mut self, on: bool) {
        a11y::set_a11y_capture(self, on)
    }

    // ── frame：实现见 `frame` 子模块的自由函数 ──
    pub(crate) fn needs_frame(&self, wake: bool) -> bool {
        frame::needs_frame(self, wake)
    }
    pub(crate) fn begin_frame(&mut self) -> Option<FrameTicket> {
        frame::begin_frame(self)
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
    ) -> crate::render::RenderStats {
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
    ) -> Result<PaintReport, EngineError> {
        paint::paint_frame(self, geometry)
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
    #[cfg(test)]
    pub(crate) fn encoded_geometry(&self) -> &[crate::paint::instance::GeometryDrawGpu] {
        paint::encoded_geometry(self)
    }
    pub(crate) fn request_damage(&mut self, rect: Rect) {
        paint::request_damage(self, rect)
    }
    pub(crate) fn request_repaint(&mut self) {
        paint::request_repaint(self)
    }

    // ── surface：实现见 `surface` 子模块的自由函数 ──
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
    pub(crate) fn atlas_page_matches(&self, kind: AtlasKind, page: u32) -> bool {
        surface::atlas_page_matches(self, kind, page)
    }
    pub(crate) fn mark_atlas_page_received(&mut self, kind: AtlasKind, page: u32) {
        surface::mark_atlas_page_received(self, kind, page)
    }
    pub(crate) fn mark_atlas_page_stale(&mut self, kind: AtlasKind) {
        surface::mark_atlas_page_stale(self, kind)
    }
    pub(crate) fn atlas_rows(&self, kind: AtlasKind) -> &crate::render::DirtyRows {
        surface::atlas_rows(self, kind)
    }
    pub(crate) fn atlas_rows_mut(&mut self, kind: AtlasKind) -> &mut crate::render::DirtyRows {
        surface::atlas_rows_mut(self, kind)
    }
    pub(crate) fn clear_atlas_rows(&mut self, kind: AtlasKind) {
        surface::clear_atlas_rows(self, kind)
    }
    pub(crate) fn atlas_rows_pending(&self) -> bool {
        surface::atlas_rows_pending(self)
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
        page: u32,
        pixels: &[u8],
        rows: crate::render::DirtyRows,
    ) -> bool {
        surface::queue_atlas_upload(self, kind, page, pixels, rows)
    }
    pub(crate) fn atlas_idle(&self, kind: AtlasKind) -> bool {
        surface::atlas_idle(self, kind)
    }
    pub(crate) fn set_renderer(&mut self, renderer: Renderer) {
        surface::set_renderer(self, renderer)
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
    pub(crate) fn note_debug_hud_commit(&mut self, now: std::time::Instant) {
        record::note_debug_hud_commit(self, now)
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
