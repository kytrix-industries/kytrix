//! 窗口：句柄、建窗参数、节点树 + 绘制场景的持有者。
//!
//! 设计见 `design/phase-03-node-state.md` §5.2 / §15。P03 是 headless 的：只保存参数，不驱动平台
//! （平台窗口句柄、DPI 事件、标题栏属 P12）。

use std::{cell::Cell, rc::Rc, time::Instant};

use smol_str::SmolStr;

#[cfg(feature = "test-support")]
use crate::test_support::record::{FrameRecord, FrameStats, PaintLog};
use crate::{
    error::EngineError,
    interaction::{FocusScope, PointerState, scroll},
    layout::LayoutReport,
    math::{PhysicalRect, Point, Rect, Scale, Size},
    node::{dirty::DirtyFlags, id::NodeId, tree::NodeTree},
    paint::{
        atlas::AtlasKind,
        color::{Color, ColorResolver, Interpolation},
        instance::{InstanceBuffer, InstanceEncoder},
        pass::{PaintPass, PaintReport},
        scene::Scene,
    },
    render::{
        backend::Renderer,
        compositor::{Plan, plan},
        damage::DamageAccumulator,
        frame::{FramePayload, FrameRequest},
        timer::FrameTimings,
    },
};

/// 图集行没赶上帧时，最多连续补几帧（窗口的共享槽长期不空时避免空转）。
const MAX_ATLAS_FOLLOWUPS: u8 = 3;

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

/// headless 版帧状态机（设计 §3.5 的 10 态在一次同步调用内部，见 F-66）。
///
/// crate 私有：P06 没有公开的观察者（测试用 `FrameOutcome`/`frame_index` 断言）；P09 的事件循环
/// 需要时再决定是否公开。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameState {
    /// 没有待办的帧。
    Idle,
    /// 已请求一帧，尚未开始。
    Pending,
    /// 正在跑 pass（`begin` 之后、`commit`/`abort` 之前）。
    Draining,
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

/// 窗口级帧调度（`FrameIndex` 的唯一递增点在这里）。
pub(crate) struct FrameScheduler {
    /// 帧状态。用 `Rc<Cell<_>>` 而不是裸字段：`FrameTicket` 的 `Drop` 需要在没有
    /// `&mut FrameScheduler` 的情况下复位它（panic 展开时拿不到借用）。
    state: Rc<Cell<FrameState>>,
    committed: u32,
}

impl FrameScheduler {
    pub(crate) fn new() -> Self {
        Self {
            state: Rc::new(Cell::new(FrameState::Idle)),
            committed: 0,
        }
    }

    /// `Idle → Pending`；返回是否真的发生了转换（幂等）。
    pub(crate) fn request_frame(&mut self) -> bool {
        if self.state.get() == FrameState::Idle {
            self.state.set(FrameState::Pending);
            true
        } else {
            false
        }
    }

    /// `Pending → Draining`；不在 `Pending` 时返回 `None`（调用方须先 `request_frame`）。
    pub(crate) fn begin(&mut self) -> Option<FrameTicket> {
        if self.state.get() == FrameState::Pending {
            self.state.set(FrameState::Draining);
            Some(FrameTicket {
                state: Rc::clone(&self.state),
                consumed: false,
            })
        } else {
            None
        }
    }

    /// `Draining → Idle`，`committed += 1`，返回新序号（**唯一**递增点）。
    pub(crate) fn commit(&mut self, mut ticket: FrameTicket) -> FrameIndex {
        ticket.consumed = true;
        self.state.set(FrameState::Idle);
        let next = self.committed.checked_add(1);
        debug_assert!(next.is_some(), "FrameIndex overflow (u32::MAX frames)");
        // 溢出时**饱和**而不是回绕到 0：0 是「从未提交」的哨兵，回绕会让 in-flight 判据失效。
        self.committed = next.unwrap_or(u32::MAX);
        let index = FrameIndex(self.committed);
        debug_assert!(index.get() >= 1, "提交序号必须 >= 1");
        index
    }

    /// `Draining → Idle`，序号不变（空闲帧 / 错误中止）。
    pub(crate) fn abort(&mut self, mut ticket: FrameTicket) {
        ticket.consumed = true;
        self.state.set(FrameState::Idle);
    }

