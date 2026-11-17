//! 绘制：lane 重建 / damage / 增量协议采集 / 合成（inline）/ 场景访问。
//!
//! **编码不再发生在这里**：`paint_frame` 只产 `SceneDelta`（变化 lane 快照 + 绘制序 + 滚动位
//! 移 + 几何描述）与 damage。编码与载荷构造在 `render::compose::WindowComposer`（inline 模式，
//! 无 GPU 线程的窗口）或合成线程（真实窗口）里完成 —— 见 `render/compositor_thread.rs`。

use super::Window;
#[cfg(feature = "test-support")]
use crate::paint::instance::InstanceBuffer;
#[cfg(feature = "test-support")]
use crate::test_support::record::PaintLog;
use crate::{
    error::EngineError,
    math::Rect,
    paint::{
        pass::{PaintPass, PaintReport},
        scene::Scene,
    },
    render::{
        compose::ComposeStats,
        delta::{FrameHeader, SceneDelta},
    },
};

// 消费者：`impl Window::paint_frame` 的薄委托（外部调用点见该方法文档）。
/// lane 重建/复用 + damage + `SceneDelta` 采集（+ inline 模式的合成）。
///
/// # Errors
/// [`EngineError::ClipDepthExceeded`]：clip 嵌套超过上限（inline 合成路径；release 也失败）。
/// 返回 `Err` 时本帧未提交：调用方必须 `abort_frame`，damage/dirty 保留待重试。
pub(crate) fn paint_frame(
    window: &mut Window,
    geometry: &crate::paint::geometry_store::GeometryStore,
) -> Result<PaintReport, EngineError> {
    // 借用式取出（保容量）：`tree` 与 `scene` 是不同字段，循环内可同时可变借用。
    let mut removed: Vec<crate::paint::scene::LaneId> = Vec::new();
    for lane in window.tree.drain_removed_lanes() {
        window.scene.release_lane(lane);
        removed.push(lane);
    }
    let scale = window.scale;
    let frame = window.frame_rect();
    window.damage.reset(scale);
    // 几何内容在「隐藏 / 卸载 / 布局移动」路径上无法精确算出溢出范围（拿不到
    // `GeometryDesc`）：保守升级为全帧 damage，避免溢出部分残影（复审 A3）。
    if window.tree.take_geometry_damage_escalation() {
        window.full_damage_pending = true;
    }
    let has_damage = window.full_damage_pending || window.pending_damage.is_some();
    if window.full_damage_pending {
        window.damage.push_full();
        window.full_damage_pending = false;
    }
    // 滚动（P10 §7.8）：唯一来源是视口矩形；节点级 damage 不参与（偏移不写 `layout.bounds`）。
    if let Some(pending) = window.pending_damage.take() {
        window.damage.push(pending, 1.0, None);
    }
    // P11：只有 `A11Y` 脏（`bind_a11y` 重算）的帧**没有任何像素要画**：早退让 lane 保持
    // `REUSABLE`、damage 保持空，帧末门禁把本帧退回 `Idle`（P11 审查 #6）。
    if !has_damage && !window.needs_node_repaint() && removed.is_empty() {
        window.pending_delta = None;
        return Ok(PaintReport {
            rebuilt: 0,
            damage: crate::render::damage::DamageSummary::default(),
            #[cfg(feature = "test-support")]
            instances: 0,
            #[cfg(feature = "test-support")]
            encoded_lanes: 0,
            #[cfg(feature = "test-support")]
            reused_segments: 0,
            #[cfg(feature = "test-support")]
            uploaded_bytes: 0,
            #[cfg(feature = "test-support")]
            log: crate::test_support::record::PaintLog::default(),
        });
    }
    window.paint_order.clear();
    let focused = window.focus.focused();
    let mut pass = PaintPass {
        tree: &mut window.tree,
        geometry,
        scene: &mut window.scene,
        damage: &mut window.damage,
        scale,
        frame,
        order: &mut window.paint_order,
        focused,
        #[cfg(feature = "test-support")]
        log: PaintLog::default(),
    };
    let report = pass.run()?;
    // ── B6-2：采集 `SceneDelta`（UI 线程的产物到此为止）────────────────────────
    let surface = window.physical_surface();
    let mut delta = SceneDelta::empty(FrameHeader {
        window: window.id(),
        // 头部在 `submit_gpu` 里补齐（这里还不知道是否真的提交）。
        index: crate::window::FrameIndex::default(),
        plan: crate::render::compositor::Plan::Idle,
        surface,
        scale,
        #[cfg(target_os = "linux")]
        surface_local: window.size(),
        #[cfg(target_os = "linux")]
        buffer_scale: window.buffer_scale,
        published_at: std::time::Instant::now(),
    });
    let full = window.delta_builder.full_for(scale);
    {
        let Window {
            scene,
            delta_builder,
            scroll_slots,
            ..
        } = window;
        delta_builder.collect(scene, geometry, scroll_slots, full, &mut delta);
    }
    for lane in removed {
        window.delta_builder.note_removed(lane, &mut delta);
    }
    // inline 模式（无 GPU 线程可分发的窗口）：就地合成，报告里带上编码统计（测试与 HUD 读）。
    let mut report = report;
    if let Some(composed) = compose_inline(window, &delta)? {
        report = annotate_report(report, composed.stats);
        window.pending_payload = Some(composed);
    }
    window.pending_delta = Some(delta);
    #[cfg(feature = "hud")]
    {
        window.frame_cpu = window.frame_started_at.map(|started| started.elapsed());
    }
    Ok(report)
}

