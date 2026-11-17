//! 帧推进：门禁 / 调度 / 布局 / 编码 / 提交。
//!
//! `window::Window` 的帧方法体搬到这里成为自由函数；`impl Window` 只保留委托。

use std::{cell::Cell, rc::Rc, time::Instant};

use super::{FrameIndex, FrameOutcome, FrameTicket, Window};
use crate::{
    error::EngineError,
    layout::LayoutReport,
    paint::pass::PaintReport,
    render::{
        backend::Renderer,
        compositor::{Plan, plan},
        frame::{FramePayload, FrameRequest},
    },
};

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

// 消费者：`impl Window::needs_frame` 的薄委托（外部调用点见该方法文档）。
// ── P06：帧推进 ───────────────────────────────────────────────────────────

/// 本帧是否需要画面（两段门禁的第一段）。
///
/// `wake` = `App::take_wake_request()` 的结果（`App::frame` 传入；`Window::frame` 传 `false`）。
/// `!REUSABLE` 表示 lane 内容与树/样式不一致；`LAYOUT | PAINT` 覆盖「新建/未标 REUSABLE」的节点。
pub(crate) fn needs_frame(window: &Window, wake: bool) -> bool {
    // `a11y_bind_pending`：只有 `bind_a11y` 的节点被写信号时**没有**像素要画，但
    // `refresh_styles` 仍必须跑一次（否则补丁永远不更新）。帧末的门禁会把它退回 `Idle`。
    //
    // 全部判据都是 O(1)：读的是 `NodeTree` 的反向脏集合是否为空（R03-1 / S1）。旧实现用
    // `any_node(|n| LAYOUT|PAINT || !REUSABLE)` 做全树扫描，10k 节点每帧白付 ~50 µs。
    wake || window.full_damage_pending
        || window.pending_damage.is_some()
        || window.a11y.bind_work()
        || window.tree.has_bind_work()
        || window.tree.has_layout_work()
        || window.tree.has_paint_work()
}

// 消费者：`impl Window::begin_frame` 的薄委托（外部调用点见该方法文档）。
/// `Idle → Pending → Draining`；空闲/重复调用返回 `None`。
pub(crate) fn begin_frame(window: &mut Window) -> Option<FrameTicket> {
    if !window.scheduler.request_frame() {
        // 已经是 Pending/Draining：重复请求不产生新帧。
        return None;
    }
    let ticket = window.scheduler.begin()?;
    // 帧簿记的扫描计数从本帧开始重新统计（`FrameRecord::scanned_nodes`，仅 test-support）。
    #[cfg(feature = "test-support")]
    window.tree.reset_scanned_nodes();
    #[cfg(feature = "hud")]
    {
        window.frame_started_at = Some(std::time::Instant::now());
    }
    Some(ticket)
}

// 消费者：`impl Window::layout_if_dirty` 的薄委托（外部调用点见该方法文档）。
/// 仅当有 `LAYOUT` 时才重排（纯颜色变化不跑布局）。
pub(crate) fn layout_if_dirty(window: &mut Window) -> Result<Option<LayoutReport>, EngineError> {
    // O(1) 门禁（R03-1）：读 `dirty_layout` 反向集合，不再全树扫描。
    if !window.tree.has_layout_work() {
        return Ok(None);
    }
    window.compute_layout().map(Some)
}

