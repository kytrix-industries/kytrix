//! 应用层的几何入口：`App::set_geometry_transform`（R05-12 / AR2 P1-D4）。
//!
//! 几何节点（`NodeBuilder::geometry`）的顶点在**装载期**一次写入 `GeometryStore`，之后不可变；
//! 平移/缩放一块图表只需要改一个 per-draw 变换（`GeometryDraw.transform` → 顶点着色器的
//! `draw.basis` / `draw.translation`），**不重建顶点缓冲**。这正是 R05-12 的核心诉求，但在
//! wave-1 里写入口只存在于 `#[cfg(test)]`（`node/tree/content.rs`），生产不可达 ⇒ 整条
//! 「变换 → uniform → damage」链路是死代码（AR2 P1-D4）。本模块补上公开入口。
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

/// `GeometryStore::update_*` 的引擎侧包装（X5）：数据更新 + 绑定几何节点自动标脏。
///
/// # 为什么不是纯数据更新
///
/// 几何数据变了以后，绘制输出（`Primitive::Geometry` / 块 lane 的块 AABB）必须跟着重算，
/// 否则 GPU 读到的还是旧快照。`GeometryStore` 是进程级、不知道哪些节点引用它，因此这里用
/// 节点树的反向绑定表把「数据更新」翻译成「绑定节点标 PAINT」：paint 走几何分块路径，
/// 只重建**块版本变化**的块 lane（追加 K 线只重建尾块）。
///
/// 节点级 `REUSABLE` 不清、节点盒 damage 不并（见 `NodeTree::mark_geometry_updated`）：
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
    mark_bound_nodes(app, id, update.damaged);
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
    mark_bound_nodes(app, id, update.damaged);
    Ok(update)
}

/// 把绑定到 `geometry` 的节点标 PAINT（`damaged` 为空 = 没有块受影响 ⇒ 不需要重绘）。
fn mark_bound_nodes(app: &mut App, geometry: GeometryId, damaged: Rect) {
    if damaged.is_empty() {
        return;
    }
    for window in app.windows.values_mut() {
        window.tree_mut().mark_geometry_updated(geometry);
    }
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
        node::builder::NodeBuilder,
        paint::{
            geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
            instance::FrameOp,
            primitive::Primitive,
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
}