    pub(crate) fn committed(&self) -> u32 {
        self.committed
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

    /// 焦点状态（只读；写入口是 `App::{set_focus, focus_next, focus_prev}`）。
    #[must_use]
    pub fn focus(&self) -> &FocusScope {
        &self.focus
    }

    /// 焦点状态的可写引用（写入口是 `App::set_focus`，它负责标脏）。
    pub(crate) fn focus_mut(&mut self) -> &mut FocusScope {
        &mut self.focus
    }

    /// 指针状态（crate 私有：`App::dispatch_input` 与平台泵是唯一读者）。
    pub(crate) fn pointer(&self) -> &PointerState {
        &self.pointer
    }

    /// 指针状态的可写引用。
    pub(crate) fn pointer_mut(&mut self) -> &mut PointerState {
        &mut self.pointer
    }

    // ── P16：IME caret hint ────────────────────────────────────────────────────

    /// 登记 IME caret hint；返回是否真的变化（同值 no-op）。
    pub(crate) fn set_ime_cursor_request(&mut self, node: NodeId, byte_index: usize) -> bool {
        let next = ImeCursorRequest { node, byte_index };
        if self.ime_cursor == Some(next) {
            return false;
        }
        self.ime_cursor = Some(next);
        true
    }

    /// 清除 IME caret hint；返回是否真的变化。
    pub(crate) fn clear_ime_cursor_request(&mut self) -> bool {
        self.ime_cursor.take().is_some()
    }

    /// 解析当前 IME caret hint（窗口客户区逻辑坐标）。
    ///
    /// `Ok(None)` = 没有 hint；`Err(TextShape)` = 已登记但文本尚未 shape / 文本已变化，
    /// 平台层应跳过本次更新并保留上一次有效区域。
    pub(crate) fn ime_cursor_area(&self) -> Result<Option<Rect>, EngineError> {
        let Some(request) = self.ime_cursor else {
            return Ok(None);
        };
        self.text_caret_rect(request.node, request.byte_index)
            .map(Some)
    }

    /// 文本插入点的窗口绝对矩形（逻辑像素）。
    ///
    /// `NodeTree::text_caret_rect` 给出文本盒局部矩形；本方法补上节点 border box 原点、
    /// 内容盒偏移（border + padding）与祖先滚动偏移。
    ///
    /// # Errors
    /// `StaleNode`：节点不存在；`TextShape`：不是已 shape 的文本节点或 `byte_index` 非法。
    pub fn text_caret_rect(&self, node: NodeId, byte_index: usize) -> Result<Rect, EngineError> {
        let local = self.tree.text_caret_rect(node, byte_index)?;
        let bounds = self.tree.get(node)?.layout().bounds;
        let content_offset = self.tree.content_offset(node);
        let origin = Point::new(
            bounds.origin.x + content_offset.x,
            bounds.origin.y + content_offset.y,
        );
        Ok(scroll::to_screen(&self.tree, node, local.translate(origin)))
    }

    /// 请求下一帧重绘一个逻辑矩形（滚动视口；并集语义）。
    pub(crate) fn request_damage(&mut self, rect: Rect) {
        self.pending_damage = Some(match self.pending_damage {
            Some(existing) => existing.union(rect),
            None => rect,
        });
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

    /// 只读场景（S-10 / S-33：应用层**拿不到** `&mut Scene`，`Scene` 也**没有**公开构造器）。
    #[must_use]
    pub fn scene(&self) -> &Scene {
        &self.scene
    }

    /// 建窗参数快照。`background` 可在运行期被 [`Window::set_background_color`] 改写。
    #[must_use]
    pub fn options(&self) -> &WindowOptions {
        &self.options
    }

    /// 改窗口背景色（同值 no-op）：写 `options.background`；有渲染器时把新颜色交给平台层。
    ///
    /// Windows 上值变化会标全量重绘：组合交换链没有可用的 `SetBackgroundColor`，新颜色既作为
    /// persist 的 clear color（linear），也重建独立的 **1×1 背景层**（`refresh_background`）。
    /// GPU 线程在**空闲等待前**或下一次取帧录制前应用，因此不要求提交内容帧；UI 线程只写共享槽
    /// 并唤醒（I1）。
    ///
    /// macOS 上值变化同样标全量重绘：`CAMetalLayer.backgroundColor` 立刻在主线程写，persist 的
    /// clear color 经共享槽交给 GPU 线程（同样在空闲或下一帧录制前应用）。
    ///
    /// Linux 上值变化同样标全量重绘：clear color（linear）经共享槽交给 GPU 线程，在空闲或下一帧
    /// 录制前写进持久目标，下一次 present 生效。
    pub fn set_background_color(&mut self, color: Color) {
        if self.options.background == color {
            return;
        }
        self.options.background = color;
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        {
            // 背景色不是绘制图元：没有 damage 的帧可能永远不被 present，必须显式标全量。
            self.full_damage_pending = true;
            if let Some(renderer) = self.renderer.as_mut() {
                renderer.set_background_color(color);
            }
        }
    }

    /// 当前生效的 DPI 比例。
    #[must_use]
    pub fn scale(&self) -> Scale {
        self.scale
    }

    /// 窗口逻辑尺寸（根布局的可用空间）。
    #[must_use]
    pub fn size(&self) -> Size {
        self.options.size
    }

    /// 逻辑尺寸变化（消费者：P12 的 resize 事件）：写尺寸 + 标根 `LAYOUT` + 清根缓存。
    ///
    /// **不调整 OS 窗口大小**：只改逻辑布局与 damage。平台层在真实 resize 之后用 `set_surface`
    /// 写入真实渲染目标；平台已写过真值时本方法保留它，应用代码不要在活窗口上用本方法改
    /// 客户区（F-186 / F-189）。只有平台尚未写入过目标（headless / 快照 / 首帧前）时，
    /// `surface` 才跟随 `size × scale` 的 covering。
    pub fn set_size(&mut self, size: Size) {
        // 同值短路：DPI 路径当轮已按新 DPI 写回逻辑尺寸，随后那条 `WM_SIZE` 会以同值再调
        // 一次；不做早退就会多出一整帧布局 + 全量重绘（`set_surface` 一直是这么做的）。
        if self.size() == size {
            return;
        }
        self.options.size = size;
        self.full_damage_pending = true;
        let root = self.tree.root();
        // 与 `set_style` / `set_layout_override` 同一条失效路径（P11：它也标 `A11Y`）。
        self.tree.invalidate_layout(root);
        // 平台真值一旦写入就不再被 covering 覆盖（F-186）：否则应用在活窗口上调本方法会把
        // 渲染目标重置成 `size × scale` 的猜测值。
        if !self.surface_from_platform {
            self.surface = covering_surface(self.size(), self.scale);
        }
    }

    /// 用窗口逻辑尺寸重排，返回本次布局统计（P06/P09 的帧驱动调用它）。
    ///
    /// **文本节点必须先在本帧调用 `App::refresh_text`**：`NodeBuilder::text` 不设 measure，
    /// 没跑文本 pass 的文本节点测量为 `0×0`（P05 §13.4）。
    ///
    /// # Errors
    /// `EngineError::InvalidMeasure { node, axis, value }`：某个 measure 闭包返回 NaN / 负数（首条优先）。
    /// 出错时布局盒可能为空，调用方**不得**提交本帧。
    pub fn compute_layout(&mut self) -> Result<LayoutReport, EngineError> {
        let report = self.tree.compute_layout(taffy::geometry::Size {
            width: Some(self.options.size.width),
            height: Some(self.options.size.height),
        })?;
        // P11：布局盒变了的节点必须重发 a11y（bounds = 物理化的 layout 盒）。窗口 resize /
        // 虚拟列表重窗都只标根，子树的新盒由布局 pass 写入，不经过任何 `mark_*`。
        self.tree.mark_moved_a11y();
        Ok(report)
    }

    /// 测试专用：最近一次编码出的几何绘制（校验 `App` 的几何表真的接进了编码器）。
    #[cfg(test)]
    pub(crate) fn encoded_geometry(&self) -> &[crate::paint::instance::GeometryDrawGpu] {
        self.encoder.buffer().geometry()
    }

    /// **不调整 OS 窗口 / 渲染目标**：只改比例与布局失效。平台层随后用 `set_surface` 写入真实
    /// 渲染目标；平台已写过真值时本方法保留它，应用代码不要在活窗口上用本方法改客户区
    /// （F-186 / F-189）。只有平台尚未写入过目标（headless / 快照 / 首帧前）时，`surface` 才
    /// 跟随 `size × scale` 的 covering。
    ///
    /// DPI 变化（P12 的事件处理调用它）：更新比例并标根 `LAYOUT | PAINT`。
    ///
    /// **逻辑布局盒与 scale 无关**（I11：布局全用逻辑像素，物理化在 paint/P06），这里只保守地失效。
    ///
    /// **不**置 `App` 的唤醒请求：DPI 事件本身来自平台，平台总是会调度一帧（P12）。
    pub fn set_scale(&mut self, scale: Scale) {
        self.scale = scale;
        self.scale_epoch = self.scale_epoch.wrapping_add(1);
        self.full_damage_pending = true;
        let root = self.tree.root();
        self.tree.invalidate_layout(root);
        // **所有文本必须重新 shape**：`font_size` 与 `ppem` 在 shape 时按当时的 `scale`
        // 烘焙进 parley `Run` 与字形位图。只标 LAYOUT 时 `layout_text` 会"重排"，但用的仍是
        // 旧 ppem 的 `ShapedText` → 旧位图以新比例放大，文字发虚、字面不一致，直到该文本内容
        // 再次变化。旧实现每帧全量扫描时靠 `slot.scale_epoch != epoch` 覆盖了这条路径，
        // 改成脏集合驱动后必须显式标脏（P1 复审发现）。
        self.tree.mark_all_text_dirty();
        // a11y 的 bounds 全是物理像素 ⇒ 比例变了必须整棵重发（P11 §5.5）。
        self.a11y.note_bounds_changed();
        // 平台真值一旦写入就不再被 covering 覆盖（F-186）。
        if !self.surface_from_platform {
            self.surface = covering_surface(self.size(), self.scale);
        }
    }

    /// 平台层：写入真实渲染目标物理尺寸（Windows `WM_SIZE` / `WM_DPICHANGED`、macOS
    /// `convertSizeToBacking` 之后、Linux `round(逻辑 × scale)` 的 Wayland buffer 尺寸）。
    ///
    /// 值变化 ⇒ 标全量重绘（新暴露的像素没有对应 damage）；**不**标 layout、**不**动
    /// `scale_epoch`：本方法只改渲染目标，不改布局；DPI 的布局变化由 `set_scale` / `set_size` 负责。
    ///
    /// 调用一次之后 `surface` 归平台层所有：`set_size` / `set_scale` 不再用 covering 覆盖它
    /// （F-186 / F-189）。同值调用也会把 `surface` 的所有权交给平台（先于 early-return）。
    ///
    /// macOS 还必须在这里刷新图层属性（`contentsScale` / `drawableSize`）：这是主线程的写入口，
    /// 且必须在「值没变就返回」**之前**执行 —— DPI 变化但 surface 恰好相同时也要更新
    /// `contentsScale`。只变 scale 时也必须刷新。
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    pub(crate) fn set_surface(&mut self, surface: (u32, u32)) {
        let surface = (surface.0.max(1), surface.1.max(1));
        self.surface_from_platform = true;
        #[cfg(target_os = "macos")]
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.set_layer_scale(surface, self.scale.factor());
        }
        if self.surface == surface {
            return;
        }
        self.surface = surface;
        self.full_damage_pending = true;
    }

    /// macOS：换屏后把 display link 的偏好帧率更新到新屏幕的刷新率。
    ///
    /// 旧实现只在建窗时读一次 `maximumFramesPerSecond`；窗口被拖到不同刷新率的屏幕上之后
    /// 仍按旧值请求（ProMotion 120 Hz 屏上被限在 60 fps，反之在 60 Hz 屏上请求 120）。
    /// 其他平台没有「每屏刷新率」概念，方法不定义（平台层按 cfg 调用）。
    #[cfg(target_os = "macos")]
    pub(crate) fn set_max_frame_rate(&mut self, max_frame_rate: f32) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.set_frame_rate(max_frame_rate);
        }
    }

    /// Linux：写入整数回退的 `wl_surface` buffer scale（fractional 路径恒 1）。
    ///
    /// **不**标 damage / layout、**不**动 `scale_epoch`：它只是渲染目标的属性，随
    /// [`FrameRequest`] 过线，由 GPU 线程在 present 前与 attach/commit 同一次生效（V-110）。
    /// scale 本身的布局变化由 `set_scale` 负责；同值调用是 no-op，钳到 ≥1。
    #[cfg(target_os = "linux")]
    pub(crate) fn set_buffer_scale(&mut self, scale: i32) {
        self.buffer_scale = scale.max(1);
    }

    // ── P11：a11y（快照交付 + 捕获） ─────────────────────────────────────────

    /// 本轮是否有 a11y 工作（O(1)；平台泵每轮用它过滤 `update_if_active` 调用）。
    #[must_use]
    pub(crate) fn a11y_pending(&self) -> bool {
        self.a11y.pending(&self.tree)
    }

    /// 产出一笔 a11y 交付（消费 `A11Y|ORDER` 位与 `full_pending`）。
    ///
    /// 调用方：平台层的 `update_if_active` 闭包（生产）、`App::frame` 的捕获路径（测试）。
    pub(crate) fn a11y_take(
        &mut self,
        diagnostics: &mut Vec<crate::a11y::A11yDiagnostic>,
    ) -> Option<crate::a11y::A11yTreeSnapshot> {
        let Self {
            tree,
            a11y,
            focus,
            scale,
            options,
            ..
        } = self;
        a11y.take(
            tree,
            focus.focused(),
            *scale,
            options.title.as_str(),
            diagnostics,
        )
    }

    /// 某个子树内容整体平移（滚动：子孙 bounds 全变 ⇒ 重发整棵子树）。
    pub(crate) fn a11y_note_scroll(&mut self, container: crate::node::id::NodeId) {
        self.a11y.invalidate_subtree(container);
    }

    /// 只有 `bind_a11y` 的节点被写信号（没有像素要画，但下帧仍要跑样式 pass）。
    pub(crate) fn a11y_note_bind_work(&mut self) {
        self.a11y.note_bind_work();
    }

    /// 清掉「a11y 绑定待求值」旗标（`refresh_styles` 的入口处）。
    pub(crate) fn a11y_clear_bind_work(&mut self) {
        self.a11y.clear_bind_work();
    }

    /// 打开/关闭 a11y 快照捕获（仅 `test-support` 的 `TestApp` 调用）。
    #[cfg(feature = "test-support")]
    pub(crate) fn set_a11y_capture(&mut self, on: bool) {
        self.a11y.set_capture(on);
    }

    /// DPI 代际（`set_scale` 递增；`App::refresh_text` 的图集换代判据）。
    #[must_use]
    pub fn scale_epoch(&self) -> u32 {
        self.scale_epoch
    }

    // ── P06：帧推进 ───────────────────────────────────────────────────────────

    /// 提交游标（0 = 从未提交；空闲帧不变）。
    #[must_use]
    pub fn frame_index(&self) -> FrameIndex {
        FrameIndex(self.scheduler.committed())
    }

    /// 本帧是否需要画面（两段门禁的第一段）。
    ///
    /// `wake` = `App::take_wake_request()` 的结果（`App::frame` 传入；`Window::frame` 传 `false`）。
    /// `!REUSABLE` 表示 lane 内容与树/样式不一致；`LAYOUT | PAINT` 覆盖「新建/未标 REUSABLE」的节点。
    pub(crate) fn needs_frame(&self, wake: bool) -> bool {
        // `a11y_bind_pending`：只有 `bind_a11y` 的节点被写信号时**没有**像素要画，但
        // `refresh_styles` 仍必须跑一次（否则补丁永远不更新）。帧末的门禁会把它退回 `Idle`。
        wake || self.full_damage_pending
            || self.pending_damage.is_some()
            || self.a11y.bind_work()
            || self.tree.any_node(|node| {
                let dirty = node.dirty();
                dirty.intersects(DirtyFlags::LAYOUT | DirtyFlags::PAINT)
                    || !dirty.contains(DirtyFlags::REUSABLE)
            })
    }

    /// `Idle → Pending → Draining`；空闲/重复调用返回 `None`。
    pub(crate) fn begin_frame(&mut self) -> Option<FrameTicket> {
        if !self.scheduler.request_frame() {
            // 已经是 Pending/Draining：重复请求不产生新帧。
            return None;
        }
        let ticket = self.scheduler.begin()?;
        #[cfg(feature = "hud")]
        {
            self.frame_started_at = Some(std::time::Instant::now());
        }
        Some(ticket)
    }

    /// 仅当有 `LAYOUT` 时才重排（纯颜色变化不跑布局）。
    pub(crate) fn layout_if_dirty(&mut self) -> Result<Option<LayoutReport>, EngineError> {
        if !self
            .tree
            .any_node(|node| node.dirty().contains(DirtyFlags::LAYOUT))
        {
            return Ok(None);
        }
        self.compute_layout().map(Some)
    }

    /// 最近一帧的编码结果（渲染层上传源；P08 的 `Renderer` 消费）。
    #[must_use]
    pub(crate) fn instance_buffer(&self) -> &InstanceBuffer {
        self.encoder.buffer()
    }

    /// 本帧编码出的实例字节（P07 的上传入口；应用层不得依赖其内容）。
    ///
    /// 返回最近一帧 `InstanceEncoder` 的 `&[u8]`（`GpuPrimitive` 数组的字节视图，P02 §8.1 的 128 B 布局）。
    #[must_use]
    pub fn instance_bytes(&self) -> &[u8] {
        self.instance_buffer().as_bytes()
    }

    /// 是否已有 GPU 渲染器（`App::upload_atlas_pages` 的判据）。
    pub(crate) fn has_renderer(&self) -> bool {
        self.renderer.is_some()
    }

    /// 该窗口的渲染器是否已收到完整字形图集页（测试诊断）。
    #[cfg(test)]
    pub(crate) fn glyph_page_received(&self) -> bool {
        self.glyph_page_received.is_some()
    }

    /// 该窗口的渲染器是否已收到完整图片图集页（测试诊断）。
    #[cfg(test)]
    pub(crate) fn image_page_received(&self) -> bool {
        self.image_page_received.is_some()
    }

    /// 已收到的整页尺寸是否就是 `page`（否则这一帧必须整页重传）。
    pub(crate) fn atlas_page_matches(&self, kind: AtlasKind, page: u32) -> bool {
        match kind {
            AtlasKind::Glyph => self.glyph_page_received == Some(page),
            AtlasKind::Image => self.image_page_received == Some(page),
        }
    }

    /// 记账：该图集页已整页入队到本窗口的渲染器（记录页尺寸）。
    pub(crate) fn mark_atlas_page_received(&mut self, kind: AtlasKind, page: u32) {
        match kind {
            AtlasKind::Glyph => self.glyph_page_received = Some(page),
            AtlasKind::Image => self.image_page_received = Some(page),
        }
    }

    /// 该图集在本窗口的整页记账作废：下一次必须整页重传（渲染器换代 / 图集清空）。
    pub(crate) fn mark_atlas_page_stale(&mut self, kind: AtlasKind) {
        match kind {
            AtlasKind::Glyph => self.glyph_page_received = None,
            AtlasKind::Image => self.image_page_received = None,
        }
    }

    /// 本窗口待上传的图集脏行（忙窗口攒着，空闲时补发）。
    pub(crate) fn atlas_rows(&self, kind: AtlasKind) -> &crate::render::DirtyRows {
        &self.atlas_rows[kind.index() as usize]
    }

    /// 本窗口待上传的图集脏行（可变：并入本帧新写的行）。
    pub(crate) fn atlas_rows_mut(&mut self, kind: AtlasKind) -> &mut crate::render::DirtyRows {
        &mut self.atlas_rows[kind.index() as usize]
    }

    /// 本窗口的待发行已入队（或已被整页上传覆盖）。
    pub(crate) fn clear_atlas_rows(&mut self, kind: AtlasKind) {
        self.atlas_rows[kind.index() as usize].clear();
    }

    /// 本窗口还有没赶上帧的图集行：这一帧会以旧图集呈现，需要补一帧。
    pub(crate) fn atlas_rows_pending(&self) -> bool {
        self.atlas_rows.iter().any(|rows| !rows.is_empty())
    }

    /// 记一次「图集行没赶上帧」；返回是否还应再补一帧（上限 [`MAX_ATLAS_FOLLOWUPS`]，
    /// 槽长期不空时不能无限空转）。
    pub(crate) fn note_atlas_deferral(&mut self) -> bool {
        if self.atlas_deferrals >= MAX_ATLAS_FOLLOWUPS {
            return false;
        }
        self.atlas_deferrals += 1;
        true
    }

    /// 图集行已经全部入队：补帧计数复位。
    pub(crate) fn reset_atlas_deferrals(&mut self) {
        self.atlas_deferrals = 0;
    }

    /// 请求下一帧（全量 damage）：平台泵用它补发没赶上本帧的图集行。
    pub(crate) fn request_repaint(&mut self) {
        self.full_damage_pending = true;
    }

    /// 把一页图集像素交给渲染器；`false` = 没有渲染器或渲染器已关闭（**没有**入队）。
    ///
    /// `pixels` 是 `rows` 顺序**紧密打包**的行（每行 `page * texel` 字节；整页上传时就是整页），
    /// 因此增量上传只搬脏行（DEVIATIONS F-211）。消费者：`App::upload_atlas_pages`（逐窗口
    /// 分发；图集页是进程级共享数据，纹理是每窗口一份）。
    pub(crate) fn queue_atlas_upload(
        &mut self,
        kind: AtlasKind,
        page: u32,
        pixels: &[u8],
        rows: crate::render::DirtyRows,
    ) -> bool {
        self.renderer
            .as_mut()
            .is_some_and(|renderer| renderer.queue_atlas_upload(kind, page, pixels, rows))
    }

    /// 该图集的图集上传槽是否空闲（UI 线程据此决定这一帧能否入队，DEVIATIONS F-211）。
    pub(crate) fn atlas_idle(&self, kind: AtlasKind) -> bool {
        self.renderer
            .as_ref()
            .is_none_or(|renderer| renderer.atlas_idle(kind))
    }

    /// 挂上 GPU 渲染器（平台层在窗口创建之后调用；`Renderer` 已带 HWND swapchain）。
    ///
    /// 换渲染器 = 换一套空纹理（`AtlasTextures`），所以两张图集页都要重新整页上传。
    pub(crate) fn set_renderer(&mut self, renderer: Renderer) {
        self.renderer = Some(renderer);
        self.glyph_page_received = None;
        self.image_page_received = None;
        self.atlas_rows = std::array::from_fn(|_| crate::render::DirtyRows::default());
    }

    /// 轮询 renderer 的反馈 / 错误（三平台窗口路径；空闲时也要跑）。
    ///
    /// GPU 线程以错误退出后这里会持续返回同一个错误（粘性），平台泵在下一轮冒泡出去。
    /// 反馈本身由渲染器消费（统计 / GPU 采样），这里只把 HUD 需要的发布时间延迟记到窗口上：
    /// 反馈是最新者胜，窗口永远拿到最近一次完成帧的 `latency`。
    pub(crate) fn poll_renderer(&mut self) -> Result<(), EngineError> {
        let Some(renderer) = self.renderer.as_mut() else {
            return Ok(());
        };
        // 三平台窗口路径一致：`poll` 返回 `Result<Option<FrameFeedback>, EngineError>`，
        // 离屏路径恒为 `Ok(None)`。
        let feedback = renderer.poll()?;
        #[cfg(feature = "hud")]
        if let Some(feedback) = feedback {
            self.last_frame_latency = Some(feedback.latency);
        }
        #[cfg(not(feature = "hud"))]
        let _ = feedback;
        Ok(())
    }

    /// display link 的暂停/恢复（macOS 平台泵**每轮**调用一次，§5.3 / I3）。
    ///
    /// 三平台里只有 macOS 需要它：Windows/Linux 的节奏完全由事件 / 合成器驱动，没有可暂停的源。
    /// 离屏渲染器的 `sync_pacing` 是空操作。
    #[cfg(target_os = "macos")]
    pub(crate) fn sync_pacing(&mut self, continuous: bool) {
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.sync_pacing(continuous);
        }
    }

    /// 启用**离屏** GPU 渲染（headless 行为不变：不调用就恒为 `None`）。
    ///
    /// P09 的原生窗口层会在此之后 attach swapchain；本方法本身只把窗口的绘制目标变成
    /// 持久 offscreen（快照 / 导出 / 无窗口验证），从不 present。
    ///
    /// # Errors
    /// `EngineError::UnsupportedGpu`（D8 基线不足）、`EngineError::AppInit`（设备 / 管线创建失败）、
    /// `EngineError::ShaderLoad`（DXIL 产物缺失或被跳过）。
    pub fn enable_offscreen_renderer(&mut self) -> Result<(), EngineError> {
        if self.renderer.is_some() {
            return Ok(());
        }
        let surface = self.physical_surface();
        self.renderer = Some(Renderer::new_offscreen(surface)?);
        self.glyph_page_received = None;
        self.image_page_received = None;
        self.atlas_rows = std::array::from_fn(|_| crate::render::DirtyRows::default());
        Ok(())
    }

    /// 窗口的物理像素尺寸（平台层写入的真实渲染目标；headless / 平台首次写入前是 covering）。
    fn physical_surface(&self) -> (u32, u32) {
        self.surface
    }

    /// 最近一次**完成 present** 的内容尺寸（物理像素）。
    ///
    /// Windows 的 DirectComposition 几何用它：交换链里实际有内容的区域就是这个尺寸，内容
    /// visual 的 clip 按它设置（**不缩放**，恒 1:1）；客户区只决定背景层的缩放
    /// （DEVIATIONS §36 F-205 / S-152）。
    ///
    /// 没有渲染器（headless / 平台尚未 attach）时等于本帧目标尺寸：没有「已呈现内容」可言。
    #[cfg(windows)]
    pub(crate) fn presented_surface(&self) -> (u32, u32) {
        match self.renderer.as_ref() {
            Some(renderer) => renderer.presented_surface(),
            None => self.surface,
        }
    }

    /// 合成几何兜底标记：GPU 线程的两次唤醒都失败时置位，泵的 ②b′ 消费后强制重算几何。
    #[cfg(windows)]
    pub(crate) fn take_composition_pending(&self) -> bool {
        self.renderer
            .as_ref()
            .is_some_and(Renderer::take_composition_pending)
    }

    /// 本帧的物理矩形（`paint_frame` / `commit_frame` 的唯一来源）。
    ///
    /// 三平台一致：平台层通过 `set_surface` 写入的真实渲染目标 —— Windows 真实客户区
    /// （至少 1 px）、macOS 真实 backing（`convertSizeToBacking`，文字按真实像素 1:1）、
    /// Linux Wayland buffer 的 `round(逻辑 × scale)`（文字按 buffer 像素 1:1）。
    /// headless / 平台首次写入之前是 `size × scale` 的 covering。
    fn frame_rect(&self) -> PhysicalRect {
        PhysicalRect::new(0, 0, self.surface.0 as i32, self.surface.1 as i32)
    }

    /// 是否有节点需要重建 lane（`LAYOUT` / `PAINT` / `REUSABLE` 不在）。
    ///
    /// `A11Y` 不在判据里：a11y 更新不产生 damage（`design §7.5` / P03 §9.3 表第 9 行）。
    fn needs_node_repaint(&self) -> bool {
        self.tree.any_node(|node| {
            let dirty = node.dirty();
            dirty.intersects(DirtyFlags::LAYOUT | DirtyFlags::PAINT)
                || !dirty.contains(DirtyFlags::REUSABLE)
        })
    }

    /// lane 重建/复用 + damage + 实例编码（`design/phase-06-paint-runtime.md` §9.6）。
    ///
    /// # Errors
    /// [`EngineError::ClipDepthExceeded`]：clip 嵌套超过上限（release 也失败，见 P14 审查）。
    /// 返回 `Err` 时本帧未提交：调用方必须 `abort_frame`，damage/dirty 保留待重试。
    pub(crate) fn paint_frame(
        &mut self,
        geometry: &crate::paint::geometry_store::GeometryStore,
    ) -> Result<PaintReport, EngineError> {
        // 借用式取出（保容量）：`tree` 与 `scene` 是不同字段，循环内可同时可变借用。
        for lane in self.tree.drain_removed_lanes() {
            self.scene.release_lane(lane);
        }
        let scale = self.scale;
        let frame = self.frame_rect();
        self.damage.reset(scale);
        // 几何内容在「隐藏 / 卸载 / 布局移动」路径上无法精确算出溢出范围（拿不到
        // `GeometryDesc`）：保守升级为全帧 damage，避免溢出部分残影（复审 A3）。
        if self.tree.take_geometry_damage_escalation() {
            self.full_damage_pending = true;
        }
        let has_damage = self.full_damage_pending || self.pending_damage.is_some();
        if self.full_damage_pending {
            self.damage.push_full();
            self.full_damage_pending = false;
        }
        // 滚动（P10 §7.8）：唯一来源是视口矩形；节点级 damage 不参与（偏移不写 `layout.bounds`）。
        if let Some(pending) = self.pending_damage.take() {
            self.damage.push(pending, 1.0, None);
        }
        // P11：只有 `A11Y` 脏（`bind_a11y` 重算）的帧**没有任何像素要画**：早退让 lane 保持
        // `REUSABLE`、damage 保持空，帧末门禁把本帧退回 `Idle`（P11 审查 #6）。
        if !has_damage && !self.needs_node_repaint() {
            return Ok(PaintReport {
                rebuilt: 0,
                damage: crate::render::damage::DamageSummary::default(),
                #[cfg(feature = "test-support")]
                instances: 0,
                #[cfg(feature = "test-support")]
                log: crate::test_support::record::PaintLog::default(),
            });
        }
        self.paint_order.clear();
        let focused = self.focus.focused();
        let mut pass = PaintPass {
            tree: &mut self.tree,
            geometry,
            scene: &mut self.scene,
            damage: &mut self.damage,
            scale,
            frame,
            order: &mut self.paint_order,
            focused,
            #[cfg(feature = "test-support")]
            log: PaintLog::default(),
        };
        let mut report = pass.run()?;
        let buffer = self
            .encoder
            .encode(&self.scene, scale, &self.resolver, geometry);
        #[cfg(feature = "test-support")]
        {
            report.instances = buffer.len();
        }
        #[cfg(not(feature = "test-support"))]
        {
            let _ = buffer;
            let _ = &mut report;
        }
        #[cfg(feature = "hud")]
        {
            self.frame_cpu = self.frame_started_at.map(|started| started.elapsed());
        }
        Ok(report)
    }

    /// 提交（或在本帧不会产生 GPU 工作时退回 `Idle`）。
    ///
    /// 第二段门禁：`rebuilt == 0` 且没有任何 damage ⇒ 没有像素要画；或者有像素但
    /// [`frame_reaches_gpu`] 判定本帧不会产生 GPU 工作（`Plan::Idle` 且目标尺寸未变）。
    /// 两种情况都不提交、`FrameIndex` 不变，按 `Idle` 结算 —— Linux 帧门禁依赖
    /// 「`Committed` ⇒ 已 publish」（V-122 ⑧ / F-183）。
    ///
    /// 窗口路径的 GPU 错误（device lost / present 失败 / swapchain 重建失败）**不**从这里返回：
    /// 本方法只把帧发布给 GPU 线程（不等待），错误由 `poll_renderer` 粘性冒泡。编码 / 帧结算
    /// 已在 `paint_frame` 与布局阶段完成，本方法体内没有任何可失败的步骤，因此返回
    /// [`FrameOutcome`] 而不是恒 `Ok` 的 `Result`。
    pub(crate) fn commit_frame(
        &mut self,
        ticket: FrameTicket,
        report: PaintReport,
    ) -> FrameOutcome {
        let has_pixels = report.rebuilt > 0 || report.damage.full || report.damage.rects > 0;
        // 默认构建也算的读点：摘要计数必须与累加器的实际矩形数一致（P07 消费同一累加器）。
        debug_assert_eq!(
            report.damage.rects,
            self.damage.rects().len(),
            "damage summary and accumulator disagree"
        );
        if !has_pixels {
            // `paint_frame` 没有跑（或没产出任何 lane / 像素）：没有节点状态需要结算，
            // damage 与 dirty 保留给下一次重试。
            self.scheduler.abort(ticket);
            #[cfg(feature = "hud")]
            {
                self.frame_started_at = None;
                self.frame_cpu = None;
            }
            #[cfg(feature = "test-support")]
            self.record_idle();
            return FrameOutcome::Idle;
        }
        // 本帧只算一次：`submit_gpu` 消费同一份 `plan`（目标尺寸也来自同一次 `frame_rect`）。
        let frame = self.frame_rect();
        let renderer_surface = self.renderer.as_ref().map(Renderer::surface);
        let plan = plan(report.damage, self.damage.rects(), frame);
        // Windows：目标尺寸变化必须整帧重画（`set_content_size` 在 buffer 内是纯 CPU 记账，
        // `Plan::Idle` 会消费帧延迟 token 却不 present）。macOS / Linux 保持原规则。
        #[cfg(windows)]
        let plan = surface_change_plan(plan, renderer_surface, self.surface);
        if !frame_reaches_gpu(plan, renderer_surface, self.surface) {
            // paint 已经消费了 lane 重建与 damage：即使不发布，也必须像已提交的帧一样结算节点
            // 状态（damage / prev_layout / REUSABLE），否则后续会 over-damage、a11y 多标（P2-1）。
            // 只是不推进 `FrameIndex`、不发布（V-122 ⑧ / F-183）。
            self.tree.settle_frame();
            self.scheduler.abort(ticket);
            #[cfg(feature = "hud")]
            {
                self.frame_started_at = None;
                self.frame_cpu = None;
            }
            #[cfg(feature = "test-support")]
            self.record_idle();
            return FrameOutcome::Idle;
        }
        let index = self.scheduler.commit(ticket);
        self.tree.settle_frame();
        if let Some(mut renderer) = self.renderer.take() {
            let stats = self.submit_gpu(&mut renderer, index, plan);
            self.renderer = Some(renderer);
            #[cfg(feature = "hud")]
            {
                self.render_stats = stats;
            }
            #[cfg(not(feature = "hud"))]
            {
                let _ = stats;
            }
        }
        #[cfg(feature = "test-support")]
        {
            let stats = std::mem::take(&mut self.stats);
            let record = crate::test_support::record::capture(
                &self.scene,
                &self.tree,
                &self.resolver,
                &report,
                self.damage.rects(),
                frame,
                index,
                stats,
            );
            self.record = Some(record);
        }
        FrameOutcome::Committed(index)
    }

    /// 把本帧交给 GPU：窗口路径只构造 `FrameRequest` 并 `publish`（**不等待**，I1）。
    ///
    /// `plan` 由 [`Self::commit_frame`] 用同一份 damage 与 `frame` 算好；`Committed` 帧在这里
    /// **恰好发布一次**（`Plan::Idle` 且目标尺寸未变的帧已在 `commit_frame` 退回 `Idle`）。
    ///
    /// 返回最近一次**已完成**帧的统计（窗口路径落后 ≤1 帧，HUD 允许）；GPU 线程的错误在
    /// `poll_renderer` 里冒泡，提交路径没有同步错误。
    fn submit_gpu(
        &mut self,
        renderer: &mut Renderer,
        index: FrameIndex,
        plan: Plan,
    ) -> crate::render::RenderStats {
        let surface = self.physical_surface();
        // `plan` 的矩形已由 `commit_frame` 的 `plan(...)` 夹进本帧 frame；这里把「不变量被破坏」
        // 变成 debug 期显式失败（否则 D3D12 的 `Present1` / scissor 会以
        // `DXGI_ERROR_INVALID_CALL` 形式在 GPU 线程炸）。
        if let Plan::Partial(rects) = plan {
            for rect in rects.as_slice() {
                debug_assert!(
                    rect.x0 >= 0
                        && rect.x0 <= rect.x1
                        && rect.x1 <= surface.0 as i32
                        && rect.y0 >= 0
                        && rect.y0 <= rect.y1
                        && rect.y1 <= surface.1 as i32,
                    "damage rect {rect:?} escapes the surface {surface:?}"
                );
            }
        }
        #[cfg(target_os = "macos")]
        renderer.set_layer_scale(surface, self.scale.factor());
        // 窗口路径的 GPU 采样来自 `poll` 的反馈；本帧的同步采样表清空以免 HUD 显示过期数据。
        self.timings.reset(index);
        renderer.publish(FrameRequest {
            index,
            payload: FramePayload::from_buffer(self.instance_buffer()),
            plan,
            surface,
            scale: self.scale.factor(),
            // Linux：逻辑尺寸与 `surface`（buffer 尺寸）同帧过线，GPU 线程在 present 前用它写
            // `wp_viewport.set_destination`（destination 与 buffer 必须成对，D1 / D2）。
            #[cfg(target_os = "linux")]
            surface_local: self.size(),
            // 整数回退的 buffer scale：与 attach/commit 同一次生效（fractional 模式恒 1，
            // 整数回退 = N；surface 的逻辑尺寸因此等于 `surface / buffer_scale`，V-110）。
            #[cfg(target_os = "linux")]
            buffer_scale: self.buffer_scale,
            published_at: Instant::now(),
        })
    }

    /// 中止（错误路径）：不结算、不递增序号（damage/dirty 保留，下一次重试）。
    pub(crate) fn abort_frame(&mut self, ticket: FrameTicket) {
        self.scheduler.abort(ticket);
        #[cfg(feature = "hud")]
        {
            self.frame_started_at = None;
            self.frame_cpu = None;
        }
        #[cfg(feature = "test-support")]
        self.record_idle();
    }

    /// 窗口本地帧（**无 App 通道**：不跑样式/文本 pass）。
    ///
    /// 含 `bind_style` / `bind_text` 的窗口必须走 `App::frame`，否则补丁不会重新求值
    /// （debug 构建会直接断言失败）。
    ///
    /// # Errors
    /// `EngineError::InvalidMeasure`：布局闭包返回非有限值/负尺寸（本帧已中止，未提交）。
    pub fn frame(&mut self) -> Result<FrameOutcome, EngineError> {
        if !self.needs_frame(false) {
            #[cfg(feature = "test-support")]
            self.record_idle();
            return Ok(FrameOutcome::Idle);
        }
        debug_assert!(
            !self.tree.any_node(crate::node::tree::Node::has_bindings),
            "Window::frame does not run style/text passes; use App::frame for bound nodes"
        );
        let Some(ticket) = self.begin_frame() else {
            return Ok(FrameOutcome::Idle);
        };
        match self.layout_if_dirty() {
            Ok(_) => {}
            Err(error) => {
                self.abort_frame(ticket);
                return Err(error);
            }
        }
        // 无 `App` 的独立窗口（probe / 离屏测试）没有几何注册表：传一个进程级空表。
        static NO_GEOMETRY: std::sync::OnceLock<crate::paint::geometry_store::GeometryStore> =
            std::sync::OnceLock::new();
        let geometry = NO_GEOMETRY.get_or_init(crate::paint::geometry_store::GeometryStore::new);
        let report = match self.paint_frame(geometry) {
            Ok(report) => report,
            Err(error) => {
                self.abort_frame(ticket);
                return Err(error);
            }
        };
        Ok(self.commit_frame(ticket, report))
    }

    /// 最近一帧的调度记录（仅 `test-support`）。
    #[cfg(feature = "test-support")]
    pub(crate) fn record(&self) -> Option<&FrameRecord> {
        self.record.as_ref()
    }

    /// 记录一次空闲帧（不提交、序号不变）。
    #[cfg(feature = "test-support")]
    pub(crate) fn record_idle(&mut self) {
        self.record = Some(FrameRecord::idle(
            FrameIndex(self.scheduler.committed()),
            std::mem::take(&mut self.stats),
        ));
    }

    /// 把一笔 a11y 交付挂到**最近一帧**的记录上（`TestApp::drain` 在帧之后调用，与平台泵同序）。
    #[cfg(feature = "test-support")]
    pub(crate) fn attach_a11y(&mut self, snapshot: crate::a11y::A11yTreeSnapshot) {
        if let Some(record) = self.record.as_mut() {
            record.a11y = Some(snapshot);
        }
    }

    /// `App::frame` 在提交前写入本帧的统计（仅 `test-support`）。
    #[cfg(feature = "test-support")]
    pub(crate) fn set_frame_stats(&mut self, stats: FrameStats) {
        self.stats = stats;
    }

    /// 重置 HUD 的提交时间基准（开启/重新开启 HUD 时调用）。
    #[cfg(feature = "hud")]
    pub(crate) fn reset_debug_hud_timing(&mut self) {
        self.render_stats = crate::render::RenderStats::default();
        self.frame_cpu = None;
        self.frame_interval = None;
        self.last_commit_at = None;
        self.frame_started_at = None;
        self.last_frame_latency = None;
    }

    /// 记录一次提交的时间戳（P17 HUD 的帧间隔/FPS 来源）。
    #[cfg(feature = "hud")]
    pub(crate) fn note_debug_hud_commit(&mut self, now: std::time::Instant) {
        self.frame_interval = self
            .last_commit_at
            .map(|previous| now.saturating_duration_since(previous));
        self.last_commit_at = Some(now);
    }

    /// 最近提交帧的 HUD 指标（P17）。
    #[cfg(feature = "hud")]
    pub(crate) fn debug_hud_metrics(&self) -> crate::debug_hud::DebugHudMetrics {
        use crate::render::timer::Section;

        let (physical_width, physical_height) = self.physical_surface();
        let upload_time = self
            .timings
            .sample(Section::Upload)
            .map(|sample| std::time::Duration::from_nanos(sample.cpu_ns));
        let gpu_time = self
            .timings
            .sample(Section::Gpu)
            .map(|sample| std::time::Duration::from_nanos(sample.gpu_ns));
        let present_time = self
            .timings
            .sample(Section::Present)
            .map(|sample| std::time::Duration::from_nanos(sample.cpu_ns));
        // 窗口路径的 GPU 工作已移到 GPU 线程：主线程没有本帧的 samples，
        // 用最近完成帧的采样补齐（HUD 允许落后一帧）。
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        let (upload_time, gpu_time, present_time) = match self.renderer.as_ref() {
            Some(renderer) => (
                upload_time.or_else(|| {
                    renderer
                        .timing_sample(Section::Upload)
                        .map(|sample| std::time::Duration::from_nanos(sample.cpu_ns))
                }),
                gpu_time.or_else(|| {
                    renderer
                        .timing_sample(Section::Gpu)
                        .map(|sample| std::time::Duration::from_nanos(sample.gpu_ns))
                }),
                present_time.or_else(|| {
                    renderer
                        .timing_sample(Section::Present)
                        .map(|sample| std::time::Duration::from_nanos(sample.cpu_ns))
                }),
            ),
            None => (upload_time, gpu_time, present_time),
        };
        crate::debug_hud::DebugHudMetrics {
            frame_index: self.frame_index(),
            frame_interval: self.frame_interval,
            cpu_time: self.frame_cpu,
            upload_time,
            gpu_time,
            present_time,
            latency: self.last_frame_latency,
            draw_calls: self.render_stats.draw_calls,
            instances: self.render_stats.instances,
            primitives: self.scene.primitive_count(),
            physical_width,
            physical_height,
            scale: self.scale.factor(),
        }
    }

    /// 强制全量重绘（oracle 的「全量」路径）：清 `REUSABLE` + 清空所有 lane 的内容。
    #[cfg(feature = "test-support")]
    pub(crate) fn force_full_repaint(&mut self) {
        self.tree.clear_all_reusable();
        self.scene.clear_frame();
    }
}

