//! `App`：状态表 + 依赖图 + 窗口集合 + runtime 句柄。
//!
//! P03 是 headless 的：没有事件循环、没有
//! `Application`/`AppHandle`（推迟 P08/P09），`App` 只负责「状态 + 窗口 + 语义脏标记」。
//!
//! 写路径是 push 模型（§7.5）：一次 `write` 沿依赖图把 owner 标脏，**不**遍历整棵树；
//! 帧内唯一的重活是 `refresh_styles`（样式求值 pass，P06 的 paint 阶段复用同一实现）。

mod a11y;
mod animation;
#[cfg(feature = "hud")]
mod debug;
mod event_queue;
mod focus;
mod frame;
mod geometry;
mod input;
mod invalidate;
mod overlay;
mod resources;
mod scroll;
mod signal_api;
mod style_pass;
mod text_pass;
mod timer;
mod virtual_list;
mod window_set;

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(feature = "hud")]
use crate::debug_hud::DebugHudState;
use crate::{
    error::EngineError,
    event::{
        Action, ActionHandler, ActionSource, CloseHandler, Event, InputEvent, InputOutcome,
        KeyBinding, KeyCode, KeyState, Modifiers, PointerButton,
    },
    interaction::{
        ActionCtx, AnimatedValue, AnimationId, AnimationSpec, DragInfo, DragState, Easing,
        InputThresholds, ItemAlign, Overlay, OverlayDismiss, OverlayOptions, OverlayPlacement,
        ScrollConfig, ScrollHandle, ScrollState, VirtualList, VirtualListConfig, VirtualListState,
    },
    layout::hit_test::HitResult,
    math::{Point, Rect, Scale, Size},
    node::{
        builder::NodeBuilder,
        dependency::{DependencyGraph, EvalStack, ReadCtx},
        id::NodeId,
        signal::{Derived, Signal, SignalId, SignalRef, SignalScope, StateStore, Transient},
    },
    paint::{
        color::Color,
        style::{Clip, Cursor},
    },
    platform::dispatcher::{Dispatcher, UserTask},
    render::{AtlasAllocator, MIN_ATLAS_PAGE},
    resources::{DecodeState, PendingResource, ResourceCache},
    text::{
        FontStack, FontStackStatus, InlineBoxSpec, ShapedInlineBox, ShapedTextHandle,
        TextDiagnostic, TextLayoutReport, TextPassReport, TextStyle, atlas::GlyphAtlas,
        context::TextContext, raster::GlyphRasterizer,
    },
    window::{FrameOutcome, FramePresentation, FrameTicket, Window, WindowId, WindowOptions},
};

/// a11y 诊断的环形容量（与 `text_diagnostics` 同口径：超出丢最旧）。
const A11Y_DIAGNOSTIC_CAPACITY: usize = 64;

/// a11y 交付的最小间隔（按窗口；P2-6 / E5）。
///
/// 100 ms 对应 10 Hz：屏幕阅读器自身也消化不了更快的变化，而实时行情会让 A11Y 脏位每帧为真。
/// **只约束「已接入后」的高频数据增量**（E5）：首树 / DPI 全量重建的分批（[`A11yBridge::backlog`]）
/// 与焦点变化、消费端重置一样走不节流旁路 —— 否则 40k 节点的 20 批首树会被拖成 ≈2 s。
const A11Y_THROTTLE: std::time::Duration = std::time::Duration::from_millis(100);

/// 一次样式求值 pass 的结果（P06 的 `FrameRecord` 会复用这些字段）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StylePassReport {
    /// 实际求值的 `bind_style` 闭包个数（不区分「补丁是否变化」）。
    pub evaluated: usize,
    /// pass 结束后该窗口内是否仍有节点待重排（读 `dirty_layout` 反向集合，O(1)）。
    pub layout_dirty: bool,
    /// pass 结束后该窗口内是否仍有节点待重建 lane（读 `dirty_paint` 反向集合，O(1)）。
    ///
    /// 两个字段都是 O(1) 的集合判空（R03-1 / R01-4）：旧实现为它们各做一次全树扫描，
    /// 而报告在非 test 构建里被直接丢弃。
    pub paint_dirty: bool,
    /// pass 结束后 `App::take_wake_request()` 会返回的值（**本报告不消费它**）。
    pub wake_requested: bool,
    /// 本次求值的 `bind_text` 闭包个数（含值未变的求值）。
    pub text_evaluated: usize,
    /// 本次文本值**真的变了**的节点数。
    pub text_changed: usize,
}

/// 一次节点子树卸载的清理计数（[`App::remove_node`] / [`App::remove_node_with_scope`]）。
///
/// 只做**诊断 / 断言**用：计数描述「引擎替应用清掉了什么」，不是业务状态。没有可清理的条目
/// 时为零（例如叶子节点没有任何 scoped 绑定 / 定时器 / 动画）。
///
/// **计数口径不同**：`scoped_key_bindings` / `timers` 按「作用域已不在树里」判定（会把其它
/// 路径遗留的死作用域一并收口，不能归属到本次删除）；`animations` 只按本次 `removed` 集合
/// 归属（共享动画保留）；`released_values` 来自显式作用域释放。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RemoveOutcome {
    /// 被卸载的节点数（含整棵子树）。
    pub removed_nodes: usize,
    /// 自动清除的焦点作用域化键绑定条数（`App::bind_key_scoped`）。
    pub scoped_key_bindings: usize,
    /// 自动取消的一次性定时器条数（`App::after_scoped`）。
    pub timers: usize,
    /// 自动取消的动画槽条数（重绘节点全部落在被卸载子树内 / 滚动动画目标节点被卸载 /
    /// 作用域释放路径取消）。
    pub animations: usize,
    /// [`App::remove_node_with_scope`] 释放的信号作用域值槽位数（信号 + 瞬态值；
    /// [`App::remove_node`] 恒为 0）。
    pub released_values: usize,
}

/// 引擎侧全部状态。字段全私有（`field_scoped_visibility_modifiers` 禁止字段级可见性）。
pub struct App {
    store: StateStore,
    graph: DependencyGraph,
    windows: ahash::HashMap<WindowId, Window>,
    eval_stack: EvalStack,
    wake_requested: bool,
    /// 下一个窗口 id（从 **1** 起单调 +1；`WindowId` 永不复用，0 非法）。
    next_window_id: u32,
    /// tokio 运行时的句柄（只保存；引擎内不得调用 `Handle::current()`，`clippy.toml` 已禁用）。
    runtime: tokio::runtime::Handle,
    /// 文本引擎（字体库 / 布局 scratch / 字体栈 / 形状缓存 / 诊断）：进程级一份。
    text: TextContext,
    /// 字形光栅器（swash `ScaleContext` + 被 pin 住的字体 blob）。
    raster: GlyphRasterizer,
    /// 字形图集页（进程级一份；DPI 换代时 `clear()` 换代码）。
    atlas: AtlasAllocator,
    /// 图片资源缓存（解码 + 图片图集页；与字形页分图集，且**不随 DPI 换代**，P13 §8.1）。
    resources: ResourceCache,
    /// 异步解码已交付、等待应用取走的结果（D-6）。`App::poll_resources` 每轮填充；
    /// 应用在 `on_frame` 钩子里用 [App::take_resource_results] 消费（把 Ready tile 换到节点 /
    /// Failed 画占位）。缓冲只由 UI 线程读写，取走时 `mem::take` 换空（容量归还）。
    pending_resource_results: Vec<(PendingResource, DecodeState)>,
    /// 静态几何的注册表（进程级；设备无关的 CPU 字节，各后端自缓存 GPU 缓冲）。
    geometry: crate::paint::geometry_store::GeometryStore,
    /// 字形 `key → tile` 缓存（与图集同事务换代）。
    glyph_atlas: GlyphAtlas,
    /// 文本脏集合的跨帧复用缓冲（`refresh_text` 的 `take_text_dirty` 输出；保容量）。
    text_dirty_scratch: Vec<crate::node::id::NodeId>,
    /// 「需要重建 `ShapedLine`」的文本节点集合的跨帧复用缓冲（R12-14；`layout_text` 的
    /// `take_text_layout_dirty` 输出，保容量）。
    text_layout_scratch: Vec<crate::node::id::NodeId>,
    /// 每窗口「图集已同步到的 `scale_epoch`」（DPI 变化 ⇒ 图集换代）。
    scale_epochs: ahash::HashMap<WindowId, u32>,
    /// 每窗口「图片内在尺寸已按哪个 `scale_epoch` 解析」（E7；DPI 变化 ⇒ 重解析 + 重排）。
    image_scale_epochs: ahash::HashMap<WindowId, u32>,
    /// 已按其重绘过文本的图集代际（P1-2.6）。
    ///
    /// 图集是进程级共享的：任一窗口 DPI 变化或字形容量逃逸都会让 `generation` 递增、
    /// 页内所有 tile 重排。文本 lane 里存的是旧 `AtlasTile`（旧 UV），所以代际一变就必须
    /// 让**所有窗口**的文本节点重绘；否则画的是旧 UV（错字/花字）。
    atlas_generation_seen: u32,
    /// 图集租约：`ShapedTextHandle::is_stale` 与 `Canvas::push_shaped_text` 读取的
    /// **当前** `(generation, eviction_epoch)`（单一真源仍是图集本身：`atlas.generation()` 与
    /// `glyph_atlas.eviction_epoch()`，打包见 [`crate::text::pack_atlas_lease`]）。
    ///
    /// 任何可能改变图集状态的入口（`layout_text` 的扩容/清空/逐 tile 淘汰、`set_font_stack`、
    /// `reload_system_fonts`、`shape_text` 自身）返回后都由
    /// `App::sync_atlas_generation_lease` 写入，因此独立于窗口的 shaping 句柄也能在图集换代
    /// **或**任一 tile 被淘汰后立即判定失活（D-5：不画旧 UV）。
    atlas_generation_lease: Arc<AtomicU64>,
    /// canvas 文本的**App 级默认光栅 scale**（v2.1 E1b，`§11.1`）：默认 1.0，焦点窗口的 DPI
    /// 变化时跟随（`App::note_window_focus` / `App::note_window_dpi`），可被
    /// [`App::set_text_scale`] 显式覆盖。
    ///
    /// 节点文本不用它（节点路径按各自窗口的 `Window::scale` 光栅）；它只服务
    /// `App::shape_text` 的无窗口 shaping。
    default_text_scale: f32,
    /// 应用显式覆盖的文本 scale（`None` = 跟随焦点窗口 DPI）。
    text_scale_override: Option<f32>,
    /// 当前持有键盘焦点的窗口（`Event::FocusChanged` 维护；`None` = 没有窗口有焦点）。
    ///
    /// 只用于「焦点窗口 DPI → canvas 文本默认 scale」这一条派生；无焦点时保留上一次的
    /// `default_text_scale`（最后一个焦点窗口的 DPI，语义见 [`App::text_scale`]）。
    focused_window: Option<WindowId>,
    /// 累积的图集脏矩形（P09 的增量上传输入；`layout_text` 合并、上传后清空；R06-3 起是 AOI 矩形）。
    pending_dirty: crate::render::DirtyRects,
    /// 字形页整页缓冲上次按哪个页尺寸重建（`None` = 还没建 / 刚清空）。
    ///
    /// 页尺寸变化时缓冲必须**清零重建**：旧页的行宽不同，残留字节会被当成新页内容上传。
    glyph_page_extent: Option<u32>,
    /// 图片图集 texture array 的**已建立**规格（`None` = 三端仍是 1×1 占位数组）。
    ///
    /// 「已建立」= 至少有一个窗口的渲染器已按该规格**整组入队**过（每层都是整页上传）。
    /// 它是 fail-closed 的增长闸门（RBT-01 不变量 2）：层数提升只有在**所有带渲染器的窗口**
    /// 本帧都能整组入队时才发生；否则这一帧不扩（旧 spec 的层内容仍然有效），下一帧重试。
    /// 设备侧的 `AtlasUploads::reconcile_pages` / `AtlasTextures::prepare` 会在 release 也
    /// 拒绝「有层没有整页上传」的重建请求（`EngineError::AtlasGrowthIncomplete`）。
    image_array_established: Option<crate::render::AtlasArraySpec>,
    /// 扩层闸门连续被「有窗口的共享槽不空闲」挡住的帧数（RBT-10 的有界兜底计数）。
    ///
    /// 正常窗口的忙槽只持续 1–2 帧；达到 `text_pass::IMAGE_GROWTH_DEFER_LIMIT` 仍被挡 ⇒ 该
    /// 窗口的 GPU 循环已经不消费上传（遮挡 / 最小化 / 卡死），本帧把忙窗口按「不参与闸门」
    /// 处理（记账作废、可画层 0），只用参与且空闲的窗口推进数组。
    ///
    /// 只在**成功推进**时归零（RBT-16）：blocked 变空但没有成功推进（例如这一帧
    /// `needs_growth` 短暂为假、参与集合变化）时也保留计数 —— 刻意跨 episode 累计，持续慢的
    /// 窗口不必让每个 episode 都等满 8 帧，第二次起更快触发降级。行为仍是 fail-closed。
    image_growth_deferrals: u32,
    /// 字形页的整页 CPU 缓冲（跨上传复用，避免每次上传都分配）。
    glyph_page: Vec<u8>,
    /// 增量上传的打包缓冲（脏矩形逐个紧密排列；跨帧复用，避免每次上传都分配）。
    packed: Vec<u8>,
    /// 跨线程唤醒队列（`AppHandle` 的载荷；主线程之外唯一的引擎入口）。
    dispatcher: Arc<Dispatcher>,
    /// 本帧从平台层收到的、尚未被应用取走的事件（**有界**环形队列，R02-5）。
    events: event_queue::EventQueue,
    /// 每个窗口当前按住的物理键集合（R02-16；`KeyState::Up` 落地后的权威源）。
    ///
    /// 用 `HashSet` 而不是位图：`KeyCode` 含 `Unidentified(u16)`，位图需要先为它分配序号，
    /// 而按住集合的规模是个位数 —— 直接哈希正好。**按窗口**：平台只把按键投递给拥有键盘
    /// 焦点的窗口，`App::key_down(window, code)` 必须只回答该窗口的状态。窗口失焦 / 关窗时
    /// 清空该窗口的集合（抬起消息不再到达）。
    held_keys: ahash::HashMap<WindowId, ahash::HashSet<KeyCode>>,
    /// 每轮事件循环调用一次的应用钩子（读事件 / 写状态；`Application::run` 唯一调用点）。
    frame_hook: Option<Box<dyn FnMut(&mut App)>>,
    /// 动作处理器表（P10；同 `Action` 后写覆盖）。
    handlers: ahash::HashMap<Action, ActionHandler>,
    /// 每窗口的「关闭前」处理器（App::on_close）。
    ///
    /// 用户点窗口 X / Cmd-W 时平台**先问它**：返回 false 就拦下这次关闭（用于保存
    /// 未提交状态、弹确认框）。没有处理器 ⇒ 直接关（旧行为）。
    close_handlers: ahash::HashMap<WindowId, CloseHandler>,
    /// 键盘绑定表（线性扫描：默认 3 条 + 应用若干）。
    bindings: Vec<KeyBinding>,
    /// **焦点作用域化**的键盘绑定（线性扫描；只在焦点落在 `scope` 子树内时消费）。
    ///
    /// 与全局 `bindings` 表的区别是「谁有资格吃这个键」：全局绑定（`Ctrl+S` / `Tab` / `Esc`）
    /// 是应用级语义；而文本框的编辑键、树的方向键这类只在**某个实例聚焦时**成立。放在全局表里
    /// 会吞掉其它控件的默认键盘行为（引擎在命中绑定后不再走 `default_key_scroll`）。
    /// 生命周期：`App::remove_node` 卸载 scope 子树时自动清除，`App::close_window` 清空该窗口
    /// 全部条目；组件也可显式 `unbind_key_scoped` / `unbind_scope_keys`。
    scoped_bindings: Vec<ScopedKeyBinding>,
    /// 输入判定阈值（probe 真值；平台在 `platform_init` 覆盖）。
    thresholds: InputThresholds,
    /// 引擎时钟（生产由平台每轮 `sync_clock`，测试由 `TestApp::advance` 推进）。
    clock: Instant,
    /// 一次性定时器的登记基准是否取**登记瞬间的真实时间**（活时钟模式）。
    ///
    /// E10c-R01：真正的语义不能以「泵是否启动过」为准 —— 应用在 `Application::run` **之前**
    /// 就能建窗并登记定时器，那也必须「只晚不早」。因此 `App::new` 的生产默认就是置位；
    /// 测试宿主 `TestApp` 构造后显式关闭（`TestApp::advance` 是唯一推进源，deadline 必须能
    /// 精确断言）。平台泵通过 [`App::sync_clock_live`] 每轮刷新时钟并保持置位；
    /// `App::dispatch` 的 `sync_clock` 不置位（测试也会派发输入）。
    clock_live: bool,
    /// 每个窗口最近一次 a11y 交付的时钟值（节流用，P2-6）。
    ///
    /// 高频实时数据（行情数字）会让 A11Y 脏位每帧都为真，而每次交付都要全树扫描 +
    /// TreeUpdate 构造。按窗口限流到 A11Y_THROTTLE 一次，屏幕阅读器本身也消化不了
    /// 更快的变化。Assertive live region 同样受此限制（≤100ms 延迟，可接受）。
    a11y_delivered: ahash::HashMap<WindowId, Instant>,
    /// **本轮泵**是否真的交付过一笔 a11y（`a11y_take` 产出过快照才置位；平台泵的
    /// `wake_for_a11y_backlog` 消费并清零）。无 AT 客户端 / 无测试捕获时恒 false ⇒ 泵绝不
    /// 因 a11y backlog 自唤醒（RE-05 / I3 空闲零唤醒）。
    a11y_delivered_round: bool,
    /// 最近一次点击（双击判定）。
    last_click: Option<ClickRecord>,
    /// 动画槽（P10；`tick_animations` 推进，结束时删槽）。
    animations: Vec<AnimationSlot>,
    /// 下一个动画 id（单调；`AnimationId` 的代际）。
    next_animation: u32,
    /// 动画终止事件的待派发队列（自然完成 / 取消 / 替换 / 目标释放；E10）。
    ///
    /// 事件在**产生它的那一帧内**（自然完成：tick 之后、样式/布局/绘制之前）或**下一帧入口**
    /// （帧外产生的取消 / 替换 / 释放 / 0 ms 动画）派发 —— 见 [`App::on_animation_complete`]
    /// 的时序契约。
    animation_completions: std::collections::VecDeque<AnimationCompletion>,
    /// 应用注册的动画完成回调（单槽：重复注册替换上一个，与 [`App::on_frame`] 同口径）。
    animation_complete_handler: Option<Box<dyn FnMut(&mut App, AnimationCompletion)>>,
    /// 正在派发完成事件：回调里再次取消 / 注册动画时，事件排队等外层循环派发（不重入）。
    dispatching_animation_completions: bool,
    /// 一次性定时器表（`App::after` / `App::after_scoped`；按注册顺序，id 单调）。
    timers: Vec<timer::TimerEntry>,
    /// 下一个定时器 id（单调；`TimerId` 的代际）。
    next_timer: u32,
    /// 待推给平台的光标变化（`None` = 没变）。
    /// 待推给平台的**光标变化**（带窗口 id：光标是每窗口状态，P2 复审）。
    cursor_pending: Option<(WindowId, Cursor)>,
    /// 待推给平台的**指针捕获目标状态**（每窗口：`SetCapture` 是每窗口状态，R02-8/R02-22）。
    ///
    /// 用「每窗口目标状态」而不是单槽请求：同一泵轮里多个窗口各自改一次捕获时单槽会丢请求。
    /// 平台泵每轮 `take_capture_requests()` 取走并应用。
    capture_requests: ahash::HashMap<WindowId, bool>,
    /// IME 被应用**禁用**的窗口集合（P2-7；默认启用）。
    ///
    /// 按窗口的**目标状态**：平台泵每轮把它与本窗口已应用状态比较，只有变化才调
    /// `ImmAssociateContext`（Windows）。用集合而不是单槽：一轮内多个窗口各自关一次 IME
    /// 时单槽会丢请求。
    ime_disabled: ahash::HashSet<WindowId>,
    /// 待执行的窗口控制请求（自绘标题栏的按钮 / 拖动 / 缩放；E6 起三端共用）。
    ///
    /// Linux 由 Wayland 事件循环在同一轮执行；Windows/macOS 由各自的泵在下一轮执行
    /// （`show` / `performMiniaturize:` 等系统调用不能发生在持有 `&mut App` 的借用里）。
    window_control: Option<(WindowId, crate::window::WindowControl)>,
    /// a11y 动作的 handler 表（P11；同 `A11yAction` 后写覆盖）。
    a11y_handlers: ahash::HashMap<
        crate::a11y::A11yAction,
        Box<dyn FnMut(&mut crate::a11y::A11yActionCtx<'_>)>,
    >,
    /// a11y 诊断（P11；环形上限 `A11Y_DIAGNOSTIC_CAPACITY`，超出丢最旧）。
    a11y_diagnostics: Vec<crate::a11y::A11yDiagnostic>,
    /// 每窗口的 Debug HUD 运行状态（P17；feature 关闭时字段不存在）。
    #[cfg(feature = "hud")]
    debug_hud: ahash::HashMap<WindowId, DebugHudState>,
    /// 每窗口的信号作用域（窗口销毁时自动释放；`App::window_signal` 的归属）。
    ///
    /// 作用域释放后，其信号的写变成 no-op 且依赖边被立即删除 —— 关闭窗口后的迟到写入
    /// （异步任务 / 定时器回投）不会产生僵尸唤醒（R03-6 的可观测半边）。
    window_scopes: ahash::HashMap<WindowId, SignalScope>,
    /// 批处理嵌套深度（`App::batch`）。> 0 时写操作只累积，帧请求被推迟。
    batch_depth: u32,
    /// 批处理期间是否有被推迟的帧请求（`batch_depth` 归零时合并成一次）。
    batch_wake: bool,
    /// 累计的**帧请求**次数（进入平台唤醒队列的次数；诊断 / 基准的「N 次写 ⇒ 1 帧」证据）。
    frame_requests: u64,
}

/// 一次点击的记录（双击判定的输入）。
#[derive(Clone, Copy, Debug)]
struct ClickRecord {
    window: WindowId,
    node: Option<NodeId>,
    /// 产生这次点击的按键（连击要求按键一致：左右键交替不算双击）。
    button: PointerButton,
    position: Point,
    at: Instant,
    count: u8,
}

/// 一条焦点作用域化的键盘绑定（`App::bind_key_scoped`）。
///
/// 命中条件 = `(key, modifiers)` 精确匹配 **且** 该窗口的焦点节点在 `scope` 子树内（含
/// `scope` 自身）。不满足焦点条件时**不消费**，继续走全局绑定 / 默认键盘行为。
///
/// 多个 scope 同时命中（嵌套）时由 `app::input::on_key` 从焦点向上**由内向外**尝试；
/// 表本身不排序 —— 查找顺序由祖先链决定，因此「后写覆盖」只针对同一个 scope。
struct ScopedKeyBinding {
    /// 绑定所属窗口（焦点是每窗口状态；同一 `NodeId` 不会跨窗口）。
    window: WindowId,
    /// 焦点作用域根：焦点必须是它或它的后代。
    scope: NodeId,
    /// 实际绑定（键 + 修饰键 + 动作）。
    binding: KeyBinding,
}

/// 一个动画槽（`tick_animations` 推进；`t == 1.0` 时删槽）。
struct AnimationSlot {
    /// 单调 id（`AnimationId` 的载荷）。
    id: u32,
    window: WindowId,
    target: AnimationTarget,
    start: Instant,
    duration: Duration,
    easing: Easing,
    nodes: Vec<NodeId>,
}

/// 被驱动的值。
enum AnimationTarget {
    Scalar {
        transient: Transient<f32>,
        from: f32,
        to: f32,
    },
    Color {
        transient: Transient<Color>,
        from: Color,
        to: Color,
    },
    Scroll {
        handle: ScrollHandle,
        from: Point,
        to: Point,
    },
}

impl AnimationTarget {
    /// 是否驱动**同一个**值（重定向判定；`Scroll` 按 handle 比）。
    fn same_target(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Scalar {
                    transient: left, ..
                },
                Self::Scalar {
                    transient: right, ..
                },
            ) => left.id() == right.id(),
            (
                Self::Color {
                    transient: left, ..
                },
                Self::Color {
                    transient: right, ..
                },
            ) => left.id() == right.id(),
            (Self::Scroll { handle: left, .. }, Self::Scroll { handle: right, .. }) => {
                *left == *right
            }
            _ => false,
        }
    }

    /// 是否驱动 `ids` 里的某个瞬态值（作用域释放时取消动画用）。
    ///
    /// **扩展点（必须同步）**：给 [`AnimationTarget`] 新增任何「引用 `Transient`」的变体时，
    /// 必须在这里把它纳入判据，并同步 [`App::animate`] 的注册路径 —— 否则作用域释放后
    /// `tick_animations` 会继续写已回收的槽位（引擎不变量被破坏，见 `signal_api::release_scope`）。
    /// 不引用瞬态值的变体（如 `Scroll`）返回 `false`。
    fn targets_any(&self, ids: &[SignalId]) -> bool {
        match self {
            Self::Scalar { transient, .. } => ids.contains(&transient.id()),
            Self::Color { transient, .. } => ids.contains(&transient.id()),
            Self::Scroll { .. } => false,
        }
    }
}

