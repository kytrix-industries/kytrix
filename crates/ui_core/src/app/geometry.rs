//! 应用层的几何入口。
//!
//! - **变换**（`App::set_geometry_transform`，R05-12 / AR2 P1-D4）：几何节点的 per-draw
//!   变换（`GeometryDraw.transform` → 顶点着色器的 `draw.basis` / `draw.translation`），
//!   **不重建顶点缓冲**；
//! - **数据更新**（`App::update_geometry_vertices/indices`，X5）：按 range 就地更新几何快照，
//!   只重建块版本变化的块 lane / 只上传变化的 4 KiB 页；
//! - **成对截断**（`App::truncate_geometry`，X5）：滑动窗口（删尾 + 追加）把顶点/索引同时
//!   缩短，块数只减、被移除块的旧 AABB 进 damage。
//!
//! 数据更新 / 截断会把**所有窗口**里绑定该几何的节点标 PAINT（`GeometryStore` 是进程级，一个
//! `GeometryId` 可被多个窗口的节点引用），并自动请求一帧；空 range / 没有任何块受影响 /
//! 截断到当前长度时不标脏、不请求帧。绑定节点包括几何节点（`NodeBuilder::geometry`）与内容里
//! 引用该几何的 `Canvas` 节点（`Canvas::push_geometry` / `push_path`）。
//!
//! # damage 必须两侧都算
//!
//! 变换改变后，屏幕上**旧位置**的像素也要重画，否则局部刷新会留残影。节点的 damage 累加器
//! 由 `touch` 按**节点布局盒**并入，而几何的可见范围是 `transform(GeometryDesc.bounds)` ——
//! 平移/缩放后它可能落在盒外。因此这里把「旧变换 ∪ 新变换」下的可见范围一起算出来，交给
//! `NodeTree::set_geometry_transform` 并进**同一条** damage 记账（不是第二条 damage 路径）。
//!
//! # 语义边界
//!
//! - **只影响绘制，不影响命中**：命中测试（`layout/hit_test.rs`）用的是节点布局盒，per-draw
//!   变换不参与。让命中跟随变换需要把同一变换的逆应用到命中点，并重估滚动容器的裁剪链 ——
//!   代价与风险都远超本项，故显式不做（应用若要变点击区，改节点布局盒）。
//! - 变换只对 `NodeKind::Geometry` 节点有效；其它节点返回 [`EngineError::StaleNode`]。

use super::App;
use crate::{
    error::EngineError,
    math::{Rect, Transform},
    node::{id::NodeId, tree::NodeKind},
    paint::geometry::{GeometryId, GeometryUpdate},
    window::WindowId,
};

/// `GeometryStore::update_*` 的引擎侧包装（X5）：数据更新 + 绑定节点自动标脏。
///
/// # 为什么不是纯数据更新
///
/// 几何数据变了以后，绘制输出（`Primitive::Geometry` / 块 lane 的块 AABB）必须跟着重算，
/// 否则 GPU 读到的还是旧快照。`GeometryStore` 是进程级、不知道哪些节点引用它，因此这里用
/// 节点树的反向绑定表把「数据更新」翻译成「绑定节点标 PAINT」：paint 走几何分块路径，
/// 只重建**块版本变化**的块 lane（追加 K 线只重建尾块）。引用该几何的 Canvas 节点也在绑定表
/// 里（`NodeTree` 在挂载 / `set_canvas` / 卸载时维护），它们没有块级判据 ⇒ 整条 lane 重建。
///
/// 几何节点级 `REUSABLE` 不清、节点盒 damage 不并（见 `NodeTree::mark_geometry_updated`）：
/// 块 lane 的 damage 由块 bounds 的旧∪新精确给出。
///
/// # Errors
/// 同 [`crate::paint::geometry_store::GeometryStore::update_vertices`] /
/// [`crate::paint::geometry_store::GeometryStore::update_indices`]（陈旧句柄 / 越界 / 非对齐 /
/// 字节数不一致的强类型错误）。
pub(crate) fn update_vertices(
    app: &mut App,
    id: GeometryId,
    range: std::ops::Range<u32>,
    bytes: &[u8],
) -> Result<GeometryUpdate, EngineError> {
    let update = app.geometry.update_vertices(id, range, bytes)?;
    if mark_bound_nodes(app, id, update.damaged) {
        app.request_frame();
    }
    Ok(update)
}

/// [`update_vertices`] 的索引版本（语义对称）。
pub(crate) fn update_indices(
    app: &mut App,
    id: GeometryId,
    range: std::ops::Range<u32>,
    bytes: &[u8],
) -> Result<GeometryUpdate, EngineError> {
    let update = app.geometry.update_indices(id, range, bytes)?;
    if mark_bound_nodes(app, id, update.damaged) {
        app.request_frame();
    }
    Ok(update)
}

