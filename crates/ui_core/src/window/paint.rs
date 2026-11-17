//! 绘制：lane 重建 / damage / 实例编码 / 场景访问。

use super::Window;
#[cfg(feature = "test-support")]
use crate::test_support::record::PaintLog;
use crate::{
    error::EngineError,
    math::Rect,
    paint::{
        instance::InstanceBuffer,
        pass::{PaintPass, PaintReport},
        scene::Scene,
    },
};

// 消费者：`impl Window::paint_frame` 的薄委托（外部调用点见该方法文档）。
/// lane 重建/复用 + damage + 实例编码（`design/phase-06-paint-runtime.md` §9.6）。
///
/// # Errors
/// [`EngineError::ClipDepthExceeded`]：clip 嵌套超过上限（release 也失败，见 P14 审查）。
/// 返回 `Err` 时本帧未提交：调用方必须 `abort_frame`，damage/dirty 保留待重试。
pub(crate) fn paint_frame(
    window: &mut Window,
    geometry: &crate::paint::geometry_store::GeometryStore,
) -> Result<PaintReport, EngineError> {
    // 借用式取出（保容量）：`tree` 与 `scene` 是不同字段，循环内可同时可变借用。
    for lane in window.tree.drain_removed_lanes() {
        window.scene.release_lane(lane);
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
    if !has_damage && !window.needs_node_repaint() {
        return Ok(PaintReport {
            rebuilt: 0,
            damage: crate::render::damage::DamageSummary::default(),
            #[cfg(feature = "test-support")]
            instances: 0,
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
    let mut report = pass.run()?;
    let buffer = window
        .encoder
        .encode(&window.scene, scale, &window.resolver, geometry)?;
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
        window.frame_cpu = window.frame_started_at.map(|started| started.elapsed());
    }
    Ok(report)
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
/// 最近一帧的编码结果（渲染层上传源；P08 的 `Renderer` 消费）。
pub(crate) fn instance_buffer(window: &Window) -> &InstanceBuffer {
    window.encoder.buffer()
}

// 消费者：`impl Window::instance_bytes` 的薄委托（外部调用点见该方法文档）。
/// 本帧编码出的实例字节（P07 的上传入口；应用层不得依赖其内容）。
///
/// 返回最近一帧 `InstanceEncoder` 的 `&[u8]`（`GpuPrimitive` 数组的字节视图，P02 §8.1 的 128 B 布局）。
pub(crate) fn instance_bytes(window: &Window) -> &[u8] {
    window.instance_buffer().as_bytes()
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
    window.encoder.buffer().geometry()
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