/// `size × scale` 的 covering 物理尺寸（向外 floor/ceil，至少 1 px）。
///
/// 只在平台层首次调用 [`Window::set_surface`] 之前使用（headless / 快照 / 首帧前）；平台真值
/// 一旦写入，`set_size` / `set_scale` 就保留它，不再用 covering 覆盖（F-186 / F-189 / V-122）。
fn covering_surface(size: Size, scale: Scale) -> (u32, u32) {
    let rect = Rect::new(0.0, 0.0, size.width, size.height).to_physical_covering(scale);
    (rect.width().max(1) as u32, rect.height().max(1) as u32)
}

/// 本帧是否会**产生 GPU 工作**（`commit_frame` 的提交判据之一）。
///
/// - 没有渲染器（`renderer_surface == None`，headless / 纯 CPU 帧）⇒ `true`：没有 GPU 线程可
///   交付，帧仍按 `Committed` 结算。
/// - 有渲染器 ⇒ `plan != Plan::Idle || renderer_surface != Some(surface)`。`Plan::Idle` 只表示
///   没有像素要画；若 `surface`（本帧目标尺寸）与渲染器最近一次发布的目标尺寸不同，GPU 线程
///   仍必须处理新目标尺寸，因此算有工作。
///
/// Windows 的 `commit_frame` 在调用本函数之前先用 [`surface_change_plan`] 把「目标尺寸变了」
/// 夹成 `Plan::Full`：组合交换链按高水位桶分配（2 s 静止后可能缩容），`set_content_size` 在
/// buffer 内不会重建任何资源，`Plan::Idle` 发布只会消费帧延迟 token 而不 present。
///
/// 返回 `false` 的帧不会 publish：`commit_frame` 必须把它按 `Idle` 结算，否则 Linux 帧门禁
/// 会等一次永远不会到来的 `wl_callback`（V-122 ⑧ / F-183）。
fn frame_reaches_gpu(
    plan: Plan,
    renderer_surface: Option<(u32, u32)>,
    surface: (u32, u32),
) -> bool {
    match renderer_surface {
        None => true,
        Some(published) => plan != Plan::Idle || published != surface,
    }
}