/// [`update_vertices`] 的截断版本（X5 滑动窗口）：数据层成对缩短 + 绑定节点自动标脏。
///
/// 与 range 更新共用同一条「damaged 非空 ⇒ 标脏 ⇒ request_frame」路径；截断到当前个数时
/// `damaged` 为空 ⇒ 不标脏、不请求帧。
///
/// # Errors
/// 同 [`crate::paint::geometry_store::GeometryStore::truncate`]（陈旧句柄 / 目标个数大于当前）。
pub(crate) fn truncate(
    app: &mut App,
    id: GeometryId,
    vertices: u32,
    indices: u32,
) -> Result<GeometryUpdate, EngineError> {
    let update = app.geometry.truncate(id, vertices, indices)?;
    if mark_bound_nodes(app, id, update.damaged) {
        app.request_frame();
    }
    Ok(update)
}

/// 把所有窗口里绑定到 `geometry` 的节点（几何节点 / 引用该几何的 Canvas）标 PAINT，
/// 返回「是否有节点被标脏」。
///
/// `damaged` 为空 = 没有可见变化（空 range / 只改了未被块引用的顶点 / 截断到当前长度）⇒
/// 不标脏、不请求帧。返回 `true` 时调用方负责 `request_frame`（与 `App::request_repaint` /
/// signal 写入同语义）。
fn mark_bound_nodes(app: &mut App, geometry: GeometryId, damaged: Rect) -> bool {
    if damaged.is_empty() {
        return false;
    }
    let mut marked = false;
    for window in app.windows.values_mut() {
        marked |= window.tree_mut().mark_geometry_updated(geometry) > 0;
    }
    marked
}

/// 几何 damage 的额外外扩（逻辑像素）。
///
/// 与编码器 `paint/instance.rs::bounds_of_aabb` 的 ±1 px 同口径：`GeometryPipeline::Ui` 的解析
/// AA 让覆盖率带向外扩约 1 个**物理**像素，`FrameOp::Geometry.bounds` 因此再外扩 1 px。这里在
/// **逻辑**坐标里取 1：`scale >= 1` 时等价保守，`scale < 1` 时更宽（damage 允许偏大）。
const GEOMETRY_DAMAGE_PAD: f32 = 1.0;

/// 设置几何节点的 per-draw 变换（顶点不重建）。消费者：`App::set_geometry_transform`。
///
/// `transform` 是**逻辑像素**的仿射变换：几何局部空间 → 内容空间（与
/// `GeometryDraw.transform` 同一语义）。
///
/// # Errors
/// - [`EngineError::UnknownWindow`]：窗口不存在；
/// - [`EngineError::StaleNode`]：节点不存在或不是几何节点。
pub(crate) fn set_geometry_transform(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    transform: Transform,
) -> Result<(), EngineError> {
    let App {
        windows, geometry, ..
    } = app;
    let target = windows
        .get_mut(&window)
        .ok_or(EngineError::UnknownWindow { window })?;
    let tree = target.tree_mut();
    let id = match tree.get(node).map(|node| node.kind()) {
        Ok(NodeKind::Geometry(id)) => *id,
        _ => return Err(EngineError::StaleNode { node }),
    };
    let previous = tree.geometry_transform(node);
    // 未注册（陈旧句柄）⇒ 拿不到真实范围，只能依赖 `touch` 的节点盒（与 paint 阶段的退化一致）。
    let extra_damage = match geometry.snapshot_arc(id) {
        Some(snapshot) => transformed_damage(previous, snapshot.bounds())
            .union(transformed_damage(transform, snapshot.bounds())),
        None => Rect::default(),
    };
    tree.set_geometry_transform(node, transform, extra_damage)
}