/// 动画终止原因（[`AnimationCompletion`] 的载荷；E10）。
///
/// 只描述**这次注册的动画**为什么结束；应用可据此区分「正常收尾」与「被中止」，不需要轮询
/// `App::animations_running`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AnimationEndKind {
    /// 到达终值：`t >= 1.0`（终值已写入目标 `Transient`）或 0 时长动画的立即到位。
    ///
    /// **只有**这一种是「完成」；取消 / 替换 / 目标释放是另外三种（见 [`App::on_animation_complete`]）。
    Finished,
    /// 被 [`App::cancel_animation`] 取消（终值**没有**写入）。
    Cancelled,
    /// 同一目标上注册了新动画，旧槽被替换（新动画继续驱动同一个值）。
    Replaced,
    /// 驱动值所属作用域被释放（`App::release_scope`）—— 目标已回收，旧槽同批取消。
    Released,
    /// 动画的重绘节点 / 滚动目标所在子树被 [`App::remove_node`] 卸载（E16）。
    ///
    /// 与 [`AnimationEndKind::Cancelled`] 区分：应用没有持有 [`AnimationId`] 主动取消，是引擎
    /// 在卸载路径上替它收尾（[`RemoveOutcome::animations`] 也给出条数）。终值**没有**写入。
    Removed,
}

/// 一次动画终止事件（[`App::on_animation_complete`] 的载荷；E10）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AnimationCompletion {
    /// 结束的动画句柄（`App::animate` 的返回值；`AnimationId` 带窗口身份）。
    pub id: AnimationId,
    /// 终止原因。
    pub kind: AnimationEndKind,
}

/// 一次性定时器句柄（`Copy`；`(window, generation)` 唯一，取消 / 到期后旧句柄失效）。
///
/// 由 [`App::after`] / [`App::after_scoped`] 铸造，[`App::cancel_timer`] 消费。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimerId {
    window: WindowId,
    /// 单调代际（`App::cancel_timer` 只认这一对 `(window, generation)`）。
    generation: u32,
}

impl TimerId {
    pub(crate) fn new(window: WindowId, generation: u32) -> Self {
        Self { window, generation }
    }

    /// 所属窗口。
    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }

    pub(crate) fn generation(&self) -> u32 {
        self.generation
    }
}

/// 默认键盘绑定（Tab / Shift+Tab / Esc；应用可 `unbind_key` 或覆盖）。
///
/// 键位身份是布局无关的 [`KeyCode`]（W3C `KeyboardEvent.code`）：同一张表在三个平台、任意
/// 键盘布局下命中同一物理键。
fn default_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding {
            key: KeyCode::Tab,
            modifiers: Modifiers::empty(),
            action: Action::FocusNext,
        },
        KeyBinding {
            key: KeyCode::Tab,
            modifiers: Modifiers::SHIFT,
            action: Action::FocusPrev,
        },
        KeyBinding {
            key: KeyCode::Escape,
            modifiers: Modifiers::empty(),
            action: Action::Dismiss,
        },
    ]
}

/// 浮层节点的静态样式：绝对定位 + `inset { left, top }` + 内容自适应尺寸。
fn overlay_style(origin: Point) -> crate::paint::style::Style {
    let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
    inner.position = taffy::style::Position::Absolute;
    inner.inset = taffy::geometry::Rect {
        left: taffy::style::LengthPercentageAuto::length(origin.x),
        right: taffy::style::LengthPercentageAuto::auto(),
        top: taffy::style::LengthPercentageAuto::length(origin.y),
        bottom: taffy::style::LengthPercentageAuto::auto(),
    };
    crate::paint::style::Style {
        inner,
        clip: Clip::None,
    }
}

/// 应用层持有的跨线程句柄（`App::handle` 产出）。
///
/// 三个方法就是全部的跨线程入口：
/// `wake` 请求一帧、`send` 回投一个主线程闭包、`spawn` 经 `App` 的 tokio 句柄提交任务。
#[derive(Clone)]
pub struct AppHandle {
    dispatcher: Arc<Dispatcher>,
    runtime: tokio::runtime::Handle,
}

impl AppHandle {
    /// 请求一帧（合并；任意线程）。
    pub fn wake(&self) {
        self.dispatcher.wake();
    }

    /// 回投一个在主线程执行的闭包（业务异步结果的唯一入口）。
    pub fn send(&self, task: impl FnOnce(&mut App) + Send + 'static) {
        self.dispatcher.push(UserTask::new(task));
    }

    /// 经 `App` 的 tokio 句柄提交任务。
    ///
    /// **主线程内不得直接 `tokio::spawn`**（不在 runtime 上下文会 panic，`clippy.toml` 也禁了
    /// `Handle::current`）；跨线程/主线程都走这里。
    pub fn spawn<F>(&self, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.runtime.spawn(future);
    }
}

/// 引擎自有事件循环的入口（三平台各一份平台实现：Win32 泵 / AppKit run loop / calloop）。
///
/// 用法：`let mut app = App::new(handle); ...建窗/建树...; Application::new().run(app)?`。
/// `run` 负责：DPI 感知 → 建窗口（初始隐藏）→ 挂渲染器 → 显示 → 消息泵 → 关闭。
#[derive(Clone, Copy, Debug, Default)]
pub struct Application;

impl Application {
    /// 无参数：tokio 句柄由 `App` 持有。
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// 跑到没有窗口为止。
    ///
    /// # Errors
    /// `EngineError::WindowCreate`（窗口/消息泵）、`UnsupportedGpu` / `AppInit`（设备）、
    /// `Swapchain` / `DeviceLost`（present）、`InvalidDpiScale`（DPI 事件）。
    pub fn run(self, app: App) -> Result<(), EngineError> {
        crate::platform::native::run(app)
    }
}

impl App {
    /// 建一个 headless 引擎（消费者：composition root，§9.2）。
    ///
    /// `runtime` 由调用方传入（`main` 建 runtime 后 `runtime.handle().clone()`）；引擎内不建 runtime，
    /// 也不做 `Handle::current()`（在 runtime 之外会 panic，且会让引擎依赖「调用线程恰好在 runtime 内」）。
    #[must_use]
    pub fn new(runtime: tokio::runtime::Handle) -> Self {
        // D-6：`ResourceCache` 需要同一个 `Dispatcher` 在解码完成时唤醒泵（空闲时泵阻塞在
        // `GetMessageW`/`poll`，没有唤醒就永远不会有人调 `poll()`）。字段顺序要求它先于
        // `resources` 构造，因此提到结构体字面量之前。
        let dispatcher = Dispatcher::new();
        // 图集构造在字面量之前：代际租约的初值必须与图集的初始代际一致。
        let atlas = AtlasAllocator::with_page(
            crate::paint::atlas::AtlasKind::Glyph,
            (MIN_ATLAS_PAGE, MIN_ATLAS_PAGE),
        );
        let atlas_generation_lease = Arc::new(AtomicU64::new(crate::text::pack_atlas_lease(
            atlas.generation(),
            0,
        )));
        Self {
            store: StateStore::with_capacity(64, 16),
            graph: DependencyGraph::new(),
            windows: ahash::HashMap::default(),
            eval_stack: EvalStack::new(),
            wake_requested: false,
            next_window_id: 1,
            runtime: runtime.clone(),
            text: TextContext::new(),
            raster: GlyphRasterizer::new(),
            atlas,
            resources: ResourceCache::new(runtime, Arc::clone(&dispatcher)),
            pending_resource_results: Vec::new(),
            geometry: crate::paint::geometry_store::GeometryStore::new(),
            glyph_atlas: GlyphAtlas::new(),
            text_dirty_scratch: Vec::new(),
            text_layout_scratch: Vec::new(),
            scale_epochs: ahash::HashMap::default(),
            image_scale_epochs: ahash::HashMap::default(),
            atlas_generation_seen: 0,
            atlas_generation_lease,
            default_text_scale: 1.0,
            text_scale_override: None,
            focused_window: None,
            pending_dirty: crate::render::DirtyRects::default(),
            glyph_page_extent: None,
            image_array_established: None,
            image_growth_deferrals: 0,
            glyph_page: Vec::new(),
            packed: Vec::new(),
            dispatcher,
            events: event_queue::EventQueue::new(),
            held_keys: ahash::HashMap::default(),
            frame_hook: None,
            handlers: ahash::HashMap::default(),
            close_handlers: ahash::HashMap::default(),
            bindings: default_bindings(),
            scoped_bindings: Vec::new(),
            thresholds: InputThresholds::platform_default(),
            clock: Instant::now(),
            // E10c-R01：生产默认活时钟（首次泵启动前登记的定时器也以登记瞬间为基准）；
            // `TestApp` 在构造后显式关闭（见 `TestApp::with_options`）。
            clock_live: true,
            a11y_delivered: ahash::HashMap::default(),
            a11y_delivered_round: false,
            last_click: None,
            animations: Vec::new(),
            next_animation: 1,
            animation_completions: std::collections::VecDeque::new(),
            animation_complete_handler: None,
            dispatching_animation_completions: false,
            timers: Vec::new(),
            next_timer: 1,
            cursor_pending: None,
            capture_requests: ahash::HashMap::default(),
            ime_disabled: ahash::HashSet::default(),
            window_control: None,
            a11y_handlers: ahash::HashMap::default(),
            a11y_diagnostics: Vec::new(),
            #[cfg(feature = "hud")]
            debug_hud: ahash::HashMap::default(),
            window_scopes: ahash::HashMap::default(),
            batch_depth: 0,
            batch_wake: false,
            frame_requests: 0,
        }
    }

    /// 注册每轮事件循环的钩子：在平台事件已落地、提交本帧之前调用一次。
    ///
    /// 形态是 `&mut App`（可以建信号 / 改树 / 关窗），因此钩子内部**不能**再调用
    /// `Application::run`。重复注册会替换上一个钩子。
    pub fn on_frame(&mut self, hook: impl FnMut(&mut App) + 'static) {
        resources::on_frame(self, hook)
    }

    /// 单槽是否已被 `App::on_frame` 占用。
    ///
    /// `on_frame` 是单槽（后注册覆盖先注册），静默替换会让先安装的帧钩子停摆；安装协议因此
    /// 需要一个「槽是否空闲」的读点。消费者：`widget::frame::FrameRouter::new` / `install`，
    /// 它们在槽被占用时返回 [`EngineError::FrameHookInstalled`]。
    #[must_use]
    pub fn frame_hook_installed(&self) -> bool {
        self.frame_hook.is_some()
    }

    /// 调用一次帧钩子（crate 私有；临时 take 以避免 `&mut self` 与闭包参数重叠）。
    ///
    /// D-6 起钩子之前先轮询一次异步解码结果（`App::poll_resources`）：平台泵每轮都会调用本函数，
    /// 因此解码完成（工作线程已 `Dispatcher::wake`）后，结果会在这一轮交付并请求一帧。
    ///
    /// **E10**：在应用钩子之前先派发两类引擎事件 —— 已到期的**一次性定时器**与上一轮之后产生的
    /// **动画终止事件**。顺序保证应用钩子（`App::on_frame`）在同一轮就能看到定时器 / 完成回调
    /// 造成的新状态；钩子仍然是「平台事件已落地、提交本帧之前」的唯一应用同步点。
    pub(crate) fn run_frame_hook(&mut self) {
        timer::run_due(self);
        animation::dispatch_completions(self);
        resources::run_frame_hook(self)
    }

    /// 轮询图片异步解码结果并请求一帧（D-6）。
    ///
    /// 交付项进入 [`App::take_resource_results`] 的缓冲；有交付才 `request_frame`（空闲
    /// 无 pending 时不产生帧，I3 不变式保持）。平台泵的 `run_frame_hook` 每轮调用一次；
    /// `App::frame` 入口也兜底调用（headless / 自驱帧宿主）。
    pub(crate) fn poll_resources(&mut self) {
        resources::poll_resources(self)
    }

    /// 取走已交付的异步解码结果（D-6 的公开消费面）。
    ///
    /// 应用在 `on_frame` 钩子里取走并把 `Ready(ResourceImage)` 应用到节点（例如
    /// `Window::set_canvas`）或对 `Failed(DecodeCause)` 画占位/记录；每项只交付一次。
    /// 任务仍在途时返回空列表（稍后的轮次再交付）。
    ///
    /// **E7**：引用 `PendingResource` 的图片节点由引擎在 `poll_resources` 里**自动**改成
    /// 已就绪（内在尺寸 + 失效重绘），应用不需要为它们再处理本列表。
    pub fn take_resource_results(&mut self) -> Vec<(PendingResource, DecodeState)> {
        resources::take_resource_results(self)
    }

    /// 释放一个图片资源并让所有引用它的图片节点失效重绘（E7）。
    ///
    /// 资源 tile 的契约是「恒有效直到 unload」（`资源 §11.8 R-12`）：卸载后若继续复用上一帧
    /// 的 lane，会把已回收的 tile（可能已被别的图复用）画出来。本入口把 unload 与节点失效
    /// 合成一步 —— 应用应使用它而不是直接 `resources_mut().unload()`。
    ///
    /// 返回 `false` = 资源不存在（已释放 / 陈旧句柄）。
    pub fn unload_resource(&mut self, id: crate::resources::ResourceId) -> bool {
        resources::unload_resource(self, id)
    }

    /// E7：在布局之前解析图片节点的内在尺寸（`App::frame` 内部调用；低频）。
    ///
    /// 触发条件：新挂载的图片节点 / `set_image_resource` / 窗口 DPI 变化。没有图片节点或
    /// 没有变化时是 O(1) 早退（不扫描树，I3 的空闲路径不受影响）。
    pub(crate) fn refresh_images(&mut self, window: WindowId) -> usize {
        resources::refresh_images(self, window)
    }

    /// 跨线程句柄（`AppHandle`）。
    #[must_use]
    pub fn handle(&self) -> AppHandle {
        resources::handle(self)
    }

    /// 平台层共享的唤醒队列（crate 私有）。
    pub(crate) fn dispatcher(&self) -> Arc<Dispatcher> {
        resources::dispatcher(self)
    }

    /// 取走本帧的平台事件（`Application::run` 每帧填充；顺序即产生顺序）。
    ///
    /// 队列**有界**（`EVENT_QUEUE_CAPACITY` = 1024 条，超出丢最旧并计入
    /// [`App::events_dropped`]）：应用忘了每帧 drain 也不会无界增长（R02-5）。
    #[must_use]
    pub fn drain_events(&mut self) -> Vec<(WindowId, Event)> {
        resources::drain_events(self)
    }

    /// 只取走 `keep` 认可的平台事件，其余**留在队列里**（原顺序，丢弃计数不变）。
    ///
    /// 给「只拥有事件的一部分」的消费者：一个模态对话框把**自己窗口的文本 / 按键 / IME**交给
    /// 自己的输入框（kit 的契约是「外壳 `drain_events` 之后逐条转发给 `InputNode::dispatch`」），
    /// 而其它窗口、以及指针 / 尺寸 / 焦点 / 关闭这类**引擎自己已经分发过**的事件仍留在观测队列
    /// 里，组合根照常读到它们 —— 对话框因此不会把应用的事件吞掉。
    ///
    /// 队列有界（同 [`App::drain_events`]）；被取走的事件不影响累计的丢弃计数。
    #[must_use]
    pub fn drain_events_where(
        &mut self,
        keep: impl FnMut(WindowId, &Event) -> bool,
    ) -> Vec<(WindowId, Event)> {
        resources::drain_events_where(self, keep)
    }

    /// 累计被丢弃的事件条数（队列溢出诊断；单调递增）。
    #[must_use]
    pub fn events_dropped(&self) -> u64 {
        self.events.dropped()
    }

    /// 平台层投递一个事件（`crate::platform::native` 唯一写入点）。
    ///
    /// `Event::Input` 除了进观测缓冲，还会**立刻**走一次交互分发（P10 §5.3）；观测缓冲只用于
    /// `drain_events`（应用读事件），不承担分发。
    pub(crate) fn dispatch(&mut self, window: WindowId, event: Event) {
        resources::dispatch(self, window, event)
    }

    /// 写剪贴板文本（Win32 `CF_UNICODETEXT` / macOS `NSPasteboard` / Wayland `wl_data_source`）。
    ///
    /// # Errors
    /// `EngineError::Clipboard`：平台拒绝（剪贴板被占用、分配失败、无 owner 窗口）。
    pub fn set_clipboard_text(&self, text: &str) -> Result<(), EngineError> {
        resources::set_clipboard_text(self, text)
    }

    /// 读剪贴板文本（无文本内容时是空串）。
    ///
    /// # Errors
    /// `EngineError::Clipboard`：平台拒绝；Wayland 下「尚未收到过选区内容」也走这里
    /// （剪贴板是异步拉模型）。
    pub fn clipboard_text(&self) -> Result<smol_str::SmolStr, EngineError> {
        resources::clipboard_text(self)
    }

    /// 把两张图集页分发给所有窗口的渲染器（P13 §8.2；评审阻塞 ②）。
    ///
    /// 图集页是**进程级**共享数据（`AtlasAllocator` / `GlyphAtlas` 一份），纹理是**每窗口一份**
    /// （`AtlasTextures`）。`frame_all` 逐窗提交，若第一次 `take` 就把脏行消费给当前窗口，
    /// 其余窗口的纹理永远收不到内容。因此脏行由本方法统一消费一次，然后**逐窗口**分发：
    ///
    /// 1. 还没收到过整页的窗口（新建窗口 / 换过渲染器 / 页尺寸变化）拿整页，其余按脏行增量；
    /// 2. 共享槽忙的窗口不等待：本帧的脏行并进它自己的待发行，槽空闲后补发。任何一个窗口
    ///    不出帧都不会拖住其它窗口（DEVIATIONS F-211）；
    /// 3. 窗口只有在**真正入队成功**之后才记账「已收到整页」。
    ///
    /// 调用点：`App::frame`（每帧，paint 之前）。`TestApp::snapshot` 走自己的临时渲染器。
    pub(crate) fn upload_atlas_pages(&mut self) {
        text_pass::upload_atlas_pages(self)
    }