/// 把合成统计写进 `PaintReport`（`test-support` 的帧记录读这四项；生产构建没有这些字段）。
#[cfg(feature = "test-support")]
fn annotate_report(mut report: PaintReport, stats: ComposeStats) -> PaintReport {
    report.instances = stats.drawn_instances as usize;
    report.encoded_lanes = stats.encoded_lanes;
    report.reused_segments = stats.reused_segments;
    report.uploaded_bytes = stats.written_bytes;
    report
}

#[cfg(not(feature = "test-support"))]
fn annotate_report(report: PaintReport, _stats: ComposeStats) -> PaintReport {
    report
}

/// inline 合成（`composer` 在 UI 线程时）：返回 `None` = 本帧没有可发布的载荷（threaded 模式）。
fn compose_inline(
    window: &mut Window,
    delta: &SceneDelta,
) -> Result<Option<crate::render::compose::ComposedFrame>, EngineError> {
    let resolver = window.resolver;
    let Some(composer) = window.composer.as_mut() else {
        return Ok(None);
    };
    composer.compose(delta, resolver).map(Some)
}

// 消费者：`impl Window::needs_node_repaint` 的薄委托（外部调用点见该方法文档）。
/// 是否有节点需要重建 lane（`LAYOUT` / `PAINT` / `REUSABLE` 不在）。
///
/// `A11Y` 不在判据里：a11y 更新不产生 damage（`design §7.5` / P03 §9.3 表第 9 行）。
pub(crate) fn needs_node_repaint(window: &Window) -> bool {
    // O(1) 门禁（R03-1）：读 `dirty_paint` 反向集合是否为空，不再全树扫描。
    window.tree.has_paint_work()
}

// 消费者：`impl Window::instance_buffer` 的薄委托（外部调用点见该方法文档）。
/// 最近一帧的编码结果（**只有 inline 模式有**：真实窗口的编码器在合成线程里）。
#[cfg(feature = "test-support")]
pub(crate) fn instance_buffer(window: &Window) -> Option<&InstanceBuffer> {
    window
        .composer
        .as_ref()
        .map(crate::render::compose::WindowComposer::buffer)
}

// 消费者：`impl Window::scene` 的薄委托（外部调用点见该方法文档）。
/// 只读场景（S-10 / S-33：应用层**拿不到** `&mut Scene`，`Scene` 也**没有**公开构造器）。
pub(crate) fn scene(window: &Window) -> &Scene {
    &window.scene
}

// 消费者：`impl Window::encoded_geometry` 的薄委托（外部调用点见该方法文档）。
/// 测试专用：最近一次编码出的几何绘制（校验 `App` 的几何表真的接进了编码器）。
#[cfg(test)]
pub(crate) fn encoded_geometry(window: &Window) -> &[crate::paint::instance::GeometryDrawGpu] {
    window
        .composer
        .as_ref()
        .map_or(&[], |composer| composer.buffer().geometry())
}

// 消费者：`impl Window::request_damage` 的薄委托（外部调用点见该方法文档）。
/// 请求下一帧重绘一个逻辑矩形（滚动视口；并集语义）。
pub(crate) fn request_damage(window: &mut Window, rect: Rect) {
    window.pending_damage = Some(match window.pending_damage {
        Some(existing) => existing.union(rect),
        None => rect,
    });
}

// 消费者：`impl Window::request_repaint` 的薄委托（外部调用点见该方法文档）。
/// 请求下一帧（全量 damage）：平台泵用它补发没赶上本帧的图集行。
pub(crate) fn request_repaint(window: &mut Window) {
    window.full_damage_pending = true;
}