// 消费者：`impl Window::commit_frame` 的薄委托（外部调用点见该方法文档）。
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
    window: &mut Window,
    ticket: FrameTicket,
    report: PaintReport,
) -> FrameOutcome {
    let has_pixels = report.rebuilt > 0 || report.damage.full || report.damage.rects > 0;
    // 默认构建也算的读点：摘要计数必须与累加器的实际矩形数一致（P07 消费同一累加器）。
    debug_assert_eq!(
        report.damage.rects,
        window.damage.rects().len(),
        "damage summary and accumulator disagree"
    );
    if !has_pixels {
        // `paint_frame` 没有跑（或没产出任何 lane / 像素）：没有节点状态需要结算，
        // damage 与 dirty 保留给下一次重试。
        window.scheduler.abort(ticket);
        #[cfg(feature = "hud")]
        {
            window.frame_started_at = None;
            window.frame_cpu = None;
        }
        #[cfg(feature = "test-support")]
        window.record_idle();
        return FrameOutcome::Idle;
    }
    // 本帧只算一次：`submit_gpu` 消费同一份 `plan`（目标尺寸也来自同一次 `frame_rect`）。
    let frame = window.frame_rect();
    let renderer_surface = window.renderer.as_ref().map(Renderer::surface);
    let plan = plan(report.damage, window.damage.rects(), frame);
    // Windows：目标尺寸变化必须整帧重画（`set_content_size` 在 buffer 内是纯 CPU 记账，
    // `Plan::Idle` 会消费帧延迟 token 却不 present）。macOS / Linux 保持原规则。
    #[cfg(windows)]
    let plan = surface_change_plan(plan, renderer_surface, window.surface);
    if !frame_reaches_gpu(plan, renderer_surface, window.surface) {
        // paint 已经消费了 lane 重建与 damage：即使不发布，也必须像已提交的帧一样结算节点
        // 状态（damage / REUSABLE），否则后续会 over-damage、a11y 多标（P2-1）。
        // 只是不推进 `FrameIndex`、不发布（V-122 ⑧ / F-183）。
        window.tree.settle_frame();
        window.scheduler.abort(ticket);
        #[cfg(feature = "hud")]
        {
            window.frame_started_at = None;
            window.frame_cpu = None;
        }
        #[cfg(feature = "test-support")]
        window.record_idle();
        return FrameOutcome::Idle;
    }
    let index = window.scheduler.commit(ticket);
    window.tree.settle_frame();
    if let Some(mut renderer) = window.renderer.take() {
        let stats = window.submit_gpu(&mut renderer, index, plan);
        window.renderer = Some(renderer);
        #[cfg(feature = "hud")]
        {
            window.render_stats = stats;
        }
        #[cfg(not(feature = "hud"))]
        {
            let _ = stats;
        }
    }
    #[cfg(feature = "test-support")]
    {
        let stats = std::mem::take(&mut window.stats);
        let record = crate::test_support::record::capture(
            &window.scene,
            &window.tree,
            &window.resolver,
            report,
            window.damage.rects(),
            frame,
            index,
            stats,
            window.tree.scanned_nodes(),
        );
        window.record = Some(record);
    }
    FrameOutcome::Committed(index)
}

// 消费者：`impl Window::submit_gpu` 的薄委托（外部调用点见该方法文档）。
/// 把本帧交给 GPU：窗口路径只构造 `FrameRequest` 并 `publish`（**不等待**，I1）。
///
/// `plan` 由 [`Window::commit_frame`] 用同一份 damage 与 `frame` 算好；`Committed` 帧在这里
/// **恰好发布一次**（`Plan::Idle` 且目标尺寸未变的帧已在 `commit_frame` 退回 `Idle`）。
///
/// 返回最近一次**已完成**帧的统计（窗口路径落后 ≤1 帧，HUD 允许）；GPU 线程的错误在
/// `poll_renderer` 里冒泡，提交路径没有同步错误。
pub(crate) fn submit_gpu(
    window: &mut Window,
    renderer: &mut Renderer,
    index: FrameIndex,
    plan: Plan,
) -> crate::render::RenderStats {
    let surface = window.physical_surface();
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
    renderer.set_layer_scale(surface, window.scale.factor());
    // 窗口路径的 GPU 采样来自 `poll` 的反馈；本帧的同步采样表清空以免 HUD 显示过期数据。
    window.timings.reset(index);
    renderer.publish(FrameRequest {
        index,
        payload: FramePayload::from_buffer(window.instance_buffer()),
        plan,
        surface,
        scale: window.scale.factor(),
        // Linux：逻辑尺寸与 `surface`（buffer 尺寸）同帧过线，GPU 线程在 present 前用它写
        // `wp_viewport.set_destination`（destination 与 buffer 必须成对，D1 / D2）。
        #[cfg(target_os = "linux")]
        surface_local: window.size(),
        // 整数回退的 buffer scale：与 attach/commit 同一次生效（fractional 模式恒 1，
        // 整数回退 = N；surface 的逻辑尺寸因此等于 `surface / buffer_scale`，V-110）。
        #[cfg(target_os = "linux")]
        buffer_scale: window.buffer_scale,
        published_at: Instant::now(),
    })
}

// 消费者：`impl Window::abort_frame` 的薄委托（外部调用点见该方法文档）。
/// 中止（错误路径）：不结算、不递增序号（damage/dirty 保留，下一次重试）。
pub(crate) fn abort_frame(window: &mut Window, ticket: FrameTicket) {
    window.scheduler.abort(ticket);
    #[cfg(feature = "hud")]
    {
        window.frame_started_at = None;
        window.frame_cpu = None;
    }
    #[cfg(feature = "test-support")]
    window.record_idle();
}