/// Windows：目标尺寸变化一律映射成 `Plan::Full`（任何输入计划）。
///
/// 组合交换链按高水位 buffer 分配（2 s 静止后可能缩容），`set_content_size` 在 buffer 内只是
/// CPU 记账；一个 `Plan::Idle` 的发布会让 GPU 线程消费一次帧延迟 waitable token 却不 present
/// （DXGI 的 token 守恒被破坏，下一帧白等一个 vsync）。`Plan::Partial` 的 damage 是**按旧目标**算的：新暴露的
/// 像素没有 damage，只画旧矩形会在新客户区留下未初始化内容，因此同样整帧重画。
/// macOS / Linux 的 GPU 线程会按新 surface 重建 swapchain，保持 [`frame_reaches_gpu`] 原规则。
#[cfg(windows)]
fn surface_change_plan(
    plan: Plan,
    renderer_surface: Option<(u32, u32)>,
    surface: (u32, u32),
) -> Plan {
    if renderer_surface.is_some_and(|published| published != surface) {
        Plan::Full
    } else {
        plan
    }
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::surface_change_plan;
    use super::{
        FrameIndex, FrameOutcome, Window, WindowId, WindowOptions, covering_surface,
        frame_reaches_gpu,
    };
    use crate::{
        math::{PhysicalRect, Scale, Size},
        node::dirty::DirtyFlags,
        paint::color::Color,
        render::compositor::{DamageRects, Plan},
    };

    fn window() -> Window {
        Window::new(WindowId::for_test(1), WindowOptions::default())
    }

    #[test]
    fn new_window_has_root_and_empty_scene() {
        let window = window();

        assert_eq!(window.id(), WindowId::for_test(1));
        assert_eq!(window.tree().order().len(), 1, "初始只有 root");
        assert_eq!(window.scene().lane_count(), 0);
        assert!(window.scene().is_empty());
        assert_eq!(window.scale(), Scale::ONE);
    }

    #[test]
    fn scene_starts_without_lanes() {
        let window = window();
        let scene = window.scene();

        assert_eq!(scene.lane_count(), 0);
        assert!(scene.order().is_empty());
        assert_eq!(scene.primitive_count(), 0);
        assert!(scene.is_empty());
    }

    #[test]
    fn set_scale_marks_root_layout() {
        let mut window = window();
        window.tree_mut().clear_all_dirty_for_test();
        let scale = Scale::new(2.0).expect("合法比例");

        window.set_scale(scale);

        assert_eq!(window.scale(), scale);
        let root = window.tree().root();
        let flags = window.tree().get(root).expect("root 存在").dirty();
        assert!(flags.contains(DirtyFlags::LAYOUT));
        assert!(flags.contains(DirtyFlags::PAINT));
    }

    #[test]
    fn window_options_default_values() {
        let options = WindowOptions::default();

        assert_eq!(options.title, "");
        assert_eq!(options.size.width, 800.0);
        assert_eq!(options.size.height, 600.0);
        assert_eq!(options.scale, Scale::ONE);
        assert_eq!(
            options.background,
            Color::BLACK,
            "默认背景必须是不透明黑（Color::default() 是透明）"
        );
    }

    #[test]
    fn set_background_color_updates_options_and_ignores_the_same_value() {
        let mut window = window();
        assert_eq!(window.options().background, Color::BLACK);
        assert!(!window.full_damage_pending);

        window.set_background_color(Color::WHITE);
        assert_eq!(window.options().background, Color::WHITE);
        #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
        assert!(
            window.full_damage_pending,
            "改色必须标一帧，否则背景色可能永不被 present"
        );

        // 同值 no-op：清掉 damage 之后再调同值，不得重新置位。
        window.full_damage_pending = false;
        window.set_background_color(Color::WHITE);
        assert_eq!(window.options().background, Color::WHITE);
        assert!(!window.full_damage_pending, "同值 no-op 不得标 damage");
    }

    /// 接线：`commit_frame` 的 frame 用真实 surface（不是 `size × scale` 的 covering）。
    #[cfg(all(windows, feature = "test-support"))]
    #[test]
    fn a_committed_frame_uses_the_platform_surface_not_the_covering_size() {
        let (mut window, _node) = painted_window();
        // 800×600 的 covering 与 449×300 不同：记录里的整帧矩形能区分两者。
        window.set_surface((449, 300));

        let outcome = window.frame().expect("帧成功");
        assert!(matches!(outcome, FrameOutcome::Committed(_)));
        let record = window.record().expect("已提交帧必须有记录");
        assert!(record.damage_full, "set_surface 标全量重绘");
        assert_eq!(
            record.damage,
            vec![crate::math::PhysicalRect::new(0, 0, 449, 300)],
            "frame 必须用真实 surface，而不是 covering"
        );
    }

    /// Linux：`frame_rect` 用平台层写入的真实 Wayland buffer 尺寸（`set_surface`），
    /// 不再是 `size × scale` 的 covering（文字按 buffer 像素 1:1）。
    #[cfg(target_os = "linux")]
    #[test]
    fn frame_rect_uses_the_platform_buffer_on_linux() {
        let mut window = window();
        // 初值是 covering（800×600）；平台层随后在 event_loop 里写入 round 后的 buffer。
        assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 800, 600));
        window.set_surface((999, 777));
        assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 999, 777));

        // 平台已写入真值：`set_size` 不得把它重算成 covering（F-186 / V-122）。
        window.set_size(Size::new(256.8, 100.5));
        assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 999, 777));
        window.set_surface((449, 176));
        assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 449, 176));

        // 0 尺寸由 `set_surface` 钳到 1 px（不能给 swapchain 0 尺寸）。
        window.set_surface((0, 0));
        assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 1, 1));
    }

    /// macOS：`frame_rect` 用平台层写入的真实 backing surface，不再是 covering（文字 1:1）。
    #[cfg(target_os = "macos")]
    #[test]
    fn frame_rect_uses_the_platform_backing_on_macos() {
        let mut window = window();
        // 初值是 covering（800×600）；平台层随后写入真实 backing。
        assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 800, 600));
        window.set_surface((1234, 777));
        assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 1234, 777));

        // 平台已写入真值：`set_size` 不得把它重算成 covering（F-186 / V-122）。
        window.set_size(Size::new(256.8, 100.5));
        assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 1234, 777));
        window.set_surface((449, 176));
        assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 449, 176));
    }

    #[test]
    fn surface_starts_and_tracks_the_covering_size() {
        let mut window = Window::new(
            WindowId::for_test(1),
            WindowOptions {
                size: Size::new(800.0, 600.0),
                ..WindowOptions::default()
            },
        );
        assert_eq!(window.physical_surface(), (800, 600));

        // 125 / 150 / 175%：`set_scale` / `set_size` 先把 surface 重算成 covering（向外取整）；
        // 平台层随后用 `set_surface` 写真值（Linux 是 round 的 Wayland buffer）。
        for (factor, expected) in [
            (1.25, (1000, 750)),
            (1.5, (1200, 900)),
            (1.75, (1400, 1050)),
        ] {
            window.set_scale(Scale::new(factor).expect("合法比例"));
            assert_eq!(window.physical_surface(), expected, "scale {factor}");
            assert_eq!(
                window.physical_surface(),
                covering_surface(window.size(), window.scale()),
                "surface 必须与 covering 公式一致"
            );
        }

        // 亚像素：covering 向外取整（256.8 × 1.75 = 449.4 → 450），不是 round。
        window.set_size(Size::new(256.8, 100.5));
        assert_eq!(window.physical_surface(), (450, 176));
        assert_eq!(
            window.physical_surface(),
            covering_surface(window.size(), window.scale())
        );
        // 零尺寸至少 1 px（不能给 DXGI 0 尺寸的 swapchain）。
        window.set_size(Size::new(0.0, 0.0));
        assert_eq!(window.physical_surface(), (1, 1));
    }

    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    #[test]
    fn set_surface_clamps_marks_full_damage_and_ignores_the_same_value() {
        let mut window = window();
        assert_eq!(window.physical_surface(), (800, 600));
        assert!(!window.full_damage_pending);
        window.tree_mut().clear_all_dirty_for_test();
        let epoch = window.scale_epoch();

        window.set_surface((0, 0));
        assert_eq!(window.physical_surface(), (1, 1), "零尺寸钳到 1 px");
        assert!(window.full_damage_pending, "surface 变化必须标全量 damage");
        let root = window.tree().root();
        assert!(
            !window
                .tree()
                .get(root)
                .expect("root")
                .dirty()
                .contains(DirtyFlags::LAYOUT),
            "set_surface 不标 layout（只是目标尺寸，不是布局变化）"
        );
        assert_eq!(window.scale_epoch(), epoch, "set_surface 不动 scale_epoch");

        window.full_damage_pending = false;
        window.set_surface((1, 1));
        assert!(!window.full_damage_pending, "同值 no-op 不得重复标全量");

        window.set_surface((10, 20));
        assert_eq!(window.physical_surface(), (10, 20));
        assert!(window.full_damage_pending);
    }

    /// Linux：整数回退的 buffer scale 默认 1（fractional 路径也固定 1），钳到 ≥1。
    #[cfg(target_os = "linux")]
    #[test]
    fn buffer_scale_defaults_to_one_and_clamps() {
        let mut window = window();
        assert_eq!(window.buffer_scale, 1, "默认 1（fractional 路径恒 1）");

        window.set_buffer_scale(0);
        assert_eq!(
            window.buffer_scale, 1,
            "0 必须钳到 1（wl_surface 不接受 0）"
        );

        window.set_buffer_scale(2);
        assert_eq!(window.buffer_scale, 2, "整数回退写入 N");
    }

    /// F-186 / F-189 / V-122：平台写入真实目标之后，`set_size` / `set_scale` 只做布局 / DPI
    /// 失效，不得把 `surface` 重置成 `size × scale` 的 covering。
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    #[test]
    fn set_size_and_set_scale_keep_a_platform_written_surface() {
        let mut window = window();
        window.tree_mut().clear_all_dirty_for_test();
        window.set_surface((999, 777));
        window.full_damage_pending = false;
        let epoch = window.scale_epoch();
        let root = window.tree().root();

        window.set_size(Size::new(256.8, 100.5));
        assert_eq!(
            window.physical_surface(),
            (999, 777),
            "平台真值不得被 covering（257×101）覆盖"
        );
        assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 999, 777));
        assert!(window.full_damage_pending, "尺寸变化仍必须整帧重绘");
        assert!(
            window
                .tree()
                .get(root)
                .expect("root 存在")
                .dirty()
                .contains(DirtyFlags::LAYOUT),
            "尺寸变化仍必须标根 LAYOUT"
        );
        assert_eq!(window.scale_epoch(), epoch, "set_size 不动 scale_epoch");

        window.tree_mut().clear_all_dirty_for_test();
        window.full_damage_pending = false;
        window.set_scale(Scale::new(1.25).expect("合法比例"));
        assert_eq!(
            window.physical_surface(),
            (999, 777),
            "DPI 变化不得覆盖平台真值"
        );
        assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 999, 777));
        assert!(window.full_damage_pending, "DPI 变化仍必须整帧重绘");
        assert!(
            window
                .tree()
                .get(root)
                .expect("root 存在")
                .dirty()
                .contains(DirtyFlags::LAYOUT),
            "DPI 变化仍必须标根 LAYOUT"
        );
        assert_eq!(window.scale_epoch(), epoch + 1, "scale_epoch 照常递增");

        // 平台层下一次显式写入仍然生效。
        window.set_surface((449, 176));
        assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 449, 176));

        // 同值 `set_surface` 也必须把 `surface` 的所有权交给平台（flag 先于 early-return）：
        // 之后 `set_scale` 不得把它重算成 covering（1000×750）。
        let mut same_value = Window::new(WindowId::for_test(1), WindowOptions::default());
        same_value.set_surface(covering_surface(Size::new(800.0, 600.0), Scale::ONE));
        same_value.set_scale(Scale::new(1.25).expect("合法比例"));
        assert_eq!(
            same_value.physical_surface(),
            (800, 600),
            "同值 set_surface 之后的 set_scale 不得把 800×600 重算成 1000×750"
        );
    }

    #[test]
    fn window_size_is_root_available_space() {
        use crate::node::builder::NodeBuilder;

        let mut window = Window::new(
            WindowId::for_test(1),
            WindowOptions {
                size: Size::new(120.0, 60.0),
                ..WindowOptions::default()
            },
        );
        let root = window.tree().root();
        let child = NodeBuilder::boxed()
            .build(window.tree_mut(), root)
            .expect("挂载成功");

        let report = window.compute_layout().expect("布局成功");

        assert_eq!(window.size(), Size::new(120.0, 60.0));
        assert_eq!(
            window.tree().get(root).expect("root").layout().bounds.size,
            Size::new(120.0, 60.0),
            "根 100% × 100% 撑满窗口"
        );
        assert!(report.laid_out >= 2, "根与子节点都要布局：{report:?}");
        let _ = child;
    }

    #[test]
    fn dpi_scale_does_not_change_logical_boxes() {
        use crate::{
            math::Rect,
            node::builder::NodeBuilder,
            paint::style::{Clip, Style},
        };

        fn fixed(width: f32, height: f32) -> Style {
            let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
            inner.size = taffy::geometry::Size {
                width: taffy::style::Dimension::length(width),
                height: taffy::style::Dimension::length(height),
            };
            Style {
                inner,
                clip: Clip::None,
            }
        }

        let mut window = Window::new(
            WindowId::for_test(1),
            WindowOptions {
                size: Size::new(120.0, 60.0),
                ..WindowOptions::default()
            },
        );
        let root = window.tree().root();
        let child = NodeBuilder::boxed()
            .style(fixed(10.5, 3.25))
            .build(window.tree_mut(), root)
            .expect("挂载成功");
        let _ = window.compute_layout().expect("布局成功");
        let before = window.tree().get(child).expect("存在").layout().bounds;

        window.set_scale(Scale::new(1.5).expect("合法比例"));
        let _ = window.compute_layout().expect("布局成功");

        assert_eq!(
            window.tree().get(child).expect("存在").layout().bounds,
            before,
            "逻辑布局盒与 scale 无关（物理化在 P06）"
        );
        assert_eq!(before, Rect::new(0.0, 0.0, 10.5, 3.25));
    }

    #[test]
    fn resize_relayouts_percent_children() {
        use crate::{
            node::builder::NodeBuilder,
            paint::style::{Clip, Style},
        };

        let mut window = Window::new(
            WindowId::for_test(1),
            WindowOptions {
                size: Size::new(100.0, 100.0),
                ..WindowOptions::default()
            },
        );
        let root = window.tree().root();
        let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::percent(1.0),
            height: taffy::style::Dimension::percent(1.0),
        };
        let child = NodeBuilder::boxed()
            .style(Style {
                inner,
                clip: Clip::None,
            })
            .build(window.tree_mut(), root)
            .expect("挂载成功");
        let _ = window.compute_layout().expect("布局成功");
        assert_eq!(
            window.tree().get(child).expect("存在").layout().bounds.size,
            Size::new(100.0, 100.0)
        );

        window.set_size(Size::new(300.0, 50.0));
        let _ = window.compute_layout().expect("布局成功");

        assert_eq!(
            window.tree().get(child).expect("存在").layout().bounds.size,
            Size::new(300.0, 50.0),
            "百分比尺寸随根可用空间变化"
        );
    }

    #[test]
    fn set_size_marks_root_layout_and_clears_cache() {
        use crate::node::builder::NodeBuilder;

        let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
        let root = window.tree().root();
        let _ = NodeBuilder::boxed()
            .build(window.tree_mut(), root)
            .expect("挂载成功");
        let _ = window.compute_layout().expect("布局成功");

        window.set_size(Size::new(640.0, 480.0));

        let flags = window.tree().get(root).expect("root 存在").dirty();
        assert!(
            flags.contains(DirtyFlags::LAYOUT),
            "尺寸变化必须标根 LAYOUT（实际 {flags:?}）"
        );
        assert_eq!(window.size(), Size::new(640.0, 480.0));
    }
    #[test]
    fn scale_epoch_bumps_on_dpi_change() {
        let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
        assert_eq!(window.scale_epoch(), 0);

        window.set_scale(Scale::new(1.5).expect("finite scale"));
        assert_eq!(window.scale_epoch(), 1);

        window.set_scale(Scale::new(2.0).expect("finite scale"));
        assert_eq!(window.scale_epoch(), 2);
        assert_eq!(window.scale().factor(), 2.0);
    }

    // ── P06：帧推进 ──

    use crate::{
        node::builder::NodeBuilder,
        paint::style::{PaintSpec, StylePatch},
    };

    fn painted_window() -> (Window, crate::node::id::NodeId) {
        let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
        let root = window.tree().root();
        let node = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(0.0, 0.0, 40.0, 16.0))
            .paint(PaintSpec {
                background: Color::WHITE,
                ..PaintSpec::default()
            })
            .build(window.tree_mut(), root)
            .expect("挂载成功");
        (window, node)
    }

    /// V-122 ⑧：提交判据的纯函数（不碰 GPU）。Full/Partial 有工作；Idle 只在目标尺寸变化时
    /// 才有工作；没有 renderer 的 headless 帧恒有工作。
    #[test]
    fn frame_reaches_gpu_tracks_plan_and_target_size() {
        let mut rects = DamageRects::new();
        assert!(rects.push(PhysicalRect::new(0, 0, 4, 4)));
        let partial = Plan::Partial(rects);
        let surface = (100, 50);

        assert!(
            frame_reaches_gpu(Plan::Full, Some(surface), surface),
            "Full 一定有 GPU 工作"
        );
        assert!(
            frame_reaches_gpu(partial, Some(surface), surface),
            "Partial 一定有 GPU 工作"
        );
        assert!(
            frame_reaches_gpu(Plan::Idle, None, surface),
            "没有 renderer（headless / 纯 CPU 帧）：仍按 Committed 结算"
        );
        assert!(
            !frame_reaches_gpu(Plan::Idle, Some(surface), surface),
            "Idle 且目标尺寸没变 ⇒ 不 publish"
        );
        assert!(
            frame_reaches_gpu(Plan::Idle, Some((99, 50)), surface),
            "Idle 但目标尺寸变了 ⇒ GPU 线程仍要重建目标"
        );
    }

    /// P2-4：Windows 的目标尺寸变化必须变成非 `Idle` 计划 —— `set_content_size` 在 buffer 内
    /// 只是 CPU 记账，`Idle` 发布会让 GPU 线程消费 waitable token 却不 present。
    #[cfg(windows)]
    #[test]
    fn a_windows_surface_change_never_publishes_an_idle_plan() {
        let surface = (100, 50);
        assert_eq!(
            surface_change_plan(Plan::Idle, Some((99, 50)), surface),
            Plan::Full,
            "目标尺寸变了 ⇒ 整帧重画（新暴露的像素没有 damage）"
        );
        assert_eq!(
            surface_change_plan(Plan::Idle, Some(surface), surface),
            Plan::Idle,
            "尺寸没变 ⇒ 保持 Idle（不凭空产生 GPU 工作）"
        );
        assert_eq!(
            surface_change_plan(Plan::Idle, None, surface),
            Plan::Idle,
            "没有渲染器时由 frame_reaches_gpu 决定，不在这里造 Full"
        );

        let mut rects = DamageRects::new();
        assert!(rects.push(PhysicalRect::new(0, 0, 4, 4)));
        assert_eq!(
            surface_change_plan(Plan::Partial(rects), Some((99, 50)), surface),
            Plan::Full,
            "目标尺寸变了：Partial 的 damage 是按旧目标算的，必须整帧重画"
        );
        assert_eq!(
            surface_change_plan(Plan::Partial(rects), Some(surface), surface),
            Plan::Partial(rects),
            "尺寸没变：Partial 原样保留"
        );
        assert_eq!(
            surface_change_plan(Plan::Partial(rects), None, surface),
            Plan::Partial(rects),
            "没有渲染器：不在这里造 Full"
        );
        assert!(frame_reaches_gpu(
            surface_change_plan(Plan::Idle, Some((99, 50)), surface),
            Some((99, 50)),
            surface
        ));
    }

    /// P2-1 / V-122 ⑧（端到端）：真实 renderer 下「有像素但 `plan == Plan::Idle`、目标尺寸
    /// 未变」的帧不 publish、不递增 `FrameIndex`，但 paint 已消费 lane 与 damage，必须像已提交
    /// 帧一样 `settle_frame` 复位节点状态（否则后续 over-damage / a11y 多标）。
    ///
    /// 构造：节点从一开始就在 800×600 目标之外（900,900 起）；先跑一帧结算 root 的初始布局
    /// damage（首帧必定是全帧，会掩掉被测的 `Plan::Idle`），再只改节点颜色（PAINT-only，
    /// 不冒泡到 root）。被测帧的 damage 只来自越界节点，被 `plan` 全部裁空 ⇒ `Plan::Idle`；
    /// 离屏渲染器目标仍是 800×600 ⇒ 没有 GPU 工作。
    ///
    /// 回归保护：旧实现只有 `!has_pixels` 一道门禁，本帧 `has_pixels == true`（paint 重建了
    /// lane 并产出 damage），因此会返回 `Committed(FrameIndex(2))` 并推进序号 —— 本测试的
    /// `Idle` / `FrameIndex(1)` 断言会失败。
    #[cfg(windows)]
    #[test]
    fn an_idle_plan_with_a_real_renderer_settles_node_damage() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
        window
            .enable_offscreen_renderer()
            .expect("offscreen renderer");
        let root = window.tree().root();
        let node = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(900.0, 900.0, 40.0, 16.0))
            .paint(PaintSpec {
                background: Color::WHITE,
                ..PaintSpec::default()
            })
            .build(window.tree_mut(), root)
            .expect("挂载成功");
        let warmup = window.frame().expect("预热帧成功");
        assert_eq!(
            warmup,
            FrameOutcome::Committed(FrameIndex(1)),
            "预热帧用于结算 root 的初始 damage"
        );
        assert!(
            window
                .tree()
                .get(node)
                .expect("节点存在")
                .damage()
                .is_empty(),
            "预热帧提交后节点 damage 已复位"
        );

        // 只改颜色：PAINT-only，不触发 root 布局，因此本帧 damage 只来自越界节点。
        let patch = StylePatch {
            background: Some(Color::BLACK),
            ..StylePatch::default()
        };
        assert!(window.tree_mut().set_style_patch(node, patch));
        assert!(window.tree_mut().mark_paint(node));
        assert!(
            !window
                .tree()
                .get(node)
                .expect("节点存在")
                .damage()
                .is_empty(),
            "PAINT 变更必须让节点累积 damage（否则下面的复位断言没有意义）"
        );

        let outcome = window.frame().expect("帧成功");

        assert_eq!(outcome, FrameOutcome::Idle, "有像素但到不了 GPU ⇒ Idle");
        assert_eq!(
            window.frame_index(),
            FrameIndex(1),
            "Idle 不推进 FrameIndex"
        );
        assert!(
            window
                .tree()
                .get(node)
                .expect("节点存在")
                .damage()
                .is_empty(),
            "paint 已消费 damage：Idle 也必须结算节点（P2-1）"
        );
    }

    #[test]
    fn frame_commits_once_and_increments_index() {
        let (mut window, _node) = painted_window();
        assert_eq!(window.frame_index().get(), 0);

        let outcome = window.frame().expect("帧成功");
        assert_eq!(outcome, FrameOutcome::Committed(FrameIndex(1)));
        assert_eq!(window.frame_index().get(), 1);
    }

    #[test]
    fn idle_frame_does_not_commit_or_increment() {
        let (mut window, _node) = painted_window();
        let _ = window.frame().expect("首帧");
        assert_eq!(window.frame_index().get(), 1);

        let outcome = window.frame().expect("第二帧");
        assert_eq!(outcome, FrameOutcome::Idle);
        assert_eq!(window.frame_index().get(), 1, "空闲帧不得递增序号");
    }

    #[test]
    fn paint_only_frame_skips_layout_and_reuses_lanes() {
        let (mut window, node) = painted_window();
        let _ = window.frame().expect("首帧");

        let patch = StylePatch {
            background: Some(Color::BLACK),
            ..StylePatch::default()
        };
        assert!(window.tree_mut().set_style_patch(node, patch));
        let _ = window.tree_mut().mark_paint(node);

        assert!(
            window.layout_if_dirty().expect("无错误").is_none(),
            "只改颜色不应触发布局"
        );
        let outcome = window.frame().expect("第二帧");
        assert!(matches!(outcome, FrameOutcome::Committed(_)));
    }

    /// P1-2.4 回归：帧内 panic 之后窗口必须还能出帧。
    ///
    /// 旧实现里 `FrameTicket` 没有 `Drop`，panic 展开只是把它丢掉，`FrameScheduler` 永久停在
    /// `Draining` —— `request_frame()` 之后恒 false，`App::frame` 永远返回 `Idle`，窗口冻结且
    /// 无 API 可复位。这里用一个会 panic 的 measure 闭包复现，然后断言第二帧仍能提交。
    #[test]
    fn a_panicking_frame_does_not_freeze_the_scheduler() {
        let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
        let root = window.tree().root();
        let node = NodeBuilder::boxed()
            .measure(Box::new(
                |_: taffy::Size<Option<f32>>, _: taffy::Size<taffy::AvailableSpace>| {
                    panic!("user measure closure exploded");
                },
            ))
            .build(window.tree_mut(), root)
            .expect("挂载成功");

        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| window.frame()));
        assert!(caught.is_err(), "闭包 panic 必须冒泡出 frame");
        assert_eq!(window.frame_index().get(), 0, "panic 帧不得递增序号");

        // 关键断言：调度器没有卡在 `Draining`。移除会 panic 的 measure 之后必须仍能出帧。
        let mut healthy = NodeBuilder::boxed();
        window.tree_mut().remove(node).expect("移除会 panic 的节点");
        healthy = healthy.paint(crate::paint::style::PaintSpec {
            background: crate::paint::color::Color::WHITE,
            ..crate::paint::style::PaintSpec::default()
        });
        let _ = healthy.build(window.tree_mut(), root).expect("健康节点");
        let outcome = window.frame();
        assert!(
            outcome.is_ok(),
            "panic 之后必须仍能出帧（否则窗口永久冻结）：{outcome:?}"
        );
    }

    #[test]
    fn frame_aborts_on_layout_error() {
        let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
        let root = window.tree().root();
        let _ = NodeBuilder::boxed()
            .measure(Box::new(
                |_: taffy::Size<Option<f32>>, _: taffy::Size<taffy::AvailableSpace>| {
                    Size::new(f32::NAN, 10.0)
                },
            ))
            .build(window.tree_mut(), root)
            .expect("挂载成功");

        let outcome = window.frame();
        assert!(matches!(
            outcome,
            Err(crate::error::EngineError::InvalidMeasure { .. })
        ));
        assert_eq!(window.frame_index().get(), 0, "中止不得递增序号");
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "use App::frame")]
    fn frame_rejects_bound_nodes_without_styles() {
        let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
        let root = window.tree().root();
        let _ = NodeBuilder::boxed()
            .layout_override(crate::math::Rect::new(0.0, 0.0, 10.0, 10.0))
            .bind_style(|_cx| StylePatch::default())
            .build(window.tree_mut(), root)
            .expect("挂载成功");

        let _ = window.frame();
    }
}
