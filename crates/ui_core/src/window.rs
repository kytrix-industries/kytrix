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
use crate::test_support::record::{FrameRecord, FrameStats};
use crate::{
    error::EngineError,
    interaction::{FocusScope, PointerState},
    layout::LayoutReport,
    math::{PhysicalRect, Rect, Scale, Size},
    node::{id::NodeId, tree::NodeTree},
    paint::{
        atlas::AtlasKind,
        color::{Color, ColorResolver, Interpolation},
        instance::{InstanceBuffer, InstanceEncoder},
        pass::PaintReport,
        scene::Scene,
    },
    render::{backend::Renderer, compositor::Plan, damage::DamageAccumulator, timer::FrameTimings},
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
    /// 开始交互式移动（`xdg_toplevel._move(seat, serial)`）。
    BeginMove,
    /// 开始交互式缩放（`xdg_toplevel.resize(seat, serial, edge)`）。
    BeginResize(WindowResizeEdge),
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
/// # Windows 标题栏交互（P2-9）
///
/// 系统标题栏仍由 Windows **绘制**（最小化/最大化/关闭按钮的行为不变），但按下后原本会进入
/// Win32 模态循环的交互改由引擎消息循环接管：拖动标题栏、双击标题栏最大化/还原都在引擎自己
/// 的泵里处理，拖动期间事件分发、动画与 HUD 照常推进，不会阻塞。标题栏左上角图标区按普通
/// 标题栏处理（拖动 / 双击最大化）。
///
/// 代价：拖动不再经过系统的 move 循环，因此没有 Aero Snap（拖到屏幕边缘吸附/最大化）与
/// 摇动最小化；多显示器拖动按光标所在显示器的工作区钳制（标题栏不会被拖出屏幕）。需要吸附
/// 行为时由应用在窗口位置上自行实现。
///
/// 引擎**不提供**系统菜单：右键标题栏、标题栏左上角图标、`Alt+Space` 都不会弹出菜单
/// （系统菜单是 `TrackPopupMenu` 的模态循环，会重新引入 P2-9 的泵阻塞），标题栏右键也**不会**
/// 转成应用事件。需要上下文菜单的应用请用浮层能力在自绘控件上实现。
///
/// **无障碍影响**（第三轮复审 W2-1）：系统菜单原本提供键盘可达的「移动 / 大小（方向键）」与
/// 「最小化 / 最大化 / 关闭」入口，屏蔽后键鼠用户仍可用 `Win+方向键`、`Alt+F4`、双击标题栏与
/// 系统按钮，但「用方向键移动/缩放窗口」这一 AT/键盘路径不再存在。需要完整键盘窗口管理的应用
/// 应自行提供对应快捷键并在 UI 中暴露。
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
    /// 实例编码器（容量跨帧保留；P07 消费 `InstanceBuffer`）。
    encoder: InstanceEncoder,
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
        Self {
            id,
            options,
            tree: NodeTree::new(),
            scene: Scene::empty(),
            scale,
            surface,
            surface_from_platform: false,
            #[cfg(target_os = "linux")]
            buffer_scale: 1,
            scale_epoch: 0,
            scheduler: FrameScheduler::new(),
            encoder: InstanceEncoder::new(),
            damage: DamageAccumulator::new(scale),
            resolver: ColorResolver::new(Interpolation::default()),
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
    #[must_use]
    pub(crate) fn instance_buffer(&self) -> &InstanceBuffer {
        paint::instance_buffer(self)
    }
    /// 实现见子模块 `paint::instance_bytes`（本文件只保留签名）。
    #[must_use]
    pub fn instance_bytes(&self) -> &[u8] {
        paint::instance_bytes(self)
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