    /// 字形页分发：页级变化（尺寸 / 清空）走整页，其余按脏矩形（AOI）增量。
    fn upload_glyph_page(&mut self) {
        text_pass::upload_glyph_page(self)
    }

    /// 图片图集分发：懒分配（没插过图片就没有页）+ 逐层整页 / 按脏矩形（AOI）增量
    /// （DEVIATIONS F-211 / R06-3；图片图集的 texture array 层数 = 当前打开的页数，layer = 页号）。
    fn upload_image_page(&mut self) {
        text_pass::upload_image_page(self)
    }

    /// 字形页的整页像素（`R8`；快照路径用；只在 `test-support` 构建里存在）。
    #[cfg(feature = "test-support")]
    pub(crate) fn glyph_page_pixels(&self) -> ((u32, u32), Vec<u8>) {
        text_pass::glyph_page_pixels(self)
    }

    /// 全部已打开图片页的当前内容（BGRA8 预乘 sRGB；`TestApp::snapshot` 的逐层上传源）。
    /// 没有图片页时是空集；顺序 = 层号升序。
    #[cfg(feature = "test-support")]
    pub(crate) fn resource_page_pixels_all(&self) -> Vec<((u32, u32), &[u8])> {
        text_pass::resource_page_pixels_all(self)
    }

    /// 图片资源缓存（只读）。应用层在**建树时**用 [`App::resources_mut`] 换 `AtlasTile`。
    #[must_use]
    pub fn resources(&self) -> &ResourceCache {
        resources::resources(self)
    }

    /// 图片资源缓存（可变）：`insert` / `unload` 的唯一入口。
    pub fn resources_mut(&mut self) -> &mut ResourceCache {
        resources::resources_mut(self)
    }

    /// 静态几何注册表（只读）。
    #[must_use]
    pub fn geometry(&self) -> &crate::paint::geometry_store::GeometryStore {
        resources::geometry(self)
    }

    /// 静态几何注册表（可变）：`insert` / `release` 的唯一入口。
    ///
    /// 注意：直接调用 store 的 `update_vertices` / `update_indices` / `truncate` 只改数据，
    /// **不会**标脏绑定节点、也不会请求帧；要自动标脏请用 [`App::update_geometry_vertices`] /
    /// [`App::update_geometry_indices`] / [`App::truncate_geometry`]。
    pub fn geometry_mut(&mut self) -> &mut crate::paint::geometry_store::GeometryStore {
        resources::geometry_mut(self)
    }

    /// 设置几何节点的 per-draw 变换（R05-12 / AR2 P1-D4）：顶点**不**重建，只改一个 uniform。
    ///
    /// 语义边界：只影响**绘制**，不影响命中（命中用节点布局盒，见 `layout/hit_test.rs`）；
    /// 变换后旧位置的像素与新位置的像素都会并入该节点的 damage（局部刷新不留残影）。
    ///
    /// # Errors
    /// [`EngineError::UnknownWindow`] / [`EngineError::StaleNode`]（节点不是几何节点）。
    pub fn set_geometry_transform(
        &mut self,
        window: WindowId,
        node: NodeId,
        transform: crate::math::Transform,
    ) -> Result<(), EngineError> {
        geometry::set_geometry_transform(self, window, node, transform)
    }

    /// 几何数据的**顶点 range 更新**（X5）：原地修改一段顶点字节，不重注册整块几何。
    ///
    /// `range` 是字节区间（半开），`bytes` 长度必须与之相等；区间必须按 `vertex_stride`
    /// 对齐。允许在缓冲区**末尾追加**（`range.end` 超过当前长度 ⇒ 缓冲区增长），但
    /// `range.start` 不得越过当前末尾（不允许中间留空洞）。合法空区间是 no-op。
    ///
    /// 成功返回 [`crate::paint::geometry::GeometryUpdate`]：新版本号 + 受影响块的
    /// 旧∪新 AABB（**逻辑像素**，与 `GeometryDesc.bounds` 同一坐标空间；per-draw 变换在
    /// paint 阶段才应用）。引擎会把**所有窗口**里绑定该几何的节点标 PAINT 并自动请求一帧
    /// （与 signal 写入一致）：几何节点按块版本增量重建（K 线追加只重建尾块），内容里引用该
    /// 几何的 `Canvas` 节点整条 lane 重建（Canvas 内容按引用重新编码，没有块级 revision 判据）。
    /// 空 range / 没有块受影响时不标脏、不请求帧。
    ///
    /// # Errors
    /// - [`EngineError::UnknownGeometry`]：句柄陈旧 / 已释放；
    /// - [`EngineError::GeometryRangeOutOfBounds`] / [`EngineError::GeometryRangeUnaligned`] /
    ///   [`EngineError::GeometryRangeBytes`]：区间参数错误（分别表示越界 / 未对齐 / 字节数不符）。
    pub fn update_geometry_vertices(
        &mut self,
        geometry: crate::paint::geometry::GeometryId,
        range: std::ops::Range<u32>,
        bytes: &[u8],
    ) -> Result<crate::paint::geometry::GeometryUpdate, EngineError> {
        geometry::update_vertices(self, geometry, range, bytes)
    }

    /// 几何数据的**索引 range 更新**（X5；契约与 [`App::update_geometry_vertices`] 对称）。
    ///
    /// 末尾追加会创建新的尾块（块大小固定）并只重建/重传新块；跨块更新按块逐个重建。
    ///
    /// # Errors
    /// 同 [`App::update_geometry_vertices`]（`GeometryRange*` 的 `buffer` 字段为 `Index`）。
    pub fn update_geometry_indices(
        &mut self,
        geometry: crate::paint::geometry::GeometryId,
        range: std::ops::Range<u32>,
        bytes: &[u8],
    ) -> Result<crate::paint::geometry::GeometryUpdate, EngineError> {
        geometry::update_indices(self, geometry, range, bytes)
    }

    /// 几何数据的**成对截断**（X5 滑动窗口）：把顶点/索引缓冲同时缩短到给定的**元素个数**。
    ///
    /// 参数顺序固定为 `(vertices, indices)`：`vertices` 是目标**顶点个数**，`indices` 是目标
    /// **索引个数**（都是元素个数，不是字节数），各自必须 `<=` 当前个数。两个字面量相邻且同型，
    /// 建议用命名局部变量写清单位（K 线示例：每根 4 顶点 + 6 索引、保留 `roots` 根 ⇒
    /// `vertices = roots * 4, indices = roots * 6`）。
    ///
    /// 截断只能缩短；增长走 [`App::update_geometry_vertices`] /
    /// [`App::update_geometry_indices`]（尾随追加）。两者都等于当前个数是 no-op：不推进版本
    /// （返回当前 `revision`）、不产生 damage、不标脏、不请求帧。
    ///
    /// 语义（K 线滑动窗口：删掉最旧的 N 根 + 追加新的）：
    /// - 块数按新索引个数重算（**只减**）：被移除的块 lane 由 paint 释放，返回的 `damaged`
    ///   覆盖它们的旧 AABB ∪ 变短尾块的旧 ∪ 新 AABB（**逻辑像素**，与 update 的「旧 ∪ 新」同
    ///   口径），旧像素因此会被重画；
    /// - **截断到 0 个索引**时节点仍保留一条承载装饰（背景 / 边框 / 焦点环 / 节点盒 damage）的
    ///   lane（`paint` 的 `desired.max(1)`），几何 op 不发射；之后追加会自动恢复块 lane。这是
    ///   有意行为：空数据不应让节点自己的样式消失；
    /// - 末端页按新长度缩短、多余页丢弃；未触碰的页仍与上一版本共享同一分配 ⇒ GPU 侧继续按页
    ///   增量上传（不需要 `release` + `insert` 全量重建）；
    /// - 截断后可以继续用 `update_geometry_*` 做**尾随追加**（同一条追加路径，只重建尾块）。
    ///
    /// 契约：应用必须保证截断后剩余索引不引用被移除的顶点（与注册期一致，引擎不校验索引值的
    /// 合法性；越界索引在 GPU 侧是未定义行为）。
    ///
    /// 与 range 更新一样：所有窗口里绑定该几何的节点（几何节点 / 引用该几何的 Canvas 节点）
    /// 会自动标脏，并自动请求一帧。
    ///
    /// # Errors
    /// - [`EngineError::UnknownGeometry`]：句柄陈旧 / 已释放；
    /// - [`EngineError::GeometryTruncateGrows`]：目标个数大于当前个数（增长应走
    ///   `update_geometry_*`）。
    pub fn truncate_geometry(
        &mut self,
        geometry: crate::paint::geometry::GeometryId,
        vertices: u32,
        indices: u32,
    ) -> Result<crate::paint::geometry::GeometryUpdate, EngineError> {
        geometry::truncate(self, geometry, vertices, indices)
    }

    /// 仅测试（`test-support`）：值槽位表的分配统计（分配过 / 存活 / 空闲）。
    ///
    /// 公开入口是 `test_support::diagnostics::signal_slot_stats`：作用域释放后槽位归还空闲栈，
    /// 「反复开关一个面板后槽位总数 / 存活数不增长」因此可断言（R03-6 的泄漏面）。
    #[cfg(feature = "test-support")]
    #[must_use]
    pub(crate) fn signal_slot_counts_for_test(&self) -> (usize, usize, usize) {
        (
            self.store.slot_count_for_test(),
            self.store.live_slot_count_for_test(),
            self.store.free_slot_count_for_test(),
        )
    }

    /// 依赖图（只读；`test-support` 的 `diagnostics::dependency_dump` 用）。
    #[cfg(feature = "test-support")]
    pub(crate) fn dependency_graph(&self) -> &DependencyGraph {
        signal_api::dependency_graph(self)
    }

    /// 仅测试：依赖图当前的表项数（作用域释放「立即删边」的可测证据）。
    #[cfg(test)]
    pub(crate) fn dependency_edge_count_for_test(&self) -> usize {
        signal_api::dependency_edge_count(self)
    }

    /// 测试：App 记账的「已建立图片 texture array」（`Window::image_array_spec` 是每窗口收据）。
    ///
    /// RBT-13 用它断言「部分层入队失败 ⇒ 数组不推进」。
    #[cfg(all(test, windows))]
    pub(crate) fn image_array_established_for_test(&self) -> Option<crate::render::AtlasArraySpec> {
        self.image_array_established
    }

    /// 仅测试：把下一个窗口句柄推到指定值（验证「句柄用尽」的行为）。
    #[cfg(test)]
    pub(crate) fn set_next_window_id_for_test(&mut self, next: u32) {
        self.next_window_id = next;
    }

    /// 本 App 绑定的 tokio 句柄（**借用**，不 clone）。
    ///
    /// 命名：不占用 `handle()` —— P09 会加 `App::handle() -> AppHandle`（那时是纯新增，零破坏）。
    #[must_use]
    pub fn runtime_handle(&self) -> &tokio::runtime::Handle {
        resources::runtime_handle(self)
    }

