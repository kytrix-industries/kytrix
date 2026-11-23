use std::{cell::Cell, rc::Rc, time::Instant};

use super::{FrameIndex, FrameOutcome, FrameTicket, Window};
use crate::{
    error::EngineError,
    layout::LayoutReport,
    paint::pass::PaintReport,
    render::{
        backend::Renderer,
        compositor::{Plan, plan},
        compositor_thread::CompositorTicket,
        frame::FrameRequest,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameState {
    Idle,
    Pending,
    Draining,
}

pub(crate) struct FrameScheduler {
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

    pub(crate) fn request_frame(&mut self) -> bool {
        if self.state.get() == FrameState::Idle {
            self.state.set(FrameState::Pending);
            true
        } else {
            false
        }
    }

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

    pub(crate) fn commit(&mut self, mut ticket: FrameTicket) -> FrameIndex {
        ticket.consumed = true;
        self.state.set(FrameState::Idle);
        let next = self.committed.checked_add(1);
        debug_assert!(next.is_some(), "FrameIndex overflow (u32::MAX frames)");
        self.committed = next.unwrap_or(u32::MAX);
        let index = FrameIndex(self.committed);
        debug_assert!(index.get() >= 1, "提交序号必须 >= 1");
        index
    }

    pub(crate) fn abort(&mut self, mut ticket: FrameTicket) {
        ticket.consumed = true;
        self.state.set(FrameState::Idle);
    }

    pub(crate) fn committed(&self) -> u32 {
        self.committed
    }
}

pub(crate) fn needs_frame(window: &Window, wake: bool) -> bool {
    wake || window.full_damage_pending
        || window.pending_damage.is_some()
        || window.a11y_binding_work()
        || window.tree.has_bind_work()
        || window.tree.has_layout_work()
        || window.tree.has_paint_work()
}

pub(crate) fn begin_frame(window: &mut Window) -> Option<FrameTicket> {
    if !window.scheduler.request_frame() {
        return None;
    }
    let ticket = window.scheduler.begin()?;
    if let Some(epoch) = window
        .renderer
        .as_ref()
        .and_then(Renderer::frame_sink)
        .map(|sink| sink.reset_epoch())
    {
        let _ = note_reset_epoch(window, epoch);
    }
    #[cfg(feature = "test-support")]
    window.tree.reset_scanned_nodes();
    #[cfg(feature = "hud")]
    {
        window.frame_started_at = Some(std::time::Instant::now());
    }
    Some(ticket)
}

pub(crate) fn note_reset_epoch(window: &mut Window, epoch: u64) -> bool {
    if !window.resync.observe(epoch) {
        return false;
    }
    window.delta_builder.force_full();
    window.full_damage_pending = true;
    true
}

pub(crate) fn force_full_republish(window: &mut Window) {
    window.delta_builder.force_full();
    window.full_damage_pending = true;
    window.request_repaint();
}

pub(crate) fn layout_if_dirty(window: &mut Window) -> Result<Option<LayoutReport>, EngineError> {
    if !window.tree.has_layout_work() {
        return Ok(None);
    }
    window.compute_layout().map(Some)
}

pub(crate) enum PublishDecision {
    NoPixels,
    NoGpuWork,
    Publish(Plan),
}

pub(crate) fn publish_decision(
    window: &Window,
    report: &PaintReport,
    scroll_pending: bool,
) -> PublishDecision {
    let has_pixels =
        report.rebuilt > 0 || report.damage.full || report.damage.rects > 0 || scroll_pending;
    if !has_pixels {
        return PublishDecision::NoPixels;
    }
    let frame = window.frame_rect();
    let renderer_surface = window.renderer.as_ref().map(Renderer::surface);
    let plan = plan(report.damage, window.damage.rects(), frame);
    #[cfg(windows)]
    let plan = surface_change_plan(plan, renderer_surface, window.surface);
    if scroll_pending || frame_reaches_gpu(plan, renderer_surface, window.surface) {
        PublishDecision::Publish(plan)
    } else {
        PublishDecision::NoGpuWork
    }
}

pub(crate) fn commit_frame(
    window: &mut Window,
    ticket: FrameTicket,
    report: PaintReport,
) -> FrameOutcome {
    let scroll_pending = window
        .pending_delta
        .as_ref()
        .is_some_and(|delta| !delta.scroll.is_empty());
    debug_assert_eq!(
        report.damage.rects,
        window.damage.rects().len(),
        "damage summary and accumulator disagree"
    );
    let plan = match publish_decision(window, &report, scroll_pending) {
        PublishDecision::NoPixels => {
            window.scheduler.abort(ticket);
            release_unpublished(window);
            #[cfg(feature = "hud")]
            {
                window.frame_started_at = None;
                window.frame_cpu = None;
            }
            #[cfg(feature = "test-support")]
            window.record_idle();
            return FrameOutcome::Idle;
        }
        PublishDecision::NoGpuWork => {
            window.tree.settle_frame();
            window.scheduler.abort(ticket);
            release_unpublished(window);
            #[cfg(feature = "hud")]
            {
                window.frame_started_at = None;
                window.frame_cpu = None;
            }
            #[cfg(feature = "test-support")]
            window.record_idle();
            return FrameOutcome::Idle;
        }
        PublishDecision::Publish(plan) => plan,
    };
    #[cfg(feature = "test-support")]
    let frame = window.frame_rect();
    let index = window.scheduler.commit(ticket);
    window.tree.settle_frame();
    if let Some(mut renderer) = window.renderer.take() {
        let stats = window.submit_gpu(&mut renderer, index, plan);
        window.renderer = Some(renderer);
        #[cfg(feature = "hud")]
        record_render_stats(window, stats);
        #[cfg(not(feature = "hud"))]
        {
            let _ = stats;
        }
    }
    window.pending_delta = None;
    release_unpublished(window);
    #[cfg(feature = "test-support")]
    {
        if window.renderer.is_none() {
            window.stats.encoded_lanes = report.encoded_lanes;
            window.stats.reused_segments = report.reused_segments;
            window.stats.uploaded_bytes = report.uploaded_bytes;
            window.stats.instances = report.instances;
            window.stats.scroll_offset_frames = report.scroll_slots;
        }
        let stats = std::mem::take(&mut window.stats);
        let mut record = crate::test_support::record::capture(
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
        record.skipped_by_budget = window.frame_budget_deferrals();
        window.record = Some(record);
    }
    FrameOutcome::Committed(index)
}

fn release_unpublished(window: &mut Window) {
    window.pending_payload = None;
}

pub(crate) fn submit_gpu(
    window: &mut Window,
    renderer: &mut Renderer,
    index: FrameIndex,
    plan: Plan,
) -> Option<crate::render::RenderStats> {
    let surface = window.physical_surface();
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
    window.timings.reset(index);
    if let Some(sink) = renderer.frame_sink() {
        if window.compositor_ticket.is_none() {
            window.compositor_ticket = Some(CompositorTicket::new(window.id()));
            crate::render::compositor_thread::compositor().register(window.id(), sink);
            window.composer = None;
            window.pending_payload = None;
        }
        if let Some(mut delta) = window.pending_delta.take() {
            fill_header(&mut delta, window, index, plan, surface);
            if crate::render::compositor_thread::compositor()
                .publish(delta)
                .is_err()
            {
                window.request_repaint();
            }
        }
        renderer.note_published_surface(surface);
        window.pending_payload = None;
        #[cfg(feature = "test-support")]
        {
            let counters = crate::render::compositor_thread::compositor().counters();
            window.stats.compositor_inbox_latency = Some(counters.inbox_latency);
            window.stats.encode_on_compositor = counters.encoded_on_compositor();
            window.stats.encoded_lanes = counters.encoded_lanes;
            window.stats.reused_segments = counters.reused_segments;
            window.stats.scroll_offset_frames =
                usize::from(counters.encoded_lanes == 0 && counters.frames > 0);
        }
        return None;
    }
    let composed = window.pending_payload.take()?;
    #[cfg(feature = "test-support")]
    {
        window.stats.encoded_lanes = composed.stats.encoded_lanes;
        window.stats.reused_segments = composed.stats.reused_segments;
        window.stats.instances = composed.stats.drawn_instances as usize;
        window.stats.scroll_offset_frames = composed.stats.scroll_slots;
        window.stats.encode_on_compositor = false;
        window.stats.compositor_inbox_latency = None;
    }
    let payload = composed.payload;
    #[cfg(feature = "test-support")]
    {
        window.stats.uploaded_bytes = payload.uploaded_bytes();
    }
    Some(renderer.publish(FrameRequest {
        index,
        payload,
        plan: composed.plan,
        surface,
        scale: window.scale.factor(),
        #[cfg(target_os = "linux")]
        surface_local: window.size(),
        #[cfg(target_os = "linux")]
        buffer_scale: window.buffer_scale,
        published_at: Instant::now(),
    }))
}

#[cfg(feature = "hud")]
fn record_render_stats(window: &mut Window, stats: Option<crate::render::RenderStats>) {
    if let Some(stats) = stats {
        window.render_stats = stats;
    }
}

fn fill_header(
    delta: &mut crate::render::delta::SceneDelta,
    window: &Window,
    index: FrameIndex,
    plan: Plan,
    surface: (u32, u32),
) {
    delta.header.index = index;
    delta.header.plan = plan;
    delta.header.surface = surface;
    delta.header.scale = window.scale;
    #[cfg(target_os = "linux")]
    {
        delta.header.surface_local = window.size();
        delta.header.buffer_scale = window.buffer_scale;
    }
    delta.header.published_at = Instant::now();
}

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
    static NO_GEOMETRY: std::sync::OnceLock<crate::paint::geometry_store::GeometryStore> =
        std::sync::OnceLock::new();
    let geometry = NO_GEOMETRY.get_or_init(crate::paint::geometry_store::GeometryStore::new);
    let report = match window.paint_frame(geometry, None, None) {
        Ok(report) => report,
        Err(error) => {
            window.abort_frame(ticket);
            return Err(error);
        }
    };
    window.note_skipped_inline_images(&report.skipped_inline_images);
    Ok(window.commit_frame(ticket, report))
}

pub(crate) fn frame_index(window: &Window) -> FrameIndex {
    FrameIndex(window.scheduler.committed())
}

pub(crate) fn compute_layout(window: &mut Window) -> Result<LayoutReport, EngineError> {
    let report = window.tree.compute_layout(taffy::geometry::Size {
        width: Some(window.options.size.width),
        height: Some(window.options.size.height),
    })?;
    window.tree.mark_moved_a11y();
    Ok(report)
}

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

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;

    use super::FrameOutcome;
    #[cfg(feature = "test-support")]
    use super::note_reset_epoch;
    use crate::{
        app::App,
        math::{Rect, Scale, Size},
        node::builder::NodeBuilder,
        window::{WindowId, WindowOptions},
    };

    #[test]
    fn a_committed_headless_frame_does_not_retain_its_intermediates() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut app = App::new(runtime.handle().clone());
        let window: WindowId = app.create_window(WindowOptions {
            title: SmolStr::new_static("frame-release"),
            size: Size::new(320.0, 240.0),
            scale: Scale::ONE,
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("window").tree().root();
        let _ = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 32.0, 32.0))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("box");

        let outcome = app.frame(window).expect("window").expect("frame");
        assert!(
            matches!(outcome, FrameOutcome::Committed(_)),
            "headless 帧必须提交（没有渲染器时 `frame_reaches_gpu` 恒真）"
        );
        let view = app.window(window).expect("window");
        assert!(view.pending_delta.is_none(), "帧末必须释放 SceneDelta");
        assert!(view.pending_payload.is_none(), "帧末必须释放载荷");
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn a_renderer_rebuild_forces_the_next_frame_full() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut app = App::new(runtime.handle().clone());
        let window: WindowId = app.create_window(WindowOptions {
            title: SmolStr::new_static("resync"),
            size: Size::new(320.0, 240.0),
            scale: Scale::ONE,
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("window").tree().root();
        let mut nodes = Vec::new();
        for index in 0..3 {
            let node = NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, index as f32 * 24.0, 32.0, 20.0))
                .paint(crate::paint::style::PaintSpec {
                    background: crate::paint::color::Color::WHITE,
                    ..crate::paint::style::PaintSpec::default()
                })
                .build(app.window_mut(window).expect("window").tree_mut(), root)
                .expect("box");
            nodes.push(node);
        }
        let _ = app.frame(window).expect("window").expect("full frame");
        let view = app.window(window).expect("window");
        assert_eq!(
            view.record().expect("record").stats.encoded_lanes,
            3,
            "首帧必须全量编码"
        );
        let _ = app
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_paint(nodes[0]);
        let _ = app
            .frame(window)
            .expect("window")
            .expect("incremental frame");
        let view = app.window(window).expect("window");
        assert_eq!(
            view.record().expect("record").stats.encoded_lanes,
            1,
            "前置条件：稳态帧是增量（只重编码 1 条 lane）"
        );
        let view = app.window_mut(window).expect("window");
        assert!(
            !note_reset_epoch(view, 1),
            "首次观察只记录代际（首帧本来就整帧发布）"
        );
        assert!(note_reset_epoch(view, 2), "代际变化必须强制整帧重发");
        let outcome = app.frame(window).expect("window").expect("resync frame");
        assert!(
            matches!(outcome, FrameOutcome::Committed(_)),
            "整帧重发必须真的提交"
        );
        let view = app.window(window).expect("window");
        let record = view.record().expect("record");
        assert!(
            record.damage_full,
            "重建后的第一帧必须是 Plan::Full（持久目标也是新的）"
        );
        assert_eq!(
            record.stats.encoded_lanes, 3,
            "重建后的第一帧必须重编码全部 lane（只发变化 lane ⇒ 镜像里其余段是 0）"
        );
        let _ = app
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_paint(nodes[1]);
        let _ = app
            .frame(window)
            .expect("window")
            .expect("incremental frame");
        let view = app.window(window).expect("window");
        assert_eq!(
            view.record().expect("record").stats.encoded_lanes,
            1,
            "整帧重发只发生一次，之后回到增量"
        );
    }

    #[cfg(feature = "hud")]
    #[test]
    fn a_threaded_commit_keeps_the_last_completed_draw_stats() {
        use crate::render::RenderStats;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut app = App::new(runtime.handle().clone());
        let window: WindowId = app.create_window(WindowOptions {
            title: SmolStr::new_static("render-stats"),
            size: Size::new(320.0, 240.0),
            scale: Scale::ONE,
            ..WindowOptions::default()
        });
        let view = app.window_mut(window).expect("window");
        super::record_render_stats(
            view,
            Some(RenderStats {
                draw_calls: 7,
                instances: 9,
                geometry_uploaded_bytes: 0,
            }),
        );
        super::record_render_stats(view, None);
        let metrics = view.debug_hud_metrics();
        assert_eq!((metrics.draw_calls, metrics.instances), (7, 9));

        super::record_render_stats(
            view,
            Some(RenderStats {
                draw_calls: 3,
                instances: 4,
                geometry_uploaded_bytes: 0,
            }),
        );
        let metrics = view.debug_hud_metrics();
        assert_eq!((metrics.draw_calls, metrics.instances), (3, 4));
    }
}