/// `transform(bounds)` 的 AABB + [`GEOMETRY_DAMAGE_PAD`]（内容坐标）。
///
/// 与 `paint/pass.rs::geometry_damage` 同一算法（四角取极值）；非有限输入退回 `bounds`
/// （不制造 NaN damage：`Rect::union` 对空矩形是恒等）。
fn transformed_damage(transform: Transform, bounds: Rect) -> Rect {
    if bounds.is_empty() {
        return bounds;
    }
    let far = crate::math::Point::new(
        bounds.origin.x + bounds.size.width,
        bounds.origin.y + bounds.size.height,
    );
    let corners = [
        transform.apply(bounds.origin),
        transform.apply(crate::math::Point::new(far.x, bounds.origin.y)),
        transform.apply(crate::math::Point::new(bounds.origin.x, far.y)),
        transform.apply(far),
    ];
    if !corners
        .iter()
        .all(|point| point.x.is_finite() && point.y.is_finite())
    {
        return bounds;
    }
    let min_x = corners
        .iter()
        .map(|point| point.x)
        .fold(f32::INFINITY, f32::min);
    let min_y = corners
        .iter()
        .map(|point| point.y)
        .fold(f32::INFINITY, f32::min);
    let max_x = corners
        .iter()
        .map(|point| point.x)
        .fold(f32::NEG_INFINITY, f32::max);
    let max_y = corners
        .iter()
        .map(|point| point.y)
        .fold(f32::NEG_INFINITY, f32::max);
    Rect::new(min_x, min_y, max_x - min_x, max_y - min_y).outset(GEOMETRY_DAMAGE_PAD)
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use crate::{
        error::EngineError,
        math::{Rect, Transform},
        node::{builder::NodeBuilder, dirty::DirtyFlags},
        paint::{
            canvas::Canvas,
            color::Color,
            geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
            instance::FrameOp,
            primitive::{GeometryDraw, Primitive},
        },
        test_support::app::TestApp,
    };

    /// 注册一块 20×10 的几何（真实三角形顶点，块 AABB = (0,0,20,10)；damage / op 范围可精确断言）。
    fn register(app: &mut TestApp) -> GeometryId {
        let points = [[0.0f32, 0.0, -1.0], [20.0, 0.0, -1.0], [0.0, 10.0, -1.0]];
        let vertex_bytes: Vec<u8> = points
            .iter()
            .flat_map(|point| point.iter().flat_map(|value| value.to_ne_bytes()))
            .collect();
        let desc = GeometryDesc {
            vertex_bytes: bytes::Bytes::from(vertex_bytes),
            index_bytes: bytes::Bytes::from_static(&[0, 0, 1, 0, 2, 0]),
            vertex_stride: 12,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: Rect::new(0.0, 0.0, 20.0, 10.0),
            pipeline: GeometryPipeline::Ui,
        };
        app.app_mut().geometry_mut().insert(desc).expect("注册几何")
    }

    /// 挂一个几何节点（布局盒 40×40）并跑一帧，返回节点 id。
    fn mount(app: &mut TestApp, id: GeometryId) -> crate::node::id::NodeId {
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口存在").tree().root();
        let node = NodeBuilder::geometry(id)
            .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("挂载成功");
        app.drain().expect("首帧");
        node
    }

    /// 帧里 `Primitive::Geometry` 的 per-draw 变换（paint 阶段的消费结果）。
    fn painted_transform(app: &TestApp, window: crate::window::WindowId) -> Option<Transform> {
        let scene = app.app().window(window)?.scene();
        for lane in scene.order() {
            for primitive in scene.lane_primitives(*lane).unwrap_or(&[]) {
                if let Primitive::Geometry(draw) = primitive {
                    return Some(draw.transform);
                }
            }
        }
        None
    }

    /// 帧里那条几何 op 的可见范围（物理像素）。
    fn geometry_op_bounds(
        app: &TestApp,
        window: crate::window::WindowId,
    ) -> Option<crate::paint::instance::PhysicalBounds> {
        let payload = app.app().window(window)?.instance_buffer()?;
        payload.ops().iter().find_map(|op| match op {
            FrameOp::Geometry { bounds, .. } => Some(*bounds),
            FrameOp::Instances { .. } => None,
        })
    }

    /// AR2 P1-D4 ①：**生产入口可达** —— 调用后下一帧的 `Primitive::Geometry` 带新变换，
    /// 编码出的 op 可见范围也按变换后的 `desc.bounds` 算（旧实现只有 `#[cfg(test)]` 写入口）。
    #[test]
    fn the_public_entry_sets_the_transform_that_the_paint_pass_consumes() {
        let mut app = TestApp::new();
        let id = register(&mut app);
        let node = mount(&mut app, id);
        let window = app.window_id();
        let scale = app.app().window(window).expect("窗口存在").scale().factor();
        assert_eq!(painted_transform(&app, window), Some(Transform::IDENTITY));

        let transform = Transform::translate(12.0, 7.0);
        app.app_mut()
            .set_geometry_transform(window, node, transform)
            .expect("几何节点必须接受变换");
        assert_eq!(
            app.app()
                .window(window)
                .expect("窗口存在")
                .tree()
                .geometry_transform(node),
            transform
        );

        app.drain().expect("变换后的帧");
        assert_eq!(
            painted_transform(&app, window),
            Some(transform),
            "paint 必须把节点级变换写进 `GeometryDraw.transform`"
        );
        let bounds = geometry_op_bounds(&app, window).expect("必须有一条几何 op");
        // 变换后的 AABB (12,7,32,17) 经编码器 `bounds_of_aabb`（floor−1 / ceil+1）。
        assert_eq!(bounds.x0, (12.0 * scale).floor() as i32 - 1);
        assert_eq!(bounds.y0, (7.0 * scale).floor() as i32 - 1);
        assert_eq!(bounds.x1, (32.0 * scale).ceil() as i32 + 1);
        assert_eq!(bounds.y1, (17.0 * scale).ceil() as i32 + 1);
    }

    /// AR2 P1-D4 ②③：damage 两侧都要算（旧 ∪ 新），且回退到单位矩阵后旧位置也会重画。
    #[test]
    fn damage_union_covers_both_the_old_and_the_new_transform() {
        let mut app = TestApp::new();
        let id = register(&mut app);
        let node = mount(&mut app, id);
        let window = app.window_id();

        app.app_mut()
            .set_geometry_transform(window, node, Transform::translate(100.0, 50.0))
            .expect("设置变换");
        let damage = app
            .app()
            .window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("节点存在")
            .damage();
        assert!(
            damage.origin.x <= 0.0 && damage.origin.y <= 0.0,
            "旧变换下的可见范围必须并入 damage：{damage:?}"
        );
        assert!(
            damage.max().x >= 120.0 && damage.max().y >= 60.0,
            "新变换下的可见范围必须并入 damage：{damage:?}"
        );

        app.drain().expect("一帧");
        app.app_mut()
            .set_geometry_transform(window, node, Transform::IDENTITY)
            .expect("回退");
        let tree = app.app().window(window).expect("窗口存在").tree();
        assert_eq!(tree.geometry_transform(node), Transform::IDENTITY);
        let restore = tree.get(node).expect("节点存在").damage();
        assert!(
            restore.max().x >= 120.0 && restore.max().y >= 60.0,
            "回退时位移后的旧像素也必须重画：{restore:?}"
        );
        app.drain().expect("回退帧");
        assert_eq!(painted_transform(&app, window), Some(Transform::IDENTITY));
    }

    /// 只有该节点的 damage 被扩大：其它节点的 damage 必须保持为空（局部刷新不被拖成全帧）。
    #[test]
    fn the_transform_damage_stays_local_to_the_geometry_node() {
        let mut app = TestApp::new();
        let id = register(&mut app);
        let node = mount(&mut app, id);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口存在").tree().root();
        let sibling = NodeBuilder::boxed()
            .layout_override(Rect::new(200.0, 200.0, 10.0, 10.0))
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("挂载成功");
        app.drain().expect("一帧");

        app.app_mut()
            .set_geometry_transform(window, node, Transform::translate(5.0, 5.0))
            .expect("设置变换");
        let window_state = app.app_mut().window_mut(window).expect("窗口存在");
        let tree = window_state.tree_mut();
        assert!(
            tree.get(sibling).expect("节点存在").damage().is_empty(),
            "无关节点不得被标 damage"
        );
        assert!(
            !tree.take_geometry_damage_escalation(),
            "精确 damage 不该升级成全帧"
        );
    }

    /// 非几何节点 / 不存在的窗口：强类型错误，不是静默无效。
    #[test]
    fn invalid_targets_are_typed_errors() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口存在").tree().root();
        let boxed = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 10.0, 10.0))
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("挂载成功");

        match app
            .app_mut()
            .set_geometry_transform(window, boxed, Transform::translate(1.0, 1.0))
        {
            Err(EngineError::StaleNode { node }) => assert_eq!(node, boxed),
            other => panic!("普通盒子必须报 StaleNode，实际 {other:?}"),
        }
        let missing = crate::window::WindowId::for_test(9999);
        match app
            .app_mut()
            .set_geometry_transform(missing, boxed, Transform::IDENTITY)
        {
            Err(EngineError::UnknownWindow { window }) => assert_eq!(window, missing),
            other => panic!("不存在的窗口必须报 UnknownWindow，实际 {other:?}"),
        }
    }
    // ── X5：Geometry range 更新（数据层 + 节点标脏 + damage / 顺序快路径）─────────

    use crate::math::PhysicalRect;

    /// `triangles` 个互不共享顶点的三角形（真实位置；每 4096 个三角形一块）。
    fn long_desc(triangles: u32) -> GeometryDesc {
        let mut vertex_bytes = Vec::with_capacity(triangles as usize * 36);
        let mut index_bytes = Vec::with_capacity(triangles as usize * 6);
        for triangle in 0..triangles {
            let base = (triangle * 3) as u16;
            let x = (triangle % 64) as f32;
            let y = (triangle / 64) as f32;
            for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)] {
                vertex_bytes.extend_from_slice(&(x + dx).to_ne_bytes());
                vertex_bytes.extend_from_slice(&(y + dy).to_ne_bytes());
                vertex_bytes.extend_from_slice(&(-1.0f32).to_ne_bytes());
            }
            for index in [base, base + 1, base + 2] {
                index_bytes.extend_from_slice(&index.to_ne_bytes());
            }
        }
        GeometryDesc {
            vertex_bytes: bytes::Bytes::from(vertex_bytes),
            index_bytes: bytes::Bytes::from(index_bytes),
            vertex_stride: 12,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: Rect::new(0.0, 0.0, 64.0, 100.0),
            pipeline: GeometryPipeline::Ui,
        }
    }

    fn register_long(app: &mut TestApp, triangles: u32) -> GeometryId {
        app.app_mut()
            .geometry_mut()
            .insert(long_desc(triangles))
            .expect("注册几何")
    }

    /// 每个块的 lane 内容版本（按块序）。
    fn chunk_lane_versions(app: &TestApp, node: crate::node::id::NodeId) -> Vec<u32> {
        let window = app.window_id();
        let window_state = app.app().window(window).expect("窗口存在");
        let tree = window_state.tree();
        (0..tree.lane_count(node))
            .map(|chunk| {
                let lane = tree.lane_at(node, chunk).expect("块 lane");
                window_state.scene().lane_state(lane).expect("lane").0
            })
            .collect()
    }

    fn vertex(x: f32, y: f32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(12);
        bytes.extend_from_slice(&x.to_ne_bytes());
        bytes.extend_from_slice(&y.to_ne_bytes());
        bytes.extend_from_slice(&(-1.0f32).to_ne_bytes());
        bytes
    }

    /// 在几何末尾追加一个三角形（顶点 range + 索引 range，与 K 线追加同路径）。
    fn append_triangle(app: &mut TestApp, id: GeometryId, x: f32, y: f32) {
        let snapshot = app.app().geometry().snapshot_arc(id).expect("已注册");
        let vertex_at = snapshot.vertex_len();
        let mut vertices = Vec::with_capacity(36);
        for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0)] {
            vertices.extend_from_slice(&vertex(x + dx, y + dy));
        }
        app.app_mut()
            .update_geometry_vertices(id, vertex_at..vertex_at + 36, &vertices)
            .expect("追加顶点");
        let index_at = snapshot.index_len();
        let base = (snapshot.vertex_len() / 12) as u16;
        let mut bytes = Vec::with_capacity(6);
        for index in [base, base + 1, base + 2] {
            bytes.extend_from_slice(&index.to_ne_bytes());
        }
        app.app_mut()
            .update_geometry_indices(id, index_at..index_at + 6, &bytes)
            .expect("追加索引");
    }

    fn damage_hits(damage: &[PhysicalRect], x: f32, y: f32) -> bool {
        let point = PhysicalRect::new(
            x.floor() as i32,
            y.floor() as i32,
            x.floor() as i32 + 2,
            y.floor() as i32 + 2,
        );
        damage.iter().any(|rect| !rect.intersect(point).is_empty())
    }

    /// 顶点 range 更新（热力图改色 / 改一格位置）：只重建受影响块的 lane、只产生该块旧∪新的
    /// damage，并且**不重排绘制序**（`delta.order == None` 快路径）。
    #[test]
    fn range_update_rebuilds_only_the_affected_chunk_lane() {
        let mut app = TestApp::new();
        let id = register_long(&mut app, 4096 + 10);
        let node = mount(&mut app, id);
        let before = chunk_lane_versions(&app, node);
        assert_eq!(before.len(), 2, "两块几何 = 两条块 lane");

        // 移动块 0 里的一个顶点（热力图改一格的位置/颜色都走同一个 range 更新入口）。
        let update = app
            .app_mut()
            .update_geometry_vertices(id, 0..12, &vertex(100.0, 100.0))
            .expect("顶点 range 更新");
        assert!(!update.damaged.is_empty(), "受影响块必须给出 damage 范围");
        let (stats, order_some, damage, damage_full) = {
            let record = app.drain().expect("更新帧");
            (
                record.stats,
                record.delta.order_some,
                record.damage.clone(),
                record.damage_full,
            )
        };

        let after = chunk_lane_versions(&app, node);
        assert!(
            after[0] > before[0],
            "块 0 的 lane 必须重建：before={before:?} after={after:?}"
        );
        assert_eq!(after[1], before[1], "块 1 的 lane 不得被触碰");
        assert_eq!(stats.encoded_lanes, 1, "只重编码一条块 lane");
        assert_eq!(stats.geometry_encoded_chunks, 1, "只重编码一块几何");
        assert!(!order_some, "块版本更新不得重排绘制序");
        assert!(!damage_full, "damage 不得退化为整帧");
        assert!(
            damage_hits(&damage, 100.0, 100.0),
            "新位置必须重画：{damage:?}"
        );
        assert!(
            update.damaged.origin.x <= 0.0
                && update.damaged.origin.y <= 0.0
                && update.damaged.max().x >= 100.0,
            "API 返回的 damage 必须是受影响块旧∪新：{:?}",
            update.damaged
        );
    }

    /// 追加一个三角形到未满的尾块：只重建尾块 lane、damage 只覆盖尾块新范围。
    #[test]
    fn appending_to_the_tail_chunk_rebuilds_only_the_tail_chunk_lane() {
        let mut app = TestApp::new();
        let id = register_long(&mut app, 4096 + 2);
        let node = mount(&mut app, id);
        let before = chunk_lane_versions(&app, node);

        append_triangle(&mut app, id, 100.0, 100.0);
        let (stats, order_some, damage) = {
            let record = app.drain().expect("追加帧");
            (record.stats, record.delta.order_some, record.damage.clone())
        };

        let after = chunk_lane_versions(&app, node);
        assert_eq!(after[0], before[0], "前面块 lane 不得重建（追加只动尾块）");
        assert!(
            after[1] > before[1],
            "尾块 lane 必须重建：before={before:?} after={after:?}"
        );
        assert_eq!(stats.encoded_lanes, 1);
        assert_eq!(stats.geometry_encoded_chunks, 1);
        assert_eq!(stats.uploaded_bytes, 0, "几何追加不上传实例字节");
        assert!(!order_some, "尾块增长不改变绘制序成员");
        assert!(
            damage_hits(&damage, 100.0, 100.0),
            "新三角形必须进 damage：{damage:?}"
        );
    }

    /// 块满后追加：新建块 lane，绘制序成员变化 ⇒ 本帧 `delta.order` 必须携带新序。
    #[test]
    fn appending_across_the_chunk_boundary_updates_the_draw_order_once() {
        let mut app = TestApp::new();
        let id = register_long(&mut app, 4096);
        let node = mount(&mut app, id);
        assert_eq!(chunk_lane_versions(&app, node).len(), 1);
        let before = chunk_lane_versions(&app, node);

        append_triangle(&mut app, id, 100.0, 100.0);
        let (stats, order_some) = {
            let record = app.drain().expect("跨块追加帧");
            (record.stats, record.delta.order_some)
        };
        let after = chunk_lane_versions(&app, node);
        assert_eq!(after.len(), 2, "满块后追加必须新建块 lane");
        assert_eq!(after[0], before[0], "旧块 lane 不动");
        assert_eq!(stats.encoded_lanes, 1);
        assert_eq!(stats.geometry_encoded_chunks, 1);
        assert!(order_some, "新 lane ⇒ 绘制序成员变化，delta 必须带新序");

        // 下一帧没有新变化 ⇒ 空闲（顺序已经发布，不重复重建）。
        let idle = app.drain().expect("后续帧");
        assert!(!idle.rendered, "顺序已发布后的无变化帧必须空闲");
        assert_eq!(idle.stats.encoded_lanes, 0);
    }

    /// 只追加未被索引引用的顶点：没有块受影响 ⇒ 无 damage、不标脏、帧保持空闲。
    #[test]
    fn appending_unreferenced_vertices_is_invisible_and_keeps_the_frame_idle() {
        let mut app = TestApp::new();
        let id = register_long(&mut app, 4096 + 2);
        let _node = mount(&mut app, id);
        let _ = app.drain().expect("首帧");

        let vertex_at = app
            .app()
            .geometry()
            .snapshot_arc(id)
            .expect("已注册")
            .vertex_len();
        let update = app
            .app_mut()
            .update_geometry_vertices(id, vertex_at..vertex_at + 12, &vertex(1.0, 1.0))
            .expect("追加孤立顶点");
        assert!(update.damaged.is_empty(), "没有块引用新顶点 ⇒ damaged 为空");
        let record = app.drain().expect("空闲帧");
        assert!(!record.rendered, "无可见变化 ⇒ 帧必须退化 Idle");
        assert_eq!(record.stats.encoded_lanes, 0);
        assert_eq!(record.stats.geometry_encoded_chunks, 0);
    }

    /// 陈旧句柄与参数错误是**分开的**强类型错误（应用可按类型决定重注册 / 修参数）。
    #[test]
    fn range_update_errors_are_typed_and_distinct() {
        let mut app = TestApp::new();
        let id = register_long(&mut app, 4);
        let stale = GeometryId::for_test(999, 1);
        match app
            .app_mut()
            .update_geometry_vertices(stale, 0..12, &vertex(0.0, 0.0))
        {
            Err(EngineError::UnknownGeometry { geometry }) => assert_eq!(geometry, stale),
            other => panic!("陈旧句柄必须报 UnknownGeometry，实际 {other:?}"),
        }
        let vertex_len = app
            .app()
            .geometry()
            .snapshot_arc(id)
            .expect("已注册")
            .vertex_len();
        match app.app_mut().update_geometry_vertices(
            id,
            vertex_len + 12..vertex_len + 24,
            &vertex(0.0, 0.0),
        ) {
            Err(EngineError::GeometryRangeOutOfBounds { buffer, .. }) => {
                assert_eq!(buffer, crate::paint::geometry::GeometryBuffer::Vertex);
            }
            other => panic!("越过末尾的空洞必须报 GeometryRangeOutOfBounds，实际 {other:?}"),
        }
        match app.app_mut().update_geometry_vertices(id, 0..6, &[0u8; 6]) {
            Err(EngineError::GeometryRangeUnaligned { align, .. }) => assert_eq!(align, 12),
            other => panic!("未按 stride 对齐必须报 GeometryRangeUnaligned，实际 {other:?}"),
        }
        match app.app_mut().update_geometry_vertices(id, 0..12, &[0u8; 6]) {
            Err(EngineError::GeometryRangeBytes { bytes, .. }) => assert_eq!(bytes, 6),
            other => panic!("字节数不一致必须报 GeometryRangeBytes，实际 {other:?}"),
        }
        // 合法空区间是 no-op：版本与画面都不动。
        let update = app
            .app_mut()
            .update_geometry_indices(id, 0..0, &[])
            .expect("空区间合法");
        assert!(update.damaged.is_empty());
    }

    /// RX-7/RX-8：一个 `GeometryId` 被多个窗口的几何节点引用时，数据更新必须标脏**所有**
    /// 窗口并自动请求一帧；空 range 不标脏、不请求帧。
    #[test]
    fn geometry_update_marks_bound_nodes_in_every_window_and_requests_a_frame() {
        let mut app = TestApp::new();
        let first = app.window_id();
        let second = app
            .app_mut()
            .create_window(crate::window::WindowOptions::default());
        let id = register(&mut app);
        for window in [first, second] {
            let root = app.app().window(window).expect("窗口存在").tree().root();
            let _ = NodeBuilder::geometry(id)
                .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
                .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
                .expect("挂载几何节点");
        }
        // 首帧：两个窗口各自建立块 lane。
        for window in [first, second] {
            let outcome = app.app_mut().frame(window).expect("窗口存在").expect("帧");
            assert!(
                matches!(outcome, crate::window::FrameOutcome::Committed(_)),
                "首帧必须提交，实际 {outcome:?}"
            );
        }

        let requests = app.app().frame_requests();
        let update = app
            .app_mut()
            .update_geometry_vertices(id, 0..12, &vertex(5.0, 5.0))
            .expect("顶点更新");
        assert!(
            !update.damaged.is_empty(),
            "被块引用的顶点更新必须有 damage"
        );
        assert!(
            app.app().frame_requests() > requests,
            "几何数据更新必须自动请求一帧（与 signal 写入同语义）"
        );
        // 两个窗口都绑定同一几何 ⇒ 都必须被标脏：各自的下一帧必须真的提交（未标脏会是 Idle）。
        for window in [first, second] {
            match app.app_mut().frame(window).expect("窗口存在").expect("帧") {
                crate::window::FrameOutcome::Committed(_) => {}
                other => panic!("绑定几何的窗口 {window:?} 未被标脏：{other:?}"),
            }
        }

        // 空 range：合法 no-op ⇒ 不标脏、不请求帧。
        let requests = app.app().frame_requests();
        let update = app
            .app_mut()
            .update_geometry_vertices(id, 0..0, &[])
            .expect("空区间合法");
        assert!(update.damaged.is_empty());
        assert_eq!(app.app().frame_requests(), requests, "空 range 不请求帧");
    }

    // ── 子项 1：Canvas 引用的几何，数据更新后自动标脏 ────────────────────────

    /// 挂一个 Canvas 节点（内容引用 `geometries` 里的几何），返回节点 id。
    fn mount_canvas(app: &mut TestApp, geometries: &[GeometryId]) -> crate::node::id::NodeId {
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口存在").tree().root();
        let mut canvas = Canvas::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        for id in geometries {
            canvas.push_geometry(GeometryDraw {
                id: *id,
                transform: Transform::IDENTITY,
                color: Color::WHITE,
            });
        }
        NodeBuilder::canvas(canvas)
            .layout_override(Rect::new(0.0, 0.0, 40.0, 40.0))
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("挂载 Canvas")
    }

    fn clear_dirty(app: &mut TestApp) {
        let window = app.window_id();
        app.app_mut()
            .window_mut(window)
            .expect("窗口存在")
            .tree_mut()
            .clear_all_dirty_for_test();
    }

    fn is_paint_dirty(app: &TestApp, node: crate::node::id::NodeId) -> bool {
        let window = app.window_id();
        app.app()
            .window(window)
            .expect("窗口存在")
            .tree()
            .get(node)
            .expect("节点存在")
            .dirty()
            .contains(DirtyFlags::PAINT)
    }

    /// 公开路径：`NodeBuilder::canvas` + `Canvas::push_geometry`（`GeometryDraw.id` 公开）。
    /// 更新该几何的顶点后，Canvas 节点必须被自动标 PAINT 且窗口请求一帧（旧实现只标几何节点，
    /// Canvas 引用完全不在绑定表里 ⇒ 更新要等下一次无关重绘才上屏）。
    #[test]
    fn canvas_referencing_geometry_is_marked_when_the_geometry_data_updates() {
        let mut app = TestApp::new();
        let id = register_long(&mut app, 4);
        let node = mount_canvas(&mut app, &[id]);
        let _ = app.drain().expect("首帧");
        clear_dirty(&mut app);

        let requests = app.app().frame_requests();
        let update = app
            .app_mut()
            .update_geometry_vertices(id, 0..12, &vertex(100.0, 100.0))
            .expect("顶点更新");
        assert!(
            !update.damaged.is_empty(),
            "被块引用的顶点更新必须有 damage"
        );
        assert!(
            is_paint_dirty(&app, node),
            "引用该几何的 Canvas 节点必须被标 PAINT"
        );
        assert!(
            app.app().frame_requests() > requests,
            "Canvas 绑定节点被标脏时必须请求一帧"
        );

        let record = app.drain().expect("更新帧");
        assert!(record.rendered, "Canvas 必须重新编码（整条 lane 重建）");
        assert!(
            damage_hits(&record.damage, 0.0, 0.0),
            "Canvas 重建必须把它的内容范围并入 damage：{:?}",
            record.damage
        );
    }

    /// `Window::set_canvas` 换内容后必须**重建绑定**：旧几何不再标脏该节点，新几何开始标脏。
    #[test]
    fn replacing_canvas_content_rebinds_the_referenced_geometries() {
        let mut app = TestApp::new();
        let first = register_long(&mut app, 4);
        let second = register_long(&mut app, 4);
        let node = mount_canvas(&mut app, &[first]);
        let _ = app.drain().expect("首帧");
        clear_dirty(&mut app);

        let _ = app
            .app_mut()
            .update_geometry_vertices(first, 0..12, &vertex(50.0, 50.0))
            .expect("更新旧几何");
        assert!(is_paint_dirty(&app, node), "替换前旧几何必须绑定 Canvas");
        let _ = app.drain().expect("旧几何更新帧");
        clear_dirty(&mut app);

        // 换成只引用 second 的内容。
        let window = app.window_id();
        let mut canvas = Canvas::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        canvas.push_geometry(GeometryDraw {
            id: second,
            transform: Transform::IDENTITY,
            color: Color::WHITE,
        });
        assert!(
            app.app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .set_canvas(node, canvas)
                .expect("窗口存在"),
            "Canvas 内容必须替换成功"
        );
        let _ = app.drain().expect("替换帧");
        clear_dirty(&mut app);

        // 旧几何再更新：已解绑 ⇒ 不标脏、不请求帧。
        let requests = app.app().frame_requests();
        let update = app
            .app_mut()
            .update_geometry_vertices(first, 0..12, &vertex(60.0, 60.0))
            .expect("更新旧几何");
        assert!(!update.damaged.is_empty(), "旧几何自己仍然有可见变化");
        assert!(
            !is_paint_dirty(&app, node),
            "被解绑的旧几何不得再标脏 Canvas"
        );
        assert_eq!(
            app.app().frame_requests(),
            requests,
            "没有绑定节点 ⇒ 不请求帧"
        );

        // 新几何更新：重新登记（标 PAINT + 请求帧）。
        let requests = app.app().frame_requests();
        let _ = app
            .app_mut()
            .update_geometry_vertices(second, 0..12, &vertex(70.0, 70.0))
            .expect("更新新几何");
        assert!(is_paint_dirty(&app, node), "新内容引用的几何必须登记");
        assert!(
            app.app().frame_requests() > requests,
            "新几何更新必须请求一帧"
        );
    }

    // ── 子项 2：truncate 接线：块 lane 释放 / 请求帧 / 截断后追加 ─────────────

    /// 截断 2 块 → 1 块：绑定节点标脏、窗口请求一帧，下一帧释放多余块 lane 并把被移除块的
    /// 旧 AABB 并入 damage；随后追加跨块会重新建出块 lane（绘制序再次发布）。
    #[test]
    fn truncating_geometry_releases_block_lanes_and_requests_a_frame() {
        let mut app = TestApp::new();
        let id = register_long(&mut app, 4096 + 10);
        let node = mount(&mut app, id);
        let window = app.window_id();
        assert_eq!(
            app.app()
                .window(window)
                .expect("窗口存在")
                .tree()
                .lane_count(node),
            2,
            "2 块几何 = 2 条块 lane"
        );
        let snapshot = app.app().geometry().snapshot_arc(id).expect("快照");
        let removed = snapshot.chunk(1).expect("块 1").bounds;
        let vertices = snapshot.vertex_len() / 12;
        let indices = snapshot.index_len() / 2;
        assert_eq!((vertices, indices), (4096 * 3 + 30, 4096 * 3 + 30));

        let requests = app.app().frame_requests();
        let update = app
            .app_mut()
            .truncate_geometry(id, 4096 * 3, 4096 * 3)
            .expect("截断成一块");
        assert!(
            update.damaged.origin.x <= removed.origin.x
                && update.damaged.origin.y <= removed.origin.y
                && update.damaged.max().x >= removed.max().x
                && update.damaged.max().y >= removed.max().y,
            "damage 必须覆盖被移除块的旧 AABB：removed={removed:?} update={update:?}"
        );
        assert!(
            app.app().frame_requests() > requests,
            "截断必须自动请求一帧"
        );

        let record = app.drain().expect("截断帧");
        assert!(
            record.delta.order_some,
            "释放了一条有图元的块 lane ⇒ 绘制序必须重新发布"
        );
        assert!(
            damage_hits(
                &record.damage,
                removed.origin.x + 1.0,
                removed.origin.y + 0.5
            ),
            "被移除块的旧位置必须进帧 damage：removed={removed:?} damage={:?}",
            record.damage
        );
        assert_eq!(
            app.app()
                .window(window)
                .expect("窗口存在")
                .tree()
                .lane_count(node),
            1,
            "块数减少后多余块 lane 必须释放"
        );

        // 截断到当前长度 = no-op：不标脏、不请求帧。
        let requests = app.app().frame_requests();
        let update = app
            .app_mut()
            .truncate_geometry(id, 4096 * 3, 4096 * 3)
            .expect("等长截断是 no-op");
        assert!(update.damaged.is_empty());
        assert_eq!(app.app().frame_requests(), requests, "no-op 不请求帧");

        // 截断后追加一个三角形（跨块）：重新建出块 lane，绘制序再次变化。
        append_triangle(&mut app, id, 100.0, 100.0);
        let record = app.drain().expect("追加帧");
        assert!(record.delta.order_some, "新建块 lane ⇒ 绘制序必须重新发布");
        assert!(
            damage_hits(&record.damage, 100.0, 100.0),
            "追加的三角形必须进 damage：{:?}",
            record.damage
        );
        assert_eq!(
            app.app()
                .window(window)
                .expect("窗口存在")
                .tree()
                .lane_count(node),
            2,
            "跨块追加必须重新建出块 1 的 lane"
        );
        let idle = app.drain().expect("后续帧");
        assert!(!idle.rendered, "顺序已发布后的无变化帧必须空闲");
        assert_eq!(idle.stats.encoded_lanes, 0);
    }
}