    /// 建一个普通信号（归属**根作用域**：永不释放，槽位因此永不被回收）。
    pub fn signal<T: Clone + 'static>(&mut self, initial: T) -> Signal<T> {
        signal_api::signal(self, initial)
    }

    /// 新建一个信号作用域（窗口 / 面板 / 对话框的自有生命周期）。
    ///
    /// 典型用法：为一个可关闭的面板建作用域，面板内所有信号都注册进去；面板销毁时
    /// [`App::release_scope`] 一次，之后任何迟到写入（异步结果、定时器回投）都是 no-op，
    /// 不会唤醒已经不存在的内容；旧句柄的**读**则确定性失败（值已被回收，不会读到复用后的
    /// 新值），见 [`SignalScope`]。窗口作用域由引擎自动管理（见 [`App::window_signal`]）。
    #[must_use]
    pub fn signal_scope(&mut self) -> SignalScope {
        signal_api::signal_scope(self)
    }

    /// 在指定作用域里注册信号（槽位优先复用该作用域释放时回收的空闲槽位）。
    ///
    /// # Panics
    /// `scope` 已释放（引擎误用：释放后不可再 mint 信号）。
    pub fn signal_scoped<T: Clone + 'static>(
        &mut self,
        scope: SignalScope,
        initial: T,
    ) -> Signal<T> {
        signal_api::signal_scoped(self, scope, initial)
    }

    /// 在**窗口作用域**里注册信号（`App::close_window` 时自动释放：值立即回收、槽位归还空闲表）。
    ///
    /// # Panics
    /// `window` 不存在（窗口的信号作用域随窗口一起创建 / 销毁）。
    pub fn window_signal<T: Clone + 'static>(&mut self, window: WindowId, initial: T) -> Signal<T> {
        let scope = *self
            .window_scopes
            .get(&window)
            .expect("window signal scope: window does not exist");
        signal_api::signal_scoped(self, scope, initial)
    }

    /// 释放信号作用域：其值（信号 / 瞬态值）**立即回收**（槽位归还空闲栈、代际 +1），依赖图里
    /// 以它们为键的边被**立即**删除；此后旧句柄的读 panic、写/更新是 no-op（见 [`SignalScope`]）。
    /// 引用该作用域瞬态值的动画槽同批取消，不会在下一帧写死槽位；取消后这些动画不再请求帧
    /// （**I3**：静止即回 Idle）。
    ///
    /// **派生侧**：根作用域派生的闭包若捕获了本作用域的句柄，释放后既不会再被失效（缓存永久
    /// 陈旧），又会在下次重算时读死句柄 panic。引擎无法检测不透明闭包，因此这是应用层硬契约
    /// （禁止跨作用域捕获，见 [`App::derived`] 与 [`Derived`]）。
    ///
    /// **节点侧**：`bind_style` / `bind_text` / `bind_a11y` 闭包捕获的句柄在释放后一求值就
    /// panic。组件先卸载自己的节点、再释放作用域（`base_ui` 组件的 `release` 即此顺序契约）。
    ///
    /// 返回被释放的值槽位数（信号 + 瞬态值；重复释放返回 0；根作用域不可释放）。计数口径是
    /// 「回收了多少个值」，不是「有多少个节点读它们」—— 调用方只能把它用于诊断/断言，不得
    /// 反推业务状态。
    pub fn release_scope(&mut self, scope: SignalScope) -> usize {
        signal_api::release_scope(self, scope)
    }

    /// 建一个瞬态值（读不登记、写不失效）。
    pub fn transient<T: Clone + 'static>(&mut self, initial: T) -> Transient<T> {
        signal_api::transient(self, initial)
    }

    /// 在指定作用域里注册瞬态值（随 [`App::release_scope`] 一起回收）。
    ///
    /// # 语义
    ///
    /// 与 [`App::transient`] 完全相同（读不登记依赖、写不失效、不进依赖图 —— **I3** 的
    /// 「高频状态不污染依赖图」），唯一区别是**归属**：值槽位记在 `scope` 名下，作用域释放时
    /// 与普通信号一起立即回收（槽位归还空闲栈、代际 +1）。
    ///
    /// 这是动态创建 / 销毁组件（滚动条淡出、拖拽快照、列表滚动状态）的必需入口：用
    /// [`App::transient`] 会落在**根作用域**、永不释放，长期 churn 会把槽位峰值推高。
    ///
    /// # 释放语义
    ///
    /// [`App::release_scope`] 之后：
    /// - 旧 `Transient` 句柄的**读 / 写都 panic**（确定性失败，绝不读到槽位复用后的新值；
    ///   见 `node::signal` 模块文档〈作用域回收与句柄失效〉）；
    /// - 引用它的动画槽**同批取消**（`AnimationTarget::targets_any`）：否则下一帧
    ///   `tick_animations` 会写已回收的槽位。取消发生在槽位回收之后、同一调用内，中间没有帧
    ///   推进；取消后不再因该动画请求帧（I3）。
    ///
    /// 组件契约为「**先卸载节点、再释放作用域**」：`bind_style` / `bind_text` 闭包捕获的句柄
    /// 在释放后一求值就 panic（见 [`App::release_scope`]）。
    ///
    /// # Panics
    ///
    /// `scope` 已释放（引擎误用：释放后不可再 mint 值）。
    pub fn transient_scoped<T: Clone + 'static>(
        &mut self,
        scope: SignalScope,
        initial: T,
    ) -> Transient<T> {
        signal_api::transient_scoped(self, scope, initial)
    }

    /// 注册一个派生值（**不**立即求值；环在运行期检测）。
    ///
    /// **禁止跨作用域捕获（AR5 P2-4 / R5c）**：派生归属**根作用域**，闭包不得捕获
    /// `App::signal_scoped` / `App::window_signal` 这类会随窗口/面板释放的信号句柄。作用域释放后
    /// 该派生的依赖边已被删除（不再被失效 ⇒ 缓存永久陈旧），而一旦因其它依赖重算，读死句柄会
    /// 确定性 panic。引擎无法在释放时检查不透明闭包，因此这是应用层硬契约（详见 [`Derived`]）。
    pub fn derived<T: Clone + 'static>(
        &mut self,
        compute: impl Fn(&mut ReadCtx<'_>) -> T + 'static,
    ) -> Derived<T> {
        signal_api::derived(self, compute)
    }

    /// 写一个信号。返回 `true` = 值变了（并已沿依赖图失效）；`false` = 值相等（短路）
    /// **或**该句柄已失效（作用域已释放 / 槽位已复用：no-op，不标脏、不唤醒）。
    ///
    /// 返回值不表示「有节点被标脏」（只写派生时没有节点变脏）。
    ///
    /// **应用层契约**：`T` 必须是不可变值语义。若 `T` 内部可变（`Rc<RefCell<_>>`/`Cell`/`Mutex`）而
    /// `PartialEq` 只比较外层，则内部变化会被短路吞掉（不重绘）。见 S-35 / R9。
    ///
    /// # Panics
    /// 该信号存在活跃 [`SignalRef`] 读守卫（见 `node::signal` 模块文档的借用规则）。
    pub fn write<T: Clone + PartialEq + 'static>(&mut self, signal: &Signal<T>, value: T) -> bool {
        signal_api::write(self, signal, value)
    }

    /// 「先从旧值算出新值，再按 [`App::write`] 的语义写」：`true` = 值变了并已沿依赖图失效。
    ///
    /// 读旧值与写新值在同一次调用内完成：读守卫在算出新值的那条语句末尾就析构，**之后**才写，
    /// 因此不会撞上自己的守卫（手写的 `let guard = read(); ...; write()` 会在这一步 panic）。
    /// 返回值与 [`App::write`] 一致：`false` = 等值短路 **或**该句柄已失效（no-op；闭包不会被
    /// 调用，所以失效句柄不会在"读旧值"那一步 panic）。
    ///
    /// 适合「新值 = f(旧值)」的派生写。**组合子消灭了 `match` 陷阱**：
    /// `cx.write_with(&s, |mode| match *mode { .. })` 不必先把值拷贝出来
    /// （见 `node::signal` 的 [`SignalRef`] 文档〈三种自然写法与 `match` 陷阱〉）。
    ///
    /// # Panics
    /// 该信号存在活跃 [`SignalRef`] 读守卫（见 `node::signal` 模块文档的借用规则）/ 类型不符。
    pub fn write_with<T: Clone + PartialEq + 'static>(
        &mut self,
        signal: &Signal<T>,
        body: impl FnOnce(&T) -> T,
    ) -> bool {
        signal_api::write_with(self, signal, body)
    }

    /// 原地更新一个信号（**无等值短路**；适合大对象的局部修改）。
    ///
    /// 与 [`App::write`] 的差别：不 clone 旧值、不做 `PartialEq` 比较，闭包改完直接沿依赖图失效。
    /// 返回 `false` 仅表示该句柄已失效（no-op，闭包不会被调用）。
    ///
    /// # Panics
    /// 该信号存在活跃读守卫 / 类型不符。
    pub fn update<T: 'static>(&mut self, signal: &Signal<T>, update: impl FnOnce(&mut T)) -> bool {
        signal_api::update(self, signal, update)
    }

    /// 在闭包内**就地**更新一个信号并沿依赖图失效，返回闭包的返回值（**无等值短路**）。
    ///
    /// 返回 `None` = 该句柄已失效（no-op，闭包不会被调用，与 [`App::update`] 返回 `false`
    /// 同义）；`Some(r)` = 已就地改完并已沿依赖图失效。
    ///
    /// 与 [`App::update`] 的差别只有一个：闭包在**一次借用**内完成「看旧值 ⇒ 改值」，中间不会
    /// 产生 [`SignalRef`] 读守卫。**组合子消灭了 `match` 陷阱** —— 枚举状态机的自然写法
    /// `cx.update_with(&s, |mode| match *mode { .. })` 永远是安全的，而
    /// `match *cx.read(&s) { A => cx.write(&s, ..), .. }` 必 panic（见 `node::signal` 的
    /// [`SignalRef`] 文档〈三种自然写法与 `match` 陷阱〉）。
    ///
    /// # Panics
    /// 该信号存在活跃 [`SignalRef`] 读守卫 / 类型不符（与 [`App::update`] 同口径）。
    pub fn update_with<T: 'static, R>(
        &mut self,
        signal: &Signal<T>,
        body: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        signal_api::update_with(self, signal, body)
    }

    /// 写一个瞬态值：**不**失效、**不**请求唤醒（滚动偏移/悬浮态，§3.7）。
    ///
    /// 与 [`App::write`] 的失效句柄语义**不同**：普通信号在作用域释放后的迟到写是 no-op
    /// （返回 `false`，R03-6 的僵尸唤醒防护），而瞬态值没有「返回 false」的第二形态 —— 读
    /// 会 panic，写同样 panic，绝不静默丢弃或写到槽位复用后的新值。
    ///
    /// # Panics
    ///
    /// - 该瞬态值存在活跃读守卫；
    /// - 该句柄已失效（所属作用域已释放 / 槽位已回收）：与读同口径的**确定性失败**。
    pub fn write_transient<T: 'static>(&mut self, transient: &Transient<T>, value: T) {
        signal_api::write_transient(self, transient, value)
    }

    /// 批处理：闭包内的全部写操作**合并成一次**帧请求（一次平台唤醒）。
    ///
    /// 语义：
    /// - 闭包内的写照常写值、照常沿依赖图标脏（下一帧的样式/布局/paint 会看到**最终**状态）；
    /// - 帧请求（`wake_requested` / 平台唤醒）被推迟到最外层批处理结束，且只发一次 ——
    ///   批内的中间状态不会被任何帧管线（styles / text / layout / paint）观察到，
    ///   依赖图也不会在批内被重入求值；
    /// - 嵌套批处理按最外层结算；闭包 panic 也会正确归零深度（不会把引擎卡在"永不唤醒"）。
    ///
    /// 典型用法：一次行情推送更新 20 个面板信号 → `app.batch(|app| { ... })` ⇒ 1 次唤醒。
    pub fn batch<R>(&mut self, body: impl FnOnce(&mut App) -> R) -> R {
        self.batch_depth += 1;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(self)));
        self.batch_depth = self.batch_depth.saturating_sub(1);
        if self.batch_depth == 0 && self.batch_wake {
            self.batch_wake = false;
            self.request_frame();
        }
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// 当前是否在批处理内（诊断 / 测试）。
    #[must_use]
    pub fn batching(&self) -> bool {
        self.batch_depth > 0
    }

    /// 累计的帧请求次数（进入平台唤醒队列的次数；批处理「N 次写 ⇒ 1 帧」的证据面）。
    #[must_use]
    pub fn frame_requests(&self) -> u64 {
        self.frame_requests
    }

    /// 取走「需要一帧」的请求位（取走即清零）。
    ///
    /// **crate 内部**：这是引擎与平台泵之间的唤醒协议（R01-16 把它从公开面降级 —— 应用可以
    /// 消费掉引擎的帧请求位、让泵不再出帧）。应用侧唯一的重绘入口是
    /// [`App::request_repaint`]（窗口级）与 [`App::request_node_repaint`]（节点级）。
    pub(crate) fn take_wake_request(&mut self) -> bool {
        frame::take_wake_request(self)
    }

    /// 按**窗口**请求重绘：下一帧整窗 damage（`Plan::Full`）并唤醒平台事件循环。
    ///
    /// 面向「确实需要重画整个窗口」的场景（例如外部资源整体换绑）。只需要重画某个子树时用
    /// [`App::request_node_repaint`] —— 它不会把 damage 放大到整窗。
    ///
    /// # Errors
    /// [`EngineError::UnknownWindow`]：窗口已关闭 / 从未存在。
    pub fn request_repaint(&mut self, window: WindowId) -> Result<(), EngineError> {
        invalidate::request_repaint(self, window)
    }

    /// 按**节点**请求重绘（R01-16；Chrome `InvalidateRect` / GPUI `cx.notify()` 的节点版本）。
    ///
    /// - 只把 `node` 的**子树**标 `PAINT | A11Y`：这些节点的 lane 重建、a11y 重发；
    /// - damage 只来自被重建的 lane（**不**置整窗 `full_damage_pending`）⇒ 局部刷新不会退化成
    ///   整窗重绘；
    /// - 祖先链只做有效性校验（存活 + 可见）：任一祖先不可见 ⇒ 返回 `Ok(false)` 且**不**请求帧；
    /// - 只重放绘制输出（paint spec / 画布 / 几何 / 文本快照的**当前值**），不重算
    ///   `bind_style` / 布局。`Transient` 的写者读完 `bind_style` 若没有走信号依赖图，就由它
    ///   自己调用本入口（与 [`App::write_transient`] 的文档配套）；
    /// - 幂等：重复调用只合并脏位/帧请求，不会重复出帧（下一次 `frame` 消费脏位）。
    ///
    /// # Errors
    /// [`EngineError::UnknownWindow`]（窗口已关闭）/ [`EngineError::StaleNode`]（节点陈旧，
    /// 例如已从树中卸载）。
    pub fn request_node_repaint(
        &mut self,
        window: WindowId,
        node: NodeId,
    ) -> Result<bool, EngineError> {
        invalidate::request_node_repaint(self, window, node)
    }

    /// 置「需要一帧」并**唤醒平台事件循环**（合并位由 `Dispatcher::wake` 保证幂等）。
    ///
    /// 不能只置 `wake_requested`：平台泵的顺序是「事件 → 钩子 → 帧 → a11y → 回投任务」，
    /// 回投任务（`AppHandle::send`、a11y `ActionRequest`）里写状态时循环已经过了帧门禁、
    /// 即将阻塞在 `GetMessageW` / `poll`。只置位不唤醒会让这批状态一直等到下一条无关消息
    /// （画面与 a11y 更新都迟到）——P15 用 UIA 客户端实测 `SelectionItemPattern.Select()`
    /// 不生效时定位到这一点。
    ///
    /// 批处理内（[`App::batch`]）只登记推迟位，由最外层批处理结束时合并成**一次**请求。
    ///
    /// E6 起的额外消费者：三端泵在窗口 chrome 状态变化（最大化 / 还原）时请求一帧 —— 根节点
    /// 的 `PAINT` 已由 [`Window::set_state`] 标好（§8.6.4 / I3，与应用写信号同一路径）。
    pub(crate) fn request_frame(&mut self) {
        if self.batch_depth > 0 {
            self.batch_wake = true;
            return;
        }
        self.frame_requests = self.frame_requests.saturating_add(1);
        frame::request_frame(self)
    }

    /// 注册/替换一个动作的处理器（同 `Action` 后写覆盖）。
    pub fn on_action(&mut self, action: Action, handler: impl FnMut(&mut ActionCtx<'_>) + 'static) {
        style_pass::on_action(self, action, handler)
    }

    /// 删除处理器（返回是否真的删了）。
    pub fn off_action(&mut self, action: &Action) -> bool {
        style_pass::off_action(self, action)
    }

    /// 仅测试（`test-support`）：某个动作当前是否注册了处理器。
    ///
    /// 组件 `release` 清理的直接证据面：泄漏的 handler 不一定会 panic（指针路径对死句柄是
    /// no-op、滚动条 handler 只写引擎滚动状态），「重放输入不 panic」不能证明表项被摘掉。
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn has_action_handler(&self, action: &Action) -> bool {
        style_pass::has_action_handler(self, action)
    }

    /// 仅测试（`test-support`）：某个 `(window, scope)` 上剩余的焦点作用域化键绑定条数。
    ///
    /// `App::unbind_scope_keys` 的直接证据面（`Tree::release`）：handler 被摘但绑定残留时，
    /// 重放按键既不 panic 也不消费（回落到默认行为），只能直接观察表项。
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn scoped_key_binding_count(&self, window: WindowId, scope: NodeId) -> usize {
        style_pass::scoped_binding_count(self, window, scope)
    }

    /// 添加/替换一条键盘绑定（同 `(key, modifiers)` 后写覆盖）。
    ///
    /// 这是**应用级**绑定：不论焦点落在哪个控件上都命中（`Ctrl+S` / `Tab` / `Esc`）。
    /// 只在某个实例聚焦时才成立的键（文本编辑键、树方向键）用 [`App::bind_key_scoped`]。
    pub fn bind_key(&mut self, binding: KeyBinding) {
        style_pass::bind_key(self, binding)
    }

    /// 添加/替换一条**焦点作用域化**的键盘绑定（同 `(window, scope, key, modifiers)` 后写覆盖）。
    ///
    /// # 语义
    ///
    /// 只有该窗口的**焦点节点落在 `scope` 子树内**（含 `scope` 自身）时，绑定才命中并消费按键；
    /// 否则按键继续走全局绑定（[`App::bind_key`]）与默认键盘行为（方向键滚动等）。
    ///
    /// 求值顺序（`app::input::on_key` 的 ③a/③b/④）：
    ///
    /// 1. **scoped 绑定**：从焦点节点向上到根，逐个 scope 找 `(key, modifiers)` 精确匹配的绑定，
    ///    由**内向外**依次尝试 —— 焦点在嵌套 scope（如树里的文本域）内时**内层优先**；
    /// 2. **全局绑定**：scoped 无命中 / 命中的 action 没有 `on_action` handler 时回落；
    /// 3. **默认键盘行为**：全局也无绑定时的方向键滚动等。
    ///
    /// 「消费」= 该 `Action` 注册了 handler 且分派返回 `true`。绑定存在但 handler 被
    /// [`App::off_action`] 移除时不消费、不吞键 —— 组件 `release` 先摘 handler 再解绑，
    /// 中间窗口不会出现「按键被吃但没有响应」。
    ///
    /// # 防泄漏与生命周期
    ///
    /// - 同 `(window, scope, key, modifiers)` **后写覆盖**：重复注册不会增长表项；
    /// - [`App::remove_node`] 卸载 `scope` 所在子树 ⇒ 该 scope 的绑定**自动清除**；
    ///   只卸载后代节点不影响祖先 scope 的绑定；
    /// - [`App::close_window`] ⇒ 该窗口的全部 scoped 绑定清除；
    /// - 组件需要主动清理（不卸载节点 / 不关窗）用 [`App::unbind_scope_keys`] 或
    ///   [`App::unbind_key_scoped`]。
    ///
    /// 绑定不写状态、不标脏、不请求帧（**I3**）；`scope` 与 handler 读到的状态都由调用方在
    /// Action handler 内经 `ActionCtx` 改写（**I6**：可见状态只经 Signal/Action 写入）。
    ///
    /// 焦点经 [`App::focused`] 过滤：已卸载节点的陈旧焦点、模态浮层作用域外的焦点都被视为
    /// 无焦点 ⇒ 陈旧 `NodeId` 与多窗口下「另一窗口的陈旧焦点」都不会误触本窗口的绑定
    /// （`NodeId` 本身携带窗口身份，跨窗口注册在 [`App::bind_key_scoped`] 里就被拒绝）。
    ///
    /// # Errors
    ///
    /// - [`EngineError::UnknownWindow`]：窗口不存在（`WindowId` 单调不复用，注册给未知窗口的
    ///   绑定永远不会命中，必须在注册期拒绝）；
    /// - [`EngineError::StaleNode`]：`scope` 不在该窗口的节点树中（已卸载 / 从未挂载 /
    ///   属于另一窗口）。
    ///
    /// # Panics
    ///
    /// 无。`scope` 之后被卸载是正常生命周期：绑定随 [`App::remove_node`] 自动清除。
    pub fn bind_key_scoped(
        &mut self,
        window: WindowId,
        scope: NodeId,
        binding: KeyBinding,
    ) -> Result<(), EngineError> {
        style_pass::bind_key_scoped(self, window, scope, binding)
    }

    /// 删除一条**焦点作用域化**的键盘绑定（返回是否真的删了）。
    ///
    /// 与 [`App::bind_key_scoped`] 对称的显式清理；窗口 / `scope` 已不存在时按「没有这条绑定」
    /// 处理（返回 `false`，不 panic）。同 `(window, scope, key, modifiers)` 后写覆盖保证至多
    /// 一条匹配 ⇒ 删除是幂等的。
    pub fn unbind_key_scoped(
        &mut self,
        window: WindowId,
        scope: NodeId,
        key: KeyCode,
        modifiers: Modifiers,
    ) -> bool {
        style_pass::unbind_key_scoped(self, window, scope, key, modifiers)
    }

    /// 删除某个 `scope` 在某个窗口上的**全部**焦点作用域化绑定，返回删除条数。
    ///
    /// 组件 `release` 的清理入口：调用方不需要复述自己绑定过哪些键（键表可能随组件参数变化）。
    /// 幂等；窗口 / `scope` 已不存在时返回 0。节点卸载与关窗已有自动清理，本方法是
    /// **不卸载节点就释放组件**（例如复用节点树、热切换控件）时的显式出口。
    pub fn unbind_scope_keys(&mut self, window: WindowId, scope: NodeId) -> usize {
        style_pass::unbind_scope_keys(self, window, scope)
    }

    /// 删除一条键盘绑定（返回是否真的删了）。
    pub fn unbind_key(&mut self, key: KeyCode, modifiers: Modifiers) -> bool {
        style_pass::unbind_key(self, key, modifiers)
    }

    /// Edit 菜单项的可用性快照（R09-1 收尾 / AR3 macOS P2-11）。
    ///
    /// 判据是**能力**，不是「有文本焦点」：某命令启用 ⇔ `text_editing` 为真 **且** 该命令的
    /// 键组合在引擎侧真有接收者（应用 `bind_key` 绑定了它，并且 `on_action` 注册了处理器）。
    /// 否则菜单项置灰 —— 「可点但点了没反应」比置灰更糟（用户会以为剪贴板/撤销坏了）。
    ///
    /// 为什么引擎不自己实现复制/粘贴/撤销：引擎没有内建文本控件，文本模型与选区归应用所有
    /// 菜单动作只被翻译成与物理按键等价的
    /// [`crate::event::InputEvent::Key`]，能执行它的只有应用注册的那条绑定。
    ///
    /// 与 `app::input::dispatch_action_with_hit` 的「引擎内置动作」名单保持同一份：那边新增
    /// 内置动作时这里必须同步（否则绑到内置动作上的命令会被误判成没有接收者）。
    ///
    /// 只在 macOS 与测试构建里编译：其它平台没有由应用承载的系统菜单栏。
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn edit_menu_availability(
        &self,
        text_editing: bool,
    ) -> crate::platform::menu::EditAvailability {
        crate::platform::menu::EditAvailability::resolve(text_editing, |key, modifiers| {
            self.bindings.iter().any(|binding| {
                binding.key == key
                    && binding.modifiers == modifiers
                    && (matches!(
                        binding.action,
                        Action::FocusNext | Action::FocusPrev | Action::Dismiss
                    ) || self.handlers.contains_key(&binding.action))
            })
        })
    }

    /// 非追踪读一个信号，返回**读守卫**（零拷贝；`Deref<Target = T>`）。
    ///
    /// handler 不是渲染 pass，不登记依赖。守卫的借用规则见 `node::signal` 模块文档：
    /// 守卫不借用 `App`，但在它存活期间对**同一个**信号的写会 panic（显式借用冲突，
    /// 不静默拷贝）。
    ///
    /// # Panics
    /// - 句柄已失效（作用域已释放 / 槽位已回收复用）：**确定性失败**，绝不返回复用后的新值
    ///   （见 `node::signal` 模块文档〈作用域回收与句柄失效〉）；
    /// - `signal` 的槽位不是普通信号（引擎不变量被破坏时才会发生）。
    #[must_use]
    pub fn read<T: 'static>(&self, signal: &Signal<T>) -> SignalRef<T> {
        signal_api::read(self, signal)
    }

    /// 在闭包内读一个信号并返回闭包的返回值（零拷贝）；读守卫在**返回时**析构。
    ///
    /// 与 [`App::read`] 的差别只在守卫的寿命：这里的守卫活不过这次调用，闭包拿到的 `&T` 不能
    /// 逃逸（返回值 `R` 与守卫的生命周期无关）。因此**结构上不可能**在闭包内写同一个信号，
    /// 调用点之后也不会残留守卫（不会踩到「守卫活过读取语句 ⇒ 后续写 panic」）。
    ///
    /// 需要「读了就写」请用 [`App::update_with`] / [`App::write_with`]——
    /// **组合子消灭了 `match` 陷阱**（见 `node::signal` 的 [`SignalRef`] 文档
    /// 〈三种自然写法与 `match` 陷阱〉）。
    ///
    /// # Panics
    /// 同 [`App::read`]（句柄已失效 / 槽位不是普通信号）。
    pub fn read_with<T: 'static, R>(&self, signal: &Signal<T>, body: impl FnOnce(&T) -> R) -> R {
        signal_api::read_with(self, signal, body)
    }

    /// 覆盖输入阈值（平台层用 `GetDoubleClickTime` / `SM_CXDRAG` 初始化；测试也可显式设置）。
    pub fn set_input_thresholds(&mut self, thresholds: InputThresholds) {
        input::set_input_thresholds(self, thresholds)
    }

    /// 非追踪读一个瞬态值，返回读守卫（动画进度 / 滚动偏移的公开读取面）。
    ///
    /// # Panics
    /// 句柄已失效，或槽位不是瞬态值（引擎不变量被破坏时才会发生）。
    #[must_use]
    pub fn read_transient<T: 'static>(&self, transient: &Transient<T>) -> SignalRef<T> {
        signal_api::read_transient(self, transient)
    }

    /// 已有滚动容器节点的句柄（`None` = 该节点不是滚动容器）。
    #[must_use]
    pub fn scroll_handle(&self, window: WindowId, node: NodeId) -> Option<ScrollHandle> {
        scroll::scroll_handle(self, window, node)
    }

    /// 程序性分发（测试 / 菜单桥 / a11y 的 `Click`）。返回是否被处理。
    ///
    /// **受模态作用域约束**（R02-12）：目标节点在模态作用域外 ⇒ `false`（不派发）。菜单桥与
    /// a11y 的 `Click` 走这里；a11y 的其它动作（`SetValue` / `Increment` / `Custom` /
    /// `Scroll*`）不经过本函数，它们在 a11y 动作的唯一入口
    /// （`crate::a11y::action::dispatch`）按**同一判据**（`interaction::focus::in_scope`，
    /// 即模态子树 ∪ 属于该模态的浮层链）校验 —— 因此「模态外不可激活」对指针、键盘、
    /// 辅助技术三条路径口径一致（AR1b N2）。引擎级动作（`FocusNext` / `FocusPrev` /
    /// `Dismiss`）作用于窗口级状态，不受作用域限制（模态打开时 Esc / Tab 仍必须可用）。
    pub fn dispatch_action(
        &mut self,
        window: WindowId,
        node: NodeId,
        action: Action,
        source: ActionSource,
    ) -> bool {
        input::dispatch_action(self, window, node, action, source)
    }

    /// 以当前焦点节点为 target 分发（无焦点 / 焦点在模态作用域外 ⇒ `false`）。
    pub fn dispatch_action_focused(
        &mut self,
        window: WindowId,
        action: Action,
        source: ActionSource,
    ) -> bool {
        input::dispatch_action_focused(self, window, action, source)
    }

    /// 分发的唯一实现（`hit` 只有指针路径会带；`modifiers` 由调用方显式给出 —— 键盘路径必须是
    /// **本次按键**的修饰键，而不是指针状态的缓存）。
    pub(crate) fn dispatch_action_with_hit(
        &mut self,
        window: WindowId,
        node: NodeId,
        action: Action,
        source: ActionSource,
        hit: Option<HitResult>,
        modifiers: Modifiers,
    ) -> bool {
        input::dispatch_action_with_hit(self, window, node, action, source, hit, modifiers)
    }

    /// 分发一次输入事件（平台泵与 `TestApp::inject` 的**唯一**入口）。
    pub(crate) fn dispatch_input(&mut self, window: WindowId, event: InputEvent) -> InputOutcome {
        input::dispatch_input(self, window, event)
    }

    fn hit_at(&self, window: WindowId, position: Point) -> Option<HitResult> {
        input::hit_at(self, window, position)
    }

    fn on_pointer_down(
        &mut self,
        window: WindowId,
        position: Point,
        button: PointerButton,
        modifiers: Modifiers,
        timestamp: Instant,
        outcome: &mut InputOutcome,
    ) {
        input::on_pointer_down(
            self, window, position, button, modifiers, timestamp, outcome,
        )
    }

    fn on_pointer_move(
        &mut self,
        window: WindowId,
        position: Point,
        modifiers: Modifiers,
        outcome: &mut InputOutcome,
    ) {
        input::on_pointer_move(self, window, position, modifiers, outcome)
    }

    fn on_pointer_up(
        &mut self,
        window: WindowId,
        position: Point,
        button: PointerButton,
        modifiers: Modifiers,
        timestamp: Instant,
        outcome: &mut InputOutcome,
    ) {
        input::on_pointer_up(
            self, window, position, button, modifiers, timestamp, outcome,
        )
    }

    fn on_pointer_leave(&mut self, window: WindowId, modifiers: Modifiers) {
        input::on_pointer_leave(self, window, modifiers)
    }

    fn on_scroll(
        &mut self,
        window: WindowId,
        position: Point,
        delta: Point,
        unit: crate::event::ScrollUnit,
        phase: crate::event::ScrollPhase,
        modifiers: Modifiers,
        outcome: &mut InputOutcome,
    ) {
        input::on_scroll(
            self, window, position, delta, unit, phase, modifiers, outcome,
        )
    }

    /// 该节点正在进行中的滚动动画目标（没有动画时 `None`）。
    fn scroll_animation_target(&self, node: NodeId, window: WindowId) -> Option<Point> {
        animation::scroll_animation_target(self, node, window)
    }

    fn on_key(
        &mut self,
        window: WindowId,
        code: KeyCode,
        state: KeyState,
        repeat: bool,
        modifiers: Modifiers,
        outcome: &mut InputOutcome,
    ) {
        input::on_key(self, window, code, state, repeat, modifiers, outcome)
    }

    fn click_count(
        &mut self,
        window: WindowId,
        node: NodeId,
        position: Point,
        button: PointerButton,
        timestamp: Instant,
    ) -> u8 {
        input::click_count(self, window, node, position, button, timestamp)
    }

    fn on_window_focus_lost(&mut self, window: WindowId) {
        input::on_window_focus_lost(self, window)
    }

    /// 作废本窗口当前的指针会话：结束拖拽（派发 `DragPhase::Cancel`）、清按压/悬停/光标、
    /// 释放指针捕获。
    ///
    /// 两条触发路径共用：窗口失焦（`Event::FocusChanged { focused: false }`）与平台报告的
    /// **指针会话取消**（`InputEvent::PointerCancel`：Windows 的 `WM_CAPTURECHANGED`、
    /// macOS 的窗口失去 key、Linux 的指针 `leave`）。缺了这条路径，拖拽中 Alt+Tab 会让会话
    /// 永久停在「按住」，用户只能重启应用。
    fn cancel_pointer_session(&mut self, window: WindowId) {
        input::cancel_pointer_session(self, window)
    }

    /// 当前焦点（`None` = 无焦点 / 窗口不存在 / **焦点节点已卸载** / **焦点在模态作用域外**）。
    ///
    /// 陈旧焦点一律视为无焦点：虚拟列表回收行之后，旧 `NodeId` 不得再驱动键盘路由。
    /// 模态浮层（`trap_focus`）打开后，落在作用域外的焦点同样视为无焦点 —— 背景控件不得再被
    /// Enter/Space 或 a11y 激活；下一次输入/焦点操作会把它移入作用域内第一个可聚焦节点。
    #[must_use]
    pub fn focused(&self, window: WindowId) -> Option<NodeId> {
        focus::focused(self, window)
    }

    /// 清掉指向已卸载节点的焦点（读取路径都是只读，清理集中在这里）。
    fn validate_focus(&mut self, window: WindowId) {
        focus::validate_focus(self, window)
    }

    /// 当前窗口的**模态作用域根**（最上层带 `trap_focus` 的浮层子树根）。
    ///
    /// `None` = 没有模态浮层，整棵树都可交互 / 可读。`Some(scope)` 时引擎把作用域外的一切
    /// 当作 `inert`（Blink `<dialog showModal>` 的 top layer + inert 背景、Qt 模态对话框、
    /// GTK modal 的同一语义）：
    /// - 指针按下 / 滚动 / hover 被挡住（[`App::hovered`] 返回 `None`，背景不产生 `:hover`）；
    /// - 焦点陷阱：[`App::set_focus`] / Tab 序只在作用域内；
    /// - a11y 树把作用域外节点标 `hidden`（AT 读不到背景）；
    /// - 作用域 = 模态子树 **∪** 经 `opener` 链属于该模态的弹出层（对话框里的下拉/菜单）。
    ///
    /// 调用方可以据此判断「某个交互是否属于当前模态」（例如应用自己的 tooltip / 快捷键
    /// 路由）。作用域在模态浮层打开/关闭 / 提层时变化。
    #[must_use]
    pub fn modal_scope(&self, window: WindowId) -> Option<NodeId> {
        focus::focus_scope(self, window)
    }

    /// 指针当前悬停的节点（R02-4；`None` = 空白处 / 窗口外 / 节点已卸载 / **模态作用域外**）。
    ///
    /// 悬停变化时引擎会派生 [`Event::HoverChanged`]、给变化节点标 `PAINT` 并请求一帧，因此
    /// 应用可以在 `bind_style` 里读一个由悬停事件驱动的信号来实现 `:hover` 高亮。
    ///
    /// 模态浮层（`trap_focus`）打开后，落在作用域外的悬停读作 `None`（E12）：背景的
    /// `:hover` / tooltip 不得出现在模态之上；模态内部的悬停（含属于该模态的弹出层）照常。
    #[must_use]
    pub fn hovered(&self, window: WindowId) -> Option<NodeId> {
        focus::hovered(self, window)
    }

    /// 窗口的**最新指针位置**（逻辑客户区坐标；E17）—— [`Window::pointer_position`] 的
    /// 便捷读点（`None` = 窗口不存在，或该窗口从未收到过指针事件）。
    ///
    /// **保留最后值**语义：指针离开窗口 / 窗口失焦后**不**清除、不重算（离开点是平台事件自带
    /// 的最后位置）；坐标**不裁剪**到客户区（捕获期间可以是窗口外坐标）。完整语义（坐标域 /
    /// 触摸与手势 / I3 帧语义 / 三端生产者）见 [`Window::pointer_position`]。
    ///
    /// 纯读取：**不请求帧**，不影响悬停 / 焦点 / 光标。需要「指针在不在窗口内」请读
    /// [`Self::hovered`] 或 `InputEvent::PointerEnter` / `PointerLeave`。
    #[must_use]
    pub fn pointer_position(&self, window: WindowId) -> Option<Point> {
        self.window(window).and_then(Window::pointer_position)
    }

    /// 取得**指针捕获**（R02-8）：请求平台继续把指针事件投递给本窗口，直到
    /// [`App::release_pointer`] / 指针抬起 / 会话取消 / **捕获节点被卸载**。
    ///
    /// 这是**平台级捕获**，不是「命名节点路由」：引擎不做 `setPointerCapture` 那种把事件
    /// 强制投给某个节点的重定向，事件仍按指针位置命中（`ActionCtx::hit` 在窗口外是 `None`）。
    ///
    /// 平台落地：
    /// - Windows：`SetCapture(hwnd)`（引擎在乎的正是它：移出客户区后仍能收到
    ///   `WM_MOUSEMOVE`/`WM_LBUTTONUP`）；
    /// - macOS：AppKit 在 `mouseDown:` 之后的 dragged/up 事件上**隐式捕获**（事件按按下时
    ///   命中的 view 路由），引擎不需要也不应该额外请求；
    /// - Wayland：`wl_pointer` 在按下期间**隐式抓取**（协议由 compositor 保证），同样无需请求。
    ///
    /// 因此本调用在三个平台都成立，但只有 Windows 会真正调用系统 API；其余两个平台的语义由
    /// 系统保证（R02-8 要求「在文档与类型上诚实表达」而不是假装请求成功）。
    ///
    /// 捕获目标被 [`App::close_overlay`] 等路径卸载时，引擎释放捕获并派发一次
    /// `PointerCancel` 观测事件（应用侧「捕获中」状态不会永远悬空）。
    ///
    /// 返回 `false` = 窗口/节点不存在，或已经捕获到同一节点（幂等）。
    pub fn capture_pointer(&mut self, window: WindowId, node: NodeId) -> bool {
        input::capture_pointer(self, window, node)
    }

    /// 释放指针捕获（没有捕获 ⇒ `false`）。
    pub fn release_pointer(&mut self, window: WindowId) -> bool {
        input::release_pointer(self, window)
    }

    /// 当前捕获目标（`None` = 没有捕获 / 窗口不存在 / **捕获节点已卸载**）。
    #[must_use]
    pub fn pointer_captured(&self, window: WindowId) -> Option<NodeId> {
        input::pointer_captured(self, window)
    }

    /// 该物理键在**该窗口**当前是否按住（R02-16；`KeyState::Up` 落地后的查询面）。
    ///
    /// 集合是每窗口的：平台只把按键投递给拥有键盘焦点的窗口，A 窗按住的键不会让
    /// `key_down(B, code)` 为真。窗口失焦或关闭时该窗口的集合清空（抬起消息不再到达）。
    #[must_use]
    pub fn key_down(&self, window: WindowId, code: KeyCode) -> bool {
        self.held_keys
            .get(&window)
            .is_some_and(|held| held.contains(&code))
    }

    /// 设置焦点。不可聚焦 / 同值 / **在模态浮层作用域之外** ⇒ `false`（不标脏、不唤醒）。
    pub fn set_focus(&mut self, window: WindowId, node: Option<NodeId>) -> bool {
        focus::set_focus(self, window, node)
    }

    /// 设置 IME caret hint（P16）：`node` 是该窗口内的文本节点，`byte_index` 是 UTF-8 字节偏移。
    ///
    /// 允许在文本尚未 shape 时调用；**`byte_index` 的越界/char-boundary 校验在读取时做**，
    /// 因为信号写入到下一次 text pass 之间文本内容可能还是旧快照。该调用**不标脏、不请求重绘**，
    /// 只在 hint 变化时唤醒平台事件循环。
    ///
    /// # Errors
    /// `UnknownWindow`：窗口不存在；`StaleNode`：节点不存在；`TextShape`：不是文本节点。
    pub fn set_ime_cursor_area(
        &mut self,
        window: WindowId,
        node: NodeId,
        byte_index: usize,
    ) -> Result<(), EngineError> {
        focus::set_ime_cursor_area(self, window, node, byte_index)
    }

    /// 清除 IME caret hint（P16）；不存在的窗口是 no-op。
    pub fn clear_ime_cursor_area(&mut self, window: WindowId) {
        focus::clear_ime_cursor_area(self, window)
    }

    /// 启用/禁用某个窗口的输入法（IME，P2-7；三端统一语义 B6/B9）。
    ///
    /// 禁用后该窗口的按键**不再路由给 IME**，字母/数字作为普通 `InputEvent::Key` /
    /// `InputEvent::Text` 到达应用；重新启用恢复系统路由：
    ///
    /// - **Windows**：下一轮泵对窗口调用 `ImmAssociateContext(hwnd, None)` 摘上下文
    ///   （旧 `HIMC` 由平台保存），重新启用时恢复；窗口销毁前平台自动恢复关联。
    /// - **macOS**：`GuiView` 的 `inputContext` 返回 `nil`（WebKit 同款做法），按键走普通
    ///   `keyDown:` → `characters` 通路；禁用瞬间 `discardMarkedText` 并清空组合状态。
    /// - **Linux**：焦点目标的 `zwp_text_input_v3.disable()`；重新启用且目标仍是文本时整份
    ///   重发 `enable` + `set_*` 状态。
    ///
    /// 这是**每窗口**的目标状态，默认启用；状态变化不标脏、不请求重绘，只在变化时唤醒
    /// 平台事件循环。窗口不存在时是 no-op。
    ///
    /// 若调用时该窗口正在 IME 组合中，禁用会丢弃组合并补发一条 [`crate::event::ImeEvent::End`]
    /// （与焦点移走/窗口失焦的统一清场路径一致），应用不会停在 preedit 状态。**不**改变引擎焦点。
    pub fn set_ime_enabled(&mut self, window: WindowId, enabled: bool) {
        focus::set_ime_enabled(self, window, enabled)
    }

    /// 当前 IME caret hint 对应的窗口客户区逻辑矩形（P16）。
    ///
    /// `Ok(None)` = 没有设置 hint；`Err(TextShape)` = 已设置但文本尚未 shape / 文本变化，
    /// 平台层应跳过本次更新而不是清除上一次有效位置。
    ///
    /// # Errors
    /// `UnknownWindow`：窗口不存在；`StaleNode` / `TextShape`：hint 指向的文本节点不可解析。
    pub fn ime_cursor_area(&self, window: WindowId) -> Result<Option<Rect>, EngineError> {
        focus::ime_cursor_area(self, window)
    }

    /// 焦点移到下一个可聚焦节点（末尾回绕）。
    pub fn focus_next(&mut self, window: WindowId) -> Option<NodeId> {
        focus::focus_next(self, window)
    }

    /// 焦点移到上一个可聚焦节点（开头回绕）。
    pub fn focus_prev(&mut self, window: WindowId) -> Option<NodeId> {
        focus::focus_prev(self, window)
    }

    /// 让一个节点成为滚动容器（要求 `Style.clip == Clip::Bounds`）。
    ///
    /// # Errors
    /// `StaleNode`（节点不存在）/ `NotScrollable`（未裁剪）。
    pub fn make_scroll(
        &mut self,
        window: WindowId,
        node: NodeId,
        config: ScrollConfig,
    ) -> Result<ScrollHandle, EngineError> {
        scroll::make_scroll(self, window, node, config)
    }

    /// 内容尺寸（滚动范围的来源；虚拟列表由引擎写）。
    ///
    /// # Errors
    /// `NotScrollable`（不是滚动容器）。
    pub fn set_scroll_content(
        &mut self,
        handle: &ScrollHandle,
        content: Size,
    ) -> Result<(), EngineError> {
        scroll::set_scroll_content(self, handle, content)
    }

    /// 程序化滚动（clamp 后写入；同值 ⇒ 完全 no-op）。
    ///
    /// 只产生**视口矩形**的 damage：不写脏位、不跑布局、不跑样式求值。
    ///
    /// 写入前会**取消该滚动槽上的滚轮平滑动画**（R02-10 / P1 回归）：否则动画在后续帧继续
    /// 插值，把键盘 / 程序化滚动拉回原处（例如按 `End` 跳到底后被残留动画拽回）。
    ///
    /// # Errors
    /// `StaleNode`（节点已卸载）/ `NotScrollable`（不是滚动容器）。
    pub fn scroll_to(&mut self, handle: &ScrollHandle, offset: Point) -> Result<(), EngineError> {
        scroll::scroll_to(self, handle, offset)
    }

    /// 布局之后把所有滚动容器的偏移重新夹紧（P2-3.3）。
    ///
    /// 偏移只在**显式滚动**时被 `clamp_offset` 约束（`scroll_to` 是唯一写点），而可用范围
    /// `content − viewport` 会随布局变化：列表变短、视口变高、窗口放大都会让它收缩。
    /// 不重夹就会留下「滚到超过内容末尾」的状态 —— 表现为底部空白、或下一次滚轮只把
    /// 越界量吃掉而不滚动。虚拟列表的 `rewindow` 也依赖偏移是合法的。
    ///
    /// 只在**布局实际重跑过**之后调用（帧内 `layout_if_dirty` 之后）：没有重排时范围不会变，
    /// 每帧遍历所有滚动槽是纯浪费。
    fn normalize_scroll_offsets(&mut self, window: WindowId) {
        scroll::normalize_scroll_offsets(self, window)
    }

    /// 把所有滚动槽的偏移夹回 `content − viewport` 范围；返回是否有变化。
    ///
    /// 调用点有两个：帧首（用上一帧的视口尺寸）与**布局之后**（用本帧的新视口尺寸）。
    /// 只做帧首夹紧时，窗口变大导致视口变大的那一帧仍会用旧上限，于是内容上移过多 /
    /// 底部空白会被画出来，而下一帧不一定存在（泵会因为无脏位而阻塞）——参见复审 P2-8。
    fn clamp_scroll_offsets(&mut self, window: WindowId) -> bool {
        scroll::clamp_scroll_offsets(self, window)
    }

    /// 相对滚动。
    ///
    /// # Errors
    /// 同 [`App::scroll_to`]。
    pub fn scroll_by(&mut self, handle: &ScrollHandle, delta: Point) -> Result<(), EngineError> {
        scroll::scroll_by(self, handle, delta)
    }

    /// 滚动状态快照。
    #[must_use]
    pub fn scroll_state(&self, handle: &ScrollHandle) -> Option<ScrollState> {
        scroll::scroll_state(self, handle)
    }

    /// 滚轮 → 平滑滚动（登记/重定向一个 `Scroll` 动画槽）。
    fn scroll_to_animated(
        &mut self,
        handle: &ScrollHandle,
        target: Point,
    ) -> Result<(), EngineError> {
        scroll::scroll_to_animated(self, handle, target)
    }

    /// 让一个滚动容器变成虚拟列表（要求 `Clip::Bounds` + `position: relative`）。
    ///
    /// # 行生命周期（应用契约）
    ///
    /// 已挂载的行**只能由引擎回收**：滚动 / 视口变化（`rewindow_virtual`）与
    /// [`App::set_virtual_items`] 负责挂载 / 回收，行上的 scoped 键绑定、`after_scoped` 定时器
    /// 与动画槽随回收一并清理。应用**不得**对已挂载行调用 [`App::remove_node`] —— 容器的
    /// `mounted` 索引不会随之更新，该条目此后永不再挂载（视觉空洞）。需要删除条目时改数据源
    /// 后调用 [`App::set_virtual_items`]；删除容器本身可以用 [`App::remove_node`]（行随容器
    /// 一起消失）。
    ///
    /// # Errors
    /// `StaleNode` / `NotVirtualList` / `InvalidVirtualExtent`（预置行高非法）。
    pub fn make_virtual_list(
        &mut self,
        window: WindowId,
        container: NodeId,
        config: VirtualListConfig,
    ) -> Result<VirtualList, EngineError> {
        virtual_list::make_virtual_list(self, window, container, config)
    }

    /// 设置条目总数与行工厂，并重建可见窗口（只在**新建行**时调用 `build`）。
    ///
    /// # Errors
    /// `NotVirtualList`（未 `make_virtual_list`）。
    pub fn set_virtual_items(
        &mut self,
        list: &VirtualList,
        count: usize,
        build: impl FnMut(usize) -> NodeBuilder + 'static,
    ) -> Result<(), EngineError> {
        virtual_list::set_virtual_items(self, list, count, build)
    }

    /// 虚拟列表状态快照。
    #[must_use]
    pub fn virtual_list_state(&self, list: &VirtualList) -> Option<VirtualListState> {
        virtual_list::virtual_list_state(self, list)
    }

    /// 行节点 → 条目下标（a11y 的 `pos_in_set`）。
    #[must_use]
    pub fn virtual_row_index(&self, list: &VirtualList, row: NodeId) -> Option<usize> {
        virtual_list::virtual_row_index(self, list, row)
    }

    /// 逐项高度**整体替换**（E3；`extents.len()` 必须等于当前条目数）。
    ///
    /// 引擎重算前缀和（O(n)），并按锚点补偿滚动偏移：替换前的「首个可见 index + 行内偏移」
    /// 在替换后保持不变（同一调用内完成，不额外请求帧，I3）。与旧高度逐项相同时是纯 no-op。
    ///
    /// # Errors
    /// `NotVirtualList`（list 失效 / 未 `make_virtual_list`）；`VirtualListExtents`（长度与
    /// 条目数不符）；`InvalidVirtualExtent`（逐项高度非有限、为负或累计溢出）。
    pub fn set_virtual_extents(
        &mut self,
        list: &VirtualList,
        extents: &[f32],
    ) -> Result<(), EngineError> {
        virtual_list::set_virtual_extents(self, list, extents)
    }

    /// 把第 `index` 项按 `align` 对齐到视口（E3）；`Nearest` = 已完全可见则不滚。
    ///
    /// # Errors
    /// `NotVirtualList`（list 失效）；`VirtualListExtents`（`index` 越界）。
    pub fn scroll_to_item(
        &mut self,
        list: &VirtualList,
        index: usize,
        align: ItemAlign,
    ) -> Result<(), EngineError> {
        virtual_list::scroll_to_item(self, list, index, align)
    }

    /// 布局后/视口变化后修复所有虚拟列表的可见窗口（幂等：区间不变则零工作）。
    fn rewindow_virtuals(&mut self, window: WindowId) {
        virtual_list::rewindow_virtuals(self, window)
    }

    /// 按当前偏移重建可见窗口（挂载缺的、回收多的）。
    fn rewindow_virtual(&mut self, window: WindowId, node: NodeId) {
        virtual_list::rewindow_virtual(self, window, node)
    }

    /// 打开一个浮层（root 的最后一个子节点 ⇒ DFS 先序末尾 ⇒ 命中优先）。
    ///
    /// 打开瞬间的焦点节点被记为 **opener**（`restore_focus` 时关闭后还给触发按钮，R02-12；
    /// 它同时决定该浮层是否属于某个模态的交互作用域 —— 模态里打开的下拉/菜单，`opener` 在
    /// 模态内，因此仍在作用域内）。`trap_focus` 置位时该浮层成为焦点作用域根。
    ///
    /// # Errors
    /// `UnknownWindow`（窗口已关闭）/ `StaleNode`（树结构被破坏）。
    pub fn create_overlay(
        &mut self,
        window: WindowId,
        options: OverlayOptions,
    ) -> Result<Overlay, EngineError> {
        overlay::create_overlay(self, window, options)
    }

    /// 关闭浮层（节点不存在 ⇒ `false`）。
    pub fn close_overlay(&mut self, overlay: &Overlay) -> bool {
        overlay::close_overlay(self, overlay)
    }

    /// 帧首按**上一帧布局的实测尺寸**重新求值锚点定位（P2-6）。
    ///
    /// `Above` / `Left` 需要知道浮层自己的宽/高；新建时按零尺寸定位会让浮层盖住锚点。
    /// 这里是标准的 measure-then-place：**在布局之前**用上一帧的尺寸纠位，纠位写回后本帧
    /// 布局就会用新位置；只做一次（收敛条件：位置差 < 0.5 px 早退）。
    ///
    /// **不能放在布局之后**：布局之后写 inset 会被 `settle_frame` 的「本轮已布局」判定吞掉
    /// `LAYOUT` 位，下一帧变 `Idle`，位置永远不生效（P1-2.2 的陷阱）。
    fn resolve_overlay_placements(&mut self, window: WindowId) {
        overlay::resolve_overlay_placements(self, window)
    }

    /// 重新定位浮层（相对根的逻辑像素点）。
    pub fn place_overlay(&mut self, overlay: &Overlay, placement: OverlayPlacement) -> bool {
        overlay::place_overlay(self, overlay, placement)
    }

    /// 把浮层提到最上层（重挂到 root 末尾）。
    pub fn bring_overlay_to_front(&mut self, overlay: &Overlay) -> bool {
        overlay::bring_overlay_to_front(self, overlay)
    }

    /// 当前窗口的浮层（自下而上）。
    pub fn overlays(&self, window: WindowId) -> Vec<(NodeId, OverlayDismiss)> {
        overlay::overlays(self, window)
    }

    /// `Dismiss` 动作：关掉最上层浮层（含 `Explicit`）。
    fn dismiss_overlay(&mut self, window: WindowId) -> bool {
        overlay::dismiss_overlay(self, window)
    }

    /// 卸载一棵子树，并**自动清理挂在这棵子树上的引擎级生命周期资源**（R-21 引擎下沉，E16）。
    ///
    /// 这是应用卸载容器 / 面板 / 浮层的**唯一入口**。`NodeTree::remove` / `Window::tree_mut`
    /// 是引擎内部原语：不经本方法会留下 scoped 键绑定、`after_scoped` 定时器与动画槽（见这
    /// 两个 API 的文档）。
    ///
    /// # 清理顺序（不可交换）
    ///
    /// 1. 校验窗口与节点 —— **错误路径不产生任何副作用**；
    /// 2. 记取浮层 opener / 模态状态，并收尾「捕获目标落在子树内」的指针会话（派发
    ///    `PointerCancel`、请求平台释放捕获、清按压 / 拖拽 / 悬停）；
    /// 3. 记取**悬停链**（必须删树前完成：删后 `update_hover` 的 `previous` 会把死节点过滤成
    ///    「无悬停」，补调 `revalidate_hover` 派生不出 leave 事件）；
    /// 4. `NodeTree::remove`（结构 + lane 回收 + 容器登记 + damage），逐条 detach 被删节点的
    ///    信号依赖边；
    /// 5. **指针状态收尾**：落在被删集合里的悬停 / 按压 / 待定拖拽 / 最近命中清掉；悬停链被
    ///    卸载时派生 [`Event::HoverChanged`]（`hovered: None`，`left` 是删前链由内到外）并把
    ///    光标复位为默认 —— 应用侧的 tooltip / `:hover` 不得残留到下一次指针移动；
    /// 6. **scoped 键绑定**（[`App::bind_key_scoped`]）：`scope` 落在被卸载子树内的条目删除，
    ///    祖先 scope 的绑定保留；
    /// 7. **`after_scoped` 定时器**（[`App::after_scoped`]）：`scope` 落在被卸载子树内的条目取消，
    ///    未派发的回调不会再跑在「组件已不存在」的状态上；
    /// 8. **动画槽**（[`App::animate`]）：重绘节点**全部**落在被卸载子树内、或滚动动画的
    ///    `ScrollHandle` 节点被卸载的槽取消，并按 [`AnimationEndKind::Removed`] 入队完成事件；
    ///    部分重绘节点存活的槽保留并剔除死节点（共享动画不被误杀）；
    /// 9. 模态浮层关闭的 a11y 背景语义恢复 + 焦点还给 opener（与 `App::close_overlay` 同路径）；
    /// 10. 若由 [`App::remove_node_with_scope`] 调用，在节点已卸载之后 `release_scope`：其瞬态值
    ///     被回收，引用它们的动画同批取消（[`AnimationEndKind::Released`]）；
    /// 11. 请求一帧（卸载改变了像素；`App::request_frame` 是合并位，重复请求无副作用）。
    ///
    /// 顺序不可交换：先卸载节点再释放作用域，保证释放发生时节点已不在树里、且同一动画槽只会
    /// 被一条路径取消 / 计数（`remove` 内部不求值任何 `bind_*` 闭包；真正的保证是「释放后不在
    /// 同一次调用内留下可能被求值的闭包窗口」）。
    ///
    /// # 不清理的部分（应用义务，最小安全边界）
    ///
    /// - **Action handler**（[`App::on_action`]）：按 `Action` 全局索引，同一个动作可能被多个
    ///   组件 / 窗口共享，引擎无法把它归属到一棵子树。自动摘除会静默破坏其它存活组件，因此
    ///   仍由调用方 [`App::off_action`]；
    /// - **全局键绑定**（[`App::bind_key`]）：同理，按 `(key, modifiers)` 全局索引；
    /// - **a11y Action handler**（[`App::on_a11y_action`]）：与 `on_action` 同构，按
    ///   `A11yAction` 全局索引（同一动作可被多个组件共享），同样不会被自动摘除；组件卸载后
    ///   需要调用方 [`App::off_a11y_action`]；
    /// - **`SignalScope`**：节点的生命周期与信号作用域是两套正交句柄（E8）。`remove_node`
    ///   不释放作用域；需要一次调用完成「卸载 + 回收」时用
    ///   [`App::remove_node_with_scope`]，或随后自行 [`App::release_scope`]（幂等）。
    ///
    /// # 虚拟列表行（应用契约）
    ///
    /// **已挂载的虚拟行只能由引擎回收**（`App::set_virtual_items` 与滚动回收，见
    /// [`App::make_virtual_list`]）。对已挂载行调用本方法会把它从树里删掉，但容器的
    /// `NodeId → 条目下标` 索引（`App::virtual_list_state` 的 `mounted`）不会更新 —— 该条目
    /// 此后**永不再挂载**（视觉空洞），`App::virtual_row_index` 也仍把死句柄当作已挂载。删除
    /// 条目请改数据源后调用 `App::set_virtual_items`；删除容器本身不受此限制（行随容器一起
    /// 消失）。debug 构建里本方法对「删除已挂载行」直接 `debug_assert` 失败（release 下只靠
    /// 文档：引擎看不到应用意图）。
    ///
    /// # 语义
    ///
    /// - **重复删除**：第二次返回 [`EngineError::StaleNode`] 且不产生副作用（显式失败，不是
    ///   静默 no-op）；
    /// - **祖先删除**：整个子树（含所有后代 scope / 定时器 / 动画）一并清理；只删除后代不影响
    ///   祖先 scope 的绑定与定时器；
    /// - **root 不可删除**（[`EngineError::StaleNode`]）；
    /// - 跨窗口句柄返回 [`EngineError::StaleNode`]，不会触碰其真正宿主窗口；
    /// - **I3**：取消定时器 / 动画不请求帧、不让平台进入连续帧；`animations_running` 随动画
    ///   取消回落。只有第 11 步的「树变了要重画」会请求**一帧**。
    ///
    /// # Errors
    ///
    /// - [`EngineError::UnknownWindow`]：窗口不存在（已关闭 / 从未创建）；
    /// - [`EngineError::StaleNode`]：`node` 不在该窗口的节点树中（已卸载 / 从未挂载 / 属于另一
    ///   窗口）或是 root。
    pub fn remove_node(
        &mut self,
        window: WindowId,
        node: NodeId,
    ) -> Result<RemoveOutcome, EngineError> {
        window_set::remove_node(self, window, node)
    }

    /// 卸载子树并在**节点已卸载之后**释放组件的信号作用域
    /// （= [`App::remove_node`] + [`App::release_scope`] 的同调用组合）。
    ///
    /// 适用于「一个组件 = 一棵子树 + 一个 `SignalScope`」的常规形态：先卸节点、再回收作用域值
    /// 的顺序由引擎保证（`bind_*` 闭包不会在本调用内被求值到已回收的句柄）。作用域里的瞬态值
    /// 被回收时，引用它们的动画槽同批取消（[`AnimationEndKind::Released`]），计入返回值的
    /// `animations`。
    ///
    /// # 契约：`scope` 必须属于该子树
    ///
    /// `SignalScope` 与 `NodeId` 是两套**正交**句柄（E8 设计），引擎没有
    /// `NodeId → SignalScope` 映射，因此无法完整校验归属；但引擎会拒绝**可证明的错配**：
    ///
    /// - **子树之外仍有存活读者**（依赖图里该作用域的存活信号被子树外的节点读取，依赖边来自
    ///   已求值的 `bind_*`）⇒ [`EngineError::ScopeOwnershipMismatch`]：释放必然打断那个存活
    ///   绑定，因此在任何副作用之前失败 —— 节点**不会**被卸载，作用域**不会**被释放；
    /// - 传入**已释放**的作用域：幂等（`released_values == 0`），节点删除照常完成；
    /// - 传入**没有存活读者**（或读者全在被删子树内）的错配作用域：引擎无法识别 —— 它的值会
    ///   被真实释放（不可恢复，`released_values > 0`），引用这些值的存活组件随后读信号会
    ///   确定性 panic。调用方必须保证 `scope` 正是该子树所属组件的作用域；不确定时请改用
    ///   [`App::remove_node`] + 显式 [`App::release_scope`]，把两件事分开写。
    ///
    /// # Errors
    ///
    /// 同 [`App::remove_node`]，另有：
    /// - [`EngineError::ScopeOwnershipMismatch`]：`scope` 的存活信号被子树之外的节点读取
    ///   （可证明的错配）。**错误发生在任何清理之前**，节点与作用域都保持原状。
    #[must_use = "scope 错配会不可恢复地释放其它组件的值（引擎只能拒绝可证明的错配），必须检查返回的 RemoveOutcome"]
    pub fn remove_node_with_scope(
        &mut self,
        window: WindowId,
        node: NodeId,
        scope: SignalScope,
    ) -> Result<RemoveOutcome, EngineError> {
        window_set::remove_node_with_scope(self, window, node, scope)
    }

    /// 当前拖拽会话。
    #[must_use]
    pub fn drag_state(&self, window: WindowId) -> Option<DragState> {
        input::drag_state(self, window)
    }

    /// 拖拽的只读快照。
    #[must_use]
    pub fn drag(&self, state: &DragState) -> Option<DragInfo> {
        input::drag(self, state)
    }

    /// 取消拖拽（派发 `Cancel` 阶段并释放捕获）。
    pub fn cancel_drag(&mut self, state: &DragState) -> bool {
        input::cancel_drag(self, state)
    }

    /// 注册一个动画（起点 = 目标瞬态值的当前值；同目标已有动画 ⇒ 替换）。
    ///
    /// `nodes` 是**显式注册**的「每帧要重绘的节点集合」（对应设计 §3.7）。
    ///
    /// # Errors
    /// `UnknownWindow`（窗口已关闭）。
    pub fn animate(
        &mut self,
        window: WindowId,
        value: AnimatedValue,
        nodes: &[NodeId],
        spec: AnimationSpec,
    ) -> Result<AnimationId, EngineError> {
        animation::animate(self, window, value, nodes, spec)
    }

    /// 注册**动画完成回调**（E10）：动画到达终值 / 被取消 / 被替换 / 目标作用域释放时通知。
    ///
    /// **单槽**：重复注册替换上一个（与 [`App::on_frame`] 同口径）；不需要时注册一个空闭包即可。
    /// 因此 **kit 层（overlay / tooltip 等）必须统一路由**到同一个回调（按 [`AnimationId`] /
    /// 窗口分发给各消费者），**不得各自注册** —— 后注册者会静默覆盖先注册者，表现为某条收尾
    /// 永远不发生（E10-R02 裁决：引擎保持单槽，路由责任在 kit 层）。
    /// 回调形态是 `FnMut(&mut App, AnimationCompletion)`：可以读 / 写状态、建树、再注册动画
    /// （回调里产生的新的取消 / 替换事件由**同一派发循环**继续消费，不会重入派发）。
    ///
    /// # 时序契约（与 [`App::on_frame`] 的关系）
    ///
    /// 1. **自然完成**（[`AnimationEndKind::Finished`]）：在**产生终值的那一帧内**派发 ——
    ///    `tick_animations` 写入终值、标记 `BIND | PAINT` 之后，样式 / 布局 / 绘制之前。
    ///    因此回调里做的收尾（卸载节点 / 关浮层）**参与本帧绘制**；回调里改状态触发的重绘
    ///    由应用按常规方式请求（`App::write` 自动请求）或由下一轮补上。
    /// 2. **帧外终止**（[`AnimationEndKind::Cancelled`] / [`AnimationEndKind::Replaced`] /
    ///    [`AnimationEndKind::Released`] / [`AnimationEndKind::Removed`] / 0 ms 动画的
    ///    `Finished`）：在**下一帧入口**（或平台泵
    ///    下一轮的钩子步骤）派发 —— 调用点只入队 + 唤醒泵一次（`Dispatcher::wake`，**不请求
    ///    帧**，I3）；因此应用最迟在「下一帧开始前」观测到。
    /// 3. **与 `on_frame` 的关系**：`on_frame` 钩子在帧**之前**运行，看不到本帧刚写入的终值。
    ///    引擎在动画自然完成时唤醒泵一次（不提交空帧），保证**下一轮钩子**一定能看到终值 ——
    ///    这是给「只在钩子里同步状态」的应用的兜底，不需要轮询 `App::animations_running`。
    /// 4. 取消 / 替换**不**派发 `Finished`：只有真正到达终值的动画报告 `Finished`。
    ///
    /// 回调在 UI 线程（持有 `&mut App`）内运行；不得在回调里再次调用 `Application::run` /
    /// 嵌套帧循环（与 `on_frame` 同一约束）。
    pub fn on_animation_complete(
        &mut self,
        handler: impl FnMut(&mut App, AnimationCompletion) + 'static,
    ) {
        self.animation_complete_handler = Some(Box::new(handler));
    }

    /// 注册一个**一次性定时器**（E10）：`delay` 之后在 UI 线程回调一次。
    ///
    /// `delay` 的基准是**登记瞬间**（生产取真实时间，E10c-R01；测试宿主 `TestApp` 取引擎
    /// 时钟，见 `TestApp::advance`）—— **首次泵启动之前**登记同样成立（`Application::run`
    /// 之前建窗 → 初始化 → 登记，初始化耗时不折算进 `delay`）。
    ///
    /// 定时器归属 `window`：窗口关闭时自动取消（不会在关窗后再跑应用闭包）。到期前**不请求帧、
    /// 不让平台进入连续帧**（I3）；平台泵把阻塞等待截止到 `App::next_wake_deadline()` 并在到点
    /// 唤醒**一次**。派发点 = 平台泵每轮的钩子步骤（`App::run_frame_hook`）或 `App::frame` 入口，
    /// 因此回调最迟在到期后的**下一个引擎轮次**运行（**不会早于** `delay`）。
    ///
    /// 不新建线程：复用平台泵已有的定时器（Windows `SetTimer` / macOS `CFRunLoopTimer` /
    /// Linux calloop timeout）与引擎时钟；`App` 的 tokio runtime 不参与。
    ///
    /// 组件生命周期建议用 [`App::after_scoped`]（节点卸载自动取消）；显式取消用
    /// [`App::cancel_timer`]（幂等）。
    ///
    /// # Errors
    /// [`EngineError::UnknownWindow`]：窗口不存在（关窗后再注册的定时器没有派发轮次）。
    pub fn after(
        &mut self,
        window: WindowId,
        delay: Duration,
        callback: impl FnOnce(&mut App) + 'static,
    ) -> Result<TimerId, EngineError> {
        timer::after(self, window, None, delay, callback)
    }

    /// 注册一个**节点作用域**的一次性定时器（E8 scoped 语义）：窗口关闭或 `scope` 子树被卸载
    /// （`App::remove_node`）时自动取消。
    ///
    /// `scope` 可以是组件自己的任何存活节点（通常是面板根 / 触发器）；注册期校验窗口与节点
    /// 归属（[`EngineError::UnknownWindow`] / [`EngineError::StaleNode`]），与
    /// [`App::bind_key_scoped`] 同口径。
    ///
    /// 其余语义与 [`App::after`] 完全相同。
    ///
    /// # Errors
    /// 同 [`App::after`]。
    pub fn after_scoped(
        &mut self,
        window: WindowId,
        scope: NodeId,
        delay: Duration,
        callback: impl FnOnce(&mut App) + 'static,
    ) -> Result<TimerId, EngineError> {
        timer::after(self, window, Some(scope), delay, callback)
    }

    /// 取消一个一次性定时器（幂等）。返回 `false` = 已到期派发 / 已取消 / 窗口已关闭。
    pub fn cancel_timer(&mut self, id: &TimerId) -> bool {
        timer::cancel(self, id)
    }

    /// 最近一个一次性定时器的到期时刻（平台泵的阻塞等待截止时刻；E10）。
    ///
    /// 消费者：三端事件循环（Windows 一次性 `SetTimer` / macOS 一次性 `CFRunLoopTimer` /
    /// Linux calloop timeout）。`None` = 没有待派发定时器 —— 平台保持原有空闲阻塞（I3）。
    /// Linux 的实现同时并入「被帧预算门禁推迟的帧」的到期时刻（`Window::frame_budget_deadline`）。
    pub(crate) fn next_wake_deadline(&self) -> Option<Instant> {
        #[cfg(target_os = "linux")]
        {
            let frames = self
                .windows()
                .filter_map(|(_, window)| window.frame_budget_deadline())
                .min();
            match (timer::next_deadline(self), frames) {
                (Some(timer), Some(frame)) => Some(timer.min(frame)),
                (Some(timer), None) => Some(timer),
                (None, Some(frame)) => Some(frame),
                (None, None) => None,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            timer::next_deadline(self)
        }
    }

    /// 仅测试：待派发定时器条数（泄漏 / 清理的证据面）。
    #[cfg(all(test, feature = "test-support"))]
    #[must_use]
    pub(crate) fn timer_count_for_test(&self) -> usize {
        timer::count(self)
    }

    /// 仅测试：取走平台泵的「有一帧要画」唤醒位（E10 完成唤醒 / 定时器唤醒的证据面）。
    ///
    /// 取走即清零，测试应在需要模拟「泵消费一次唤醒」的位置调用。
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn take_pump_wake_for_test(&self) -> bool {
        self.dispatcher.take_frame_wake()
    }

    /// 仅测试：待派发的动画终止事件条数。
    #[cfg(all(test, feature = "test-support"))]
    #[must_use]
    pub(crate) fn pending_animation_completions_for_test(&self) -> usize {
        self.animation_completions.len()
    }

    /// 取消动画（幂等）。
    pub fn cancel_animation(&mut self, id: &AnimationId) -> bool {
        animation::cancel_animation(self, id)
    }

    /// 该窗口是否还有未完成的动画。
    #[must_use]
    pub fn animations_running(&self, window: WindowId) -> bool {
        animation::animations_running(self, window)
    }

    /// 全部窗口是否还有未完成的动画（`needs_continuous_frames` 的组成部分：连续帧由平台泵的
    /// 合成器节拍线程 `FramePacer` 提供；`SetTimer` 只保留 a11y 补发与 E10 一次性截止时刻，
    /// 空闲无动画且无待派发定时器时不挂表）。
    pub(crate) fn has_animations(&self) -> bool {
        animation::has_animations(self)
    }

    /// 平台是否需要持续出帧：只有动画需要按显示节拍推进（Debug HUD 不参与，见
    /// `animation::needs_continuous_frames`）。
    pub(crate) fn needs_continuous_frames(&self) -> bool {
        animation::needs_continuous_frames(self)
    }

    /// 是否有窗口的帧请求正被预算门禁推迟（A5）：平台据此挂一次性唤醒。
    ///
    /// 消费者：Windows 泵 / macOS 泵（按刷新率挂一次性定时器）；Linux 的补帧走
    /// `next_wake_deadline` + calloop timeout，不需要这个布尔查询。
    #[cfg(any(windows, target_os = "macos"))]
    pub(crate) fn has_deferred_frames(&self) -> bool {
        self.windows()
            .any(|(_, window)| window.frame_budget_deferred())
    }

    fn mark_nodes_paint(&mut self, window: WindowId, nodes: &[NodeId]) {
        animation::mark_nodes_paint(self, window, nodes)
    }

    /// 推进本窗口的动画（`App::frame` 在帧门禁**之前**调用）。
    fn tick_animations(&mut self, window: WindowId) {
        animation::tick_animations(self, window)
    }

    /// 把引擎时钟推进到真实时间（平台泵每轮一次；测试用 `TestApp::advance`）。
    pub(crate) fn sync_clock(&mut self) {
        animation::sync_clock(self)
    }

    /// 平台泵的轮首时钟同步：推进到真实时间并**标记为活时钟**（E10c）。
    ///
    /// 与 [`App::sync_clock`] 的区别是置 `clock_live`：一次性定时器的登记基准据此选择
    /// 「登记瞬间真实时间」（生产）还是「引擎时钟」（测试宿主）。生产在 `App::new` 就已是
    /// 活时钟（E10c-R01：首次泵启动前登记也必须只晚不早）；平台泵（Windows/macOS/Linux）
    /// 每轮调用本方法以刷新时钟；`App::dispatch` 里的时钟同步**不**置位（测试也会派发输入）。
    pub(crate) fn sync_clock_live(&mut self) {
        animation::sync_clock_live(self)
    }

    /// 测试宿主专用：把定时器登记基准切回**引擎时钟**（`TestApp::advance` 是唯一推进源，
    /// deadline 必须能精确断言）。
    ///
    /// 生产默认是活时钟（`App::new` 即置位，E10c-R01），因此测试宿主必须显式关闭；该方法只在
    /// 测试基建（`test-support`，不进发布物）里存在。
    #[cfg(feature = "test-support")]
    pub(crate) fn use_engine_clock(&mut self) {
        self.clock_live = false;
    }

    /// 推进测试时钟（`TestApp::advance` 的唯一实现；只在测试构建里存在）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn advance_clock(&mut self, by: Duration) {
        animation::advance_clock(self, by)
    }

    /// 取走待推给平台的光标变化（**带窗口 id**：光标是每窗口状态）。
    pub(crate) fn take_cursor_change(&mut self) -> Option<(WindowId, Cursor)> {
        input::take_cursor_change(self)
    }

    /// 取走待推给平台的捕获请求（每窗口目标状态，R02-8/R02-22）。
    ///
    /// 必须带窗口 id：`SetCapture` 是**每窗口**状态，固定发给首窗会让非首窗的拖拽拿不到
    /// 捕获，鼠标移出窗口后松手就丢 `WM_LBUTTONUP`，会话永久卡在「按住」（复审 P1-3）。
    pub(crate) fn take_capture_requests(&mut self) -> Vec<(WindowId, bool)> {
        input::take_capture_requests(self)
    }

    /// 引擎时钟（`TestApp::inject` 用它给合成事件盖时间戳；R02-14）。
    #[cfg(feature = "test-support")]
    pub(crate) fn clock(&self) -> Instant {
        self.clock
    }

    /// 本窗口的 IME 目标状态（三端平台泵每轮读它与已应用状态比较，P2-7 / B6 / B9）。
    pub(crate) fn ime_enabled(&self, window: WindowId) -> bool {
        focus::ime_enabled(self, window)
    }

    /// 建一个窗口（headless：只建树与空场景）。`WindowId` 从 **1** 起单调 +1、永不复用
    /// （0 保留为非法句柄，见 `WindowId::from_raw`）。
    pub fn create_window(&mut self, options: WindowOptions) -> WindowId {
        window_set::create_window(self, options)
    }

    /// 只读窗口。
    #[must_use]
    pub fn window(&self, id: WindowId) -> Option<&Window> {
        window_set::window(self, id)
    }

    /// 最近一帧的呈现快照（A4：`commit → presented` 的**公开**消费面）。
    ///
    /// 真实窗口的 GPU 反馈每轮泵由 [`Window::poll_renderer`] 落到窗口上；`presented_at` 是
    /// 该端能给出的真实上屏时刻的**下界**（`None` = 该端/该帧没有可信值，不是「用完成时刻
    /// 顶替」）。`acceptance window` 的 `publish→presented` p50/p99 与有值率、HUD 的后续行、
    /// 应用侧延迟监控都从这里取（`FrameFeedback` 是 crate 私有）。
    ///
    /// `None`：窗口不存在，或还没有观测到任何完成帧（首帧之前 / headless）。
    #[must_use]
    pub fn last_frame_presentation(&self, window: WindowId) -> Option<FramePresentation> {
        self.window(window)
            .and_then(Window::last_frame_presentation)
    }

    /// 可写窗口（应用层建树的入口）。
    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        window_set::window_mut(self, id)
    }

    /// 设置窗口的**自绘标题栏拖动区**（X2；Chrome/Electron drag-region 模型）。
    ///
    /// 区域是**逻辑窗口坐标**（客户区左上角原点），可多个；传空切片清空。三端语义与各自的
    /// 原生路径见 [`Window::set_drag_regions`]。引擎不自动跟随布局：应用在布局/尺寸变化后
    /// 重新调用。值变化才唤醒平台泵（泵每轮对账到原生窗口）；窗口不存在时是 no-op。
    pub fn set_drag_regions(&mut self, window: WindowId, regions: &[Rect]) {
        window_set::set_drag_regions(self, window, regions)
    }

    /// 注册窗口的「关闭前」处理器：返回 `false` 拦下关闭（P2-8）。
    ///
    /// 平台在用户点 X / Cmd-W 时先调用它；应用可在此保存未提交状态、弹确认框。
    /// 窗口不存在时**忽略**注册（否则会留下一个永远不会被调用、却持有应用闭包的条目）。
    pub fn on_close(&mut self, id: WindowId, handler: impl FnMut(&mut Self) -> bool + 'static) {
        window_set::on_close(self, id, handler)
    }

    /// 平台关闭路径：先问关闭前处理器，允许才关。返回 `true` = 窗口已关闭。
    ///
    /// 处理器用「取出 → 调用 → 放回」的方式调用（与 a11y handler 同一做法）：它需要
    /// `&mut App`，不能在使用 `self.close_handlers` 的借用期内存活。
    pub(crate) fn request_close(&mut self, id: WindowId) -> bool {
        window_set::request_close(self, id)
    }

    /// 两阶段批量关闭（AR3 macOS P1-2）：**先询问全部窗口，全部同意后才统一关闭**。
    ///
    /// 返回 `false` = 有窗口的关闭处理器 veto ⇒ **一个窗口都没有被关闭**，调用方必须回滚整个
    /// 流程（macOS 退出握手就此回复 AppKit「不终止」）。
    ///
    /// 退出握手必须用它而不是逐窗 [`Self::request_close`]：后者在批准时**立刻**关窗，于是
    /// 「窗口 A 同意、窗口 B veto」的退出会留下「A 已被拆掉（Renderer 已 drop、应用状态丢失）
    /// 却告诉用户『退出已取消』」——用户点取消，另一个窗口已经没了。
    ///
    /// 每个关闭处理器在一次调用里恰好跑一次（批准后第二阶段直接关窗，不再问一遍：否则就会
    /// 两次弹确认框）。
    ///
    /// 只在 macOS 与测试构建里编译：唯一消费者是 macOS 的 `request_quit`（其它平台一次只处理
    /// 一个窗口的关闭请求，没有「一次性关掉全部窗口」的入口）。
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn request_close_all(&mut self, ids: &[WindowId]) -> bool {
        window_set::request_close_all(self, ids)
    }

    /// 关窗：精确退订该窗口全部节点的依赖，然后丢弃。返回 `false` = 窗口不存在。
    ///
    /// 同时清理所有按窗口的旁表（关闭处理器 / a11y 限流戳 / scale epoch / 动画槽），
    /// 否则 create/close 循环下这些表会无界增长。
    pub fn close_window(&mut self, id: WindowId) -> bool {
        window_set::close_window(self, id)
    }

    /// 请求一次窗口控制（自绘标题栏的系统按钮 / 拖动 / 缩放；E6 起三端统一入口）。
    ///
    /// 请求是**异步**的：只登记目标，平台泵在同一轮 / 下一轮执行（Linux Wayland 事件循环、
    /// Windows 泵、macOS 泵）。最终状态一律以 [`Window::state`] 为准 —— 不得假设请求一定生效
    /// （compositor / 系统可以拒绝或改变请求，I10）。
    ///
    /// 平台能力差异（完整表见 [`crate::window::WindowControl`]）：
    /// - **Windows**：`Minimize` / `ToggleMaximize` 原生（`ShowWindow`）；`ToggleFullscreen` /
    ///   `BeginMove` / `BeginResize` 无等价请求 ⇒ `UnsupportedWindowCapability`（拖动 / 缩放由
    ///   [`Self::set_drag_regions`] + 系统命中码承担）。
    /// - **macOS**：`Minimize` / `ToggleMaximize` / `ToggleFullscreen` 原生
    ///   （`performMiniaturize:` / `performZoom:` / `toggleFullScreen:`）；`BeginMove` /
    ///   `BeginResize` 没有可用的 `NSEvent` ⇒ `UnsupportedWindowCapability`。
    /// - **Linux/Wayland**：全部由 `xdg_toplevel` 请求承担；`BeginMove` / `BeginResize` 使用最近
    ///   一次指针按键的 serial，因此只应从输入事件的动作处理器里调用。
    ///
    /// **关闭不在本入口**：应用自己决策（[`Self::close_window`] / [`Self::on_close`]，§8.6.3），
    /// 引擎不为「关闭」提供平台请求。
    ///
    /// # Errors
    /// - [`EngineError::UnknownWindow`]：窗口不存在（已经关闭）。
    /// - [`EngineError::UnsupportedWindowCapability`]：该端没有这个控制（见上表）。
    pub fn request_window_control(
        &mut self,
        window: WindowId,
        control: crate::window::WindowControl,
    ) -> Result<(), EngineError> {
        window_set::request_window_control(self, window, control)
    }

    /// 取走待执行的窗口控制请求（三端泵的唯一读点）。
    pub(crate) fn take_window_control(
        &mut self,
    ) -> Option<(WindowId, crate::window::WindowControl)> {
        window_set::take_window_control(self)
    }

    // ── E6（P1-E6）：窗口 chrome 的运行时 API（独立区块，便于与并行批次合并） ──

    /// 运行时改窗口标题（三端生效：Windows `SetWindowTextW` / macOS `setTitle:` /
    /// Linux `xdg_toplevel.set_title`）。
    ///
    /// 只写窗口参数并唤醒平台泵：**下一轮泵**对账到原生窗口（同值 no-op，不重复系统调用）。
    /// 平台原生调用失败由平台层记诊断（窗口仍可用，标题保持旧值；不 panic），未知窗口是强类型
    /// 错误。
    ///
    /// # Errors
    /// [`EngineError::UnknownWindow`]：窗口不存在（已经关闭）。
    pub fn set_title(
        &mut self,
        window: WindowId,
        title: smol_str::SmolStr,
    ) -> Result<(), EngineError> {
        window_set::set_title(self, window, title)
    }

    /// macOS：设置三个原生交通灯的位置（其它平台返回 [`EngineError::UnsupportedWindowCapability`]，
    /// 不静默降级）。
    ///
    /// `inset` 是**逻辑点**（客户区左上原点、y 向下）：交通灯分组的左上角落在该点，按钮之间
    /// 保持系统间距。与 [`Self::traffic_light_inset`] 是同一几何的写 / 读两端：应用据此把可点击
    /// 控件放到交通灯之外（§8.6.5 —— 应用**不能**把可点击控件放到交通灯下面，会与系统按钮抢命中）。
    ///
    /// **set-once（§8.6.5 文档化限制）**：位置只在建窗时决定一次；首次调用生效后，运行期再次
    /// 调用不会改变已应用的位置（AppKit 在 resize / 全屏转场后的重排不追回）。本 API **没有**
    /// 「恢复系统默认位置」的入口 —— 引擎不保存 AppKit 系统按钮布局的副本。
    ///
    /// # Errors
    /// - [`EngineError::UnknownWindow`]：窗口不存在。
    /// - [`EngineError::UnsupportedWindowCapability`]：非 macOS。
    pub fn set_traffic_light_position(
        &mut self,
        window: WindowId,
        inset: crate::math::Point,
    ) -> Result<(), EngineError> {
        window_set::set_traffic_light_position(self, window, inset)
    }

    /// macOS：交通灯占用的**安全区**（逻辑点，客户区左上原点、y 向下；应用按返回值留白）。
    ///
    /// `None` = 非 macOS，或原生全屏下系统隐藏了交通灯（§8.6.5），或窗口不存在。这是只读查询，
    /// 与 setter 同层；应用不需要自己猜按钮位置，也不应把可点击控件放进该区域。
    #[must_use]
    pub fn traffic_light_inset(&self, window: WindowId) -> Option<crate::math::Point> {
        window_set::traffic_light_inset(self, window)
    }

    /// 遍历全部窗口（顺序不保证）并带出 `WindowId`。
    ///
    /// 带上 id 是必要的：帧是**按窗口**推进的（`refresh_styles(window)` / P06 的 paint），只有 `&Window`
    /// 拿不到「这是哪个窗口」（消费者：P09 的帧驱动、测试）。
    pub fn windows(&self) -> impl Iterator<Item = (WindowId, &Window)> {
        window_set::windows(self)
    }

    /// 显示或隐藏内置 Debug HUD（P17；需要 `hud` feature）。
    ///
    /// HUD 是窗口 root 末尾的普通子树：右上角浮动、不参与文档流、命中穿透、a11y 隐藏。
    /// 它是**纯观察者**：不产生帧，只在应用自己出帧时按 500 ms 采样窗口刷新数字；静止窗口保持
    /// 上一次文本。应用应在建完静态场景后开启 HUD；之后再追加的 root 子节点可能盖在它上面。
    ///
    /// # Errors
    /// `EngineError::UnknownWindow`：窗口不存在。建树失败时返回底层 `StaleNode` 等错误，
    /// 且不会留下半棵 HUD 子树。
    #[cfg(feature = "hud")]
    pub fn set_debug_hud(&mut self, window: WindowId, enabled: bool) -> Result<(), EngineError> {
        debug::set_debug_hud(self, window, enabled)
    }

    /// 该窗口是否安装了 Debug HUD（未知窗口返回 `false`）。
    #[cfg(feature = "hud")]
    #[must_use]
    pub fn debug_hud_enabled(&self, window: WindowId) -> bool {
        debug::debug_hud_enabled(self, window)
    }

    /// 帧门禁前更新 HUD 的采样/文本暂存，并请求 HUD damage。
    #[cfg(feature = "hud")]
    fn prepare_debug_hud(&mut self, window: WindowId) {
        debug::prepare_debug_hud(self, window)
    }

    /// `refresh_styles` 之后、`refresh_text` 之前写入 HUD 文本。
    #[cfg(feature = "hud")]
    fn apply_debug_hud(&mut self, window: WindowId) {
        debug::apply_debug_hud(self, window)
    }

    /// 新 overlay 会 append 到 root 末尾；HUD 必须保持在最上层。
    #[cfg(feature = "hud")]
    fn raise_debug_hud(&mut self, window: WindowId) {
        debug::raise_debug_hud(self, window)
    }

    /// P03 的「样式求值 pass」：对指定窗口中**带 `BIND`** 的动态节点求值。
    ///
    /// 它做四件事：求值闭包、差分替换依赖、写回 patch、按需升级 `LAYOUT` / 标 `A11Y`。
    /// **不产生任何 Scene 图元**（那是 P06 的 paint 阶段；两者共用本实现的前半段）。
    ///
    /// 返回 `None` = 窗口不存在（P03 的冻结错误表里没有「未知窗口」变体，`Option` 是唯一诚实的表达）。
    /// P11 追加：`bind_a11y` 与另两条动态通道同批求值（同一 `ReadCtx` / `ReadTracker` / 依赖边集）。
    /// R13-6：**只有** `bind_a11y` 的节点不进 `dirty_bind`，由紧随其后的
    /// [`App::refresh_a11y_bindings`] 求值 —— 本方法的语义仍是"全部动态属性都已重算"。
    pub fn refresh_styles(&mut self, window: WindowId) -> Option<StylePassReport> {
        let report = style_pass::refresh_styles(self, window);
        if report.is_some() {
            self.refresh_a11y_bindings(window);
        }
        report
    }

    /// 注册 a11y 动作的处理器（同 `A11yAction` 后写覆盖）。
    ///
    /// 返回 `false` = 该动作**不能**注册：`A11yAction::Click` 必须落到 P10 的 `Action` 表
    /// （`App::on_action`），让指针点击 / 键盘 Enter / AT 激活共享同一身份
    pub fn on_a11y_action(
        &mut self,
        action: crate::a11y::A11yAction,
        handler: impl FnMut(&mut crate::a11y::A11yActionCtx<'_>) + 'static,
    ) -> bool {
        a11y::on_a11y_action(self, action, handler)
    }

    /// 删除 a11y 动作的处理器（返回是否真的删了）。
    pub fn off_a11y_action(&mut self, action: crate::a11y::A11yAction) -> bool {
        a11y::off_a11y_action(self, action)
    }

    /// 写一个节点的**动态** a11y 补丁并登记交付（应用通道；E15b F3/F5 收敛）。
    ///
    /// 详细语义见 `app::a11y::set_a11y_patch`（与 `bind_a11y` 共用差分、`affects_descendants`
    /// 子树标记、请求帧、覆盖关系）。**公开面保留**：E15 已把本通道登记为 base_ui
    /// `List::set_row_a11y` 静态快照的动态失效面（spec-delta E15 §4/D4，消费者在 base_ui 后续
    /// 批），不是"未来可能需要"的预留 API；`A11yPatch::affects_descendants` 则收敛为
    /// `pub(crate)`（只有引擎内部消费者）。
    ///
    /// # Errors
    /// `EngineError::UnknownWindow`（窗口已关闭）/ `EngineError::StaleNode`（节点已卸载）。
    pub fn set_a11y_patch(
        &mut self,
        window: WindowId,
        node: NodeId,
        patch: crate::a11y::A11yPatch,
    ) -> Result<(), EngineError> {
        a11y::set_a11y_patch(self, window, node, patch)
    }

    /// a11y 诊断（与 `text_diagnostics` 同型；上限 `A11Y_DIAGNOSTIC_CAPACITY`，超出丢最旧）。
    #[must_use]
    pub fn a11y_diagnostics(&self) -> &[crate::a11y::A11yDiagnostic] {
        a11y::a11y_diagnostics(self)
    }

    /// 记录一条 a11y 诊断（唯一入口：计数与容量在这里统一）。
    pub(crate) fn a11y_note(&mut self, diagnostic: crate::a11y::A11yDiagnostic) {
        a11y::a11y_note(self, diagnostic)
    }

    /// 该动作是否注册了 handler（`a11y::action` 的分派判据）。
    pub(crate) fn a11y_has_handler(&self, action: crate::a11y::A11yAction) -> bool {
        a11y::a11y_has_handler(self, action)
    }

    /// 摘出 handler（借用纪律：调用期间它不在表里）。
    pub(crate) fn a11y_take_handler(
        &mut self,
        action: crate::a11y::A11yAction,
    ) -> Option<Box<dyn FnMut(&mut crate::a11y::A11yActionCtx<'_>)>> {
        a11y::a11y_take_handler(self, action)
    }

    /// 放回 handler。
    pub(crate) fn a11y_put_handler(
        &mut self,
        action: crate::a11y::A11yAction,
        handler: Box<dyn FnMut(&mut crate::a11y::A11yActionCtx<'_>)>,
    ) {
        a11y::a11y_put_handler(self, action, handler)
    }

    /// 该窗口本轮是否有 a11y 工作（O(1)；平台泵每轮用它过滤 `update_if_active`）。
    #[must_use]
    pub(crate) fn a11y_pending(&self, window: WindowId) -> bool {
        a11y::a11y_pending(self, window)
    }

    /// 建窗路径的 a11y 消费端代际句柄（三端唯一入口；RE-01）。
    ///
    /// 与窗口 `A11yBridge` 的 `consumer_epoch` 是**同一份** `Arc`：平台 adapter 的激活/停用
    /// 回调 bump 它，引擎才能观察到「消费端已重置」并把下一笔升成全树。`None` = 窗口不存在。
    #[must_use]
    pub(crate) fn window_a11y_epoch(
        &self,
        window: WindowId,
    ) -> Option<crate::a11y::adapter::A11yConsumerEpoch> {
        a11y::window_a11y_epoch(self, window)
    }

    /// 是否有窗口「a11y 脏但正被限流」——平台泵据此安排**到期补发**唤醒。
    ///
    /// 没有这条，被限流挡下的最后一次变化永远不会交付：脏位留着，但没有任何机制在
    /// `A11Y_THROTTLE` 之后把泵叫醒（复审 P1-4）。
    pub(crate) fn a11y_retry_pending(&self) -> bool {
        a11y::a11y_retry_pending(self)
    }

    /// 是否还有未交付的 a11y 批次（分批首树/DPI 全量重建未走完；不节流旁路，E5）。
    #[must_use]
    pub(crate) fn a11y_backlog_pending(&self) -> bool {
        a11y::a11y_backlog_pending(self)
    }

    /// 本轮泵交付过 a11y 且仍有未走完的批次时唤醒平台泵
    /// （三端唯一入口；E5/RE-04/RE-05）。
    pub(crate) fn wake_for_a11y_backlog(
        &mut self,
        dispatcher: &crate::platform::dispatcher::Dispatcher,
    ) {
        a11y::wake_for_a11y_backlog(self, dispatcher)
    }
    /// 产出一笔 a11y 交付（消费 `A11Y` 反向集合与 `full_pending`）。
    ///
    /// **必须在 `accesskit_windows::Adapter::update_if_active` 的闭包里调用**（adapter 不活跃时
    /// 闭包不执行 ⇒ 零开销），或由 `test-support` 的捕获路径调用。
    pub(crate) fn a11y_take(&mut self, window: WindowId) -> Option<crate::a11y::A11yTreeSnapshot> {
        a11y::a11y_take(self, window)
    }

    /// 只重算 `bind_a11y` 的 pass（R13-6）：不标 `BIND`/`PAINT`、不产生任何像素工作。
    pub(crate) fn refresh_a11y_bindings(&mut self, window: WindowId) {
        a11y::refresh_a11y_bindings(self, window)
    }

    /// 打开/关闭某个窗口的 a11y 快照捕获（`TestApp` 专用）。
    #[cfg(feature = "test-support")]
    pub(crate) fn set_a11y_capture(&mut self, window: WindowId, on: bool) {
        a11y::set_a11y_capture(self, window, on)
    }

    /// 该窗口的捕获是否打开。
    #[cfg(feature = "test-support")]
    pub(crate) fn attach_a11y(
        &mut self,
        window: WindowId,
        snapshot: crate::a11y::A11yTreeSnapshot,
    ) {
        a11y::attach_a11y(self, window, snapshot)
    }

    /// 轮询每个窗口的 GPU 线程反馈（三平台窗口路径；设备丢失 / 呈现失败在这里冒泡）。
    ///
    /// 平台泵**每轮**调用（空闲时也要跑）：错误是粘性的，早一轮读到就能早一轮退出。
    pub(crate) fn poll_renderers(&mut self) -> Result<(), EngineError> {
        resources::poll_renderers(self)
    }

    /// 完整帧：`refresh_styles` → `refresh_text` → `compute_layout`（仅 LAYOUT 脏）→ `layout_text`
    /// → paint → commit。
    ///
    /// 入口有「每 refresh 周期最多一帧」的预算门禁（A5）：同一刷新周期内的重复请求返回
    /// `Ok(FrameOutcome::Idle)`（脏位保留，平台一次性唤醒到期后补帧，最多晚 1 个 vsync）。
    /// 需要**立即**出帧的路径（resize / 拖动 / 系统模态循环 / 动画 tick）使用
    /// `App::frame_forced`（force 通道，绕过预算）。
    ///
    /// 返回 `None` = 窗口不存在（**不**消费 wake 请求）；`Ok(FrameOutcome::Idle)` = 空闲零帧（I3，
    /// 不提交、`FrameIndex` 不变）；`Err` = 本帧已中止（damage/dirty 保留，下一帧重试）。
    ///
    /// 产品路径：含 `bind_style`/`bind_text` 的窗口只能走这里；`Window::frame` 是它的无 App 通道版本。
    pub fn frame(&mut self, window: WindowId) -> Option<Result<FrameOutcome, EngineError>> {
        frame::frame(self, window, false)
    }

    /// 窗口所在 output 的刷新率提示（A5 诊断面；`None` = 平台没给 / headless）。
    #[must_use]
    pub fn frame_refresh_hz(&self, window: WindowId) -> Option<f32> {
        self.window(window).and_then(Window::frame_refresh_hz)
    }

    /// 该窗口累计被帧预算门禁推迟的请求数（A5 验收诊断；单调递增）。
    #[must_use]
    pub fn frame_budget_deferrals(&self, window: WindowId) -> u64 {
        self.window(window)
            .map_or(0, Window::frame_budget_deferrals)
    }

    /// 完整帧（force 通道）：绕过入口的帧预算门禁，立即出帧。
    ///
    /// 消费者（`crate` 内）：Windows `ResizeFrameDriver`（系统 size / move 模态循环里每条
    /// `WM_SIZE` 必须立刻重排重绘）、macOS 连续帧定时器（display link 节拍的 tick 已经代表
    /// 「这一拍该出帧」）。Linux 的 present 门禁本身就是 `wl_surface.frame`，没有 force 调用点
    /// （因此该端不编译本方法：避免 dead_code warning）。普通唤醒路径不得使用：否则
    /// `--hz 240` 的写入会绕过预算。
    #[cfg(any(windows, target_os = "macos"))]
    pub(crate) fn frame_forced(
        &mut self,
        window: WindowId,
    ) -> Option<Result<FrameOutcome, EngineError>> {
        frame::frame(self, window, true)
    }

    /// 中止本帧（不结算、不递增 `FrameIndex`）。
    fn abort_frame(&mut self, window: WindowId, ticket: FrameTicket) {
        frame::abort_frame(self, window, ticket)
    }

    /// 记录一次空闲帧（仅 `test-support`；其余构建是空操作）。
    fn record_idle(&mut self, window: WindowId) {
        frame::record_idle(self, window)
    }

    /// 文本 pass：形状（缓存）→ 光栅 → 图集 → `ShapedLine`，尺寸变化时置 `LAYOUT`。
    ///
    /// **必须在 `refresh_styles` 之后、`Window::compute_layout` 之前调用**
    /// （帧序：styles → text → layout）。`NodeBuilder::text` 不设 measure，因此没跑本 pass 的
    /// 文本节点 measure 为 `0×0`。
    ///
    /// 返回 `None` = 窗口不存在；`Some(Err(..))` = `TextShape` / `AtlasExhausted`（调用方丢弃本帧）。
    pub fn refresh_text(
        &mut self,
        window: WindowId,
    ) -> Option<Result<TextPassReport, EngineError>> {
        text_pass::refresh_text(self, window)
    }

    /// 文本的**第二阶段**：`compute_layout` 之后，按每个文本节点的**内容宽**重排、光栅、装箱。
    ///
    /// 帧序（B1）：`refresh_styles` → `refresh_text` → `Window::compute_layout` → `layout_text` → paint。
    /// 与 `refresh_text` 的分工：前者只 shape + 量固有尺寸（给 measure 用），后者拿 taffy 算出的
    /// **内容宽**做最终断行（`break_all_lines` + `align`）并产出 `ShapedLine`。
    ///
    /// 图集代际在一次 pass 内必须稳定（N3）：若中途换代（容量逃逸），整 pass **重跑一次**；
    /// 第二次仍换代则返回 `TextShape`（调用方丢弃本帧）。
    pub fn layout_text(
        &mut self,
        window: WindowId,
    ) -> Option<Result<TextLayoutReport, EngineError>> {
        let result = text_pass::layout_text(self, window);
        self.sync_atlas_generation_lease();
        result
    }

    /// 无节点归属的文本 shaping（v2.1 E1，`§11.1`）：canvas 内文本的公开入口。
    ///
    /// 按 `style` 与 `max_width` 塑形、断行并光栅/装箱，返回**可长期持有**的句柄。同一文本 +
    /// 同一 shaping 输入命中形状缓存（与 `refresh_text` 同一份 `ShapeKey`），不重复塑形；
    /// `max_width = None` = 不限宽（单行），`Some(w)` = 按 `w` 断行（等价
    /// `break_all_lines(Some(w))`）。
    ///
    /// `inline_boxes` 与节点路径同源（`TextContent.inline_boxes`）：落点经
    /// [`App::shaped_inline_boxes`] 读取；传空切片 = 无行内盒。E1 只保证落点可读，
    /// `InlineBoxPaint` 的绘制归 E2（`§11.2`）。
    ///
    /// 句柄带**图集租约** `(generation, eviction_epoch)`：图集换代**或**任一 tile 被 LRU 淘汰后
    /// [`ShapedTextHandle::is_stale`] 为真，[`crate::paint::canvas::Canvas::push_shaped_text`]
    /// 返回 `false` 且不编码旧 UV（D-5）；应用应丢弃句柄并重新调用本方法。
    ///
    /// 光栅 scale 取 [`App::text_scale`]（焦点窗口 DPI / 显式覆盖），句柄可用
    /// [`ShapedTextHandle::scale`] 读回。**应用义务**：窗口 DPI 变化 / 跨屏移动 / 焦点切换后
    /// 必须重新 shape（引擎不会自动重光栅已有句柄）。
    ///
    /// # Errors
    /// - [`EngineError::TextShapeStandalone`]：`max_width` 非有限 / 为负，或字体栈为空；
    /// - [`EngineError::AtlasExhausted`]：字形图集在扩容 + LRU 淘汰后仍放不下本次工作集。
    ///
    /// # Panics: none
    pub fn shape_text(
        &mut self,
        text: &str,
        style: &TextStyle,
        max_width: Option<f32>,
        inline_boxes: &[InlineBoxSpec],
    ) -> Result<ShapedTextHandle, EngineError> {
        let App {
            text: text_context,
            raster,
            atlas,
            glyph_atlas,
            pending_dirty,
            atlas_generation_lease,
            default_text_scale,
            windows,
            ..
        } = self;
        let scale = *default_text_scale;
        let result = crate::text::standalone::shape(
            text_context,
            raster,
            atlas,
            glyph_atlas,
            pending_dirty,
            atlas_generation_lease,
            text,
            style,
            max_width,
            inline_boxes,
            scale,
        );
        // 本次 shape 可能触发扩容 / 满页清空 / 逐 tile 淘汰：进程级图集的状态变化会让**所有
        // 窗口**已发布的 `ShapedLine` 与 Canvas 图元烘焙旧 UV（D-5）。收尾必须**无条件**执行
        // —— 失败路径（`AtlasExhausted` 等）同样可能已经淘汰了节点文本的 tile、前进了租约，
        // 用 `?` 早退会让节点文本继续画被复用的旧 UV、并让旧句柄短暂「假活」。
        let evicted = glyph_atlas.take_evicted();
        if evicted > 0 {
            for window in windows.values_mut() {
                window.tree_mut().invalidate_text_lines();
                window.tree_mut().mark_canvas_nodes_paint();
            }
        }
        // 租约同步同样无条件执行：失败路径上发生的换代 / 淘汰必须立即对已发出的句柄可见。
        self.sync_atlas_generation_lease();
        result
    }

    /// 行内盒落点（v2.1 E1，`§11.1`）：顺序与 `shape_text` 传入的 `inline_boxes` 一一对应，
    /// `id` 原样保留；供应用画 `<img>` / `<kbd>`。
    ///
    /// 坐标是**文本盒局部**逻辑像素（与 `handle.line()` 的字形位置同域）：应用画图时加上文本盒
    /// 在窗口中的绝对原点。空切片 = 无行内盒。
    ///
    /// # Panics: none
    #[must_use]
    pub fn shaped_inline_boxes<'a>(&self, handle: &'a ShapedTextHandle) -> &'a [ShapedInlineBox] {
        handle.inline_boxes()
    }

    /// 让代际租约与图集对齐：任何可能改变图集状态的 App 入口返回后调用（`shaped_text` 句柄的
    /// `is_stale` 依赖它）。原语是 `(atlas.generation(), glyph_atlas.eviction_epoch())`，这里只是
    /// 把同一真源打包写进共享槽。
    fn sync_atlas_generation_lease(&self) {
        self.atlas_generation_lease.store(
            crate::text::pack_atlas_lease(
                self.atlas.generation(),
                self.glyph_atlas.eviction_epoch(),
            ),
            Ordering::Relaxed,
        );
    }

    /// canvas 文本的当前**默认光栅 scale**（v2.1 E1b，`§11.1`）。
    ///
    /// 语义：默认 1.0；焦点窗口获得焦点 / 其 DPI 变化时跟随该窗口的 `Window::scale()`（DPI
    /// 变化用 `Event::DpiChanged.scale` 这个权威值，三端事件顺序不同但结果一致）；没有
    /// 焦点窗口时保留上一次的值；[`App::set_text_scale`] 可显式覆盖（离屏 / 测试 / 固定 DPI）。
    ///
    /// [`App::shape_text`] 用它光栅，句柄用 [`ShapedTextHandle::scale`] 记录。
    ///
    /// # Panics: none
    #[must_use]
    pub fn text_scale(&self) -> f32 {
        self.default_text_scale
    }

    /// 显式覆盖 canvas 文本的光栅 scale（`§11.1`，v2.1 E1b）。
    ///
    /// 覆盖一旦设置就**不再跟随**焦点窗口 DPI（多窗口不同 DPI 下应用应显式 `shape_text` 时
    /// 用哪个 scale）；调用 [`App::clear_text_scale_override`] 恢复跟随。已有的
    /// [`ShapedTextHandle`] 不会自动重新光栅 —— 应用须丢弃并重新 `shape_text`。
    ///
    /// # Errors
    /// [`EngineError::InvalidTextScale`]：`scale` 非有限值或 `<= 0`（光栅 ppem = 字号 × scale，
    /// 非法值会让 parley/swash 得到 NaN 尺寸；不静默钳制）。
    ///
    /// # Panics: none
    pub fn set_text_scale(&mut self, scale: f32) -> Result<(), EngineError> {
        if !scale.is_finite() || scale <= 0.0 {
            return Err(EngineError::InvalidTextScale { scale });
        }
        self.text_scale_override = Some(scale);
        self.default_text_scale = scale;
        Ok(())
    }

    /// 取消 [`App::set_text_scale`] 的显式覆盖，恢复「跟随焦点窗口 DPI」。
    ///
    /// 当前有焦点窗口 ⇒ 立即采用它的 DPI；没有焦点窗口 ⇒ 保留当前值（与「无窗口时保持上次值」
    /// 同口径）。已有句柄不会自动重新光栅（应用须重新 `shape_text`）。
    ///
    /// # Panics: none
    pub fn clear_text_scale_override(&mut self) {
        self.text_scale_override = None;
        self.follow_focused_window_scale();
    }

    /// 事件：窗口焦点变化（`Event::FocusChanged` 的唯一处理点，v2.1 E1b）。
    ///
    /// 获得焦点 ⇒ 记录焦点窗口并跟随其 DPI 作为 canvas 文本默认 scale；失去焦点 ⇒ 只清焦点
    /// 窗口记录、**保留**当前 scale（「无窗口时保持上次值」）。
    pub(crate) fn note_window_focus(&mut self, window: WindowId, focused: bool) {
        if focused {
            self.focused_window = Some(window);
            self.follow_focused_window_scale();
        } else if self.focused_window == Some(window) {
            self.focused_window = None;
        }
    }

    /// 事件：窗口 DPI 变化。只有焦点窗口的 DPI 变化才更新 canvas 文本默认 scale；显式覆盖期间
    /// 不跟随。
    ///
    /// 用**事件自带的** `scale`（`Event::DpiChanged.scale` 是本次变化的权威值），不读
    /// `Window::scale`：Wayland 的事件循环在派发事件**之后**才镜像 `Window::set_scale`
    /// （`platform/linux/event_loop.rs` 的「① 事件 → ② 平台真值镜像」顺序），读窗口会落后一次
    /// DPI。Windows / macOS 先写 `Window::scale` 再派发，两种顺序下结果一致。
    pub(crate) fn note_window_dpi(&mut self, window: WindowId, scale: Scale) {
        if self.focused_window == Some(window) {
            self.apply_text_scale(scale.factor());
        }
    }

    /// 让 `default_text_scale` 采用 `scale`（显式覆盖期间是 no-op）。
    fn apply_text_scale(&mut self, scale: f32) {
        if self.text_scale_override.is_some() {
            return;
        }
        self.default_text_scale = scale;
    }

    /// 让 `default_text_scale` 跟随焦点窗口的当前 DPI（显式覆盖期间是 no-op）。
    fn follow_focused_window_scale(&mut self) {
        let Some(window) = self.focused_window else {
            return;
        };
        let scale = self
            .windows
            .get(&window)
            .map(|target| target.scale().factor());
        if let Some(scale) = scale {
            self.apply_text_scale(scale);
        }
    }

    /// 仅测试（E1b F1/F3 回归）：把字形图集页档钉到 `page` 并同步清空字形缓存。
    ///
    /// `cap_page_for_test` 本身会换代（`clear` ⇒ `generation + 1`），这里同步把两级缓存的记录
    /// 清掉、租约前进 —— 之后的小页分配即可在几十个 tile 内触发逐 tile LRU 淘汰。
    #[cfg(test)]
    pub(crate) fn cap_glyph_atlas_page_for_test(&mut self, page: u32) {
        self.atlas.cap_page_for_test(page);
        self.glyph_atlas.clear();
        self.sync_atlas_generation_lease();
    }

    /// 仅测试：当前字形图集代际。
    #[cfg(test)]
    pub(crate) fn glyph_atlas_generation_for_test(&self) -> u32 {
        self.atlas.generation()
    }

    /// 仅测试：当前累计淘汰纪元。
    #[cfg(test)]
    pub(crate) fn glyph_atlas_eviction_epoch_for_test(&self) -> u32 {
        self.glyph_atlas.eviction_epoch()
    }

    /// 当前字体栈。
    #[must_use]
    pub fn font_stack(&self) -> &FontStack {
        text_pass::font_stack(self)
    }

    /// 当前字体栈版本（形状缓存键的一部分）。
    #[must_use]
    pub fn font_stack_version(&self) -> u32 {
        text_pass::font_stack_version(self)
    }

    /// 文本诊断（tofu / 行内盒边界 / 超宽 / 合成样式）。
    #[must_use]
    pub fn text_diagnostics(&self) -> &[TextDiagnostic] {
        text_pass::text_diagnostics(self)
    }

    /// 字体栈的健康状态（N4：测试与诊断的**唯一**跳过判据）。
    pub fn font_stack_status(&mut self) -> FontStackStatus {
        text_pass::font_stack_status(self)
    }

    /// 换字体栈：版本 +1、字体代际 +1、清形状缓存与字形图集、所有窗口的文本节点
    /// `LAYOUT | PAINT`。返回新版本号。
    pub fn set_font_stack(&mut self, stack: FontStack) -> u32 {
        let version = text_pass::set_font_stack(self, stack);
        self.sync_atlas_generation_lease();
        version
    }

    /// 系统字体安装/卸载/语言切换后调用：重载 + 与 [`App::set_font_stack`] 相同的失效。
    pub fn reload_system_fonts(&mut self) -> u32 {
        let version = text_pass::reload_system_fonts(self);
        self.sync_atlas_generation_lease();
        version
    }

    /// 数字快速路径开关（`DigitFastPath`，R12 的性能项）。
    ///
    /// 默认开启：文本全为 `[0-9 . , + - % / :]`、字体栈解析出唯一字体、无 letter-spacing /
    /// OpenType 特性时，跳过 parley 的 itemize+shaping，直接「字符 → glyph id → advance」。
    /// 每个 `(字体, 字号, 字重, 倾斜)` 组合首次使用时做一次**验证性塑形**（逐字符 + 全部有序
    /// 字符对，容差 0.01 px），不成立就永久降级到 parley。关闭它可做 A/B 对照（会清形状缓存）。
    pub fn set_digit_fast_path(&mut self, enabled: bool) {
        text_pass::set_digit_fast_path(self, enabled);
    }

    /// 数字快速路径是否开启（回退验证 / 诊断）。
    #[must_use]
    pub fn digit_fast_path_enabled(&self) -> bool {
        text_pass::digit_fast_path_enabled(self)
    }

    /// 所有窗口的文本节点标 `LAYOUT | PAINT` 并清 `node..root` 缓存。
    /// 图集代际变化时让**所有窗口**的文本节点重绘（P1-2.6）。
    ///
    /// 图集换代（DPI 变化 / 字形扩容逃逸）会让页内所有 tile 重排，而每个文本 lane 里存的是
    /// 生成时的 `AtlasTile`（旧 UV）。`set_text_line` 不标 `PAINT`，paint 的 lane 复用只看
    /// `REUSABLE`/`clip_key`，于是旧 UV 被继续使用 —— 表现为错字/花字，且要等一次无关的
    /// `PAINT` 才可能恢复。
    ///
    /// 只标 `PAINT` 不标 `LAYOUT`：换代不改变字形宽度与断行，重排是纯损失。
    ///
    /// 对照 Skia：glyph atlas / strike cache 换代时会让所有引用旧 strike 的绘制数据失效。
    fn repaint_text_if_atlas_changed(&mut self) {
        text_pass::repaint_text_if_atlas_changed(self);
        self.sync_atlas_generation_lease();
    }

    fn invalidate_all_text(&mut self) {
        text_pass::invalidate_all_text(self)
    }

    /// 沿依赖图失效：节点标脏（或经派生级联），有节点变脏则置唤醒位。
    fn invalidate(&mut self, signal: SignalId) {
        style_pass::invalidate(self, signal)
    }

    /// 仅测试：求值栈是否为空（验证 `EvalGuard` 在 panic 后仍然出栈）。
    #[cfg(test)]
    pub(crate) fn eval_stack_is_empty_for_test(&self) -> bool {
        signal_api::eval_stack_is_empty_for_test(self)
    }
}