// 消费者：`impl Window::frame` 的薄委托（外部调用点见该方法文档）。
/// 窗口本地帧（**无 App 通道**：不跑样式/文本 pass）。
///
/// 含 `bind_style` / `bind_text` 的窗口必须走 `App::frame`，否则补丁不会重新求值
/// （debug 构建会直接断言失败）。
///
/// # Errors
/// `EngineError::InvalidMeasure`：布局闭包返回非有限值/负尺寸（本帧已中止，未提交）。
pub(crate) fn frame(window: &mut Window) -> Result<FrameOutcome, EngineError> {
    if !window.needs_frame(false) {
        #[cfg(feature = "test-support")]
        window.record_idle();
        return Ok(FrameOutcome::Idle);
    }
    debug_assert!(
        !window.tree.has_bound_nodes(),
        "Window::frame does not run style/text passes; use App::frame for bound nodes"
    );
    let Some(ticket) = window.begin_frame() else {
        return Ok(FrameOutcome::Idle);
    };
    match window.layout_if_dirty() {
        Ok(_) => {}
        Err(error) => {
            window.abort_frame(ticket);
            return Err(error);
        }
    }
    // 无 `App` 的独立窗口（probe / 离屏测试）没有几何注册表：传一个进程级空表。
    static NO_GEOMETRY: std::sync::OnceLock<crate::paint::geometry_store::GeometryStore> =
        std::sync::OnceLock::new();
    let geometry = NO_GEOMETRY.get_or_init(crate::paint::geometry_store::GeometryStore::new);
    let report = match window.paint_frame(geometry) {
        Ok(report) => report,
        Err(error) => {
            window.abort_frame(ticket);
            return Err(error);
        }
    };
    Ok(window.commit_frame(ticket, report))
}

// 消费者：`impl Window::frame_index` 的薄委托（外部调用点见该方法文档）。
/// 提交游标（0 = 从未提交；空闲帧不变）。
pub(crate) fn frame_index(window: &Window) -> FrameIndex {
    FrameIndex(window.scheduler.committed())
}

// 消费者：`impl Window::compute_layout` 的薄委托（外部调用点见该方法文档）。
/// 用窗口逻辑尺寸重排，返回本次布局统计（P06/P09 的帧驱动调用它）。
///
/// **文本节点必须先在本帧调用 `App::refresh_text`**：`NodeBuilder::text` 不设 measure，
/// 没跑文本 pass 的文本节点测量为 `0×0`（P05 §13.4）。
///
/// # Errors
/// `EngineError::InvalidMeasure { node, axis, value }`：某个 measure 闭包返回 NaN / 负数（首条优先）。
/// 出错时布局盒可能为空，调用方**不得**提交本帧。
pub(crate) fn compute_layout(window: &mut Window) -> Result<LayoutReport, EngineError> {
    let report = window.tree.compute_layout(taffy::geometry::Size {
        width: Some(window.options.size.width),
        height: Some(window.options.size.height),
    })?;
    // P11：布局盒变了的节点必须重发 a11y（bounds = 物理化的 layout 盒）。窗口 resize /
    // 虚拟列表重窗都只标根，子树的新盒由布局 pass 写入，不经过任何 `mark_*`。
    window.tree.mark_moved_a11y();
    Ok(report)
}

// 消费者：`commit_frame`（提交门禁）。
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
pub(crate) fn frame_reaches_gpu(
    plan: Plan,
    renderer_surface: Option<(u32, u32)>,
    surface: (u32, u32),
) -> bool {
    match renderer_surface {
        None => true,
        Some(published) => plan != Plan::Idle || published != surface,
    }
}

// 消费者：`commit_frame`（Windows 目标尺寸变化的计划修正）。
/// Windows：目标尺寸变化一律映射成 `Plan::Full`（任何输入计划）。
///
/// 组合交换链按高水位 buffer 分配（2 s 静止后可能缩容），`set_content_size` 在 buffer 内只是
/// CPU 记账；一个 `Plan::Idle` 的发布会让 GPU 线程消费一次帧延迟 waitable token 却不 present
/// （DXGI 的 token 守恒被破坏，下一帧白等一个 vsync）。`Plan::Partial` 的 damage 是**按旧目标**算的：新暴露的
/// 像素没有 damage，只画旧矩形会在新客户区留下未初始化内容，因此同样整帧重画。
/// macOS / Linux 的 GPU 线程会按新 surface 重建 swapchain，保持 [`frame_reaches_gpu`] 原规则。
#[cfg(windows)]
pub(crate) fn surface_change_plan(
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
