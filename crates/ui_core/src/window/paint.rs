//! 绘制：lane 重建 / damage / 增量协议采集 / 合成（inline）/ 场景访问。
//!
//! **编码不再发生在这里**：`paint_frame` 只产 `SceneDelta`（变化 lane 快照 + 绘制序 + 滚动位
//! 移 + 几何描述）与 damage。编码与载荷构造在 `render::compose::WindowComposer`（inline 模式，
//! 无 GPU 线程的窗口）或合成线程（真实窗口）里完成 —— 见 `render/compositor_thread.rs`。

use super::Window;
#[cfg(feature = "test-support")]
use crate::paint::instance::InstanceBuffer;
#[cfg(feature = "test-support")]
use crate::test_support::record::{DeltaRecord, PaintLog};
use crate::{
    error::EngineError,
    math::Rect,
    paint::{
        pass::{PaintPass, PaintReport},
        primitive::Primitive,
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
/// **帧分类（A1 脏子树剪枝）**：先判断走哪条遍历路径，再交给 `PaintPass`：
/// - **全量**：整帧 damage / 有 lane 被释放 / 结构代际变化 / 滚动容器登记变化 / 强制全量 /
///   脏节点没有 lane（首画）或 lane 为空（此前隐藏）或 `clip_key` 为空（此前隐藏）—— 这些
///   情况下绘制序成员或滚动槽编号可能变化，必须重建两者；
/// - **增量**：只 DFS「脏节点祖先闭包」，其余子树整棵跳过（它们上一帧可复用）。
///
/// # Errors
/// [`EngineError::ClipDepthExceeded`]：clip 嵌套超过上限（inline 合成路径；release 也失败）。
/// 返回 `Err` 时本帧未提交：调用方必须 `abort_frame`，damage/dirty 保留待重试。
pub(crate) fn paint_frame(
    window: &mut Window,
    geometry: &crate::paint::geometry_store::GeometryStore,
    atlas: Option<&crate::render::AtlasAllocator>,
    resources: Option<&crate::resources::ResourceCache>,
) -> Result<PaintReport, EngineError> {
    window.tree.reset_paint_visited();
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
    // A1：滚动偏移变化即使不产生 lane 重建也必须进合成端槽位表（本帧可能同时有别的
    // paint 工作、用不上纯滚动快路径）。
    let scroll_damage_pending = window.pending_damage.is_some();
    // 纯滚动帧的快路径判据要用「本帧是否整帧重画」（下面会清掉这个位）。
    let full_damage = window.full_damage_pending;
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
        // 本帧没有产出新 delta，但**上一帧未被发布的 delta 必须保留**（P0：Idle/裁剪帧
        // 不能丢掉已构建的 lane 内容）；下一帧真实提交时会由下面的 `absorb` 合并。
        return Ok(PaintReport {
            rebuilt: 0,
            damage: crate::render::damage::DamageSummary::default(),
            stale_text: smallvec::SmallVec::new(),
            stale_canvas_runs: 0,
            skipped_inline_images: smallvec::SmallVec::new(),
            #[cfg(feature = "test-support")]
            instances: 0,
            #[cfg(feature = "test-support")]
            encoded_lanes: 0,
            #[cfg(feature = "test-support")]
            reused_segments: 0,
            #[cfg(feature = "test-support")]
            uploaded_bytes: 0,
            #[cfg(feature = "test-support")]
            geometry_encoded_chunks: 0,
            #[cfg(feature = "test-support")]
            geometry_uploaded_bytes: 0,
            #[cfg(feature = "test-support")]
            scroll_slots: 0,
            #[cfg(feature = "test-support")]
            log: crate::test_support::record::PaintLog::default(),
            #[cfg(feature = "test-support")]
            delta: DeltaRecord::default(),
        });
    }
    // A1 帧分类的树侧输入：结构 / 滚动容器登记代际变化或强制全量 ⇒ 本帧必须走全量路径。
    // 必须在纯滚动快路径**之前**取（它会消费 `force_full_paint`）。
    let epoch_changed = window.tree.paint_epoch_changed();
    // ── 纯滚动帧的快路径（AR4 P1-3 的性能验收）────────────────────────────────
    //
    // 有 damage（滚动视口）但**没有任何节点要重建**（没有 PAINT/LAYOUT 脏、没有移除的 lane、
    // 不是整帧重画）时，paint 遍历唯一的产出是「把每条 lane 按原序推回绘制序」—— 10k 节点
    // 约 240 µs 的白工，而滚动帧的架构目标是 ≤200 µs。
    //
    // 滚动槽位是**布局派生**的：树没变 ⇒ 槽位编号与视口都不变，只有偏移变了。于是直接按
    // `scroll_slot_nodes` 把偏移刷进槽位表，与「遍历后 100% 复用」逐字节等价（`rebuilt == 0`、
    // `order` 不变、没有节点级 damage）。槽位表与树不再一致时 `refresh_scroll_offsets` 返回
    // `None`，落到完整遍历（那是唯一会重建槽位表的路径）。
    let report = if !full_damage
        && removed.is_empty()
        && !epoch_changed
        && !window.needs_node_repaint()
        && window.scroll_slots.len() > 1
    {
        refresh_scroll_offsets(window)
    } else {
        None
    };
    let report = match report {
        Some(damage) => {
            // 没有遍历 ⇒ 本帧访问 0 个节点；计入增量帧（没有重建绘制序 / 滚动槽）。
            window.tree.note_paint_frame(false, 0);
            PaintReport {
                rebuilt: 0,
                damage,
                stale_text: smallvec::SmallVec::new(),
                stale_canvas_runs: 0,
                skipped_inline_images: smallvec::SmallVec::new(),
                #[cfg(feature = "test-support")]
                instances: 0,
                #[cfg(feature = "test-support")]
                encoded_lanes: 0,
                #[cfg(feature = "test-support")]
                reused_segments: 0,
                #[cfg(feature = "test-support")]
                uploaded_bytes: 0,
                #[cfg(feature = "test-support")]
                geometry_encoded_chunks: 0,
                #[cfg(feature = "test-support")]
                geometry_uploaded_bytes: 0,
                #[cfg(feature = "test-support")]
                scroll_slots: 0,
                #[cfg(feature = "test-support")]
                log: crate::test_support::record::PaintLog::default(),
                #[cfg(feature = "test-support")]
                delta: DeltaRecord::default(),
            }
        }
        None => {
            // ── 脏子树剪枝的帧分类（A1）────────────────────────────────────────
            let dirty = window.tree.take_paint_work();
            let mut full = full_damage || !removed.is_empty() || epoch_changed;
            if !full {
                for &node in &dirty {
                    if window.tree.get(node).is_err() {
                        continue;
                    }
                    let Some(lane) = window.tree.lane_of(node) else {
                        // 首画：该节点从未产生过 lane ⇒ 可能要新建 lane（绘制序成员变化）。
                        full = true;
                        break;
                    };
                    if window
                        .scene
                        .lane_primitives(lane)
                        .is_none_or(<[Primitive]>::is_empty)
                    {
                        // lane 为空 = 此前被隐藏 / 清空 ⇒ 重新可见时整棵子树都要重建。
                        full = true;
                        break;
                    }
                    if window.tree.clip_key(node).is_none() {
                        // 上次重建时的 clip 上下文为空 = 此前从未画过 / 被隐藏 ⇒ 子树展开。
                        full = true;
                        break;
                    }
                }
            }
            if !full && scroll_damage_pending && !sync_scroll_offsets(window) {
                // 槽位表与树不再一致（容器卸载 / 不再是滚动容器）：只有全量路径能重建。
                full = true;
            }
            if !full {
                window.tree.mark_paint_closure(&dirty);
            }
            window.tree.recycle(dirty);
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
                atlas,
                resources,
                // 全量路径每帧重写槽位表；增量路径按缓存槽号写回（见 `PaintPass` 的字段文档）。
                scroll_slots: &mut window.scroll_slots,
                scroll_slot_nodes: &mut window.scroll_slot_nodes,
                #[cfg(feature = "test-support")]
                log: PaintLog::default(),
            };
            if full {
                pass.run()?
            } else {
                pass.run_incremental()?
            }
        }
    };
    let mut report = report;
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
    // 上一帧构建了 delta 但没有提交（damage 被窗口/视口裁空、或没有渲染器可发布）：
    // 它的 lane 版本已经推进，必须并进本帧，否则这些变化永远不会到达合成端。必须等
    // `collect`/`note_removed` 先填好本帧内容再 `absorb`，否则同 lane 会产生重复条目，
    // 且旧的 removed 会与本帧的新增 lane 同时存在（违反 absorb 的「新者胜/不复活」契约）。
    if let Some(previous) = window.pending_delta.take() {
        crate::render::delta::absorb(previous, &mut delta);
    }
    // 本帧图片图集的**可画层上限**（fail-closed，RBT-01 不变量 1）：App 在图集上传 pass 里按
    // 「该窗口渲染器已整组入队的层数」写进窗口；合成端据此过滤 `Primitive::Image`，并且在
    // 上限变化的那一帧强制整帧重编码 + 全量载荷（旧实例里可能还留着不该画的层）。
    delta.image_layers = window.image_drawable_layers;
    // inline 模式（无 GPU 线程可分发的窗口）：只在**本帧确实会发布**时就地合成。
    // 判据来自 `commit_frame` 的同一个 `publish_decision`：Idle 帧如果提前调用
    // `WindowComposer::compose`，LaneStore 的版本与编码器状态会被推进，而
    // `release_unpublished` 随后丢弃载荷 —— 下一帧 absorb 旧 delta 时版本已相等、重编码
    // 为空，lane 的新实例字节永远上不了屏。
    let scroll_pending = !delta.scroll.is_empty();
    // A1 / P1A-1b 的断言面：本帧 `SceneDelta` 的结构事实（是否整帧、是否携带绘制序、
    // 各类条目数）。lane 句柄跨窗口不可比，差分 oracle 比较「结构 + 计数」。
    #[cfg(feature = "test-support")]
    {
        report.delta = DeltaRecord {
            full: delta.full,
            order_some: delta.order.is_some(),
            lanes: delta.lanes.len(),
            removed: delta.removed.len(),
            scroll: delta.scroll.len(),
        };
    }
    if let super::frame::PublishDecision::Publish(_) =
        super::frame::publish_decision(window, &report, scroll_pending)
        && let Some(composed) = compose_inline(window, &delta)?
    {
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
    report.geometry_encoded_chunks = stats.geometry_encoded_chunks;
    report.geometry_uploaded_bytes = stats.geometry_uploaded_bytes;
    report.scroll_slots = stats.scroll_slots;
    report
}

#[cfg(not(feature = "test-support"))]
fn annotate_report(report: PaintReport, _stats: ComposeStats) -> PaintReport {
    report
}

/// 纯滚动帧的快路径：把每个滚动容器的当前偏移刷进槽位表，返回本帧 damage。
///
/// 返回 `None` = 槽位表与树不再一致（容器已卸载 / 不再是滚动容器）⇒ 调用方走完整遍历。
/// **不重建任何 lane**：图元烘焙在「不含本槽位移」的内容空间里，滚动只改偏移。
fn refresh_scroll_offsets(window: &mut Window) -> Option<crate::render::damage::DamageSummary> {
    if !sync_scroll_offsets(window) {
        return None;
    }
    let frame = window.frame_rect();
    Some(window.damage.finish(frame))
}

/// 把每个滚动容器的当前偏移刷进槽位表（不改 lane、不结算 damage）。
///
/// 返回 `false` = 槽位表与树不再一致（容器已卸载 / 不再是滚动容器）⇒ 调用方必须走全量路径。
///
/// 消费者有两个：纯滚动帧的快路径（本帧唯一工作是刷偏移）与**有 paint 工作的滚动帧**
/// （偏移变化不会让脏闭包覆盖所有滚动容器，必须在这里统一刷新，否则未被访问的容器的
/// 滚动会丢）。
fn sync_scroll_offsets(window: &mut Window) -> bool {
    debug_assert_eq!(
        window.scroll_slots.len(),
        window.scroll_slot_nodes.len() + 1,
        "槽位表与节点表必须平行（哨兵之外一一对应）"
    );
    for (index, node) in window.scroll_slot_nodes.iter().copied().enumerate() {
        let Some(state) = window.tree.scroll_slot(node) else {
            return false;
        };
        let Some(slot) = window.scroll_slots.get_mut(index + 1) else {
            return false;
        };
        slot.0 = state.offset;
    }
    true
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

#[cfg(test)]
mod tests {
    use crate::{
        app::App,
        math::{Rect, Scale, Size},
        node::{builder::NodeBuilder, id::NodeId},
        paint::{color::Color, style::PaintSpec},
        window::{FrameOutcome, WindowId, WindowOptions},
    };

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
    }

    fn headless_window(app: &mut App) -> WindowId {
        app.create_window(WindowOptions {
            title: smol_str::SmolStr::new_static("paint-prune"),
            size: Size::new(1280.0, 800.0),
            scale: Scale::ONE,
            ..WindowOptions::default()
        })
    }

    fn frame_once(app: &mut App, window: WindowId) -> FrameOutcome {
        app.frame(window).expect("窗口存在").expect("帧成功")
    }

    fn counters(app: &App, window: WindowId) -> crate::node::tree::PaintCounters {
        app.window(window)
            .expect("窗口存在")
            .tree()
            .paint_counters()
    }

    /// A1 / P1A-1a：4 万节点树里改 5 个节点，增量帧的**访问数**只随脏闭包增长（≤256），
    /// 与树大小无关；全量帧计数不增长。
    ///
    /// 变异守卫：把 `paint_frame` 的帧分类强制成全量（或删掉 `paint_node` 的 `paint_marked`
    /// 检查），本用例的 `visited_nodes` 会回到 40001 而失败。
    #[test]
    fn a_40k_tree_incremental_frame_visits_a_handful_of_nodes() {
        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let window = headless_window(&mut app);
        let root = app.window(window).expect("窗口").tree().root();
        const LEAVES: usize = 40_000;
        let mut changed: Vec<NodeId> = Vec::with_capacity(5);
        {
            let tree = app.window_mut(window).expect("窗口").tree_mut();
            for index in 0..LEAVES {
                let node = NodeBuilder::boxed()
                    .layout_override(Rect::new(
                        (index % 40) as f32 * 32.0,
                        (index / 40) as f32 * 8.0,
                        24.0,
                        6.0,
                    ))
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        ..PaintSpec::default()
                    })
                    .build(tree, root)
                    .expect("叶子节点");
                if index % (LEAVES / 5) == 0 {
                    changed.push(node);
                }
            }
        }
        let node_count = app.window(window).expect("窗口").tree().len();
        assert_eq!(node_count, LEAVES + 1, "树规模 = 根 + 叶子");

        // 首帧：全量（建 lane / 绘制序 / 滚动槽）。
        let first = frame_once(&mut app, window);
        assert!(matches!(first, FrameOutcome::Committed(_)), "首帧必须提交");
        let counters_before = counters(&app, window);
        assert!(
            counters_before.visited_nodes > LEAVES,
            "首帧是全量遍历：{}",
            counters_before.visited_nodes
        );

        // 改 5 个节点：请求节点级重绘（只标这 5 棵子树的 PAINT）。
        for node in &changed {
            assert!(
                app.request_node_repaint(window, *node).expect("节点级重绘"),
                "可见节点必须接受重绘请求"
            );
        }
        let outcome = frame_once(&mut app, window);
        assert!(
            matches!(outcome, FrameOutcome::Committed(_)),
            "增量帧必须提交"
        );
        let counters = counters(&app, window);
        assert!(
            counters.visited_nodes <= 256,
            "改 5 个节点的访问数必须 ≤ 256（树共 {node_count} 节点）：{}",
            counters.visited_nodes
        );
        assert!(
            counters.visited_nodes * 100 < node_count,
            "访问数必须与树大小无关：{} vs {node_count}",
            counters.visited_nodes
        );
        assert_eq!(
            counters.full_frames, counters_before.full_frames,
            "没有结构 / lane 成员变化 ⇒ 不得退化为全量路径"
        );
        assert_eq!(
            counters.incremental_frames,
            counters_before.incremental_frames + 1,
            "本帧必须走增量路径"
        );
    }

    // ── 差分 property / oracle（test-support）───────────────────────────────
    // 同一操作序列分别喂给两个窗口：A 走**帧分类**（增量剪枝），B 每帧强制全量路径
    // （`NodeTree::force_full_paint`：只改分类、不塞脏集合 ⇒ 两边脏集合完全相同）。
    // 逐帧比较：按 DFS 归一化的 lane 内容 / 每 lane 滚动槽 / 绘制内容序 / 滚动槽表 /
    // damage / `SceneDelta` 结构快照。这是「增量产物 == 全量产物」的差分 oracle。

    #[cfg(feature = "test-support")]
    mod differential {
        use super::*;
        use crate::{
            interaction::ScrollConfig,
            layout::LayoutOverride,
            math::{PhysicalRect, Point, Rect, Size},
            node::{builder::NodeBuilder, id::NodeId, signal::Signal},
            paint::{
                color::Color,
                primitive::Primitive,
                style::{Clip, PaintSpec, Style, StylePatch},
            },
            test_support::record::DeltaRecord,
            window::{FrameOutcome, WindowId},
        };

        #[derive(Clone, Copy)]
        struct SharedSignals {
            color: Signal<Color>,
            opacity: Signal<f32>,
            visible: Signal<bool>,
            text: Signal<u32>,
        }

        struct Fixture {
            window: WindowId,
            grow: NodeId,
            movable: NodeId,
            scroll: crate::interaction::ScrollHandle,
            appended: Vec<NodeId>,
        }

        /// 绝对定位容器（与验收场景的 `viewport_style` 同型）：**带子节点的容器不能**用
        /// `layout_override`（布局叶子语义，builder 会拒绝挂子节点）。
        fn absolute_style(left: f32, top: f32, width: f32, height: f32) -> Style {
            use taffy::{
                geometry::Rect as TaffyRect,
                style::{Dimension, LengthPercentageAuto, Position},
            };
            let mut style = Style::default();
            style.inner.position = Position::Absolute;
            style.inner.inset = TaffyRect {
                left: LengthPercentageAuto::length(left),
                top: LengthPercentageAuto::length(top),
                right: LengthPercentageAuto::auto(),
                bottom: LengthPercentageAuto::auto(),
            };
            style.inner.size = taffy::geometry::Size {
                width: Dimension::length(width),
                height: Dimension::length(height),
            };
            style
        }

        fn tile(rect: Rect, color: Color) -> NodeBuilder {
            NodeBuilder::boxed().layout_override(rect).paint(PaintSpec {
                background: color,
                ..PaintSpec::default()
            })
        }

        fn build_fixture(app: &mut App, signals: &SharedSignals) -> Fixture {
            let window = headless_window(app);
            let (grow, movable, scroll_container) = {
                let tree = app.window_mut(window).expect("窗口").tree_mut();
                let root = tree.root();
                // 静态子树（不绑定任何信号）：只应被全量路径访问。
                let panel = NodeBuilder::boxed()
                    .style(absolute_style(0.0, 0.0, 400.0, 240.0))
                    .build(tree, root)
                    .expect("panel");
                for row in 0..6u32 {
                    for column in 0..6u32 {
                        let _ = tile(
                            Rect::new(column as f32 * 40.0, row as f32 * 30.0, 36.0, 26.0),
                            Color::WHITE,
                        )
                        .build(tree, panel)
                        .expect("static tile");
                    }
                }
                // 绑定背景色的节点（共享信号 ⇒ 一次写入同时失效两个窗口）。
                let source = signals.color;
                for index in 0..3u32 {
                    let _ = NodeBuilder::boxed()
                        .layout_override(Rect::new(index as f32 * 60.0 + 420.0, 0.0, 50.0, 40.0))
                        .paint(PaintSpec {
                            background: Color::WHITE,
                            ..PaintSpec::default()
                        })
                        .bind_style(move |cx| StylePatch {
                            background: Some(*cx.read(&source)),
                            ..StylePatch::default()
                        })
                        .build(tree, root)
                        .expect("colored");
                }
                let opacity = signals.opacity;
                let _ = NodeBuilder::boxed()
                    .layout_override(Rect::new(700.0, 0.0, 80.0, 40.0))
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        ..PaintSpec::default()
                    })
                    .bind_style(move |cx| StylePatch {
                        opacity: Some(*cx.read(&opacity)),
                        ..StylePatch::default()
                    })
                    .build(tree, root)
                    .expect("faded");
                // 绑定文本的节点。
                let source = signals.text;
                for index in 0..3u32 {
                    let _ = NodeBuilder::text(
                        smol_str::SmolStr::new_static("0"),
                        crate::text::TextStyle::new(12.0),
                    )
                    .layout_override(Rect::new(index as f32 * 80.0, 60.0, 72.0, 18.0))
                    .bind_text(move |cx| smol_str::SmolStr::new(format!("v{}", *cx.read(&source))))
                    .build(tree, root)
                    .expect("label");
                }
                let visible = signals.visible;
                let _hider = NodeBuilder::boxed()
                    .layout_override(Rect::new(0.0, 300.0, 120.0, 60.0))
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        ..PaintSpec::default()
                    })
                    .bind_style(move |cx| StylePatch {
                        visible: Some(*cx.read(&visible)),
                        ..StylePatch::default()
                    })
                    .build(tree, root)
                    .expect("hider");
                let grow = NodeBuilder::boxed()
                    .style(absolute_style(140.0, 300.0, 400.0, 200.0))
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        ..PaintSpec::default()
                    })
                    .build(tree, root)
                    .expect("grow");
                let movable = tile(Rect::new(600.0, 300.0, 80.0, 40.0), Color::WHITE)
                    .build(tree, root)
                    .expect("movable");
                let mut scroll_style = absolute_style(0.0, 400.0, 300.0, 120.0);
                scroll_style.clip = Clip::Bounds;
                let scroll_container = NodeBuilder::boxed()
                    .style(scroll_style)
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        ..PaintSpec::default()
                    })
                    .build(tree, root)
                    .expect("scroll container");
                for index in 0..12u32 {
                    let _ = tile(
                        Rect::new(0.0, index as f32 * 20.0, 280.0, 18.0),
                        Color::WHITE,
                    )
                    .build(tree, scroll_container)
                    .expect("scroll child");
                }
                (grow, movable, scroll_container)
            };
            let scroll = app
                .make_scroll(window, scroll_container, ScrollConfig::default())
                .expect("滚动容器");
            app.set_scroll_content(&scroll, Size::new(280.0, 12.0 * 20.0))
                .expect("滚动内容");
            Fixture {
                window,
                grow,
                movable,
                scroll,
                appended: Vec::new(),
            }
        }

        /// 场景的绘制产物（跨窗口归一化：节点 → DFS 序号，lane 内容按 DFS 序）。
        #[derive(Debug, PartialEq)]
        struct View {
            lanes: Vec<(usize, Vec<Primitive>, u32)>,
            drawn: Vec<usize>,
            scroll: Vec<(Point, PhysicalRect)>,
            damage: Vec<PhysicalRect>,
            damage_full: bool,
            delta: DeltaRecord,
        }

        fn dfs_indices(tree: &crate::node::tree::NodeTree) -> ahash::HashMap<NodeId, usize> {
            let mut indices: ahash::HashMap<NodeId, usize> = ahash::HashMap::default();
            let mut stack: Vec<NodeId> = vec![tree.root()];
            let mut next = 0usize;
            while let Some(node) = stack.pop() {
                indices.insert(node, next);
                next += 1;
                if let Ok(target) = tree.get(node) {
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            indices
        }

        fn view(app: &App, window: WindowId) -> View {
            let window = app.window(window).expect("窗口存在");
            let tree = window.tree();
            let scene = window.scene();
            let indices = dfs_indices(tree);
            let mut lane_nodes: ahash::HashMap<crate::paint::scene::LaneId, usize> =
                ahash::HashMap::default();
            let mut lanes = Vec::new();
            let mut stack = vec![tree.root()];
            while let Some(node) = stack.pop() {
                if let Ok(target) = tree.get(node) {
                    if let Some(lane) = target.lane() {
                        let index = indices[&node];
                        lane_nodes.insert(lane, index);
                        let primitives = scene
                            .lane_primitives(lane)
                            .map_or_else(Vec::new, <[Primitive]>::to_vec);
                        lanes.push((index, primitives, scene.lane_scroll(lane)));
                    }
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            lanes.sort_by_key(|entry| entry.0);
            let drawn: Vec<usize> = scene
                .order()
                .iter()
                .filter(|lane| {
                    scene
                        .lane_primitives(**lane)
                        .is_some_and(|primitives| !primitives.is_empty())
                })
                .filter_map(|lane| lane_nodes.get(lane).copied())
                .collect();
            let record = window.record().expect("提交帧必须有记录");
            View {
                lanes,
                drawn,
                scroll: window.scroll_slots().to_vec(),
                damage: record.damage.clone(),
                damage_full: record.damage_full,
                delta: record.delta,
            }
        }

        fn force_full(app: &mut App, window: WindowId) {
            app.window_mut(window)
                .expect("窗口存在")
                .tree_mut()
                .force_full_paint();
        }

        fn compare(app: &App, windows: [WindowId; 2], step: u32, label: &str) {
            let view_a = view(app, windows[0]);
            let view_b = view(app, windows[1]);
            assert_eq!(
                view_a, view_b,
                "step {step} ({label}): 增量帧与全量帧产物不一致"
            );
        }

        /// A1 / P1A-1b：脚本化操作序列下，增量路径与强制全量路径逐帧产物一致。
        #[test]
        fn incremental_matches_forced_full_over_a_scripted_sequence() {
            let runtime = runtime();
            let mut app = App::new(runtime.handle().clone());
            let signals = SharedSignals {
                color: app.signal(Color::WHITE),
                opacity: app.signal(1.0f32),
                visible: app.signal(true),
                text: app.signal(0u32),
            };
            let a = build_fixture(&mut app, &signals);
            let b = build_fixture(&mut app, &signals);
            let windows = [a.window, b.window];
            let mut fixtures = [a, b];

            // 首帧：两边都是全量。
            for window in windows {
                let outcome = frame_once(&mut app, window);
                assert!(matches!(outcome, FrameOutcome::Committed(_)));
            }
            compare(&app, windows, 0, "首帧");

            let mut seed = 0x5EED_1234_ABCD_9876u64;
            for step in 1..=30u32 {
                seed = seed
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let op = ((seed >> 33) as u32) % 9;
                match op {
                    0 => {
                        let channel = (step % 3) as u8;
                        app.write(
                            &signals.color,
                            Color::from_rgba8([0x20 + step as u8, 0x40 + channel * 40, 0x80, 0xFF]),
                        );
                    }
                    1 => {
                        app.write(&signals.opacity, 0.05 + 0.9 * ((step as f32 * 0.37) % 1.0));
                    }
                    2 => {
                        app.write(&signals.text, step);
                    }
                    3 => {
                        let offset = Point::new(0.0, ((step as f32 * 13.0) % 120.0).max(1.0));
                        for fixture in &fixtures {
                            app.scroll_to(&fixture.scroll, offset).expect("滚动");
                        }
                    }
                    4 => {
                        let index = (step as usize) % 36;
                        for fixture in &fixtures {
                            let root = app.window(fixture.window).expect("窗口").tree().root();
                            let panel = app
                                .window(fixture.window)
                                .expect("窗口")
                                .tree()
                                .get(root)
                                .expect("根")
                                .children()[0];
                            let target = app
                                .window(fixture.window)
                                .expect("窗口")
                                .tree()
                                .get(panel)
                                .expect("panel")
                                .children()[index];
                            let _ = app.request_node_repaint(fixture.window, target);
                        }
                    }
                    5 => {
                        let rect = Rect::new(
                            600.0 + (step as f32) * 3.0,
                            300.0 + (step % 2) as f32 * 5.0,
                            80.0,
                            40.0,
                        );
                        for fixture in &fixtures {
                            app.window_mut(fixture.window)
                                .expect("窗口")
                                .tree_mut()
                                .set_layout_override(
                                    fixture.movable,
                                    Some(LayoutOverride::new(rect)),
                                )
                                .expect("移动节点");
                        }
                    }
                    6 => {
                        for fixture in &mut fixtures {
                            let tree = app.window_mut(fixture.window).expect("窗口").tree_mut();
                            let node = tile(
                                Rect::new(
                                    160.0 + (fixture.appended.len() as f32) * 30.0,
                                    420.0,
                                    24.0,
                                    24.0,
                                ),
                                Color::WHITE,
                            )
                            .build(tree, fixture.grow)
                            .expect("追加节点");
                            fixture.appended.push(node);
                        }
                    }
                    7 => {
                        let mut removed = false;
                        for fixture in &mut fixtures {
                            if let Some(node) = fixture.appended.pop() {
                                let _ = app
                                    .window_mut(fixture.window)
                                    .expect("窗口")
                                    .tree_mut()
                                    .remove(node);
                                removed = true;
                            }
                        }
                        if !removed {
                            // 没有可删的节点时退化成一次文本更新，保证本帧真的提交。
                            app.write(&signals.text, step);
                        }
                    }
                    _ => {}
                }
                // 可见性闪烁：隐藏一帧（增量 hide vs 全量 hide），下一轮显示回来。
                let blink = op == 8;
                if blink {
                    app.write(&signals.visible, false);
                }
                // A：正常帧分类（增量剪枝）。
                let outcome = frame_once(&mut app, windows[0]);
                assert!(matches!(outcome, FrameOutcome::Committed(_)), "A 必须提交");
                // B：强制全量分类后再出帧（同一脏集合）。
                force_full(&mut app, windows[1]);
                let outcome = frame_once(&mut app, windows[1]);
                assert!(matches!(outcome, FrameOutcome::Committed(_)), "B 必须提交");
                compare(&app, windows, step * 2, "op");
                if blink {
                    app.write(&signals.visible, true);
                    let outcome = frame_once(&mut app, windows[0]);
                    assert!(matches!(outcome, FrameOutcome::Committed(_)), "A 重新显示");
                    force_full(&mut app, windows[1]);
                    let outcome = frame_once(&mut app, windows[1]);
                    assert!(matches!(outcome, FrameOutcome::Committed(_)), "B 重新显示");
                    compare(&app, windows, step * 2 + 1, "show");
                }
            }
        }
    }
}