#[cfg(test)]
mod tests;

/// P10 交互机制的 App 级用例（分发 / 焦点 / 滚动 / 虚拟列表 / 浮层 / 拖拽 / 动画）。
///
/// 只走公开 + `pub(crate)` 面；`FrameRecord` 的断言（damage、primitives→node）在
/// `test_support::app` 的 `--features test-support` 用例里。
#[cfg(test)]
mod interaction_tests;

/// P13：`ResourceCache` 接入 `App` 的边界用例（图片页上传 / 进程级缓存 / 零副作用）。
#[cfg(test)]
mod resources_tests;

/// R01-16：按节点 / 按窗口请求重绘的失效范围（子树 + 祖先链校验、不放大到整窗）。
#[cfg(test)]
mod invalidate_tests;

/// E7：`NodeKind::Image` 的布局 / 绘制 / 资源生命周期 / DPI / a11y 集成用例。
///
/// 断言面依赖 `test_support`（`FrameRecord::primitives` 的图片 bounds / 调制 / UV 与 a11y
/// 快照），因此只在 `--features test-support` 的 lib 测试里编译。
#[cfg(all(test, feature = "test-support"))]
mod image_tests;
/// E8：焦点作用域化键绑定（`App::bind_key_scoped`）与作用域瞬态值
/// （`App::transient_scoped`）的引擎级用例。
#[cfg(test)]
mod scoped_tests;

/// E10：动画完成回调 + 一次性定时器的引擎级用例。
///
/// 断言面走 `test_support`（`TestApp` 的帧记录 / 探针），并且用「钩子 → 帧」的真实泵序推进
/// （`App::run_frame_hook` 在 `TestApp::advance` 之后手动调用）—— 这正是 P2-4b 审查 R01/R02
/// 缺失的时序面。
#[cfg(all(test, feature = "test-support"))]
mod e10_tests;

/// E17：公开指针位置（`Window::pointer_position` / `App::pointer_position`）的引擎级用例。
///
/// 断言面走 `test_support`（`TestApp` 的指针注入 + 帧记录），与平台泵同一条
/// `App::dispatch_input` 路径。
#[cfg(all(test, feature = "test-support"))]
mod e17_tests;

/// E16：节点子树卸载的自动清理（R-21 引擎下沉）—— 清理计数 / 语义边界 / 1000 轮 churn /
/// 关窗路径不变 / I3。断言面用到 `test-support` 的观测入口（`timer_count_for_test` /
/// `has_action_handler` / `scoped_key_binding_count`），因此只在特性开启时编译。
#[cfg(all(test, feature = "test-support"))]
mod removal_tests;
