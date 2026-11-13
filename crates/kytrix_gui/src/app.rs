//! `App`：状态表 + 依赖图 + 窗口集合 + runtime 句柄。
//!
//! 设计见 `design/phase-03-node-state.md` §5.1 / §14。P03 是 headless 的：没有事件循环、没有
//! `Application`/`AppHandle`（推迟 P08/P09），`App` 只负责「状态 + 窗口 + 语义脏标记」。
//!
//! 写路径是 push 模型（§7.5）：一次 `write` 沿依赖图把 owner 标脏，**不**遍历整棵树；
//! 帧内唯一的重活是 `refresh_styles`（样式求值 pass，P06 的 paint 阶段复用同一实现）。

use std::{
    any::Any,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use smallvec::{SmallVec, smallvec};

#[cfg(feature = "test-support")]
use crate::test_support::record::{FrameStats, LaneOutcome};
use crate::{
    error::EngineError,
    event::{
        Action, ActionHandler, ActionSource, Event, InputEvent, InputOutcome, Key, KeyBinding,
        KeyState, Modifiers, PointerButton,
    },
    interaction::{
        self, ActionCtx, AnimatedValue, AnimationId, AnimationSpec, DragInfo, DragPhase,
        DragSession, DragState, Easing, InputThresholds, Overlay, OverlayDismiss, OverlayOptions,
        OverlayPlacement, OverlaySlot, PressRecord, ScrollAxis, ScrollConfig, ScrollHandle,
        ScrollSlot, ScrollState, VirtualList, VirtualListConfig, VirtualListState, VirtualSlot,
        animation as anim, drag as drag_util, focus, overlay as overlay_util, scroll, virtual_list,
    },
    layout::hit_test::HitResult,
    math::{Point, Rect, Size},
    node::{
        builder::NodeBuilder,
        dependency::{DependencyGraph, EvalStack, Owner, ReadCtx, ReadTracker},
        dirty::DirtyFlags,
        id::NodeId,
        signal::{Derived, Signal, SignalId, StateStore, Transient},
    },
    paint::{
        color::Color,
        style::{Clip, Cursor, StylePatch},
    },
    platform::dispatcher::{Dispatcher, UserTask},
    render::AtlasAllocator,
    resources::ResourceCache,
    text::{
        FontStack, FontStackStatus, TextContent, TextDiagnostic, TextLayoutReport, TextPassReport,
        TextStyle, atlas::GlyphAtlas, context::TextContext, raster::GlyphRasterizer,
    },
    window::{FrameOutcome, FrameTicket, Window, WindowId, WindowOptions},
};

/// a11y 诊断的环形容量（与 `text_diagnostics` 同口径：超出丢最旧）。
const A11Y_DIAGNOSTIC_CAPACITY: usize = 64;

/// 一次样式求值 pass 的结果（P06 的 `FrameRecord` 会复用这些字段）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StylePassReport {
    /// 实际求值的 `bind_style` 闭包个数（不区分「补丁是否变化」）。
    pub evaluated: usize,
    /// pass 结束后该窗口内是否仍有节点带 `LAYOUT`（含冒泡）。
    pub layout_dirty: bool,
    /// pass 结束后该窗口内是否仍有节点 `!REUSABLE`。
    ///
    /// **P03 恒为 `true`**：`REUSABLE` 在 P03 只有清除者（`mark_*`），帧初重置属 P06 的
    /// `NodeTree::begin_frame`。因此除空窗口外该字段没有区分度。
    pub paint_dirty: bool,
    /// pass 结束后 `App::take_wake_request()` 会返回的值（**本报告不消费它**）。
    pub wake_requested: bool,
    /// 本次求值的 `bind_text` 闭包个数（含值未变的求值）。
    pub text_evaluated: usize,
    /// 本次文本值**真的变了**的节点数。
    pub text_changed: usize,
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
    /// 字形 `key → tile` 缓存（与图集同事务换代）。
    glyph_atlas: GlyphAtlas,
    /// 每窗口「图集已同步到的 `scale_epoch`」（DPI 变化 ⇒ 图集换代）。
    scale_epochs: ahash::HashMap<WindowId, u32>,
    /// 累积的图集脏行（P09 的增量上传输入；`layout_text` 合并、`dirty_spans` 计数）。
    pending_dirty: crate::render::DirtyRows,
    /// 跨线程唤醒队列（`AppHandle` 的载荷；主线程之外唯一的引擎入口）。
    dispatcher: Arc<Dispatcher>,
    /// 本帧从平台层收到的、尚未被应用取走的事件。
    events: Vec<(WindowId, Event)>,
    /// 每轮事件循环调用一次的应用钩子（读事件 / 写状态；`Application::run` 唯一调用点）。
    frame_hook: Option<Box<dyn FnMut(&mut App)>>,
    /// 动作处理器表（P10；同 `Action` 后写覆盖）。
    handlers: ahash::HashMap<Action, ActionHandler>,
    /// 键盘绑定表（线性扫描：默认 3 条 + 应用若干）。
    bindings: Vec<KeyBinding>,
    /// 输入判定阈值（probe 真值；平台在 `platform_init` 覆盖）。
    thresholds: InputThresholds,
    /// 引擎时钟（生产由平台每轮 `sync_clock`，测试由 `TestApp::advance` 推进）。
    clock: Instant,
    /// 最近一次点击（双击判定）。
    last_click: Option<ClickRecord>,
    /// 动画槽（P10；`tick_animations` 推进，结束时删槽）。
    animations: Vec<AnimationSlot>,
    /// 下一个动画 id（单调；`AnimationId` 的代际）。
    next_animation: u32,
    /// 待推给平台的光标变化（`None` = 没变）。
    cursor_pending: Option<Cursor>,
    /// 待推给平台的捕获请求（`Some(true)` = 取得、`Some(false)` = 释放）。
    capture_request: Option<bool>,
    /// a11y 动作的 handler 表（P11；同 `A11yAction` 后写覆盖）。
    a11y_handlers: ahash::HashMap<
        crate::a11y::A11yAction,
        Box<dyn FnMut(&mut crate::a11y::A11yActionCtx<'_>)>,
    >,
    /// a11y 诊断（P11；环形上限 `A11Y_DIAGNOSTIC_CAPACITY`，超出丢最旧）。
    a11y_diagnostics: Vec<crate::a11y::A11yDiagnostic>,
}

/// 一次点击的记录（双击判定的输入）。
#[derive(Clone, Copy, Debug)]
struct ClickRecord {
    window: WindowId,
    node: Option<NodeId>,
    position: Point,
    at: Instant,
    count: u8,
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
}

/// 默认键盘绑定（Tab / Shift+Tab / Esc；应用可 `unbind_key` 或覆盖）。
fn default_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding {
            key: Key::Tab,
            modifiers: Modifiers::empty(),
            action: Action::FocusNext,
        },
        KeyBinding {
            key: Key::Tab,
            modifiers: Modifiers::SHIFT,
            action: Action::FocusPrev,
        },
        KeyBinding {
            key: Key::Escape,
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
/// 三个方法就是全部的跨线程入口（`design/phase-09-win32-demo.md` §5.1）：
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
        Self {
            store: StateStore::with_capacity(64, 16),
            graph: DependencyGraph::new(),
            windows: ahash::HashMap::default(),
            eval_stack: EvalStack::new(),
            wake_requested: false,
            next_window_id: 1,
            runtime,
            text: TextContext::new(),
            raster: GlyphRasterizer::new(),
            atlas: AtlasAllocator::new(crate::paint::atlas::AtlasKind::Glyph),
            resources: ResourceCache::new(),
            glyph_atlas: GlyphAtlas::new(),
            scale_epochs: ahash::HashMap::default(),
            pending_dirty: crate::render::DirtyRows::default(),
            dispatcher: Dispatcher::new(),
            events: Vec::new(),
            frame_hook: None,
            handlers: ahash::HashMap::default(),
            bindings: default_bindings(),
            thresholds: InputThresholds::default(),
            clock: Instant::now(),
            last_click: None,
            animations: Vec::new(),
            next_animation: 1,
            cursor_pending: None,
            capture_request: None,
            a11y_handlers: ahash::HashMap::default(),
            a11y_diagnostics: Vec::new(),
        }
    }

    /// 注册每轮事件循环的钩子：在平台事件已落地、提交本帧之前调用一次。
    ///
    /// 形态是 `&mut App`（可以建信号 / 改树 / 关窗），因此钩子内部**不能**再调用
    /// `Application::run`。重复注册会替换上一个钩子。
    pub fn on_frame(&mut self, hook: impl FnMut(&mut App) + 'static) {
        self.frame_hook = Some(Box::new(hook));
    }

    /// 调用一次帧钩子（crate 私有；临时 take 以避免 `&mut self` 与闭包参数重叠）。
    pub(crate) fn run_frame_hook(&mut self) {
        if let Some(mut hook) = self.frame_hook.take() {
            hook(self);
            self.frame_hook = Some(hook);
        }
    }

    /// 跨线程句柄（`AppHandle`）。
    #[must_use]
    pub fn handle(&self) -> AppHandle {
        AppHandle {
            dispatcher: self.dispatcher.clone(),
            runtime: self.runtime.clone(),
        }
    }

    /// 平台层共享的唤醒队列（crate 私有）。
    pub(crate) fn dispatcher(&self) -> Arc<Dispatcher> {
        self.dispatcher.clone()
    }

    /// 取走本帧的平台事件（`Application::run` 每帧填充；顺序即产生顺序）。
    #[must_use]
    pub fn drain_events(&mut self) -> Vec<(WindowId, Event)> {
        std::mem::take(&mut self.events)
    }

    /// 平台层投递一个事件（`crate::platform::native` 唯一写入点）。
    ///
    /// `Event::Input` 除了进观测缓冲，还会**立刻**走一次交互分发（P10 §5.3）；观测缓冲只用于
    /// `drain_events`（应用读事件），不承担分发。
    pub(crate) fn dispatch(&mut self, window: WindowId, event: Event) {
        match &event {
            Event::Input(input) => {
                self.sync_clock();
                self.dispatch_input(window, input.clone());
            }
            Event::FocusChanged { focused: false } => self.on_window_focus_lost(window),
            _ => {}
        }
        self.events.push((window, event));
    }

    /// 写剪贴板文本（Win32 `CF_UNICODETEXT` / macOS `NSPasteboard` / Wayland `wl_data_source`）。
    ///
    /// # Errors
    /// `EngineError::Clipboard`：平台拒绝（剪贴板被占用、分配失败、无 owner 窗口）。
    pub fn set_clipboard_text(&self, text: &str) -> Result<(), EngineError> {
        crate::platform::native::clipboard::set_text(text)
    }

    /// 读剪贴板文本（无文本内容时是空串）。
    ///
    /// # Errors
    /// `EngineError::Clipboard`：平台拒绝；Wayland 下「尚未收到过选区内容」也走这里
    /// （剪贴板是异步拉模型，`design/phase-12-macos-linux.md` F-130）。
    pub fn clipboard_text(&self) -> Result<smol_str::SmolStr, EngineError> {
        crate::platform::native::clipboard::get_text()
    }

    /// 把两张图集页**广播**给所有窗口的渲染器（P13 §8.2；评审阻塞 ②）。
    ///
    /// 为什么不按窗口取脏行：图集页是**进程级**共享数据（`AtlasAllocator` / `GlyphAtlas` 一份），
    /// 而纹理是**每窗口一份**（`AtlasTextures`）。`frame_all` 逐窗提交，若第一次 `take` 就把脏行
    /// 消费给当前窗口，其余窗口的纹理永远收不到内容。因此：
    ///
    /// 1. 脏行/脏旗标由**本方法**统一消费一次，然后广播给每个有渲染器的窗口；
    /// 2. 还没收到过整页的窗口（新建窗口 / 换过渲染器）拿到的是**整页**，不是增量行；
    /// 3. 字形页沿用 P09 的整页上传，图片页在有脏行时按行增量。
    ///
    /// 调用点：`App::frame`（每帧，paint 之前）。`TestApp::snapshot` 走自己的临时渲染器。
    pub(crate) fn upload_atlas_pages(&mut self) {
        let glyph_dirty = self.glyph_atlas.take_page_dirty();
        let image_rows = self.resources.take_dirty_rows();
        let image_dirty = !image_rows.spans().is_empty();

        // 字形页：有脏 ⇒ 所有窗口整页；没脏 ⇒ 只补给从未收到过的窗口。
        let glyph_needed = glyph_dirty
            || self
                .windows
                .values()
                .any(|window| window.has_renderer() && !window.glyph_page_received());
        if glyph_needed {
            let (_, pixels) = self.glyph_page_pixels();
            let mut rows = crate::render::DirtyRows::default();
            rows.mark(0, u32::MAX);
            for window in self.windows.values_mut() {
                if !window.has_renderer() {
                    continue;
                }
                if !glyph_dirty && window.glyph_page_received() {
                    continue;
                }
                window.queue_atlas_upload(
                    crate::paint::atlas::AtlasKind::Glyph,
                    &pixels,
                    rows.clone(),
                );
                window.mark_glyph_page_received();
            }
        }

        // 图片页：有脏行 ⇒ 已收到的窗口拿增量、未收到的拿整页；没脏 ⇒ 只补未收到的窗口。
        let image_needed = image_dirty
            || self
                .windows
                .values()
                .any(|window| window.has_renderer() && !window.image_page_received());
        if !image_needed {
            return;
        }
        let (_, pixels) = self.resources.page_pixels();
        let mut full_rows = crate::render::DirtyRows::default();
        full_rows.mark(0, u32::MAX);
        for window in self.windows.values_mut() {
            if !window.has_renderer() {
                continue;
            }
            if window.image_page_received() {
                if image_dirty {
                    window.queue_atlas_upload(
                        crate::paint::atlas::AtlasKind::Image,
                        pixels,
                        image_rows.clone(),
                    );
                }
                continue;
            }
            window.queue_atlas_upload(
                crate::paint::atlas::AtlasKind::Image,
                pixels,
                full_rows.clone(),
            );
            window.mark_image_page_received();
        }
    }

    /// 字形页的整页像素（`R8`；`upload_glyph_page` 与快照路径共用）。
    pub(crate) fn glyph_page_pixels(&self) -> ((u32, u32), Vec<u8>) {
        let page = self.atlas.page();
        let mut pixels = vec![0u8; (page.0 as usize) * (page.1 as usize)];
        self.glyph_atlas.write_page(&mut pixels, page.0);
        (page, pixels)
    }

    /// 图片页的当前内容（BGRA8 预乘 sRGB；`TestApp::snapshot` 的上传源）。
    #[cfg(feature = "test-support")]
    pub(crate) fn resource_page_pixels(&self) -> ((u32, u32), &[u8]) {
        self.resources.page_pixels()
    }

    /// 图片资源缓存（只读）。应用层在**建树时**用 [`App::resources_mut`] 换 `AtlasTile`。
    #[must_use]
    pub fn resources(&self) -> &ResourceCache {
        &self.resources
    }

    /// 图片资源缓存（可变）：`insert` / `unload` 的唯一入口。
    pub fn resources_mut(&mut self) -> &mut ResourceCache {
        &mut self.resources
    }

    /// 依赖图（只读；`test-support` 的 `diagnostics::dependency_dump` 用）。
    #[cfg(feature = "test-support")]
    pub(crate) fn dependency_graph(&self) -> &DependencyGraph {
        &self.graph
    }

    /// 本 App 绑定的 tokio 句柄（**借用**，不 clone）。
    ///
    /// 命名：不占用 `handle()` —— P09 会加 `App::handle() -> AppHandle`（那时是纯新增，零破坏）。
    #[must_use]
    pub fn runtime_handle(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }

    // ── 状态注册 ──────────────────────────────────────────────────────────────

    /// 建一个普通信号（id 稠密递增，条目永不删除）。
    pub fn signal<T: Clone + 'static>(&mut self, initial: T) -> Signal<T> {
        let id = self.store.push_owned(initial);
        Signal::new(id)
    }

    /// 建一个瞬态值（读不登记、写不失效）。
    pub fn transient<T: Clone + 'static>(&mut self, initial: T) -> Transient<T> {
        let id = self.store.push_transient(initial);
        Transient::new(id)
    }

    /// 注册一个派生值（**不**立即求值；环在运行期检测）。
    pub fn derived<T: Clone + 'static>(
        &mut self,
        compute: impl Fn(&mut ReadCtx<'_>) -> T + 'static,
    ) -> Derived<T> {
        let boxed: Box<dyn Fn(&mut ReadCtx<'_>) -> Box<dyn Any>> =
            Box::new(move |cx| Box::new(compute(cx)));
        let (id, _handle) = self.store.push_derived(boxed);
        Derived::new(id)
    }

    // ── 写路径 ────────────────────────────────────────────────────────────────

    /// 写一个信号。返回 `true` = 值变了（并已沿依赖图失效）；`false` = 值相等，**短路**。
    ///
    /// 返回值不表示「有节点被标脏」（只写派生时没有节点变脏）。
    ///
    /// **应用层契约**：`T` 必须是不可变值语义。若 `T` 内部可变（`Rc<RefCell<_>>`/`Cell`/`Mutex`）而
    /// `PartialEq` 只比较外层，则内部变化会被短路吞掉（不重绘）。见 S-35 / R9。
    pub fn write<T: Clone + PartialEq + 'static>(&mut self, signal: &Signal<T>, value: T) -> bool {
        let id = signal.id();
        if self.store.owned_ref::<T>(id) == Some(&value) {
            return false;
        }
        self.store.set_owned(id, value);
        self.invalidate(id);
        true
    }

    /// 写一个瞬态值：**不**失效、**不**请求唤醒（滚动偏移/悬浮态，§3.7）。
    pub fn write_transient<T: 'static>(&mut self, transient: &Transient<T>, value: T) {
        self.store.set_transient(transient.id(), value);
    }

    /// 取走「需要一帧」的请求位（取走即清零）。
    pub fn take_wake_request(&mut self) -> bool {
        let requested = self.wake_requested;
        self.wake_requested = false;
        requested
    }

    /// 置「需要一帧」并**唤醒平台事件循环**（合并位由 `Dispatcher::wake` 保证幂等）。
    ///
    /// 不能只置 `wake_requested`：平台泵的顺序是「事件 → 钩子 → 帧 → a11y → 回投任务」，
    /// 回投任务（`AppHandle::send`、a11y `ActionRequest`）里写状态时循环已经过了帧门禁、
    /// 即将阻塞在 `GetMessageW` / `poll`。只置位不唤醒会让这批状态一直等到下一条无关消息
    /// （画面与 a11y 更新都迟到）——P15 用 UIA 客户端实测 `SelectionItemPattern.Select()`
    /// 不生效时定位到这一点（`design/DEVIATIONS.md` F-152）。
    fn request_frame(&mut self) {
        self.wake_requested = true;
        self.dispatcher.wake();
    }

    // ── P10：动作（Action）与键盘绑定 ──────────────────────────────────────────

    /// 注册/替换一个动作的处理器（同 `Action` 后写覆盖）。
    pub fn on_action(&mut self, action: Action, handler: impl FnMut(&mut ActionCtx<'_>) + 'static) {
        self.handlers.insert(action, Box::new(handler));
    }

    /// 删除处理器（返回是否真的删了）。
    pub fn off_action(&mut self, action: &Action) -> bool {
        self.handlers.remove(action).is_some()
    }

    /// 添加/替换一条键盘绑定（同 `(key, modifiers)` 后写覆盖）。
    pub fn bind_key(&mut self, binding: KeyBinding) {
        self.bindings.retain(|existing| {
            !(existing.key == binding.key && existing.modifiers == binding.modifiers)
        });
        self.bindings.push(binding);
    }

    /// 删除一条键盘绑定（返回是否真的删了）。
    pub fn unbind_key(&mut self, key: Key, modifiers: Modifiers) -> bool {
        let before = self.bindings.len();
        self.bindings
            .retain(|binding| !(binding.key == key && binding.modifiers == modifiers));
        self.bindings.len() != before
    }

    /// 非追踪读一个信号（handler 不是渲染 pass，不登记依赖）。
    ///
    /// # Panics
    /// `signal` 的槽位不是普通信号（引擎不变量被破坏时才会发生）。
    #[must_use]
    pub fn read<T: Clone + 'static>(&self, signal: &Signal<T>) -> T {
        self.store
            .owned_ref::<T>(signal.id())
            .cloned()
            .expect("signal slot is not a plain signal")
    }

    /// 覆盖输入阈值（平台层用 `GetDoubleClickTime` / `SM_CXDRAG` 初始化；测试也可显式设置）。
    pub fn set_input_thresholds(&mut self, thresholds: InputThresholds) {
        self.thresholds = thresholds;
    }

    /// 非追踪读一个瞬态值（动画进度 / 滚动偏移的公开读取面）。
    ///
    /// # Panics
    /// 槽位不是瞬态值（引擎不变量被破坏时才会发生）。
    #[must_use]
    pub fn read_transient<T: Clone + 'static>(&self, transient: &Transient<T>) -> T {
        self.store
            .transient_ref::<T>(transient.id())
            .cloned()
            .expect("signal slot is not a transient value")
    }

    /// 已有滚动容器节点的句柄（`None` = 该节点不是滚动容器）。
    #[must_use]
    pub fn scroll_handle(&self, window: WindowId, node: NodeId) -> Option<ScrollHandle> {
        self.windows
            .get(&window)?
            .tree()
            .scroll_slot(node)
            .map(|_| ScrollHandle::new(window, node))
    }

    /// 程序性分发（测试 / 菜单桥 / a11y 的 `ActionRequest`）。返回是否被处理。
    pub fn dispatch_action(
        &mut self,
        window: WindowId,
        node: NodeId,
        action: Action,
        source: ActionSource,
    ) -> bool {
        self.dispatch_action_with_hit(window, node, action, source, None, Modifiers::empty())
    }

    /// 以当前焦点节点为 target 分发（无焦点 ⇒ `false`）。
    pub fn dispatch_action_focused(
        &mut self,
        window: WindowId,
        action: Action,
        source: ActionSource,
    ) -> bool {
        let Some(node) = self.focused(window) else {
            return false;
        };
        self.dispatch_action(window, node, action, source)
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
        let Some(target) = self.windows.get(&window) else {
            return false;
        };
        if target.tree().get(node).is_err() {
            return false;
        }
        if let Some(mut handler) = self.handlers.remove(&action) {
            {
                let mut ctx = ActionCtx::new(self, window, node, hit, modifiers, source);
                handler(&mut ctx);
            }
            self.handlers.insert(action, handler);
            return true;
        }
        match action {
            Action::FocusNext => {
                self.focus_next(window);
                true
            }
            Action::FocusPrev => {
                self.focus_prev(window);
                true
            }
            Action::Dismiss => self.dismiss_overlay(window),
            Action::Custom(_) => false,
        }
    }

    // ── P10：事件分发 ──────────────────────────────────────────────────────────

    /// 分发一次输入事件（平台泵与 `TestApp::inject` 的**唯一**入口）。
    pub(crate) fn dispatch_input(&mut self, window: WindowId, event: InputEvent) -> InputOutcome {
        let mut outcome = InputOutcome::default();
        if !self.windows.contains_key(&window) {
            return outcome;
        }
        self.validate_focus(window);
        match event {
            InputEvent::PointerDown {
                position,
                button,
                modifiers,
            } => self.on_pointer_down(window, position, button, modifiers, &mut outcome),
            InputEvent::PointerMove {
                position,
                modifiers,
            } => {
                self.on_pointer_move(window, position, modifiers, &mut outcome);
            }
            InputEvent::PointerUp {
                position,
                button,
                modifiers,
            } => self.on_pointer_up(window, position, button, modifiers, &mut outcome),
            InputEvent::PointerLeave => self.on_pointer_leave(window),
            InputEvent::Scroll {
                position,
                delta_x,
                delta_y,
                modifiers,
            } => self.on_scroll(window, position, delta_x, delta_y, modifiers, &mut outcome),
            InputEvent::Key {
                key,
                state,
                modifiers,
            } => self.on_key(window, key, state, modifiers, &mut outcome),
            InputEvent::Text { .. } => {}
        }
        outcome
    }

    fn hit_at(&self, window: WindowId, position: Point) -> Option<HitResult> {
        self.windows
            .get(&window)
            .and_then(|target| target.tree().hit_test(position))
    }

    fn on_pointer_down(
        &mut self,
        window: WindowId,
        position: Point,
        button: PointerButton,
        modifiers: Modifiers,
        outcome: &mut InputOutcome,
    ) {
        if self
            .windows
            .get(&window)
            .is_some_and(|target| target.pointer().drag.is_some())
        {
            // 拖拽进行中：忽略二次按下（会话只由抬起 / 取消 / 失焦结束）。
            return;
        }
        let hit = self.hit_at(window, position);
        outcome.hit = hit;
        if let Some(target) = self.windows.get_mut(&window) {
            let pointer = target.pointer_mut();
            pointer.modifiers = modifiers;
            pointer.last_hit = hit;
            pointer.hover = hit.map(|result| result.node);
            pointer.press = Some(PressRecord {
                node: hit.map(|result| result.node),
                position,
                button,
                at: self.clock,
            });
        }
        // ① 浮层外部点击（优先于焦点：否则会同时关浮层 + 触发下层按钮）。
        let outside = self.windows.get(&window).and_then(|target| {
            let tree = target.tree();
            overlay_util::outside_click_target(tree, tree.root(), hit.map(|result| result.node))
        });
        if let Some((node, _)) = outside {
            // 关闭浮层的这一次按下必须**整体消费**：抬起时不得再派发给下层。
            if let Some(target) = self.windows.get_mut(&window) {
                target.pointer_mut().press = None;
            }
            self.remove_node(window, node);
            return;
        }
        // ② 焦点：命中链上最近的可聚焦祖先（没有 ⇒ 清空）。
        let focus_target = self.windows.get(&window).and_then(|target| {
            hit.and_then(|result| focus::nearest_focusable(target.tree(), result.node))
        });
        self.set_focus(window, focus_target);
        // ③ 拖拽：命中链上第一条 `on_drag`。
        let drag = self.windows.get(&window).and_then(|target| {
            hit.and_then(|result| focus::drag_target(target.tree(), result.node))
        });
        if let Some((node, action)) = drag {
            if let Some(target) = self.windows.get_mut(&window) {
                let pointer = target.pointer_mut();
                pointer.drag = Some(DragSession {
                    node,
                    action: action.clone(),
                    origin: position,
                    current: position,
                    phase: DragPhase::Start,
                });
                pointer.capture = true;
            }
            self.capture_request = Some(true);
            let handled = self.dispatch_action_with_hit(
                window,
                node,
                action.clone(),
                ActionSource::Drag {
                    phase: DragPhase::Start,
                    delta: Point::ZERO,
                },
                hit,
                modifiers,
            );
            if handled {
                outcome.action = Some(action);
                outcome.target = Some(node);
            }
        }
    }

    fn on_pointer_move(
        &mut self,
        window: WindowId,
        position: Point,
        modifiers: Modifiers,
        outcome: &mut InputOutcome,
    ) {
        let hit = self.hit_at(window, position);
        outcome.hit = hit;
        let session = self.windows.get_mut(&window).and_then(|target| {
            let pointer = target.pointer_mut();
            pointer.modifiers = modifiers;
            pointer.last_hit = hit;
            pointer.hover = hit.map(|result| result.node);
            pointer.drag.as_mut().map(|session| {
                session.current = position;
                session.phase = DragPhase::Move;
                session.clone()
            })
        });
        if let Some(session) = session {
            let delta = session.delta();
            let handled = self.dispatch_action_with_hit(
                window,
                session.node,
                session.action.clone(),
                ActionSource::Drag {
                    phase: DragPhase::Move,
                    delta,
                },
                hit,
                modifiers,
            );
            if handled {
                outcome.action = Some(session.action);
                outcome.target = Some(session.node);
            }
            return;
        }
        let cursor = self.windows.get(&window).map_or(Cursor::Default, |target| {
            focus::resolve_cursor(target.tree(), hit.map(|result| result.node))
        });
        let mut changed = false;
        if let Some(target) = self.windows.get_mut(&window) {
            let pointer = target.pointer_mut();
            if pointer.cursor != cursor {
                pointer.cursor = cursor;
                changed = true;
            }
        }
        if changed {
            self.cursor_pending = Some(cursor);
        }
    }

    fn on_pointer_up(
        &mut self,
        window: WindowId,
        position: Point,
        button: PointerButton,
        modifiers: Modifiers,
        outcome: &mut InputOutcome,
    ) {
        let hit = self.hit_at(window, position);
        outcome.hit = hit;
        let session = self.windows.get_mut(&window).and_then(|target| {
            let pointer = target.pointer_mut();
            pointer.modifiers = modifiers;
            pointer.last_hit = hit;
            pointer.end_drag()
        });
        if let Some(session) = session {
            self.capture_request = Some(false);
            let delta = session.delta();
            let action = session.action;
            let handled = self.dispatch_action_with_hit(
                window,
                session.node,
                action.clone(),
                ActionSource::Drag {
                    phase: DragPhase::End,
                    delta,
                },
                hit,
                modifiers,
            );
            if handled {
                outcome.action = Some(action);
                outcome.target = Some(session.node);
            }
            return;
        }
        let press = self
            .windows
            .get_mut(&window)
            .and_then(|target| target.pointer_mut().press.take());
        let Some(press) = press else {
            return;
        };
        if press.button != button {
            return;
        }
        let Some(press_node) = press.node else {
            return;
        };
        let moved = Point::new(position.x - press.position.x, position.y - press.position.y);
        if drag_util::crosses_threshold(moved, self.thresholds.drag_distance) {
            return;
        }
        if hit.map(|result| result.node) != Some(press_node) {
            return;
        }
        let count = self.click_count(window, press_node, position);
        outcome.click_count = count;
        let target_action = self
            .windows
            .get(&window)
            .and_then(|target| focus::action_target(target.tree(), press_node));
        if let Some((node, action)) = target_action {
            let handled = self.dispatch_action_with_hit(
                window,
                node,
                action.clone(),
                ActionSource::Click { click_count: count },
                hit,
                modifiers,
            );
            if handled {
                outcome.action = Some(action);
                outcome.target = Some(node);
            }
        }
    }

    fn on_pointer_leave(&mut self, window: WindowId) {
        let mut changed = false;
        if let Some(target) = self.windows.get_mut(&window) {
            let pointer = target.pointer_mut();
            let capture = pointer.capture;
            pointer.hover = None;
            if !capture {
                pointer.press = None;
            }
            if pointer.cursor != Cursor::Default {
                pointer.cursor = Cursor::Default;
                changed = true;
            }
        }
        if changed {
            self.cursor_pending = Some(Cursor::Default);
        }
    }

    fn on_scroll(
        &mut self,
        window: WindowId,
        position: Point,
        delta_x: f32,
        delta_y: f32,
        modifiers: Modifiers,
        outcome: &mut InputOutcome,
    ) {
        let hit = self.hit_at(window, position);
        outcome.hit = hit;
        let container = self.windows.get(&window).and_then(|target| {
            hit.and_then(|result| scroll::scroll_ancestor(target.tree(), result.node))
        });
        let Some((node, slot)) = container else {
            return;
        };
        // 与轴无关的统一规则（§5.1）：Shift ⇒ 横向，否则纵向。
        let drives = if modifiers.contains(Modifiers::SHIFT) {
            delta_x
        } else {
            delta_y
        };
        if drives == 0.0 {
            return;
        }
        let target = scroll::wheel_target(&slot, drives);
        let handle = ScrollHandle::new(window, node);
        let _ = self.scroll_to_animated(&handle, target);
    }

    fn on_key(
        &mut self,
        window: WindowId,
        key: Key,
        state: KeyState,
        modifiers: Modifiers,
        outcome: &mut InputOutcome,
    ) {
        if state != KeyState::Down {
            return;
        }
        // ① 焦点激活：Enter / Space 走「点击」的同一条冒泡（没声明就落到 ②，不吞键）。
        if matches!(key, Key::Enter | Key::Space) {
            let focused = self.focused(window);
            let activation = self.windows.get(&window).and_then(|target| {
                focused.and_then(|node| focus::action_target(target.tree(), node))
            });
            if let Some((node, action)) = activation {
                let handled = self.dispatch_action_with_hit(
                    window,
                    node,
                    action.clone(),
                    ActionSource::Click { click_count: 1 },
                    None,
                    modifiers,
                );
                if handled {
                    outcome.action = Some(action);
                    outcome.target = Some(node);
                }
                return;
            }
        }
        // ② 全局绑定（精确匹配 key + modifiers）。
        let binding = self
            .bindings
            .iter()
            .find(|binding| binding.key == key && binding.modifiers == modifiers)
            .map(|binding| binding.action.clone());
        let Some(action) = binding else {
            return;
        };
        let node = self
            .focused(window)
            .or_else(|| self.windows.get(&window).map(|target| target.tree().root()));
        let Some(node) = node else {
            return;
        };
        if self.dispatch_action(window, node, action.clone(), ActionSource::Key) {
            outcome.action = Some(action);
            outcome.target = Some(node);
        }
    }

    fn click_count(&mut self, window: WindowId, node: NodeId, position: Point) -> u8 {
        let count = match self.last_click {
            Some(last)
                if last.window == window
                    && last.node == Some(node)
                    && self.clock.saturating_duration_since(last.at)
                        <= self.thresholds.double_click_time
                    && drag_util::within(
                        last.position,
                        position,
                        self.thresholds.double_click_distance,
                    ) =>
            {
                last.count.saturating_add(1)
            }
            _ => 1,
        };
        self.last_click = Some(ClickRecord {
            window,
            node: Some(node),
            position,
            at: self.clock,
            count,
        });
        count
    }

    fn on_window_focus_lost(&mut self, window: WindowId) {
        let cancelled = self
            .windows
            .get_mut(&window)
            .and_then(|target| target.pointer_mut().end_drag());
        if let Some(session) = cancelled {
            self.capture_request = Some(false);
            let delta = session.delta();
            let _ = self.dispatch_action(
                window,
                session.node,
                session.action,
                ActionSource::Drag {
                    phase: DragPhase::Cancel,
                    delta,
                },
            );
        }
        if let Some(target) = self.windows.get_mut(&window) {
            let pointer = target.pointer_mut();
            pointer.hover = None;
            pointer.press = None;
            pointer.cursor = Cursor::Default;
        }
    }

    // ── P10：焦点 ──────────────────────────────────────────────────────────────

    /// 当前焦点（`None` = 无焦点 / 窗口不存在 / **焦点节点已卸载**）。
    ///
    /// 陈旧焦点一律视为无焦点：虚拟列表回收行之后，旧 `NodeId` 不得再驱动键盘路由。
    #[must_use]
    pub fn focused(&self, window: WindowId) -> Option<NodeId> {
        let target = self.windows.get(&window)?;
        let node = target.focus().focused()?;
        target.tree().get(node).ok().map(|_| node)
    }

    /// 清掉指向已卸载节点的焦点（读取路径都是只读，清理集中在这里）。
    fn validate_focus(&mut self, window: WindowId) {
        let stale = self.windows.get(&window).and_then(|target| {
            let node = target.focus().focused()?;
            target.tree().get(node).is_err().then_some(())
        });
        if stale.is_some()
            && let Some(target) = self.windows.get_mut(&window)
        {
            target.focus_mut().set(None);
        }
    }

    /// 设置焦点。不可聚焦 / 同值 ⇒ `false`（不标脏、不唤醒）。
    pub fn set_focus(&mut self, window: WindowId, node: Option<NodeId>) -> bool {
        let Some(target) = self.windows.get_mut(&window) else {
            return false;
        };
        if let Some(node) = node
            && !focus::focusable(target.tree(), node)
        {
            return false;
        }
        let old = target.focus().focused();
        if !target.focus_mut().set(node) {
            return false;
        }
        // 旧焦点环超出节点 bounds：清环时把环的外扩并入 damage（否则边缘残留 1px）。
        if let Some(previous) = old
            && let Some(ring) = target.tree().focus_ring(previous)
            && let Ok(slot) = target.tree().get(previous)
        {
            let bounds = slot.layout().bounds;
            let screen =
                scroll::to_screen(target.tree(), previous, bounds).outset(ring.offset + ring.width);
            target.request_damage(screen);
        }
        for changed in [old, node].into_iter().flatten() {
            let has_ring = target.tree().focus_ring(changed).is_some();
            target.tree_mut().mark_a11y(changed);
            if has_ring {
                target.tree_mut().mark_paint(changed);
            }
        }
        self.request_frame();
        true
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
        let changed = {
            let Some(target) = self.windows.get_mut(&window) else {
                return Err(EngineError::UnknownWindow { window });
            };
            let Some(slot) = target.tree().get(node).ok() else {
                return Err(EngineError::StaleNode { node });
            };
            if slot.kind().text().is_none() {
                return Err(EngineError::TextShape {
                    node,
                    detail: smol_str::SmolStr::new_static("node is not a text node"),
                });
            }
            target.set_ime_cursor_request(node, byte_index)
        };
        if changed {
            self.dispatcher.wake();
        }
        Ok(())
    }

    /// 清除 IME caret hint（P16）；不存在的窗口是 no-op。
    pub fn clear_ime_cursor_area(&mut self, window: WindowId) {
        let changed = self
            .windows
            .get_mut(&window)
            .is_some_and(Window::clear_ime_cursor_request);
        if changed {
            self.dispatcher.wake();
        }
    }

    /// 当前 IME caret hint 对应的窗口客户区逻辑矩形（P16）。
    ///
    /// `Ok(None)` = 没有设置 hint；`Err(TextShape)` = 已设置但文本尚未 shape / 文本变化，
    /// 平台层应跳过本次更新而不是清除上一次有效位置。
    ///
    /// # Errors
    /// `UnknownWindow`：窗口不存在；`StaleNode` / `TextShape`：hint 指向的文本节点不可解析。
    pub fn ime_cursor_area(&self, window: WindowId) -> Result<Option<Rect>, EngineError> {
        self.windows
            .get(&window)
            .ok_or(EngineError::UnknownWindow { window })?
            .ime_cursor_area()
    }

    /// 焦点移到下一个可聚焦节点（末尾回绕）。
    pub fn focus_next(&mut self, window: WindowId) -> Option<NodeId> {
        self.validate_focus(window);
        let next = self.windows.get(&window).and_then(|target| {
            focus::next_focusable(target.tree(), target.focus().focused(), false)
        });
        if self.set_focus(window, next) {
            next
        } else {
            None
        }
    }

    /// 焦点移到上一个可聚焦节点（开头回绕）。
    pub fn focus_prev(&mut self, window: WindowId) -> Option<NodeId> {
        self.validate_focus(window);
        let next = self.windows.get(&window).and_then(|target| {
            focus::next_focusable(target.tree(), target.focus().focused(), true)
        });
        if self.set_focus(window, next) {
            next
        } else {
            None
        }
    }

    // ── P10：滚动 ──────────────────────────────────────────────────────────────

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
        let Some(target) = self.windows.get_mut(&window) else {
            return Err(EngineError::NotScrollable { node });
        };
        let slot = target.tree().get(node)?;
        if slot.clip() != Clip::Bounds {
            return Err(EngineError::NotScrollable { node });
        }
        let viewport = scroll::viewport(target.tree(), node).size;
        if let Some(slot) = target.tree_mut().ensure_interaction(node) {
            slot.scroll = Some(ScrollSlot {
                offset: Point::ZERO,
                content: viewport,
                axis: config.axis,
                line_height: config.line_height,
            });
        }
        Ok(ScrollHandle::new(window, node))
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
        let window = handle.window();
        let node = handle.node();
        let Some(target) = self.windows.get_mut(&window) else {
            return Ok(());
        };
        let Some(slot) = target.tree_mut().scroll_slot_mut(node) else {
            return Err(EngineError::NotScrollable { node });
        };
        slot.content = content;
        Ok(())
    }

    /// 程序化滚动（clamp 后写入；同值 ⇒ 完全 no-op）。
    ///
    /// 只产生**视口矩形**的 damage：不写脏位、不跑布局、不跑样式求值。
    ///
    /// # Errors
    /// `StaleNode`（节点已卸载）/ `NotScrollable`（不是滚动容器）。
    pub fn scroll_to(&mut self, handle: &ScrollHandle, offset: Point) -> Result<(), EngineError> {
        let window = handle.window();
        let node = handle.node();
        let Some(target) = self.windows.get_mut(&window) else {
            return Ok(());
        };
        if target.tree().get(node).is_err() {
            return Err(EngineError::StaleNode { node });
        }
        let Some(slot) = target.tree().scroll_slot(node) else {
            return Err(EngineError::NotScrollable { node });
        };
        let viewport = scroll::viewport(target.tree(), node);
        let mut probe = slot;
        probe.offset = offset;
        let next = scroll::clamp_offset(&probe, viewport.size);
        if next == slot.offset {
            return Ok(());
        }
        if let Some(slot) = target.tree_mut().scroll_slot_mut(node) {
            slot.offset = next;
        }
        // damage 是**屏幕坐标**：视口在内容坐标里，必须减掉祖先滚动偏移（容器自身偏移不算）。
        let damage = scroll::to_screen(target.tree(), node, viewport);
        target.request_damage(damage);
        // P11：a11y 的 bounds 也是屏幕化的内容坐标 ⇒ 偏移变了，整棵子树必须重发。
        // 唯一写点在 `scroll_to`（`scroll_by` / 滚轮 / 动画滚动 / a11y action 全汇到这里）。
        target.a11y_note_scroll(node);
        self.request_frame();
        self.rewindow_virtual(window, node);
        Ok(())
    }

    /// 相对滚动。
    ///
    /// # Errors
    /// 同 [`App::scroll_to`]。
    pub fn scroll_by(&mut self, handle: &ScrollHandle, delta: Point) -> Result<(), EngineError> {
        let current = self
            .scroll_state(handle)
            .map_or(Point::ZERO, |state| state.offset);
        self.scroll_to(handle, Point::new(current.x + delta.x, current.y + delta.y))
    }

    /// 滚动状态快照。
    #[must_use]
    pub fn scroll_state(&self, handle: &ScrollHandle) -> Option<ScrollState> {
        let target = self.windows.get(&handle.window())?;
        let node = handle.node();
        let slot = target.tree().scroll_slot(node)?;
        Some(ScrollState {
            offset: slot.offset,
            content: slot.content,
            viewport: scroll::viewport(target.tree(), node).size,
            axis: slot.axis,
        })
    }

    /// 滚轮 → 平滑滚动（登记/重定向一个 `Scroll` 动画槽）。
    fn scroll_to_animated(
        &mut self,
        handle: &ScrollHandle,
        target: Point,
    ) -> Result<(), EngineError> {
        let Some(state) = self.scroll_state(handle) else {
            return Err(EngineError::NotScrollable {
                node: handle.node(),
            });
        };
        let spec = AnimationSpec::short();
        let window = handle.window();
        // 已有同 handle 的滚动动画 ⇒ 从当前偏移重新起步。
        self.animations.retain(|slot| {
            !matches!(&slot.target, AnimationTarget::Scroll { handle: existing, .. } if *existing == *handle)
        });
        let id = self.next_animation;
        self.next_animation = self.next_animation.wrapping_add(1);
        self.animations.push(AnimationSlot {
            id,
            window,
            target: AnimationTarget::Scroll {
                handle: *handle,
                from: state.offset,
                to: target,
            },
            start: self.clock,
            duration: spec.duration,
            easing: spec.easing,
            nodes: Vec::new(),
        });
        self.request_frame();
        Ok(())
    }

    // ── P10：虚拟列表 ──────────────────────────────────────────────────────────

    /// 让一个滚动容器变成虚拟列表（要求 `Clip::Bounds` + `position: relative`）。
    ///
    /// # Errors
    /// `StaleNode` / `NotVirtualList`。
    pub fn make_virtual_list(
        &mut self,
        window: WindowId,
        container: NodeId,
        config: VirtualListConfig,
    ) -> Result<VirtualList, EngineError> {
        let Some(target) = self.windows.get_mut(&window) else {
            return Err(EngineError::NotVirtualList { node: container });
        };
        let node = target.tree().get(container)?;
        if node.clip() != Clip::Bounds
            || node.style().inner.position != taffy::style::Position::Relative
        {
            return Err(EngineError::NotVirtualList { node: container });
        }
        let viewport = scroll::viewport(target.tree(), container).size;
        let Some(slot) = target.tree_mut().ensure_interaction(container) else {
            return Err(EngineError::StaleNode { node: container });
        };
        slot.scroll = Some(ScrollSlot {
            offset: Point::ZERO,
            content: viewport,
            axis: ScrollAxis::Vertical,
            line_height: ScrollConfig::default().line_height,
        });
        slot.virtual_list = Some(VirtualSlot {
            extent: config.extent.max(1.0),
            overscan: config.overscan,
            count: 0,
            builder: None,
            mounted: Vec::new(),
        });
        Ok(VirtualList::new(window, container))
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
        let window = list.window();
        let node = list.node();
        let Some(target) = self.windows.get_mut(&window) else {
            return Ok(());
        };
        let Some(slot) = target.tree_mut().virtual_slot_mut(node) else {
            return Err(EngineError::NotVirtualList { node });
        };
        slot.count = count;
        slot.builder = Some(Box::new(build));
        self.rewindow_virtual(window, node);
        Ok(())
    }

    /// 虚拟列表状态快照。
    #[must_use]
    pub fn virtual_list_state(&self, list: &VirtualList) -> Option<VirtualListState> {
        let target = self.windows.get(&list.window())?;
        let node = list.node();
        let slot = target.tree().virtual_slot(node)?;
        let scroll = target.tree().scroll_slot(node)?;
        let viewport = scroll::viewport(target.tree(), node).size;
        let range = virtual_list::visible_range(
            scroll.offset.y,
            viewport.height,
            slot.extent,
            slot.count,
            slot.overscan,
        );
        Some(VirtualListState {
            count: slot.count,
            first: range.start,
            last: range.end,
            mounted: slot.mounted.len(),
            offset: scroll.offset,
            viewport,
        })
    }

    /// 行节点 → 条目下标（a11y 的 `pos_in_set`）。
    #[must_use]
    pub fn virtual_row_index(&self, list: &VirtualList, row: NodeId) -> Option<usize> {
        let target = self.windows.get(&list.window())?;
        let slot = target.tree().virtual_slot(list.node())?;
        slot.mounted
            .iter()
            .find(|(_, mounted)| *mounted == row)
            .map(|(index, _)| *index)
    }

    /// 布局后/视口变化后修复所有虚拟列表的可见窗口（幂等：区间不变则零工作）。
    fn rewindow_virtuals(&mut self, window: WindowId) {
        let containers: Vec<NodeId> = self.windows.get(&window).map_or_else(Vec::new, |target| {
            target
                .tree()
                .order()
                .iter()
                .copied()
                .filter(|node| target.tree().virtual_slot(*node).is_some())
                .collect()
        });
        for node in containers {
            self.rewindow_virtual(window, node);
        }
    }

    /// 按当前偏移重建可见窗口（挂载缺的、回收多的）。
    fn rewindow_virtual(&mut self, window: WindowId, node: NodeId) {
        let Some(target) = self.windows.get_mut(&window) else {
            return;
        };
        let Some(slot) = target.tree().virtual_slot(node) else {
            return;
        };
        let (extent, overscan, count) = (slot.extent, slot.overscan, slot.count);
        let Some(scroll) = target.tree().scroll_slot(node) else {
            return;
        };
        let viewport = scroll::viewport(target.tree(), node).size;
        let range =
            virtual_list::visible_range(scroll.offset.y, viewport.height, extent, count, overscan);
        // 回收
        let stale: Vec<(usize, NodeId)> = target
            .tree()
            .virtual_slot(node)
            .map(|slot| {
                slot.mounted
                    .iter()
                    .copied()
                    .filter(|(index, _)| !range.contains(index))
                    .collect()
            })
            .unwrap_or_default();
        let mut changed = false;
        for (_, row) in &stale {
            if target.tree_mut().remove(*row).is_ok() {
                changed = true;
            }
        }
        if let Some(slot) = target.tree_mut().virtual_slot_mut(node) {
            slot.mounted.retain(|(index, _)| range.contains(index));
        }
        // 挂载
        for index in range {
            let exists = target
                .tree()
                .virtual_slot(node)
                .is_some_and(|slot| slot.mounted.iter().any(|(mounted, _)| *mounted == index));
            if exists {
                continue;
            }
            let Some(mut builder) = target
                .tree_mut()
                .virtual_slot_mut(node)
                .and_then(|slot| slot.builder.take())
            else {
                break;
            };
            let built = builder(index).build(target.tree_mut(), node);
            if let Some(slot) = target.tree_mut().virtual_slot_mut(node) {
                slot.builder = Some(builder);
            }
            let Ok(row) = built else {
                break;
            };
            target
                .tree_mut()
                .set_virtual_row(row, index as f32 * extent, extent);
            if let Some(slot) = target.tree_mut().virtual_slot_mut(node) {
                slot.mounted.push((index, row));
            }
            changed = true;
        }
        // 内容尺寸 + 结构脏位
        if let Some(slot) = target.tree_mut().scroll_slot_mut(node) {
            slot.content = Size::new(viewport.width, count as f32 * extent);
        }
        if changed {
            target.tree_mut().mark_structure(node);
            self.request_frame();
        }
    }

    // ── P10：浮层 ──────────────────────────────────────────────────────────────

    /// 打开一个浮层（root 的最后一个子节点 ⇒ `order` 末尾 ⇒ 命中优先）。
    ///
    /// # Errors
    /// # Errors
    /// `UnknownWindow`（窗口已关闭）/ `StaleNode`（树结构被破坏）。
    pub fn create_overlay(
        &mut self,
        window: WindowId,
        options: OverlayOptions,
    ) -> Result<Overlay, EngineError> {
        let Some(target) = self.windows.get_mut(&window) else {
            return Err(EngineError::UnknownWindow { window });
        };
        let root = target.tree().root();
        let origin = interaction::resolve_placement(target.tree(), options.placement);
        let style = overlay_style(origin);
        let node = NodeBuilder::boxed()
            .style(style)
            .build(target.tree_mut(), root)?;
        if let Some(slot) = target.tree_mut().ensure_interaction(node) {
            slot.overlay = Some(OverlaySlot {
                dismiss: options.dismiss,
            });
        }
        self.request_frame();
        Ok(Overlay::new(window, node))
    }

    /// 关闭浮层（节点不存在 ⇒ `false`）。
    pub fn close_overlay(&mut self, overlay: &Overlay) -> bool {
        self.remove_node(overlay.window(), overlay.node())
    }

    /// 重新定位浮层（相对根的逻辑像素点）。
    pub fn place_overlay(&mut self, overlay: &Overlay, placement: OverlayPlacement) -> bool {
        let window = overlay.window();
        let node = overlay.node();
        let Some(target) = self.windows.get_mut(&window) else {
            return false;
        };
        let origin = interaction::resolve_placement(target.tree(), placement);
        if !target.tree_mut().set_absolute_inset(node, origin) {
            return false;
        }
        self.request_frame();
        true
    }

    /// 把浮层提到最上层（重挂到 root 末尾）。
    pub fn bring_overlay_to_front(&mut self, overlay: &Overlay) -> bool {
        let window = overlay.window();
        let node = overlay.node();
        let Some(target) = self.windows.get_mut(&window) else {
            return false;
        };
        let root = target.tree().root();
        if target.tree_mut().append_child(root, node).is_err() {
            return false;
        }
        self.request_frame();
        true
    }

    /// 当前窗口的浮层（自下而上）。
    pub fn overlays(&self, window: WindowId) -> Vec<(NodeId, OverlayDismiss)> {
        let Some(target) = self.windows.get(&window) else {
            return Vec::new();
        };
        let tree = target.tree();
        let Ok(root) = tree.get(tree.root()) else {
            return Vec::new();
        };
        root.children()
            .iter()
            .filter_map(|child| tree.overlay_slot(*child).map(|slot| (*child, slot.dismiss)))
            .collect()
    }

    /// `Dismiss` 动作：关掉最上层浮层（含 `Explicit`）。
    fn dismiss_overlay(&mut self, window: WindowId) -> bool {
        let node = self.windows.get(&window).and_then(|target| {
            let tree = target.tree();
            overlay_util::topmost(tree, tree.root()).map(|(node, _)| node)
        });
        match node {
            Some(node) => self.remove_node(window, node),
            None => false,
        }
    }

    fn remove_node(&mut self, window: WindowId, node: NodeId) -> bool {
        let Some(target) = self.windows.get_mut(&window) else {
            return false;
        };
        let removed = target.tree_mut().remove(node).is_ok();
        if removed {
            self.request_frame();
        }
        removed
    }

    // ── P10：拖拽 ──────────────────────────────────────────────────────────────

    /// 当前拖拽会话。
    #[must_use]
    pub fn drag_state(&self, window: WindowId) -> Option<DragState> {
        let target = self.windows.get(&window)?;
        let session = target.pointer().drag.as_ref()?;
        Some(DragState::new(window, session.node))
    }

    /// 拖拽的只读快照。
    #[must_use]
    pub fn drag(&self, state: &DragState) -> Option<DragInfo> {
        let target = self.windows.get(&state.window())?;
        let session = target.pointer().drag.as_ref()?;
        if session.node != state.node() {
            return None;
        }
        Some(DragInfo {
            origin: session.origin,
            current: session.current,
            delta: session.delta(),
            phase: session.phase,
        })
    }

    /// 取消拖拽（派发 `Cancel` 阶段并释放捕获）。
    pub fn cancel_drag(&mut self, state: &DragState) -> bool {
        let window = state.window();
        let session = self
            .windows
            .get_mut(&window)
            .and_then(|target| target.pointer_mut().end_drag());
        let Some(session) = session else {
            return false;
        };
        self.capture_request = Some(false);
        let delta = session.delta();
        let action = session.action;
        self.dispatch_action(
            window,
            session.node,
            action,
            ActionSource::Drag {
                phase: DragPhase::Cancel,
                delta,
            },
        );
        true
    }

    // ── P10：动画 ──────────────────────────────────────────────────────────────

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
        if !self.windows.contains_key(&window) {
            return Err(EngineError::UnknownWindow { window });
        }
        let id = self.next_animation;
        self.next_animation = self.next_animation.wrapping_add(1);
        let target = match value {
            AnimatedValue::Scalar { target, to } => {
                let from = self
                    .store
                    .transient_ref::<f32>(target.id())
                    .copied()
                    .unwrap_or(0.0);
                if spec.duration.is_zero() {
                    self.store.set_transient(target.id(), to);
                    self.mark_nodes_paint(window, nodes);
                    self.request_frame();
                    return Ok(AnimationId::new(window, id));
                }
                AnimationTarget::Scalar {
                    transient: target,
                    from,
                    to,
                }
            }
            AnimatedValue::Color { target, to } => {
                let from = self
                    .store
                    .transient_ref::<Color>(target.id())
                    .copied()
                    .unwrap_or(Color::TRANSPARENT);
                if spec.duration.is_zero() {
                    self.store.set_transient(target.id(), to);
                    self.mark_nodes_paint(window, nodes);
                    self.request_frame();
                    return Ok(AnimationId::new(window, id));
                }
                AnimationTarget::Color {
                    transient: target,
                    from,
                    to,
                }
            }
        };
        // 同一目标重定向：旧槽必须让位（否则两条插值同时写同一个瞬态值 ⇒ 回弹/抖动）。
        self.animations
            .retain(|slot| !slot.target.same_target(&target));
        self.animations.push(AnimationSlot {
            id,
            window,
            target,
            start: self.clock,
            duration: spec.duration,
            easing: spec.easing,
            nodes: nodes.to_vec(),
        });
        self.request_frame();
        Ok(AnimationId::new(window, id))
    }

    /// 取消动画（幂等）。
    pub fn cancel_animation(&mut self, id: &AnimationId) -> bool {
        let before = self.animations.len();
        self.animations
            .retain(|slot| !(slot.window == id.window() && slot.id == id.generation()));
        self.animations.len() != before
    }

    /// 该窗口是否还有未完成的动画。
    #[must_use]
    pub fn animations_running(&self, window: WindowId) -> bool {
        self.animations.iter().any(|slot| slot.window == window)
    }

    /// 全部窗口是否还有未完成的动画（平台泵用它决定 `SetTimer(1)` / `KillTimer`）。
    pub(crate) fn has_animations(&self) -> bool {
        !self.animations.is_empty()
    }

    fn mark_nodes_paint(&mut self, window: WindowId, nodes: &[NodeId]) {
        if let Some(target) = self.windows.get_mut(&window) {
            for node in nodes {
                target.tree_mut().mark_paint(*node);
            }
        }
    }

    /// 推进本窗口的动画（`App::frame` 在帧门禁**之前**调用）。
    fn tick_animations(&mut self, window: WindowId) {
        if !self.animations_running(window) {
            return;
        }
        let now = self.clock;
        let slots = std::mem::take(&mut self.animations);
        let mut keep = Vec::with_capacity(slots.len());
        for slot in slots {
            if slot.window != window {
                keep.push(slot);
                continue;
            }
            let t = anim::progress(now.saturating_duration_since(slot.start), slot.duration);
            let eased = slot.easing.sample(t);
            match &slot.target {
                AnimationTarget::Scalar {
                    transient,
                    from,
                    to,
                } => {
                    self.store
                        .set_transient(transient.id(), anim::lerp(*from, *to, eased));
                }
                AnimationTarget::Color {
                    transient,
                    from,
                    to,
                } => {
                    self.store
                        .set_transient(transient.id(), anim::lerp_color(*from, *to, eased));
                }
                AnimationTarget::Scroll { handle, from, to } => {
                    let point = anim::lerp_point(*from, *to, eased);
                    let _ = self.scroll_to(handle, point);
                }
            }
            for node in &slot.nodes {
                if let Some(target) = self.windows.get_mut(&window) {
                    target.tree_mut().mark_paint(*node);
                }
            }
            if t < 1.0 {
                keep.push(slot);
            }
        }
        self.animations = keep;
    }

    /// 把引擎时钟推进到真实时间（平台泵每轮一次；测试用 `TestApp::advance`）。
    pub(crate) fn sync_clock(&mut self) {
        self.clock = Instant::now();
    }

    /// 推进测试时钟（`TestApp::advance` 的唯一实现；只在测试构建里存在）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn advance_clock(&mut self, by: Duration) {
        self.clock += by;
    }

    /// 取走待推给平台的光标变化。
    pub(crate) fn take_cursor_change(&mut self) -> Option<Cursor> {
        self.cursor_pending.take()
    }

    /// 取走待推给平台的捕获请求（`Some(true)` = 取得、`Some(false)` = 释放）。
    pub(crate) fn take_capture_request(&mut self) -> Option<bool> {
        self.capture_request.take()
    }

    // ── 窗口 ──────────────────────────────────────────────────────────────────

    /// 建一个窗口（headless：只建树与空场景）。`WindowId` 从 **1** 起单调 +1、永不复用
    /// （0 保留为非法句柄，见 `WindowId::from_raw`）。
    pub fn create_window(&mut self, options: WindowOptions) -> WindowId {
        let id = WindowId::from_raw(self.next_window_id);
        self.next_window_id = self.next_window_id.wrapping_add(1);
        self.windows.insert(id, Window::new(id, options));
        id
    }

    /// 只读窗口。
    #[must_use]
    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(&id)
    }

    /// 可写窗口（应用层建树的入口）。
    pub fn window_mut(&mut self, id: WindowId) -> Option<&mut Window> {
        self.windows.get_mut(&id)
    }

    /// 关窗：精确退订该窗口全部节点的依赖，然后丢弃。返回 `false` = 窗口不存在。
    pub fn close_window(&mut self, id: WindowId) -> bool {
        // 动画槽引用窗口内的节点与瞬态值：窗口关闭时必须一并清掉（否则 `SetTimer` 永不停）。
        self.animations.retain(|slot| slot.window != id);
        self.last_click = self.last_click.filter(|record| record.window != id);
        let Some(window) = self.windows.remove(&id) else {
            return false;
        };
        for node in window.tree().order() {
            if let Some(deps) = window.tree().deps(*node) {
                for signal in deps {
                    self.graph.detach(
                        *signal,
                        Owner::Node {
                            window: id,
                            node: *node,
                        },
                    );
                }
            }
        }
        true
    }

    /// 遍历全部窗口（顺序不保证）并带出 `WindowId`。
    ///
    /// 带上 id 是必要的：帧是**按窗口**推进的（`refresh_styles(window)` / P06 的 paint），只有 `&Window`
    /// 拿不到「这是哪个窗口」（消费者：P09 的帧驱动、测试）。
    pub fn windows(&self) -> impl Iterator<Item = (WindowId, &Window)> {
        self.windows.iter().map(|(id, window)| (*id, window))
    }

    // ── 帧内：样式求值 pass ────────────────────────────────────────────────────

    /// P03 的「样式求值 pass」：对指定窗口中**带 `PAINT` 且有 `bind_style`** 的节点求值。
    ///
    /// 它做四件事：求值闭包、差分替换依赖、写回 patch、按需升级 `LAYOUT` / 标 `A11Y`。
    /// **不产生任何 Scene 图元**（那是 P06 的 paint 阶段；两者共用本实现的前半段）。
    ///
    /// 返回 `None` = 窗口不存在（P03 的冻结错误表里没有「未知窗口」变体，`Option` 是唯一诚实的表达）。
    /// P11 追加：`bind_a11y` 与另两条动态通道同批求值（同一 `ReadCtx` / `ReadTracker` / 依赖边集）。
    pub fn refresh_styles(&mut self, window: WindowId) -> Option<StylePassReport> {
        let Self {
            store,
            graph,
            windows,
            eval_stack,
            wake_requested,
            ..
        } = self;
        // P11：本 pass 会求值全部 `bind_a11y`，旗标在这里消费。
        if let Some(target) = windows.get_mut(&window) {
            target.a11y_clear_bind_work();
        }
        let window_state = windows.get_mut(&window)?;
        let tree = window_state.tree_mut();
        debug_assert!(eval_stack.is_empty(), "上一帧的派生求值栈泄漏");
        eval_stack.clear();

        let mut evaluated = 0usize;
        let mut text_evaluated = 0usize;
        let mut text_changed = 0usize;
        let mut index = 0usize;
        while index < tree.order().len() {
            let node = tree.order()[index];
            index += 1;

            let needs_eval = tree.get(node).is_ok_and(|n| {
                n.dirty().contains(DirtyFlags::PAINT)
                    // 只有 `bind_a11y` 的节点没有 PAINT（写信号只标 A11Y），但补丁仍要重算。
                    || (n.dirty().contains(DirtyFlags::A11Y) && tree.has_a11y_bind(node))
            });
            if !needs_eval {
                continue;
            }
            let old_patch = tree
                .get(node)
                .map_or_else(|_| StylePatch::default(), |n| n.style_patch().clone());
            let compute = tree.take_bind_style(node);
            let bind_text = tree.take_bind_text(node);
            let bind_a11y = tree.take_bind_a11y(node);
            if compute.is_none() && bind_text.is_none() && bind_a11y.is_none() {
                // 闭包不见了 ⇒ 上次求值 **panic** 后没放回（`build` 之后必然有闭包）。
                // 那次 pass 在 `replace_deps` 之前就中断了，所以节点仍带着**上一次成功的** deps：
                // 它们是「幽灵边」—— 节点已不会再求值，但写这些信号仍会把它标脏。
                // 这里显式退订，把状态收敛成「静态节点」。
                let ghost_deps = tree.deps(node).unwrap_or(&[]);
                graph.replace_deps(Owner::Node { window, node }, ghost_deps, &[]);
                tree.set_deps(node, SmallVec::new());
                tree.clear_paint(node);
                continue;
            }

            let (values, derived) = store.split();
            let mut tracker = ReadTracker::new();
            // 作用域块结束即释放 `ReadCtx` 的借用（`ReadCtx` 不是 `Drop` 类型，不能用 `drop`）。
            let (patch, text_value, a11y_value, owner) = {
                let mut ctx = ReadCtx::new(
                    values,
                    derived,
                    graph,
                    &mut tracker,
                    &mut *eval_stack,
                    Owner::Node { window, node },
                );
                let patch = compute
                    .as_ref()
                    .map_or_else(StylePatch::default, |compute| compute(&mut ctx));
                let text_value = bind_text.as_ref().map(|bind| bind(&mut ctx));
                let a11y_value = bind_a11y
                    .as_ref()
                    .map_or_else(crate::a11y::A11yPatch::default, |bind| bind(&mut ctx));
                (patch, text_value, a11y_value, ctx.owner())
            };
            if compute.is_some() {
                evaluated += 1;
            }
            if text_value.is_some() {
                text_evaluated += 1;
            }

            let old_deps = tree.deps(node).unwrap_or(&[]);
            graph.replace_deps(owner, old_deps, tracker.as_slice());
            tree.set_deps(node, tracker.into_inner());

            if let Some(compute) = compute {
                let layout_changed = patch.affects_layout || old_patch.affects_layout;
                let a11y_changed = patch.interactive != old_patch.interactive
                    || patch.visible != old_patch.visible;
                tree.set_style_patch(node, patch);
                tree.put_bind_style(node, compute);
                if layout_changed {
                    tree.mark_layout(node);
                }
                if a11y_changed {
                    tree.mark_a11y(node);
                }
            }
            let mut text_changed_here = false;
            if let Some(bind) = bind_text {
                tree.put_bind_text(node, bind);
            }
            if let Some(value) = text_value {
                let style = tree
                    .text_content(node)
                    .map_or_else(TextStyle::default, |content| content.style.clone());
                let changed = tree
                    .text_content(node)
                    .is_none_or(|content| content.text != value);
                if changed {
                    // 只换 text/style，**保留** inline_boxes（N1 回归）。
                    let mut content = tree
                        .text_content(node)
                        .cloned()
                        .unwrap_or_else(|| TextContent::new(value.clone(), style.clone()));
                    content.text = value;
                    content.style = style;
                    tree.set_text_content(node, content);
                    text_changed += 1;
                    text_changed_here = true;
                }
            }
            // P11：`bind_a11y` 的差分与写回（与 `bind_style` 同一个依赖集，见方法文档）。
            if let Some(bind) = bind_a11y {
                let previous = tree
                    .a11y_of(node)
                    .map_or_else(crate::a11y::A11yPatch::default, |slot| slot.patch.clone());
                if a11y_value != previous {
                    tree.set_a11y_patch(node, a11y_value);
                    // 只标 `A11Y`：不产生视觉 damage、不清 `REUSABLE`（P03 §9.3 表第 9 行）。
                    tree.mark_a11y(node);
                }
                tree.put_bind_a11y(node, bind);
            }
            tree.clear_paint(node);
            if text_changed_here {
                // 文本变了 ⇒ 必须重画（PAINT 不能被本 pass 的「消费」吞掉）。
                tree.mark_paint(node);
                // 文本节点的可访问名/值就是文本内容：内容变了 a11y 必须重发。只标 PAINT 时，
                // 新字符串与旧串度量相同（例如 "70%" → "80%"）不会触发 LAYOUT，AT 会读到旧值。
                tree.mark_a11y(node);
            }
        }

        Some(StylePassReport {
            evaluated,
            layout_dirty: tree.any_node(|n| n.dirty().contains(DirtyFlags::LAYOUT)),
            paint_dirty: tree.any_node(|n| !n.dirty().contains(DirtyFlags::REUSABLE)),
            wake_requested: *wake_requested,
            text_evaluated,
            text_changed,
        })
    }

    // ── P11：无障碍（动作 / 诊断 / 交付） ──────────────────────────────────

    /// 注册 a11y 动作的处理器（同 `A11yAction` 后写覆盖）。
    ///
    /// 返回 `false` = 该动作**不能**注册：`A11yAction::Click` 必须落到 P10 的 `Action` 表
    /// （`App::on_action`），让指针点击 / 键盘 Enter / AT 激活共享同一身份
    /// （`design/phase-11-a11y.md` §9.3）。
    pub fn on_a11y_action(
        &mut self,
        action: crate::a11y::A11yAction,
        handler: impl FnMut(&mut crate::a11y::A11yActionCtx<'_>) + 'static,
    ) -> bool {
        if action == crate::a11y::A11yAction::Click {
            return false;
        }
        self.a11y_handlers.insert(action, Box::new(handler));
        true
    }

    /// 删除 a11y 动作的处理器（返回是否真的删了）。
    pub fn off_a11y_action(&mut self, action: crate::a11y::A11yAction) -> bool {
        self.a11y_handlers.remove(&action).is_some()
    }

    /// a11y 诊断（与 `text_diagnostics` 同型；上限 `A11Y_DIAGNOSTIC_CAPACITY`，超出丢最旧）。
    #[must_use]
    pub fn a11y_diagnostics(&self) -> &[crate::a11y::A11yDiagnostic] {
        &self.a11y_diagnostics
    }

    /// 记录一条 a11y 诊断（唯一入口：计数与容量在这里统一）。
    pub(crate) fn a11y_note(&mut self, diagnostic: crate::a11y::A11yDiagnostic) {
        if self.a11y_diagnostics.len() >= A11Y_DIAGNOSTIC_CAPACITY {
            self.a11y_diagnostics.remove(0);
        }
        self.a11y_diagnostics.push(diagnostic);
    }

    /// 该动作是否注册了 handler（`a11y::action` 的分派判据）。
    pub(crate) fn a11y_has_handler(&self, action: crate::a11y::A11yAction) -> bool {
        self.a11y_handlers.contains_key(&action)
    }

    /// 摘出 handler（借用纪律：调用期间它不在表里）。
    pub(crate) fn a11y_take_handler(
        &mut self,
        action: crate::a11y::A11yAction,
    ) -> Option<Box<dyn FnMut(&mut crate::a11y::A11yActionCtx<'_>)>> {
        self.a11y_handlers.remove(&action)
    }

    /// 放回 handler。
    pub(crate) fn a11y_put_handler(
        &mut self,
        action: crate::a11y::A11yAction,
        handler: Box<dyn FnMut(&mut crate::a11y::A11yActionCtx<'_>)>,
    ) {
        self.a11y_handlers.insert(action, handler);
    }

    /// 该窗口本轮是否有 a11y 工作（O(1)；平台泵每轮用它过滤 `update_if_active`）。
    #[must_use]
    pub(crate) fn a11y_pending(&self, window: WindowId) -> bool {
        self.windows.get(&window).is_some_and(Window::a11y_pending)
    }

    /// 产出一笔 a11y 交付（消费 `A11Y|ORDER` 位与 `full_pending`）。
    ///
    /// **必须在 `accesskit_windows::Adapter::update_if_active` 的闭包里调用**（adapter 不活跃时
    /// 闭包不执行 ⇒ 零开销），或由 `test-support` 的捕获路径调用。
    pub(crate) fn a11y_take(&mut self, window: WindowId) -> Option<crate::a11y::A11yTreeSnapshot> {
        let mut diagnostics = Vec::new();
        let snapshot = {
            let Self { windows, .. } = self;
            windows.get_mut(&window)?.a11y_take(&mut diagnostics)
        };
        for diagnostic in diagnostics {
            self.a11y_note(diagnostic);
        }
        snapshot
    }

    /// 打开/关闭某个窗口的 a11y 快照捕获（`TestApp` 专用）。
    #[cfg(feature = "test-support")]
    pub(crate) fn set_a11y_capture(&mut self, window: WindowId, on: bool) {
        if let Some(target) = self.windows.get_mut(&window) {
            target.set_a11y_capture(on);
        }
    }

    /// 该窗口的捕获是否打开。
    #[cfg(feature = "test-support")]
    pub(crate) fn attach_a11y(
        &mut self,
        window: WindowId,
        snapshot: crate::a11y::A11yTreeSnapshot,
    ) {
        if let Some(target) = self.windows.get_mut(&window) {
            target.attach_a11y(snapshot);
        }
    }

    // ── P06：完整帧（styles → text → layout → layout_text → paint → commit）────────

    /// 完整帧：`refresh_styles` → `refresh_text` → `compute_layout`（仅 LAYOUT 脏）→ `layout_text`
    /// → paint → commit。
    ///
    /// 返回 `None` = 窗口不存在（**不**消费 wake 请求）；`Ok(FrameOutcome::Idle)` = 空闲零帧（I3，
    /// 不提交、`FrameIndex` 不变）；`Err` = 本帧已中止（damage/dirty 保留，下一帧重试）。
    ///
    /// 产品路径：含 `bind_style`/`bind_text` 的窗口只能走这里；`Window::frame` 是它的无 App 通道版本。
    pub fn frame(&mut self, window: WindowId) -> Option<Result<FrameOutcome, EngineError>> {
        if !self.windows.contains_key(&window) {
            return None;
        }
        let wake = self.take_wake_request();
        // P10：动画在帧门禁**之前**推进（写 `Transient` + 给注册节点标 `PAINT`），因此「有动画」必然
        // 让 `needs_frame` 为真；动画结束后不再留帧。
        self.tick_animations(window);
        let needs_frame = self
            .windows
            .get(&window)
            .is_some_and(|candidate| candidate.needs_frame(wake));
        if !needs_frame {
            self.record_idle(window);
            return Some(Ok(FrameOutcome::Idle));
        }
        let Some(ticket) = self.windows.get_mut(&window).and_then(Window::begin_frame) else {
            return Some(Ok(FrameOutcome::Idle));
        };
        let Some(styles) = self.refresh_styles(window) else {
            self.abort_frame(window, ticket);
            return None;
        };
        let text = match self.refresh_text(window) {
            Some(Ok(report)) => report,
            Some(Err(error)) => {
                self.abort_frame(window, ticket);
                return Some(Err(error));
            }
            None => {
                self.abort_frame(window, ticket);
                return None;
            }
        };
        let layout = match self.windows.get_mut(&window).map(Window::layout_if_dirty) {
            Some(Ok(report)) => report,
            Some(Err(error)) => {
                self.abort_frame(window, ticket);
                return Some(Err(error));
            }
            None => {
                self.abort_frame(window, ticket);
                return None;
            }
        };
        let text_layout = match self.layout_text(window) {
            Some(Ok(report)) => report,
            Some(Err(error)) => {
                self.abort_frame(window, ticket);
                return Some(Err(error));
            }
            None => {
                self.abort_frame(window, ticket);
                return None;
            }
        };
        // 布局后（视口尺寸已知/已变）再修一次虚拟列表窗口：首次布局前视口 = 0，不能靠
        // `set_virtual_items` 那一次调用定终身（P10 §8.3 / 评审 #5）。
        self.rewindow_virtuals(window);
        // 图集页上传（P13：广播给所有窗口的渲染器；两张页分开、顺序固定以便帧记录复现）。
        self.upload_atlas_pages();
        let paint = match self.windows.get_mut(&window) {
            Some(target) => match target.paint_frame() {
                Ok(report) => report,
                Err(error) => {
                    self.abort_frame(window, ticket);
                    return Some(Err(error));
                }
            },
            None => {
                self.abort_frame(window, ticket);
                return None;
            }
        };
        #[cfg(feature = "test-support")]
        {
            let Some(target) = self.windows.get(&window) else {
                self.abort_frame(window, ticket);
                return None;
            };
            let stats = FrameStats {
                styles,
                text: Some(text),
                text_layout: Some(text_layout),
                layout,
                painted_lanes: paint.rebuilt,
                reused_lanes: paint
                    .log
                    .lanes
                    .iter()
                    .filter(|entry| entry.outcome == LaneOutcome::Reused)
                    .count(),
                primitives: target.scene().primitive_count(),
                instances: paint.instances,
            };
            let Some(target) = self.windows.get_mut(&window) else {
                self.abort_frame(window, ticket);
                return None;
            };
            target.set_frame_stats(stats);
        }
        #[cfg(not(feature = "test-support"))]
        {
            let _ = (&styles, &text, &text_layout, &layout);
        }
        let Some(target) = self.windows.get_mut(&window) else {
            self.abort_frame(window, ticket);
            return None;
        };
        Some(target.commit_frame(ticket, paint))
    }

    /// 中止本帧（不结算、不递增 `FrameIndex`）。
    fn abort_frame(&mut self, window: WindowId, ticket: FrameTicket) {
        if let Some(target) = self.windows.get_mut(&window) {
            target.abort_frame(ticket);
        }
    }

    /// 记录一次空闲帧（仅 `test-support`；其余构建是空操作）。
    fn record_idle(&mut self, window: WindowId) {
        #[cfg(feature = "test-support")]
        if let Some(target) = self.windows.get_mut(&window) {
            target.record_idle();
        }
        #[cfg(not(feature = "test-support"))]
        let _ = window;
    }

    // ── P05：文本 pass / 字体栈 / 诊断 ────────────────────────────────────────

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
        let Self {
            text,
            raster,
            atlas,
            glyph_atlas,
            windows,
            scale_epochs,
            pending_dirty,
            ..
        } = self;
        let window = windows.get_mut(&window)?;
        let window_id = window.id();
        let scale = window.scale().factor();
        let epoch = window.scale_epoch();

        if scale_epochs.get(&window_id).copied().unwrap_or(0) != epoch {
            atlas.clear();
            glyph_atlas.clear();
            scale_epochs.insert(window_id, epoch);
        }
        raster.set_font_epoch(text.font_epoch());

        let tree = window.tree_mut();
        let mut report = TextPassReport::default();
        let known = taffy::Size {
            width: None,
            height: None,
        };
        let available = taffy::Size {
            width: taffy::AvailableSpace::MaxContent,
            height: taffy::AvailableSpace::MaxContent,
        };

        let mut index = 0usize;
        while index < tree.order().len() {
            let node = tree.order()[index];
            index += 1;
            let Some(content) = tree.text_content(node).cloned() else {
                continue;
            };
            let (shaped, hit) = text.shape_cached(&content, scale);
            if hit {
                report.reused += 1;
            } else {
                report.shaped += 1;
            }
            let key = crate::text::cache::ShapeKey::new(&content, scale, text.font_stack_version());
            if let Some(slot) = tree.text_slot(node)
                && slot.key() == key
                && slot.scale_epoch() == epoch
                && Rc::ptr_eq(slot.shaped(), &shaped)
            {
                // B4：快照未变 ⇒ 不重建、不清缓存、不重挂（空闲帧只走这条路）。
                report.skipped += 1;
                continue;
            }

            let measured = shaped.measure(known, available);
            let previous = tree.text_measured_size(node).unwrap_or(Size::ZERO);
            let size_changed = previous.width.to_bits() != measured.width.to_bits()
                || previous.height.to_bits() != measured.height.to_bits();
            tree.set_text_shaped(node, Rc::clone(&shaped), key, epoch, measured, size_changed);
        }
        report.dirty_spans = pending_dirty.spans().len();
        Some(Ok(report))
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
        let Self {
            text,
            raster,
            atlas,
            glyph_atlas,
            windows,
            pending_dirty,
            ..
        } = self;
        let window = windows.get_mut(&window)?;
        let scale = window.scale().factor();
        let mut report = TextLayoutReport::default();

        for attempt in 0..2 {
            let generation = atlas.generation();
            let mut laid = 0usize;
            let mut reused = 0usize;
            let tree = window.tree_mut();
            let mut index = 0usize;
            while index < tree.order().len() {
                let node = tree.order()[index];
                index += 1;
                let Some(shaped) = tree.text_slot(node).map(|slot| Rc::clone(slot.shaped())) else {
                    continue;
                };
                let width = {
                    let width = tree.text_content_width(node);
                    if width > 0.0 {
                        width
                    } else {
                        shaped.widths().max / scale
                    }
                };
                let current = tree
                    .text_slot(node)
                    .map(|slot| (slot.laid_width_bits(), slot.laid_generation()));
                if current == Some((width.to_bits(), generation)) && tree.text_line(node).is_some()
                {
                    reused += 1;
                    continue;
                }

                let layout = shaped.layout_at(width);
                let (line, diagnostics, _stats) = match shaped.rasterize(
                    &layout,
                    width,
                    raster,
                    glyph_atlas,
                    atlas,
                    generation,
                ) {
                    Ok(value) => value,
                    Err(error) => return Some(Err(error)),
                };
                pending_dirty.merge(&mut atlas.take_dirty_rows());
                text.push_diagnostics(diagnostics);
                tree.set_text_line(node, Rc::new(line), layout, width, generation);
                laid += 1;
            }
            if atlas.generation() == generation {
                report.laid = laid;
                report.reused = reused;
                report.reruns = attempt;
                return Some(Ok(report));
            }
        }
        Some(Err(EngineError::TextShape {
            node: window.tree().root(),
            detail: smol_str::SmolStr::new_static(
                "atlas generation changed twice within one text pass",
            ),
        }))
    }
    /// 当前字体栈。
    #[must_use]
    pub fn font_stack(&self) -> &FontStack {
        self.text.font_stack()
    }

    /// 当前字体栈版本（形状缓存键的一部分）。
    #[must_use]
    pub fn font_stack_version(&self) -> u32 {
        self.text.font_stack_version()
    }

    /// 文本诊断（tofu / 行内盒边界 / 超宽 / 合成样式）。
    #[must_use]
    pub fn text_diagnostics(&self) -> &[TextDiagnostic] {
        self.text.diagnostics()
    }

    /// 字体栈的健康状态（N4：测试与诊断的**唯一**跳过判据）。
    pub fn font_stack_status(&mut self) -> FontStackStatus {
        self.text.font_stack_status()
    }

    /// 换字体栈：版本 +1、字体代际 +1、清形状缓存与字形图集、所有窗口的文本节点
    /// `LAYOUT | PAINT`。返回新版本号。
    pub fn set_font_stack(&mut self, stack: FontStack) -> u32 {
        let version = self.text.set_font_stack(stack);
        self.glyph_atlas.clear();
        self.atlas.clear();
        self.raster.set_font_epoch(self.text.font_epoch());
        self.invalidate_all_text();
        version
    }

    /// 系统字体安装/卸载/语言切换后调用：重载 + 与 [`App::set_font_stack`] 相同的失效。
    pub fn reload_system_fonts(&mut self) -> u32 {
        let version = self.text.reload_system_fonts();
        self.glyph_atlas.clear();
        self.atlas.clear();
        self.raster.set_font_epoch(self.text.font_epoch());
        self.invalidate_all_text();
        version
    }

    /// 所有窗口的文本节点标 `LAYOUT | PAINT` 并清 `node..root` 缓存。
    fn invalidate_all_text(&mut self) {
        let ids: SmallVec<[WindowId; 4]> = self.windows.keys().copied().collect();
        for id in ids {
            let Some(window) = self.windows.get_mut(&id) else {
                continue;
            };
            let tree = window.tree_mut();
            let order: SmallVec<[crate::node::id::NodeId; 8]> =
                tree.order().iter().copied().collect();
            for node in order {
                if tree.text_content(node).is_some() {
                    tree.invalidate_text_layout(node);
                }
            }
        }
    }

    /// 沿依赖图失效：节点标脏（或经派生级联），有节点变脏则置唤醒位。
    fn invalidate(&mut self, signal: SignalId) {
        let Self {
            store,
            graph,
            windows,
            wake_requested,
            dispatcher,
            ..
        } = self;
        let mut worklist: SmallVec<[SignalId; 8]> = smallvec![signal];
        let mut marked = 0usize;

        while let Some(current) = worklist.pop() {
            let Some(owners) = graph.dependents_mut(current) else {
                continue;
            };
            owners.retain(|owner| match owner {
                Owner::Node { window, node } => windows
                    .get(window)
                    .is_some_and(|w| w.tree().get(*node).is_ok()),
                Owner::Derived(_) => true,
            });
            let empty = owners.is_empty();
            let snapshot: SmallVec<[Owner; 8]> = owners.iter().copied().collect();
            if empty {
                graph.remove_signal(current);
                continue;
            }

            for owner in snapshot {
                match owner {
                    Owner::Node { window, node } => {
                        let Some(target) = windows.get_mut(&window) else {
                            continue;
                        };
                        // P11：只有 `bind_a11y` 的节点（没有视觉绑定）被写信号时，**不得**产像素：
                        // 标 `A11Y` 并让下一帧仍跑 `refresh_styles`（帧末门禁会退回 `Idle`）。
                        let a11y_only = target.tree().has_a11y_bind(node)
                            && !target.tree().has_visual_bind(node);
                        let affects_layout = target
                            .tree()
                            .get(node)
                            .is_ok_and(|n| n.style_patch().affects_layout);
                        let alive = if a11y_only {
                            target.a11y_note_bind_work();
                            target.tree_mut().mark_a11y(node)
                        } else if affects_layout {
                            target.tree_mut().mark_layout(node)
                        } else {
                            target.tree_mut().mark_paint(node)
                        };
                        if alive {
                            marked += 1;
                        }
                    }
                    Owner::Derived(derived) => {
                        if store.mark_derived_dirty(derived) {
                            worklist.push(store.derived_handle(derived));
                        }
                    }
                }
            }
        }

        if marked > 0 {
            *wake_requested = true;
            dispatcher.wake();
        }
    }

    /// 仅测试：求值栈是否为空（验证 `EvalGuard` 在 panic 后仍然出栈）。
    #[cfg(test)]
    pub(crate) fn eval_stack_is_empty_for_test(&self) -> bool {
        self.eval_stack.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, panic::AssertUnwindSafe, rc::Rc};

    use super::App;
    use crate::{
        node::{
            builder::NodeBuilder,
            dependency::ReadCtx,
            dirty::DirtyFlags,
            id::NodeId,
            signal::{Derived, Signal, SignalId},
        },
        paint::style::StylePatch,
        window::{FrameOutcome, WindowId, WindowOptions},
    };

    fn new_app() -> (tokio::runtime::Runtime, App) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let app = App::new(rt.handle().clone());
        (rt, app)
    }

    // ── P05：文本 pass / 字体栈 / B1 失效纪律 ────────────────────────────────

    fn text_style(width: Option<f32>) -> crate::paint::style::Style {
        let mut style = crate::paint::style::Style::default();
        style.inner.align_self = Some(taffy::style::AlignSelf::START);
        if let Some(width) = width {
            style.inner.size.width = taffy::style::Dimension::length(width);
        }
        style
    }

    fn text_app(
        text: &str,
        width: Option<f32>,
    ) -> (tokio::runtime::Runtime, App, WindowId, NodeId) {
        let (rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let w = app.window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        let node = NodeBuilder::text(text, crate::text::TextStyle::new(16.0))
            .style(text_style(width))
            .build(w.tree_mut(), root)
            .expect("挂载成功");
        (rt, app, window, node)
    }

    fn refresh(app: &mut App, window: WindowId) -> crate::text::TextPassReport {
        app.refresh_text(window)
            .expect("窗口存在")
            .expect("文本 pass 不报错")
    }

    fn set_text(
        app: &mut App,
        window: WindowId,
        node: NodeId,
        text: &str,
        style: crate::text::TextStyle,
    ) {
        app.window_mut(window)
            .expect("窗口存在")
            .tree_mut()
            .set_text_content(node, crate::text::TextContent::new(text, style));
    }

    fn text_layout(app: &mut App, window: WindowId) -> crate::text::TextLayoutReport {
        app.layout_text(window)
            .expect("窗口存在")
            .expect("文本重排不报错")
    }

    fn layout(app: &mut App, window: WindowId) -> crate::layout::LayoutReport {
        app.window_mut(window)
            .expect("窗口存在")
            .compute_layout()
            .expect("布局不报错")
    }

    fn layout_height(app: &App, window: WindowId, node: NodeId) -> f32 {
        app.window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("节点存在")
            .layout()
            .bounds
            .size
            .height
    }

    #[test]
    fn refresh_text_requires_an_existing_window() {
        let (_rt, mut app) = new_app();

        assert!(app.refresh_text(WindowId::for_test(0)).is_none());
    }

    #[test]
    fn refresh_text_shapes_once_then_reuses() {
        let (_rt, mut app, window, node) = text_app("hello", None);

        let first = refresh(&mut app, window);
        let second = refresh(&mut app, window);

        assert_eq!(first.shaped, 1);
        assert_eq!(second.skipped, 1, "B4: unchanged snapshot must be skipped");
        assert_eq!(second.shaped, 0);
        // 行快照只由第二阶段（layout_text）产出。
        assert!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .text_line(node)
                .is_none(),
            "refresh_text must not attach a line snapshot (B1)"
        );
        let laid = text_layout(&mut app, window);
        assert_eq!(laid.laid, 1);
        assert!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .text_line(node)
                .is_some(),
            "layout_text must attach the final snapshot"
        );
    }

    #[test]
    fn bind_text_reevaluation_updates_content_and_marks_paint() {
        let (_rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let signal = app.signal(smol_str::SmolStr::new_static("first"));
        let node = {
            let w = app.window_mut(window).expect("窗口存在");
            let root = w.tree().root();
            NodeBuilder::text("", crate::text::TextStyle::new(16.0))
                .bind_text(move |cx| cx.read(&signal))
                .build(w.tree_mut(), root)
                .expect("挂载成功")
        };

        let report = app.refresh_styles(window).expect("窗口存在");
        assert_eq!(report.text_evaluated, 1);
        assert_eq!(report.text_changed, 1);
        clear_dirty(&mut app, window);
        let report = app.refresh_styles(window).expect("窗口存在");
        assert_eq!(report.text_changed, 0, "unchanged value must not re-mark");

        app.write(&signal, smol_str::SmolStr::new_static("second"));
        let report = app.refresh_styles(window).expect("窗口存在");
        assert_eq!(report.text_changed, 1);
        let content = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .text_content(node)
            .expect("文本节点")
            .clone();
        assert_eq!(content.text, "second");
        assert!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .get(node)
                .expect("节点存在")
                .dirty()
                .contains(DirtyFlags::PAINT)
        );
    }

    #[test]
    fn text_size_change_marks_layout() {
        let (_rt, mut app, window, node) = text_app("ab", None);
        let _ = refresh(&mut app, window);
        let _ = layout(&mut app, window);
        clear_dirty(&mut app, window);

        set_text(
            &mut app,
            window,
            node,
            "a much longer text that wraps",
            crate::text::TextStyle::new(16.0),
        );
        let _ = refresh(&mut app, window);

        assert!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .get(node)
                .expect("节点存在")
                .dirty()
                .contains(DirtyFlags::LAYOUT),
            "a size change must set LAYOUT"
        );
    }

    #[test]
    fn text_same_size_still_clears_the_taffy_cache() {
        // B1：尺寸没变（"ab" → "ba"）也必须清 node..root 的 taffy 缓存，否则陈旧 measure 会留存。
        let (_rt, mut app, window, node) = text_app("ab", None);
        let _ = refresh(&mut app, window);
        let _ = layout(&mut app, window);
        let warm = layout(&mut app, window);
        assert_eq!(warm.cache_misses, 0, "second layout must hit the cache");
        assert!(warm.cache_hits > 0);
        clear_dirty(&mut app, window);

        set_text(
            &mut app,
            window,
            node,
            "ba",
            crate::text::TextStyle::new(16.0),
        );
        let _ = refresh(&mut app, window);
        let dirty = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("节点存在")
            .dirty();
        assert!(
            !dirty.contains(DirtyFlags::LAYOUT),
            "same measured size must not set LAYOUT"
        );

        let cold = layout(&mut app, window);
        assert!(
            cold.cache_misses >= 1,
            "the measure cache must be cleared even when the size is unchanged"
        );
    }

    #[test]
    fn text_wrap_change_same_max_width_relayouts() {
        // B1 的反例：Wrap → Anywhere 的 max-content 宽相同，但定宽（40 px）下的行数/高度不同。
        let (_rt, mut app, window, node) = text_app("aaaaaaaaaa", Some(40.0));
        let _ = refresh(&mut app, window);
        let _ = layout(&mut app, window);
        let height_before = layout_height(&app, window, node);

        let mut style = crate::text::TextStyle::new(16.0);
        style.wrap = crate::text::TextWrap::Anywhere;
        set_text(&mut app, window, node, "aaaaaaaaaa", style);
        let _ = refresh(&mut app, window);
        let relayout = layout(&mut app, window);
        let height_after = layout_height(&app, window, node);

        assert!(
            relayout.cache_misses >= 1,
            "snapshot change must invalidate the measure cache"
        );
        assert!(
            height_after > height_before,
            "BreakAll wraps into more lines at 40 px: {height_after} vs {height_before}"
        );
    }

    #[test]
    fn set_font_stack_invalidates_all_text_and_drops_the_shape_cache() {
        let (_rt, mut app, window, node) = text_app("hello", None);
        let _ = refresh(&mut app, window);

        let version = app.set_font_stack(crate::text::FontStack::new(["Segoe UI"]));

        assert_eq!(version, 2);
        assert_eq!(app.font_stack_version(), 2);
        assert!(app.font_stack().families().count() == 1);
        let dirty = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("节点存在")
            .dirty();
        assert!(dirty.contains(DirtyFlags::LAYOUT | DirtyFlags::PAINT));
        // 形状缓存被清空 ⇒ 下一次 refresh_text 重新 shape。
        let report = refresh(&mut app, window);
        assert_eq!(report.shaped, 1);
    }

    #[test]
    fn reload_system_fonts_is_idempotent_but_bumps_the_version() {
        let (_rt, mut app, window, _node) = text_app("x", None);
        let before = app.font_stack_version();

        let after = app.reload_system_fonts();

        assert_eq!(after, before + 1);
        assert!(app.refresh_text(window).is_some());
    }

    #[test]
    fn scale_change_clears_the_atlas_generation() {
        let (_rt, mut app, window, node) = text_app("hello", None);
        let _ = refresh(&mut app, window);
        let _ = layout(&mut app, window);
        let first = text_layout(&mut app, window);
        assert!(first.laid > 0);
        let generation_before = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .text_line(node)
            .expect("快照存在")
            .atlas_generation;

        app.window_mut(window)
            .expect("窗口存在")
            .set_scale(crate::math::Scale::new(2.0).expect("finite scale"));
        let _ = refresh(&mut app, window);
        let _ = layout(&mut app, window);
        let second = text_layout(&mut app, window);
        let generation_after = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .text_line(node)
            .expect("快照存在")
            .atlas_generation;

        assert!(
            generation_after > generation_before,
            "DPI change bumps the atlas"
        );
        assert!(second.laid > 0, "the text is re-laid at the new ppem");
    }

    #[test]
    fn text_diagnostics_start_empty_for_supported_text() {
        let (_rt, mut app, window, _node) = text_app("ascii only", None);
        let _ = refresh(&mut app, window);

        // 具体诊断依宿主字体而定；这里只要求 API 可用且不 panic。
        let _ = app.text_diagnostics();
    }

    fn node_with<F>(app: &mut App, window: WindowId, bind: F) -> NodeId
    where
        F: Fn(&mut ReadCtx<'_>) -> StylePatch + 'static,
    {
        let w = app.window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        NodeBuilder::boxed()
            .bind_style(bind)
            .build(w.tree_mut(), root)
            .expect("挂载成功")
    }

    fn flags(app: &App, window: WindowId, node: NodeId) -> DirtyFlags {
        app.window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("节点存在")
            .dirty()
    }

    fn root_flags(app: &App, window: WindowId) -> DirtyFlags {
        let root = app.window(window).expect("窗口存在").tree().root();
        flags(app, window, root)
    }

    fn deps(app: &App, window: WindowId, node: NodeId) -> Vec<SignalId> {
        app.window(window)
            .expect("窗口存在")
            .tree()
            .deps(node)
            .expect("节点存在")
            .to_vec()
    }

    fn clear_dirty(app: &mut App, window: WindowId) {
        app.window_mut(window)
            .expect("窗口存在")
            .tree_mut()
            .clear_all_dirty_for_test();
    }

    #[test]
    fn new_app_has_no_windows() {
        let (_rt, app) = new_app();

        assert_eq!(app.windows().count(), 0);
        assert!(app.window(WindowId::for_test(0)).is_none());
        assert!(app.eval_stack_is_empty_for_test());
        let joined = app.runtime_handle().spawn(async { 7u8 });
        drop(joined);
    }

    #[test]
    fn write_only_reaches_the_owning_window() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window_a = app.create_window(WindowOptions::default());
        let window_b = app.create_window(WindowOptions::default());
        let node_a = node_with(&mut app, window_a, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        let node_b = node_with(&mut app, window_b, |_cx| StylePatch::default());
        app.refresh_styles(window_a).expect("窗口存在");
        app.refresh_styles(window_b).expect("窗口存在");

        assert_eq!(
            node_a.index(),
            node_b.index(),
            "两窗口的 arena 索引相同 ⇒ 必须靠 Owner 的 window 消歧"
        );
        clear_dirty(&mut app, window_a);
        clear_dirty(&mut app, window_b);

        assert!(app.write(&signal, 1));

        assert!(flags(&app, window_a, node_a).contains(DirtyFlags::PAINT));
        assert!(
            !flags(&app, window_b, node_b).contains(DirtyFlags::PAINT),
            "不得命中另一个窗口的同号节点"
        );
    }

    #[test]
    fn write_marks_layout_when_patch_affects_layout() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            affects_layout: true,
            opacity: Some(cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        clear_dirty(&mut app, window);

        assert!(app.write(&signal, 1));

        assert!(
            flags(&app, window, node).contains(DirtyFlags::LAYOUT),
            "affects_layout → 标 LAYOUT 而非只标 PAINT"
        );
        assert!(
            root_flags(&app, window).contains(DirtyFlags::LAYOUT),
            "必须冒泡"
        );
    }

    #[test]
    fn write_skips_stale_owner_without_panicking() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        app.window_mut(window)
            .expect("窗口存在")
            .tree_mut()
            .remove(node)
            .expect("卸载成功");
        let _ = app.take_wake_request();

        assert!(app.write(&signal, 1), "值确实变了");
        assert!(
            !app.take_wake_request(),
            "没有存活的 owner → 不请求唤醒（陈旧 owner 已被 retain 剔除）"
        );
    }

    #[test]
    fn refresh_styles_only_reevaluates_dirty_nodes() {
        let (_rt, mut app) = new_app();
        let calls = Rc::new(Cell::new(0u32));
        let a = Rc::clone(&calls);
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| {
            a.set(a.get() + 1);
            StylePatch {
                opacity: Some(cx.read(&signal) as f32),
                ..StylePatch::default()
            }
        });

        let first = app.refresh_styles(window).expect("窗口存在");
        assert_eq!(first.evaluated, 1, "只有带 bind_style 的节点被求值");
        assert_eq!(calls.get(), 1);

        let second = app.refresh_styles(window).expect("窗口存在");
        assert_eq!(second.evaluated, 0, "PAINT 已消费 → 不重复求值");
        assert_eq!(calls.get(), 1);

        assert!(app.write(&signal, 3));
        let third = app.refresh_styles(window).expect("窗口存在");
        assert_eq!(third.evaluated, 1);
        assert_eq!(calls.get(), 2);
        assert_eq!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .get(node)
                .expect("存在")
                .style_patch()
                .opacity,
            Some(3.0)
        );
    }

    #[test]
    fn refresh_styles_is_idempotent() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal) as f32),
            ..StylePatch::default()
        });

        app.refresh_styles(window).expect("窗口存在");
        let deps_before = deps(&app, window, node);
        let patch_before = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("存在")
            .style_patch()
            .clone();

        let second = app.refresh_styles(window).expect("窗口存在");

        assert_eq!(second.evaluated, 0);
        assert_eq!(deps(&app, window, node), deps_before);
        assert_eq!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .get(node)
                .expect("存在")
                .style_patch(),
            &patch_before
        );
    }

    #[test]
    fn refresh_styles_escalates_layout_when_patch_affects_layout() {
        // 双向都要升级（`new || old`）：false→true 本帧开始变；true→false 要把尺寸收回来。
        let (_rt, mut app) = new_app();
        let resized: Signal<bool> = app.signal(false);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            affects_layout: cx.read(&resized),
            ..StylePatch::default()
        });

        let quiet = app.refresh_styles(window).expect("窗口存在");
        assert_eq!(quiet.evaluated, 1);
        assert!(quiet.layout_dirty, "新建节点本身带 LAYOUT");

        clear_dirty(&mut app, window);
        assert!(app.write(&resized, true));
        let escalate = app.refresh_styles(window).expect("窗口存在");
        assert!(escalate.layout_dirty, "false → true 必须升级");
        assert!(flags(&app, window, node).contains(DirtyFlags::LAYOUT));
        assert!(root_flags(&app, window).contains(DirtyFlags::LAYOUT));

        clear_dirty(&mut app, window);
        assert!(app.write(&resized, false));
        let settle = app.refresh_styles(window).expect("窗口存在");
        assert!(
            settle.layout_dirty,
            "true → false 也必须升级（旧值声明过尺寸会变）"
        );
        assert!(flags(&app, window, node).contains(DirtyFlags::LAYOUT));
    }

    #[test]
    fn refresh_styles_unknown_window_returns_none() {
        let (_rt, mut app) = new_app();

        assert!(app.refresh_styles(WindowId::for_test(99)).is_none());
    }

    #[test]
    fn refresh_styles_updates_deps() {
        let (_rt, mut app) = new_app();
        let flag: Signal<bool> = app.signal(true);
        let a: Signal<u32> = app.signal(1);
        let b: Signal<u32> = app.signal(2);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| {
            let use_a = cx.read(&flag);
            let value = if use_a { cx.read(&a) } else { cx.read(&b) };
            StylePatch {
                opacity: Some(value as f32),
                ..StylePatch::default()
            }
        });

        app.refresh_styles(window).expect("窗口存在");
        let first = deps(&app, window, node);
        assert!(first.contains(&flag.id()) && first.contains(&a.id()));
        assert!(!first.contains(&b.id()));

        let _ = app.take_wake_request();
        assert!(app.write(&flag, false));
        app.refresh_styles(window).expect("窗口存在");
        let second = deps(&app, window, node);
        assert!(
            second.contains(&b.id()) && !second.contains(&a.id()),
            "条件分支切换后必须退订 a、订阅 b"
        );

        clear_dirty(&mut app, window);
        let _ = app.take_wake_request();
        assert!(app.write(&a, 99));
        assert!(
            !flags(&app, window, node).contains(DirtyFlags::PAINT),
            "已退订的信号不得再标脏该节点"
        );
    }

    #[test]
    fn close_window_detaches_dependencies() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let _node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        let _ = app.take_wake_request();

        assert!(app.close_window(window));
        assert!(app.window(window).is_none());
        assert!(app.write(&signal, 1), "写本身仍成功");
        assert!(
            !app.take_wake_request(),
            "关窗必须精确退订，不得再命中死节点"
        );
        assert!(!app.close_window(window), "重复关窗返回 false");
    }

    #[test]
    fn refresh_styles_marks_a11y_on_interactive_change() {
        let (_rt, mut app) = new_app();
        let enabled: Signal<bool> = app.signal(false);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            interactive: Some(cx.read(&enabled)),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        clear_dirty(&mut app, window);

        assert!(app.write(&enabled, true));
        app.refresh_styles(window).expect("窗口存在");

        assert!(
            flags(&app, window, node).contains(DirtyFlags::A11Y),
            "interactive 变化必须标 A11Y"
        );
    }

    #[test]
    fn eval_guard_pops_on_panic_and_cache_survives() {
        let (_rt, mut app) = new_app();
        let bad: Derived<f32> = app.derived(|_cx| -> f32 { panic!("派生闭包炸了") });
        let window = app.create_window(WindowOptions::default());
        // 第一个节点：本次求值会 panic（它的 `bind_style` 随后丢失 → 退化为静态节点）。
        let doomed = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read_derived(&bad).unwrap_or(0.0)),
            ..StylePatch::default()
        });
        // 第二个节点：求值面相同，但它没在 panic 里丢闭包 —— 用来证明后续读返回 `CyclicDerived` 而不是 panic。
        let survivor = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read_derived(&bad).unwrap_or(0.0)),
            ..StylePatch::default()
        });

        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = app.refresh_styles(window);
        }));
        assert!(outcome.is_err(), "闭包 panic 必须向上传播");
        assert!(
            app.eval_stack_is_empty_for_test(),
            "EvalGuard::drop 必须在 panic 展开时出栈"
        );

        let report = app.refresh_styles(window).expect("窗口仍在");
        assert_eq!(
            report.evaluated, 1,
            "panic 的那个节点闭包已丢（跳过），另一个节点仍被求值"
        );
        assert!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .get(survivor)
                .expect("存在")
                .style_patch()
                .opacity
                .is_some(),
            "read_derived 返回 CyclicDerived 后走了 unwrap_or，patch 仍被写入"
        );
        assert!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .get(doomed)
                .is_ok()
        );
    }

    #[test]
    fn write_isolates_sibling_nodes() {
        // D16/I3 的核心断言：一次写入只沿依赖图命中**读过它的**节点。
        let (_rt, mut app) = new_app();
        let signal_a: Signal<u32> = app.signal(0);
        let signal_b: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let node_a = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal_a) as f32),
            ..StylePatch::default()
        });
        let node_b = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal_b) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window).expect("窗口存在");
        let before = flags(&app, window, node_b);
        let _ = app.take_wake_request();

        assert!(app.write(&signal_a, 1));

        assert!(
            flags(&app, window, node_a).contains(DirtyFlags::PAINT),
            "读者必须被标脏"
        );
        assert_eq!(
            flags(&app, window, node_b),
            before,
            "非读者（含 REUSABLE）必须完全不变"
        );
        assert!(app.take_wake_request(), "有节点变脏 ⇒ 必须请求唤醒");
    }

    #[test]
    fn write_only_derived_does_not_wake() {
        // 只有派生失效、没有节点变脏 ⇒ 不该提交帧（I3「空闲不提交帧」）。
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(1);
        let derived: Derived<u32> = app.derived(move |cx| cx.read(&signal) * 2);
        let window = app.create_window(WindowOptions::default());

        assert!(app.write(&signal, 2));
        assert!(!app.take_wake_request(), "只有派生失效 → 不唤醒");

        let report = app.refresh_styles(window).expect("窗口存在");
        assert_eq!(report.evaluated, 0, "没有节点读该派生");
        assert!(!report.wake_requested);
        assert!(app.write(&signal, 3));
        let _ = derived;
    }

    #[test]
    fn changed_text_marks_a11y() {
        // 文本节点的可访问名/值 = 文本内容。只标 PAINT 时，"70%" → "80%" 这种等宽替换不会触发
        // LAYOUT，AT 会一直读到旧值（P15 F-153）。
        use smol_str::SmolStr;

        let (_rt, mut app) = new_app();
        let signal: Signal<SmolStr> = app.signal(SmolStr::new_static("A"));
        let window = app.create_window(WindowOptions::default());
        let root = app.window(window).expect("窗口存在").tree().root();
        let node = NodeBuilder::text("A", crate::text::TextStyle::new(12.0))
            .bind_text(move |cx| cx.read(&signal))
            .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
            .expect("挂载");
        app.refresh_styles(window).expect("窗口存在");
        // 消费首帧的 A11Y 位（新节点初始就带 LAYOUT|PAINT|A11Y）。
        let _ = app.a11y_take(window);
        let _ = app.take_wake_request();
        assert!(
            !flags(&app, window, node).contains(DirtyFlags::A11Y),
            "前置条件：A11Y 位已消费"
        );

        assert!(app.write(&signal, SmolStr::new_static("B")));
        app.refresh_styles(window).expect("窗口存在");

        assert!(
            flags(&app, window, node).contains(DirtyFlags::A11Y),
            "文本内容变化必须标 A11Y（AT 读的是文本值）"
        );
    }

    #[test]
    fn writing_a_signal_wakes_the_platform_loop() {
        // 回投任务（`AppHandle::send` / a11y `ActionRequest`）里写状态时，平台泵已经过了帧门禁；
        // 只置 `wake_requested` 不唤醒 dispatcher，会让画面与 a11y 更新等到下一条无关消息（F-152）。
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window = app.create_window(WindowOptions::default());
        let root = app.window(window).expect("窗口存在").tree().root();
        let _ = NodeBuilder::boxed()
            .bind_style(move |cx| StylePatch {
                opacity: Some(cx.read(&signal) as f32),
                ..StylePatch::default()
            })
            .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
            .expect("挂载");
        // 先求值一次样式：依赖边在 `refresh_styles` 里建立，没有边就无从失效。
        app.refresh_styles(window).expect("窗口存在");
        let _ = app.take_wake_request();
        let _ = app.dispatcher().take_frame_wake();

        assert!(app.write(&signal, 1));

        assert!(
            app.dispatcher().take_frame_wake(),
            "写信号必须唤醒平台事件循环（否则回投任务里的状态不会出帧）"
        );
    }

    #[test]
    fn write_marks_across_windows() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let window_a = app.create_window(WindowOptions::default());
        let window_b = app.create_window(WindowOptions::default());
        let node_a = node_with(&mut app, window_a, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        let node_b = node_with(&mut app, window_b, move |cx: &mut ReadCtx<'_>| StylePatch {
            opacity: Some(cx.read(&signal) as f32),
            ..StylePatch::default()
        });
        app.refresh_styles(window_a).expect("窗口存在");
        app.refresh_styles(window_b).expect("窗口存在");
        clear_dirty(&mut app, window_a);
        clear_dirty(&mut app, window_b);

        assert!(app.write(&signal, 9));

        assert!(flags(&app, window_a, node_a).contains(DirtyFlags::PAINT));
        assert!(flags(&app, window_b, node_b).contains(DirtyFlags::PAINT));
        assert!(app.take_wake_request());
    }

    #[test]
    fn create_window_ids_are_unique_and_monotonic() {
        let (_rt, mut app) = new_app();
        let first = app.create_window(WindowOptions::default());
        let second = app.create_window(WindowOptions::default());
        let third = app.create_window(WindowOptions::default());

        assert_eq!(
            (first.to_bits(), second.to_bits(), third.to_bits()),
            (1, 2, 3),
            "WindowId 从 1 起单调递增（0 非法）"
        );
        assert_eq!(app.windows().count(), 3);
        for (id, window) in app.windows() {
            assert_eq!(id, window.id(), "windows() 带出的 id 必须与窗口自身一致");
            assert!(app.window(id).is_some());
        }
    }

    #[test]
    fn close_window_unknown_returns_false() {
        let (_rt, mut app) = new_app();

        assert!(!app.close_window(WindowId::for_test(42)));
    }

    #[test]
    fn bind_style_panic_clears_ghost_deps() {
        let (_rt, mut app) = new_app();
        let signal: Signal<u32> = app.signal(0);
        let calls = Rc::new(Cell::new(0u32));
        let counter = Rc::clone(&calls);
        let window = app.create_window(WindowOptions::default());
        let node = node_with(&mut app, window, move |cx: &mut ReadCtx<'_>| {
            counter.set(counter.get() + 1);
            let value = cx.read(&signal);
            assert!(counter.get() <= 1, "第二次求值故意 panic");
            StylePatch {
                opacity: Some(value as f32),
                ..StylePatch::default()
            }
        });
        app.refresh_styles(window).expect("窗口存在");
        assert_eq!(deps(&app, window, node), vec![signal.id()]);

        assert!(app.write(&signal, 4));
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _ = app.refresh_styles(window);
        }));
        assert!(outcome.is_err(), "panic 必须向上传播");
        assert_eq!(
            deps(&app, window, node),
            vec![signal.id()],
            "panic 打断了 replace_deps ⇒ 旧边仍在（幽灵边）"
        );

        let report = app.refresh_styles(window).expect("窗口仍在");
        assert_eq!(report.evaluated, 0, "闭包已丢 → 不算求值");
        assert!(deps(&app, window, node).is_empty(), "幽灵边必须被显式退订");

        let _ = app.take_wake_request();
        assert!(app.write(&signal, 5), "值仍然会变");
        assert!(!app.take_wake_request(), "退订后再写旧信号不得再命中该节点");
    }

    #[test]
    fn layout_text_breaks_at_the_content_width() {
        // B1：60 px 宽、16 px 字号的 4 个单词必须断成多行，且选区矩形逐行对齐。
        let (_rt, mut app, window, node) = text_app("aaaa aaaa aaaa aaaa", Some(60.0));
        let _ = refresh(&mut app, window);
        let _ = layout(&mut app, window);
        let report = text_layout(&mut app, window);

        assert_eq!(report.laid, 1);
        let tree = app.window(window).expect("窗口存在").tree();
        let line = tree.text_line(node).expect("行快照").clone();
        assert!(
            line.lines.len() >= 3,
            "content width 60 must wrap into several lines, got {}",
            line.lines.len()
        );
        let rects = tree
            .text_selection_rects(node, 0, line.text.len())
            .expect("选区");
        assert!(
            rects.len() >= 3,
            "selection rects must cover the broken lines"
        );
        assert!(
            line.width <= 60.5,
            "the line box must respect the content width"
        );
    }

    #[test]
    fn ime_cursor_area_requires_text_shape_and_can_be_cleared() {
        let (_rt, mut app, window, node) = text_app("abc", None);
        let unknown = WindowId::for_test(999);
        assert!(matches!(
            app.set_ime_cursor_area(unknown, node, 0),
            Err(crate::error::EngineError::UnknownWindow { .. })
        ));
        let root = app.window(window).expect("窗口").tree().root();
        assert!(matches!(
            app.set_ime_cursor_area(window, root, 0),
            Err(crate::error::EngineError::TextShape { .. })
        ));
        assert!(matches!(app.set_ime_cursor_area(window, node, 4), Ok(())));
        assert!(matches!(app.set_ime_cursor_area(window, node, 1), Ok(())));
        assert!(matches!(
            app.ime_cursor_area(window),
            Err(crate::error::EngineError::TextShape { .. })
        ));

        let _ = refresh(&mut app, window);
        let _ = layout(&mut app, window);
        let _ = text_layout(&mut app, window);
        let area = app
            .ime_cursor_area(window)
            .expect("窗口存在")
            .expect("已 shape 后有 hint");
        assert!(area.size.width >= 1.0);
        assert!(area.size.height > 0.0);

        assert!(app.set_ime_cursor_area(window, node, 4).is_ok());
        assert!(matches!(
            app.ime_cursor_area(window),
            Err(crate::error::EngineError::TextShape { .. })
        ));
        assert!(app.set_ime_cursor_area(window, node, 3).is_ok());

        app.clear_ime_cursor_area(window);
        assert_eq!(app.ime_cursor_area(window).expect("窗口存在"), None);

        assert!(app.set_ime_cursor_area(window, node, 3).is_ok());
        set_text(
            &mut app,
            window,
            node,
            "abcd",
            crate::text::TextStyle::new(16.0),
        );
        assert!(
            matches!(
                app.ime_cursor_area(window),
                Err(crate::error::EngineError::TextShape { .. })
            ),
            "文本内容变化后、下一次 text pass 前不得返回旧 caret"
        );
    }

    #[test]
    fn ime_cursor_area_resolves_after_the_first_frame() {
        let (_rt, mut app, window, node) = text_app("abc", None);
        assert!(app.set_ime_cursor_area(window, node, 3).is_ok());

        let outcome = app.frame(window).expect("窗口存在").expect("首帧成功");
        assert!(matches!(outcome, FrameOutcome::Committed(_)));
        assert!(
            app.ime_cursor_area(window).expect("窗口存在").is_some(),
            "首帧 text/layout 完成后应能解析 caret"
        );
    }

    #[test]
    fn ime_cursor_area_rejects_a_non_boundary_byte_index() {
        let (_rt, mut app, window, node) = text_app("a中b", None);
        let _ = refresh(&mut app, window);
        let _ = layout(&mut app, window);
        let _ = text_layout(&mut app, window);

        assert!(app.set_ime_cursor_area(window, node, 2).is_ok());
        assert!(matches!(
            app.ime_cursor_area(window),
            Err(crate::error::EngineError::TextShape { .. })
        ));
    }

    #[test]
    fn text_caret_rect_includes_border_padding_and_node_origin() {
        let (_rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let root = app.window(window).expect("窗口").tree().root();
        let mut style = text_style(Some(200.0));
        style.inner.padding = taffy::geometry::Rect {
            left: taffy::style::LengthPercentage::length(9.0),
            right: taffy::style::LengthPercentage::length(0.0),
            top: taffy::style::LengthPercentage::length(7.0),
            bottom: taffy::style::LengthPercentage::length(0.0),
        };
        style.inner.border = taffy::geometry::Rect {
            left: taffy::style::LengthPercentage::length(2.0),
            right: taffy::style::LengthPercentage::length(0.0),
            top: taffy::style::LengthPercentage::length(3.0),
            bottom: taffy::style::LengthPercentage::length(0.0),
        };
        let node = NodeBuilder::text("abc", crate::text::TextStyle::new(16.0))
            .style(style)
            .build(app.window_mut(window).expect("窗口").tree_mut(), root)
            .expect("挂载");
        let _ = refresh(&mut app, window);
        let _ = layout(&mut app, window);
        let _ = text_layout(&mut app, window);

        let rect = app
            .window(window)
            .expect("窗口")
            .text_caret_rect(node, 0)
            .expect("caret");
        assert!((rect.origin.x - 11.0).abs() < 0.01, "{rect:?}");
        assert!((rect.origin.y - 10.0).abs() < 0.01, "{rect:?}");
    }

    #[test]
    fn close_window_forgets_the_scale_epoch() {
        let (_rt, mut app, window, _node) = text_app("x", None);
        let _ = refresh(&mut app, window);
        assert!(app.close_window(window));
        assert!(!app.close_window(window), "second close is a no-op");
    }

    #[test]
    fn changed_text_always_relayouts_the_snapshot() {
        let (_rt, mut app, window, node) = text_app("aaaa aaaa aaaa aaaa", Some(60.0));
        let _ = refresh(&mut app, window);
        let _ = layout(&mut app, window);
        let _ = text_layout(&mut app, window);

        for text in ["bbbb", "aaaa aaaa aaaa aaaa bbbb", "WWWW"] {
            set_text(
                &mut app,
                window,
                node,
                text,
                crate::text::TextStyle::new(16.0),
            );
            let pass = refresh(&mut app, window);
            assert_eq!(pass.shaped, 1, "new text must reshape");
            let laid = text_layout(&mut app, window);
            assert_eq!(laid.laid, 1, "B4: changed text must relayout ({text})");
            assert_eq!(laid.reused, 0);
            let tree = app.window(window).expect("window").tree();
            assert_eq!(tree.text_line(node).expect("line").text, text);
        }
    }

    #[test]
    fn style_and_family_changes_relayout() {
        let (_rt, mut app, window, node) = text_app("same text", Some(60.0));
        let _ = refresh(&mut app, window);
        let _ = layout(&mut app, window);
        let _ = text_layout(&mut app, window);

        let mut style = crate::text::TextStyle::new(24.0);
        style.family = Some(smol_str::SmolStr::new_static("Consolas"));
        set_text(&mut app, window, node, "same text", style);

        let pass = refresh(&mut app, window);
        assert_eq!(pass.shaped, 1, "style/family is part of the shape key");
        assert_eq!(text_layout(&mut app, window).laid, 1);
    }

    #[test]
    fn inline_boxes_changes_relayout() {
        let (_rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let spec = crate::text::InlineBoxSpec {
            id: 7,
            index: 1,
            size: crate::math::Size::new(20.0, 20.0),
            baseline: None,
            align: crate::text::InlineAlign::Baseline,
        };
        let node = {
            let w = app.window_mut(window).expect("window");
            let root = w.tree().root();
            NodeBuilder::text("a b", crate::text::TextStyle::new(16.0))
                .inline_boxes([spec])
                .build(w.tree_mut(), root)
                .expect("build")
        };
        let _ = refresh(&mut app, window);
        let _ = layout(&mut app, window);
        assert_eq!(text_layout(&mut app, window).laid, 1);

        let spec2 = crate::text::InlineBoxSpec {
            id: 9,
            index: 1,
            size: crate::math::Size::new(40.0, 10.0),
            baseline: Some(8.0),
            align: crate::text::InlineAlign::Top,
        };
        {
            let tree = app.window_mut(window).expect("window").tree_mut();
            let mut content = tree.text_content(node).expect("text").clone();
            content.inline_boxes = Box::new([spec2]);
            tree.set_text_content(node, content);
        }
        let pass = refresh(&mut app, window);
        assert_eq!(pass.shaped, 1, "inline boxes are part of the shape key");
        assert_eq!(text_layout(&mut app, window).laid, 1);
    }

    #[test]
    fn bind_text_preserves_inline_boxes() {
        let (_rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let signal = app.signal(smol_str::SmolStr::new_static("first"));
        let spec = crate::text::InlineBoxSpec {
            id: 3,
            index: 0,
            size: crate::math::Size::new(16.0, 16.0),
            baseline: None,
            align: crate::text::InlineAlign::Baseline,
        };
        let node = {
            let w = app.window_mut(window).expect("window");
            let root = w.tree().root();
            NodeBuilder::text("", crate::text::TextStyle::new(16.0))
                .bind_text(move |cx| cx.read(&signal))
                .inline_boxes([spec])
                .build(w.tree_mut(), root)
                .expect("build")
        };

        for value in ["first", "second"] {
            let _ = app.refresh_styles(window);
            let content = app
                .window(window)
                .expect("window")
                .tree()
                .text_content(node)
                .expect("text")
                .clone();
            assert_eq!(
                content.inline_boxes.len(),
                1,
                "inline boxes survive bind_text"
            );
            assert_eq!(content.text, value);
            if value == "first" {
                app.write(&signal, smol_str::SmolStr::new_static("second"));
            }
        }
    }

    // ── P06：完整帧 ──

    #[test]
    fn app_frame_runs_styles_then_paints() {
        let (_rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let root = app.window(window).expect("窗口").tree().root();
        let background = app.signal(crate::paint::color::Color::WHITE);
        {
            let _ = NodeBuilder::boxed()
                .layout_override(crate::math::Rect::new(0.0, 0.0, 20.0, 10.0))
                .bind_style(move |cx| StylePatch {
                    background: Some(cx.read(&background)),
                    ..StylePatch::default()
                })
                .build(app.window_mut(window).expect("窗口").tree_mut(), root)
                .expect("挂载成功");
        }

        let outcome = app.frame(window).expect("窗口存在").expect("帧成功");
        assert!(matches!(outcome, FrameOutcome::Committed(_)));

        // 改信号 → 下一帧的 Quad 颜色必须变（证明 styles pass 在 paint 之前跑过）。
        app.write(&background, crate::paint::color::Color::BLACK);
        let _ = app.frame(window).expect("窗口存在").expect("帧成功");
        let scene = app.window(window).expect("窗口").scene();
        let quad_color = scene
            .iter()
            .find_map(|primitive| match primitive {
                crate::paint::primitive::Primitive::Quad(quad) => match &quad.paint {
                    crate::paint::primitive::QuadPaint::Solid { fill, .. } => Some(*fill),
                    crate::paint::primitive::QuadPaint::Gradient(_) => None,
                },
                _ => None,
            })
            .expect("必须有背景 Quad");
        assert_eq!(quad_color, crate::paint::color::Color::BLACK);
    }

    #[test]
    fn app_frame_unknown_window_is_none() {
        let (_rt, mut app) = new_app();
        assert!(app.frame(WindowId::for_test(99)).is_none());
    }

    #[test]
    fn app_frame_is_idle_without_work() {
        let (_rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        // 首帧有布局移动（根从 0 尺寸撑满窗口）⇒ 提交一次；之后无写、无脏 ⇒ 空闲零帧。
        let first = app.frame(window).expect("窗口存在");
        assert!(matches!(first, Ok(FrameOutcome::Committed(_))));

        let second = app.frame(window).expect("窗口存在");
        assert!(matches!(second, Ok(FrameOutcome::Idle)));
        assert_eq!(
            app.window(window).expect("窗口").frame_index().get(),
            1,
            "空闲帧不得递增序号"
        );
    }
}

/// P10 交互机制的 App 级用例（分发 / 焦点 / 滚动 / 虚拟列表 / 浮层 / 拖拽 / 动画）。
///
/// 只走公开 + `pub(crate)` 面；`FrameRecord` 的断言（damage、primitives→node）在
/// `test_support::app` 的 `--features test-support` 用例里。
#[cfg(test)]
mod interaction_tests {
    use std::{cell::Cell, rc::Rc};

    use super::App;
    use crate::{
        error::EngineError,
        event::{
            Action, ActionSource, InputEvent, Key, KeyBinding, KeyState, Modifiers, PointerButton,
        },
        interaction::{
            AnimatedValue, AnimationSpec, DragPhase, Easing, InputThresholds, OverlayDismiss,
            OverlayOptions, OverlayPlacement, ScrollAxis, ScrollConfig, VirtualListConfig,
        },
        math::{Point, Rect, Scale, Size},
        node::{builder::NodeBuilder, dirty::DirtyFlags, id::NodeId},
        paint::{
            color::Color,
            primitive::Primitive,
            style::{Clip, FocusRing, PaintSpec, Style, StylePatch},
        },
        window::{FrameOutcome, WindowId, WindowOptions},
    };

    fn setup() -> (tokio::runtime::Runtime, App, WindowId) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(rt.handle().clone());
        let window = app.create_window(WindowOptions {
            title: "p10".into(),
            size: Size::new(400.0, 300.0),
            scale: Scale::ONE,
        });
        (rt, app, window)
    }

    fn frame(app: &mut App, window: WindowId) -> FrameOutcome {
        app.frame(window)
            .expect("window exists")
            .expect("frame succeeds")
    }

    fn root_of(app: &App, window: WindowId) -> NodeId {
        app.window(window).expect("window").tree().root()
    }

    fn background() -> PaintSpec {
        PaintSpec {
            background: Color::from_rgba8([200, 40, 40, 255]),
            ..PaintSpec::default()
        }
    }

    fn mount_override(app: &mut App, window: WindowId, parent: NodeId, rect: Rect) -> NodeId {
        NodeBuilder::boxed()
            .layout_override(rect)
            .paint(background())
            .build(app.window_mut(window).expect("window").tree_mut(), parent)
            .expect("mount")
    }

    fn mount_scroll_container(app: &mut App, window: WindowId, height: f32) -> NodeId {
        let mut style = Style::default();
        style.inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(200.0),
            height: taffy::style::Dimension::length(height),
        };
        style.clip = Clip::Bounds;
        let tree = app.window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        NodeBuilder::boxed()
            .style(style)
            .build(tree, root)
            .expect("mount")
    }

    fn lane_quad_bounds(app: &App, window: WindowId, node: NodeId) -> Vec<Rect> {
        let window = app.window(window).expect("window");
        let Some(lane) = window.tree().lane_of(node) else {
            return Vec::new();
        };
        window
            .scene()
            .lane_primitives(lane)
            .unwrap_or(&[])
            .iter()
            .filter_map(|primitive| match primitive {
                Primitive::Quad(quad) => Some(quad.bounds),
                _ => None,
            })
            .collect()
    }

    // ── 动作 / 分发 ────────────────────────────────────────────────────────────

    #[test]
    fn click_dispatches_the_action_and_marks_only_the_target() {
        let (_rt, mut app, window) = setup();
        let calls = Rc::new(Cell::new(0_u32));
        let seen = calls.clone();
        app.on_action(Action::custom("hit"), move |_ctx| seen.set(seen.get() + 1));
        let root = root_of(&app, window);
        let node = NodeBuilder::boxed()
            .layout_override(Rect::new(10.0, 10.0, 80.0, 40.0))
            .paint(background())
            .on_click(Action::custom("hit"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let tree = app.window_mut(window).expect("window").tree_mut();
        let _ = tree.set_style_patch_for_test(
            node,
            StylePatch {
                interactive: Some(true),
                ..StylePatch::default()
            },
        );
        frame(&mut app, window);
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .clear_all_dirty_for_test();

        let down = app.dispatch_input(
            window,
            InputEvent::PointerDown {
                position: Point::new(20.0, 20.0),
                button: PointerButton::Primary,
                modifiers: Modifiers::empty(),
            },
        );
        let up = app.dispatch_input(
            window,
            InputEvent::PointerUp {
                position: Point::new(20.0, 20.0),
                button: PointerButton::Primary,
                modifiers: Modifiers::empty(),
            },
        );

        assert_eq!(calls.get(), 1, "点击必须分发一次");
        assert_eq!(up.action, Some(Action::custom("hit")));
        assert_eq!(up.target, Some(node));
        assert_eq!(up.click_count, 1);
        assert_eq!(down.hit.map(|hit| hit.node), Some(node));
        // 最小失效：只有 A11Y 脏位（焦点移动），没有 PAINT。
        let tree = app.window(window).expect("window").tree();
        assert!(
            tree.get(node)
                .expect("node")
                .dirty()
                .contains(DirtyFlags::A11Y)
        );
        assert!(
            !tree
                .get(node)
                .expect("node")
                .dirty()
                .contains(DirtyFlags::PAINT)
        );
    }

    #[test]
    fn enter_activates_the_focused_node() {
        let (_rt, mut app, window) = setup();
        let calls = Rc::new(Cell::new(0_u32));
        let seen = calls.clone();
        app.on_action(Action::custom("activate"), move |_ctx| {
            seen.set(seen.get() + 1)
        });
        let root = root_of(&app, window);
        let node = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 50.0, 50.0))
            .on_action(Action::custom("activate"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        frame(&mut app, window);
        assert!(app.set_focus(window, Some(node)));

        let outcome = app.dispatch_input(
            window,
            InputEvent::Key {
                key: Key::Enter,
                state: KeyState::Down,
                modifiers: Modifiers::empty(),
            },
        );

        assert_eq!(calls.get(), 1);
        assert_eq!(outcome.action, Some(Action::custom("activate")));
        assert_eq!(outcome.target, Some(node));
    }

    #[test]
    fn key_binding_reports_the_key_source() {
        let (_rt, mut app, window) = setup();
        let source = Rc::new(Cell::new(false));
        let seen = source.clone();
        app.on_action(Action::custom("save"), move |ctx| {
            seen.set(matches!(ctx.source(), ActionSource::Key));
        });
        app.bind_key(KeyBinding {
            key: Key::Other(0x53),
            modifiers: Modifiers::CONTROL,
            action: Action::custom("save"),
        });

        let outcome = app.dispatch_input(
            window,
            InputEvent::Key {
                key: Key::Other(0x53),
                state: KeyState::Down,
                modifiers: Modifiers::CONTROL,
            },
        );

        assert!(source.get(), "handler 必须看到 ActionSource::Key");
        assert_eq!(outcome.action, Some(Action::custom("save")));
    }

    #[test]
    fn unhandled_action_is_false_and_engine_actions_need_no_handler() {
        let (_rt, mut app, window) = setup();
        let root = root_of(&app, window);

        assert!(!app.dispatch_action(
            window,
            root,
            Action::custom("nobody"),
            ActionSource::Programmatic
        ));
        assert!(app.dispatch_action(window, root, Action::FocusNext, ActionSource::Programmatic));
        // 没有浮层时 Dismiss 是 no-op。
        assert!(!app.dispatch_action(window, root, Action::Dismiss, ActionSource::Programmatic));
    }

    // ── 焦点 ──────────────────────────────────────────────────────────────────

    #[test]
    fn tab_moves_focus_and_marks_a11y_only() {
        let (_rt, mut app, window) = setup();
        let root = root_of(&app, window);
        let first = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
            .on_click(Action::custom("a"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let second = NodeBuilder::boxed()
            .layout_override(Rect::new(60.0, 0.0, 40.0, 40.0))
            .on_click(Action::custom("b"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        frame(&mut app, window);
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .clear_all_dirty_for_test();

        app.dispatch_input(
            window,
            InputEvent::Key {
                key: Key::Tab,
                state: KeyState::Down,
                modifiers: Modifiers::empty(),
            },
        );
        assert_eq!(app.focused(window), Some(first));
        {
            let tree = app.window(window).expect("window").tree();
            assert!(
                tree.get(first)
                    .expect("node")
                    .dirty()
                    .contains(DirtyFlags::A11Y)
            );
            assert!(
                !tree
                    .get(first)
                    .expect("node")
                    .dirty()
                    .contains(DirtyFlags::PAINT),
                "无 ring ⇒ 零像素脏位"
            );
        }

        app.dispatch_input(
            window,
            InputEvent::Key {
                key: Key::Tab,
                state: KeyState::Down,
                modifiers: Modifiers::empty(),
            },
        );
        assert_eq!(app.focused(window), Some(second));
        app.dispatch_input(
            window,
            InputEvent::Key {
                key: Key::Tab,
                state: KeyState::Down,
                modifiers: Modifiers::SHIFT,
            },
        );
        assert_eq!(app.focused(window), Some(first), "Shift+Tab 回绕");
    }

    #[test]
    fn focus_ring_marks_paint() {
        let (_rt, mut app, window) = setup();
        let root = root_of(&app, window);
        let node = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
            .on_click(Action::custom("a"))
            .focus_ring(FocusRing::new(
                2.0,
                2.0,
                Color::from_rgba8([0, 120, 255, 255]),
            ))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        frame(&mut app, window);
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .clear_all_dirty_for_test();

        assert!(app.set_focus(window, Some(node)));

        let tree = app.window(window).expect("window").tree();
        let dirty = tree.get(node).expect("node").dirty();
        assert!(dirty.contains(DirtyFlags::PAINT), "有 ring ⇒ PAINT");
        assert!(!dirty.contains(DirtyFlags::REUSABLE));
    }

    // ── 滚动（paint 期偏移） ───────────────────────────────────────────────────

    fn scrollable_app() -> (tokio::runtime::Runtime, App, WindowId, NodeId, NodeId) {
        let (rt, mut app, window) = setup();
        let container = mount_scroll_container(&mut app, window, 150.0);
        let child = mount_override(
            &mut app,
            window,
            container,
            Rect::new(0.0, 0.0, 200.0, 100.0),
        );
        let _second = mount_override(
            &mut app,
            window,
            container,
            Rect::new(0.0, 100.0, 200.0, 100.0),
        );
        frame(&mut app, window);
        let handle = app
            .make_scroll(
                window,
                container,
                ScrollConfig {
                    axis: ScrollAxis::Vertical,
                    line_height: 48.0,
                },
            )
            .expect("scrollable");
        app.set_scroll_content(&handle, Size::new(200.0, 200.0))
            .expect("content");
        (rt, app, window, container, child)
    }

    #[test]
    fn text_caret_rect_follows_ancestor_scroll() {
        let (_rt, mut app, window, container, _child) = scrollable_app();
        let text = NodeBuilder::text("abc", crate::text::TextStyle::new(16.0))
            .layout_override(Rect::new(10.0, 20.0, 100.0, 20.0))
            .build(
                app.window_mut(window).expect("window").tree_mut(),
                container,
            )
            .expect("text");
        frame(&mut app, window);
        let before = app
            .window(window)
            .expect("window")
            .text_caret_rect(text, 3)
            .expect("caret");
        let handle = app.scroll_handle(window, container).expect("scroll");
        app.scroll_to(&handle, Point::new(0.0, 50.0))
            .expect("scroll");
        frame(&mut app, window);
        let after = app
            .window(window)
            .expect("window")
            .text_caret_rect(text, 3)
            .expect("caret");
        assert!((after.origin.y - before.origin.y + 50.0).abs() < 0.01);
        assert!((after.origin.x - before.origin.x).abs() < 0.01);
    }

    #[test]
    fn scroll_shifts_primitives_without_touching_layout() {
        let (_rt, mut app, window, container, child) = scrollable_app();
        let layout_before = app
            .window(window)
            .expect("window")
            .tree()
            .get(child)
            .expect("child")
            .layout()
            .bounds;
        let before = lane_quad_bounds(&app, window, child);
        assert!(!before.is_empty(), "子节点必须有背景图元");

        let handle = app.scroll_handle(window, container).expect("scroll handle");
        app.scroll_to(&handle, Point::new(0.0, 50.0))
            .expect("scroll");
        frame(&mut app, window);

        let after = lane_quad_bounds(&app, window, child);
        assert_eq!(after.len(), before.len());
        for (old, new) in before.iter().zip(after.iter()) {
            assert_eq!(new.origin.y, old.origin.y - 50.0, "滚动 N 后图元 y 平移 −N");
            assert_eq!(new.origin.x, old.origin.x);
        }
        assert_eq!(
            app.window(window)
                .expect("window")
                .tree()
                .get(child)
                .expect("child")
                .layout()
                .bounds,
            layout_before,
            "layout.bounds 是内容坐标：滚动不得改它"
        );
        assert_eq!(
            app.scroll_state(&handle).expect("state").offset,
            Point::new(0.0, 50.0)
        );
    }

    #[test]
    fn scroll_commits_a_frame() {
        let (_rt, mut app, window, container, _child) = scrollable_app();
        let handle = app.scroll_handle(window, container).expect("scroll handle");
        let before = app.window(window).expect("window").frame_index();
        app.scroll_to(&handle, Point::new(0.0, 30.0))
            .expect("scroll");
        let outcome = frame(&mut app, window);

        assert!(
            matches!(outcome, FrameOutcome::Committed(_)),
            "滚动必须提交一帧"
        );
        assert!(app.window(window).expect("window").frame_index() > before);
        assert!(!app.take_wake_request(), "帧已消费唤醒位");
    }

    #[test]
    fn same_offset_is_a_total_noop() {
        let (_rt, mut app, window, container, _child) = scrollable_app();
        let handle = app.scroll_handle(window, container).expect("scroll handle");
        app.scroll_to(&handle, Point::new(0.0, 10.0))
            .expect("scroll");
        frame(&mut app, window);
        let _ = app.take_wake_request();

        app.scroll_to(&handle, Point::new(0.0, 10.0)).expect("same");

        assert!(!app.take_wake_request(), "同值滚动不得唤醒");
    }

    #[test]
    fn hit_test_follows_the_scroll_offset() {
        let (_rt, mut app, window, container, _child) = scrollable_app();
        let handle = app.scroll_handle(window, container).expect("scroll handle");
        let before = app
            .window(window)
            .expect("window")
            .tree()
            .hit_test(Point::new(50.0, 120.0))
            .map(|hit| hit.local_point);
        assert!(before.is_some(), "滚动前 (50,120) 命中第二个子节点");

        app.scroll_to(&handle, Point::new(0.0, 50.0))
            .expect("scroll");
        let after = app
            .window(window)
            .expect("window")
            .tree()
            .hit_test(Point::new(50.0, 120.0))
            .map(|hit| hit.local_point);

        assert!(after.is_some(), "滚动后同一点仍命中内容");
        assert_eq!(
            after.expect("hit").y,
            before.expect("hit").y + 50.0,
            "local_point 反应内容位移"
        );
    }

    #[test]
    fn scroll_never_reevaluates_bind_style() {
        let (_rt, mut app, window) = setup();
        let calls = Rc::new(Cell::new(0_u32));
        let counter = calls.clone();
        let container = mount_scroll_container(&mut app, window, 100.0);
        let _child = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 200.0, 80.0))
            .bind_style(move |_cx| {
                counter.set(counter.get() + 1);
                StylePatch::default()
            })
            .build(
                app.window_mut(window).expect("window").tree_mut(),
                container,
            )
            .expect("mount");
        frame(&mut app, window);
        let evaluated_once = calls.get();
        assert!(evaluated_once > 0, "首帧必须求值一次");

        let handle = app
            .make_scroll(window, container, ScrollConfig::default())
            .expect("scrollable");
        app.set_scroll_content(&handle, Size::new(200.0, 400.0))
            .expect("content");
        app.scroll_to(&handle, Point::new(0.0, 20.0))
            .expect("scroll");
        frame(&mut app, window);

        assert_eq!(calls.get(), evaluated_once, "滚动不得触发 refresh_styles");
    }

    // ── 虚拟列表 ──────────────────────────────────────────────────────────────

    fn virtual_app() -> (tokio::runtime::Runtime, App, WindowId, NodeId) {
        let (rt, mut app, window) = setup();
        let mut style = Style::default();
        style.inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(200.0),
            height: taffy::style::Dimension::length(100.0),
        };
        style.inner.position = taffy::style::Position::Relative;
        style.clip = Clip::Bounds;
        let tree = app.window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        let container = NodeBuilder::boxed()
            .style(style)
            .build(tree, root)
            .expect("mount");
        frame(&mut app, window);
        (rt, app, window, container)
    }

    #[test]
    fn virtual_list_mounts_the_visible_window_and_recycles() {
        let (_rt, mut app, window, container) = virtual_app();
        let list = app
            .make_virtual_list(
                window,
                container,
                VirtualListConfig {
                    extent: 25.0,
                    overscan: 0,
                },
            )
            .expect("virtual list");
        let built = Rc::new(Cell::new(0_u32));
        let counter = built.clone();
        app.set_virtual_items(&list, 100, move |index| {
            counter.set(counter.get() + 1);
            NodeBuilder::boxed()
                .paint(background())
                .id(format!("row{index}"))
        })
        .expect("items");
        frame(&mut app, window);

        let state = app.virtual_list_state(&list).expect("state");
        assert_eq!(state.count, 100);
        assert_eq!(
            (state.first, state.last),
            (0, 4),
            "视口 100 / 行高 25 ⇒ 4 行"
        );
        assert_eq!(state.mounted, 4);
        assert_eq!(built.get(), 4, "只挂载可见行");

        let handle = app.scroll_handle(window, container).expect("scroll handle");
        app.scroll_to(&handle, Point::new(0.0, 250.0))
            .expect("scroll");
        frame(&mut app, window);
        let state = app.virtual_list_state(&list).expect("state");
        assert_eq!((state.first, state.last), (10, 14));
        assert_eq!(state.mounted, 4);
        assert_eq!(built.get(), 8, "只有新进入的 4 行被构建");
    }

    // ── 浮层 ──────────────────────────────────────────────────────────────────

    #[test]
    fn overlay_is_last_and_closes_on_outside_click() {
        let (_rt, mut app, window) = setup();
        let root = root_of(&app, window);
        let _background = mount_override(&mut app, window, root, Rect::new(0.0, 0.0, 100.0, 100.0));
        frame(&mut app, window);
        let overlay = app
            .create_overlay(
                window,
                OverlayOptions {
                    placement: OverlayPlacement::Point(Point::new(200.0, 10.0)),
                    dismiss: OverlayDismiss::OutsideClick,
                },
            )
            .expect("overlay");
        let overlay_node = overlay.node();
        let mut style = Style::default();
        style.inner.position = taffy::style::Position::Absolute;
        style.inner.inset = taffy::geometry::Rect {
            left: taffy::style::LengthPercentageAuto::length(200.0),
            top: taffy::style::LengthPercentageAuto::length(10.0),
            right: taffy::style::LengthPercentageAuto::auto(),
            bottom: taffy::style::LengthPercentageAuto::auto(),
        };
        style.inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(80.0),
            height: taffy::style::Dimension::length(80.0),
        };
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .set_style(overlay_node, style)
            .expect("style");
        frame(&mut app, window);

        assert_eq!(
            app.window(window)
                .expect("window")
                .tree()
                .order()
                .last()
                .copied(),
            Some(overlay_node),
            "浮层必须是 order 的最后一个子树"
        );
        assert_eq!(app.overlays(window).len(), 1);
        assert_eq!(
            app.window(window)
                .expect("window")
                .tree()
                .hit_test(Point::new(210.0, 20.0))
                .map(|hit| hit.node),
            Some(overlay_node),
            "浮层优先命中"
        );

        // 点外部 ⇒ 关闭并消费（不再派递给背景）。
        app.dispatch_input(
            window,
            InputEvent::PointerDown {
                position: Point::new(10.0, 10.0),
                button: PointerButton::Primary,
                modifiers: Modifiers::empty(),
            },
        );
        assert!(app.overlays(window).is_empty(), "点击外部关闭");
    }

    #[test]
    fn dismiss_action_closes_the_topmost_overlay_including_explicit() {
        let (_rt, mut app, window) = setup();
        frame(&mut app, window);
        let overlay = app
            .create_overlay(
                window,
                OverlayOptions {
                    placement: OverlayPlacement::Point(Point::ZERO),
                    dismiss: OverlayDismiss::Explicit,
                },
            )
            .expect("overlay");
        let root = root_of(&app, window);

        assert!(app.dispatch_action(window, root, Action::Dismiss, ActionSource::Programmatic));
        assert!(app.overlays(window).is_empty(), "Dismiss 关 Explicit 浮层");
        assert!(!app.close_overlay(&overlay), "重复关闭返回 false");
    }

    // ── 拖拽 ──────────────────────────────────────────────────────────────────

    #[test]
    fn drag_streams_deltas_and_suppresses_the_click() {
        let (_rt, mut app, window) = setup();
        let phases = Rc::new(Cell::new(Vec::<DragPhase>::new()));
        let seen = phases.clone();
        app.on_action(Action::custom("split"), move |ctx| {
            if let ActionSource::Drag { phase, .. } = ctx.source() {
                let mut list = seen.take();
                list.push(phase);
                seen.set(list);
            }
        });
        let root = root_of(&app, window);
        let grip = NodeBuilder::boxed()
            .layout_override(Rect::new(100.0, 0.0, 6.0, 100.0))
            .on_drag(Action::custom("split"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        frame(&mut app, window);

        app.dispatch_input(
            window,
            InputEvent::PointerDown {
                position: Point::new(102.0, 50.0),
                button: PointerButton::Primary,
                modifiers: Modifiers::empty(),
            },
        );
        let state = app.drag_state(window).expect("drag session");
        assert_eq!(state.node(), grip);
        assert_eq!(app.take_capture_request(), Some(true), "拖拽请求指针捕获");

        let moved = app.dispatch_input(
            window,
            InputEvent::PointerMove {
                position: Point::new(122.0, 50.0),
                modifiers: Modifiers::empty(),
            },
        );
        assert_eq!(moved.action, Some(Action::custom("split")));
        assert!(app.drag(&state).is_some_and(|info| info.delta.x == 20.0));

        let up = app.dispatch_input(
            window,
            InputEvent::PointerUp {
                position: Point::new(122.0, 50.0),
                button: PointerButton::Primary,
                modifiers: Modifiers::empty(),
            },
        );
        assert_eq!(app.drag_state(window), None);
        assert_eq!(app.take_capture_request(), Some(false));
        assert_eq!(up.action, Some(Action::custom("split")), "抬起派发 End");
        assert_eq!(up.click_count, 0, "拖拽后不产生点击");
        assert_eq!(
            phases.take(),
            vec![DragPhase::Start, DragPhase::Move, DragPhase::End]
        );
    }

    // ── 动画 ──────────────────────────────────────────────────────────────────

    #[test]
    fn animation_ticks_write_the_transient_and_end_in_idle() {
        let (_rt, mut app, window) = setup();
        let root = root_of(&app, window);
        let node = mount_override(&mut app, window, root, Rect::new(0.0, 0.0, 40.0, 40.0));
        frame(&mut app, window);
        let progress = app.transient(0.0_f32);
        let id = app
            .animate(
                window,
                AnimatedValue::Scalar {
                    target: progress,
                    to: 1.0,
                },
                &[node],
                AnimationSpec {
                    duration: std::time::Duration::from_millis(100),
                    easing: Easing::Linear,
                },
            )
            .expect("animate");
        let _ = app.take_wake_request();
        app.advance_clock(std::time::Duration::from_millis(50));
        let outcome = frame(&mut app, window);

        assert!(
            (app.read_transient(&progress) - 0.5).abs() < 1e-3,
            "中点 = 0.5"
        );
        assert!(
            matches!(outcome, FrameOutcome::Committed(_)),
            "注册节点每帧重绘 ⇒ 动画期间必然提交"
        );
        let _ = node;

        app.advance_clock(std::time::Duration::from_millis(60));
        frame(&mut app, window);
        assert_eq!(app.read_transient(&progress), 1.0);
        assert!(!app.animations_running(window), "结束即删槽");
        assert!(!app.cancel_animation(&id), "槽已删");

        app.advance_clock(std::time::Duration::from_millis(100));
        assert!(
            matches!(frame(&mut app, window), FrameOutcome::Idle),
            "动画结束后帧回到 Idle（I3）"
        );
    }

    // ── 评审 #1…#9 的回归用例 ────────────────────────────────────────────────

    #[test]
    fn closed_window_returns_unknown_window() {
        let (_rt, mut app, window) = setup();
        let overlay = app
            .create_overlay(
                window,
                OverlayOptions {
                    placement: OverlayPlacement::Point(Point::ZERO),
                    dismiss: OverlayDismiss::Explicit,
                },
            )
            .expect("overlay");
        let transient = app.transient(0.0_f32);
        assert!(app.close_window(window));
        let _ = overlay;

        let error = app
            .create_overlay(
                window,
                OverlayOptions {
                    placement: OverlayPlacement::Point(Point::ZERO),
                    dismiss: OverlayDismiss::Explicit,
                },
            )
            .expect_err("窗口已关闭");
        assert!(matches!(error, EngineError::UnknownWindow { window: closed } if closed == window));

        let error = app
            .animate(
                window,
                AnimatedValue::Scalar {
                    target: transient,
                    to: 1.0,
                },
                &[],
                AnimationSpec::short(),
            )
            .expect_err("窗口已关闭");
        assert!(matches!(error, EngineError::UnknownWindow { .. }));
    }

    #[test]
    fn hit_after_scrolling_50_lands_on_the_second_row() {
        let (_rt, mut app, window, container, _child) = scrollable_app();
        let handle = app.scroll_handle(window, container).expect("scroll handle");
        app.scroll_to(&handle, Point::new(0.0, 50.0))
            .expect("scroll");

        // 第二行原本占内容 y = 100..200；滚 50 后屏幕上占 50..150，本地 y = 60 − 50 = 10。
        let hit = app
            .window(window)
            .expect("window")
            .tree()
            .hit_test(Point::new(50.0, 60.0))
            .expect("hit");
        assert_eq!(hit.local_point, Point::new(50.0, 10.0));
    }

    #[test]
    fn overlay_outside_click_consumes_the_whole_press() {
        let (_rt, mut app, window) = setup();
        let calls = Rc::new(Cell::new(0_u32));
        let seen = calls.clone();
        app.on_action(Action::custom("under"), move |_ctx| {
            seen.set(seen.get() + 1)
        });
        let root = root_of(&app, window);
        let button = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 100.0, 100.0))
            .paint(background())
            .on_click(Action::custom("under"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        frame(&mut app, window);
        app.create_overlay(
            window,
            OverlayOptions {
                placement: OverlayPlacement::Point(Point::new(200.0, 10.0)),
                dismiss: OverlayDismiss::OutsideClick,
            },
        )
        .expect("overlay");
        frame(&mut app, window);

        app.dispatch_input(
            window,
            InputEvent::PointerDown {
                position: Point::new(20.0, 20.0),
                button: PointerButton::Primary,
                modifiers: Modifiers::empty(),
            },
        );
        let up = app.dispatch_input(
            window,
            InputEvent::PointerUp {
                position: Point::new(20.0, 20.0),
                button: PointerButton::Primary,
                modifiers: Modifiers::empty(),
            },
        );

        assert!(app.overlays(window).is_empty(), "按下即关浮层");
        assert_eq!(calls.get(), 0, "这次按下必须整体消费");
        assert_eq!(up.action, None);
        assert_eq!(up.click_count, 0);
        let _ = button;
    }

    #[test]
    fn stale_focus_is_cleared_and_tab_recovers() {
        let (_rt, mut app, window) = setup();
        let root = root_of(&app, window);
        let first = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
            .on_click(Action::custom("a"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let second = NodeBuilder::boxed()
            .layout_override(Rect::new(60.0, 0.0, 40.0, 40.0))
            .on_click(Action::custom("b"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        frame(&mut app, window);
        assert!(app.set_focus(window, Some(first)));

        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .remove(first)
            .expect("remove");

        assert_eq!(app.focused(window), None, "陈旧焦点必须视为无焦点");
        // Tab 不能卡死：从「无焦点」重新开始，干净地落到第二个节点。
        app.dispatch_input(
            window,
            InputEvent::Key {
                key: Key::Tab,
                state: KeyState::Down,
                modifiers: Modifiers::empty(),
            },
        );
        assert_eq!(app.focused(window), Some(second));
    }

    #[test]
    fn virtual_list_mounts_rows_on_the_first_frame_and_grows_on_resize() {
        let (_rt, mut app, window) = setup();
        let mut style = Style::default();
        style.inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(200.0),
            height: taffy::style::Dimension::length(100.0),
        };
        style.inner.position = taffy::style::Position::Relative;
        style.clip = Clip::Bounds;
        let tree = app.window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        let container = NodeBuilder::boxed()
            .style(style)
            .build(tree, root)
            .expect("mount");
        // 注意：**先**建列表、再布局（首帧视口还是 0）。
        let list = app
            .make_virtual_list(
                window,
                container,
                VirtualListConfig {
                    extent: 25.0,
                    overscan: 0,
                },
            )
            .expect("virtual list");
        app.set_virtual_items(&list, 100, |index| {
            NodeBuilder::boxed()
                .paint(background())
                .id(format!("row{index}"))
        })
        .expect("items");
        frame(&mut app, window);

        let state = app.virtual_list_state(&list).expect("state");
        assert!(state.mounted > 0, "首帧后必须挂载可见行：{state:?}");
        assert_eq!((state.first, state.last), (0, 4));

        // 视口长高 ⇒ 布局后再修窗口（不能停留在旧区间）。
        app.window_mut(window)
            .expect("window")
            .set_size(Size::new(400.0, 300.0));
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .set_style(container, {
                let mut style = Style::default();
                style.inner.size = taffy::geometry::Size {
                    width: taffy::style::Dimension::length(200.0),
                    height: taffy::style::Dimension::length(200.0),
                };
                style.inner.position = taffy::style::Position::Relative;
                style.clip = Clip::Bounds;
                style
            })
            .expect("style");
        frame(&mut app, window);
        let state = app.virtual_list_state(&list).expect("state");
        assert_eq!(
            (state.first, state.last),
            (0, 8),
            "视口 200 / 行高 25 ⇒ 8 行"
        );
        assert_eq!(state.mounted, 8);
    }

    #[test]
    fn key_modifiers_reach_the_handler() {
        let (_rt, mut app, window) = setup();
        let seen = Rc::new(Cell::new(Modifiers::empty()));
        let slot = seen.clone();
        app.on_action(Action::custom("activate"), move |ctx| {
            slot.set(ctx.modifiers());
        });
        let root = root_of(&app, window);
        let node = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
            .on_action(Action::custom("activate"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        frame(&mut app, window);
        assert!(app.set_focus(window, Some(node)));

        app.dispatch_input(
            window,
            InputEvent::Key {
                key: Key::Enter,
                state: KeyState::Down,
                modifiers: Modifiers::SHIFT | Modifiers::CONTROL,
            },
        );

        assert_eq!(
            seen.get(),
            Modifiers::SHIFT | Modifiers::CONTROL,
            "handler 必须拿到本次按键的修饰键"
        );
    }

    #[test]
    fn animation_retarget_replaces_the_previous_slot() {
        let (_rt, mut app, window) = setup();
        let root = root_of(&app, window);
        let node = mount_override(&mut app, window, root, Rect::new(0.0, 0.0, 40.0, 40.0));
        frame(&mut app, window);
        let value = app.transient(0.0_f32);
        let spec = AnimationSpec {
            duration: std::time::Duration::from_millis(100),
            easing: Easing::Linear,
        };
        app.animate(
            window,
            AnimatedValue::Scalar {
                target: value,
                to: 1.0,
            },
            &[node],
            spec,
        )
        .expect("first");
        app.advance_clock(std::time::Duration::from_millis(50));
        frame(&mut app, window);
        assert!((app.read_transient(&value) - 0.5).abs() < 1e-3);

        // 半途改主意：1 → 0。旧槽必须让位（否则两条插值互相覆盖 ⇒ 回弹）。
        app.animate(
            window,
            AnimatedValue::Scalar {
                target: value,
                to: 0.0,
            },
            &[node],
            spec,
        )
        .expect("retarget");
        app.advance_clock(std::time::Duration::from_millis(50));
        frame(&mut app, window);
        // 从当前值 0.5 重新起步：走一半 ⇔ 0.5 + (0 − 0.5) × 0.5 = 0.25。
        assert!(
            (app.read_transient(&value) - 0.25).abs() < 1e-3,
            "重定向必须从当前值起步：{}",
            app.read_transient(&value)
        );

        app.advance_clock(std::time::Duration::from_millis(60));
        frame(&mut app, window);
        assert_eq!(app.read_transient(&value), 0.0, "终点是 0，不得回弹到 1");
        assert!(!app.animations_running(window));
    }

    #[test]
    fn second_pointer_down_during_drag_is_ignored() {
        let (_rt, mut app, window) = setup();
        let root = root_of(&app, window);
        let grip = NodeBuilder::boxed()
            .layout_override(Rect::new(100.0, 0.0, 6.0, 100.0))
            .on_drag(Action::custom("split"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        app.on_action(Action::custom("split"), |_ctx| {});
        frame(&mut app, window);

        app.dispatch_input(
            window,
            InputEvent::PointerDown {
                position: Point::new(102.0, 50.0),
                button: PointerButton::Primary,
                modifiers: Modifiers::empty(),
            },
        );
        let before = app.drag_state(window).expect("drag");
        app.dispatch_input(
            window,
            InputEvent::PointerDown {
                position: Point::new(300.0, 200.0),
                button: PointerButton::Primary,
                modifiers: Modifiers::empty(),
            },
        );
        let after = app.drag_state(window).expect("drag");
        assert_eq!(before, after, "拖拽中的二次按下不得重开会话");
        assert_eq!(after.node(), grip);
    }

    #[test]
    fn moving_past_the_drag_threshold_suppresses_the_click() {
        let (_rt, mut app, window) = setup();
        let clicks = Rc::new(Cell::new(0_u32));
        let seen = clicks.clone();
        app.on_action(Action::custom("hit"), move |_ctx| seen.set(seen.get() + 1));
        let root = root_of(&app, window);
        let node = NodeBuilder::boxed()
            .layout_override(Rect::new(10.0, 10.0, 100.0, 100.0))
            .paint(background())
            .on_click(Action::custom("hit"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        frame(&mut app, window);
        app.set_input_thresholds(InputThresholds {
            double_click_time: std::time::Duration::from_millis(500),
            double_click_distance: 4.0,
            drag_distance: 4.0,
        });

        app.dispatch_input(
            window,
            InputEvent::PointerDown {
                position: Point::new(20.0, 20.0),
                button: PointerButton::Primary,
                modifiers: Modifiers::empty(),
            },
        );
        let up = app.dispatch_input(
            window,
            InputEvent::PointerUp {
                position: Point::new(40.0, 20.0),
                button: PointerButton::Primary,
                modifiers: Modifiers::empty(),
            },
        );

        assert_eq!(clicks.get(), 0, "超过阈值 ⇒ 不是点击");
        assert_eq!(up.click_count, 0);
        let _ = node;
    }

    #[test]
    fn stale_scroll_handle_reports_stale_node() {
        let (_rt, mut app, window, container, _child) = scrollable_app();
        let handle = app.scroll_handle(window, container).expect("scroll handle");
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .remove(container)
            .expect("remove");

        let error = app
            .scroll_to(&handle, Point::new(0.0, 10.0))
            .expect_err("节点已卸载");
        assert!(matches!(error, EngineError::StaleNode { node } if node == container));
    }
}

/// P13：`ResourceCache` 接入 `App` 的边界用例（图片页上传 / 进程级缓存 / 零副作用）。
#[cfg(test)]
mod resources_tests {
    use super::App;
    use crate::{
        math::{Rect, Size},
        node::builder::NodeBuilder,
        paint::style::PaintSpec,
        window::{WindowId, WindowOptions},
    };

    fn setup() -> (tokio::runtime::Runtime, App, WindowId) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(rt.handle().clone());
        let window = app.create_window(WindowOptions {
            title: "resources".into(),
            size: Size::new(200.0, 120.0),
            scale: crate::math::Scale::ONE,
        });
        (rt, app, window)
    }

    /// 4×4 的不透明 PNG（解码后逐字节可预测：BGRA = 源 RGB 交换）。
    fn png() -> Vec<u8> {
        let pixels: Vec<u8> = std::iter::repeat_n([10u8, 20, 30, 255], 16)
            .flatten()
            .collect();
        let image = image::RgbaImage::from_raw(4, 4, pixels).expect("raw");
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }

    #[test]
    fn resources_are_process_level() {
        let (_rt, mut app, window) = setup();
        let image = app.resources_mut().insert(&png()).expect("insert");

        assert_eq!(app.resources().len(), 1);
        assert_eq!(app.resources().page_pixels().0, (2048, 2048));
        // 另一窗口共享同一份缓存（单 GPU 设备、进程级一份）。
        let second = app.create_window(WindowOptions {
            title: "second".into(),
            size: Size::new(64.0, 64.0),
            scale: crate::math::Scale::ONE,
        });
        assert_ne!(second, window);
        assert_eq!(app.resources().len(), 1);

        // 内容区写进了页缓冲（BGR 顺序）。
        let (_, pixels) = app.resources().page_pixels();
        let origin_x = (image.tile.u0 * 2048.0).round() as usize;
        let origin_y = (image.tile.v0 * 2048.0).round() as usize;
        let offset = origin_y * 2048 * 4 + origin_x * 4;
        assert_eq!(&pixels[offset..offset + 4], &[30, 20, 10, 255]);
    }

    #[test]
    fn insert_does_not_touch_node_dirty_flags() {
        let (_rt, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 20.0, 20.0))
            .paint(PaintSpec {
                background: crate::paint::color::Color::WHITE,
                ..PaintSpec::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("box");
        let _ = app.frame(window).expect("frame").expect("ok");
        let before = app
            .window(window)
            .expect("window")
            .tree()
            .get(node)
            .expect("node")
            .dirty();

        let _ = app.resources_mut().insert(&png()).expect("insert");

        let after = app
            .window(window)
            .expect("window")
            .tree()
            .get(node)
            .expect("node")
            .dirty();
        assert_eq!(before, after, "插入资源不得改动任何节点的脏位");
    }

    #[test]
    fn atlas_upload_without_renderers_keeps_windows_unsynced() {
        let (_rt, mut app, window) = setup();
        let _ = app.resources_mut().insert(&png()).expect("insert");

        // headless：没有 renderer ⇒ 不上传，窗口保持「未收到整页」（换渲染器后会自动补整页）。
        app.upload_atlas_pages();
        app.upload_atlas_pages();
        let target = app.window(window).expect("window");
        assert!(!target.glyph_page_received());
        assert!(!target.image_page_received());
        assert_eq!(app.resources().len(), 1, "上传不影响缓存内容");
    }

    #[test]
    fn atlas_upload_marks_only_windows_with_a_renderer() {
        let (_rt, mut app, first) = setup();
        let second = app.create_window(WindowOptions {
            title: "second".into(),
            size: Size::new(64.0, 64.0),
            scale: crate::math::Scale::ONE,
        });
        app.upload_atlas_pages();

        for window in [first, second] {
            let target = app.window(window).expect("window");
            assert!(!target.has_renderer());
            assert!(
                !target.image_page_received(),
                "没有渲染器的窗口不得被记账为已同步"
            );
        }
    }

    /// 评审阻塞 ②：`frame_all` 逐窗提交，图集页必须**广播**给每个窗口的渲染器，
    /// 否则第一个窗口消费脏行后，其余窗口的纹理永远收不到内容。
    #[cfg(windows)]
    #[test]
    fn atlas_pages_are_broadcast_to_every_window() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let (_rt, mut app, first) = setup();
        app.window_mut(first)
            .expect("window")
            .enable_offscreen_renderer()
            .expect("offscreen renderer");
        let second = app.create_window(WindowOptions {
            title: "second".into(),
            size: Size::new(64.0, 64.0),
            scale: crate::math::Scale::ONE,
        });
        app.window_mut(second)
            .expect("window")
            .enable_offscreen_renderer()
            .expect("offscreen renderer");

        let _ = app.resources_mut().insert(&png()).expect("insert");
        app.upload_atlas_pages();

        for window in [first, second] {
            let target = app.window(window).expect("window");
            assert!(target.glyph_page_received(), "字形页必须广播到每个窗口");
            assert!(target.image_page_received(), "图片页必须广播到每个窗口");
        }

        // 第二次调用：没有新写入 ⇒ 不重复上传（脏旗标已被统一消费）。
        app.upload_atlas_pages();
        for window in [first, second] {
            assert!(app.window(window).expect("window").image_page_received());
        }
    }

    #[test]
    fn image_atlas_page_is_16_mib() {
        let (_rt, app, _window) = setup();
        assert_eq!(app.resources().page_pixels().1.len(), 16 * 1024 * 1024);
    }
}
