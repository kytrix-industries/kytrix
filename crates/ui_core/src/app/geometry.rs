use super::App;
use crate::{
    error::EngineError,
    math::{Rect, Transform},
    node::{id::NodeId, tree::NodeKind},
    paint::geometry::{GeometryId, GeometryUpdate},
    window::WindowId,
};

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

const GEOMETRY_DAMAGE_PAD: f32 = 1.0;

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
    let extra_damage = match geometry.snapshot_arc(id) {
        Some(snapshot) => transformed_damage(previous, snapshot.bounds())
            .union(transformed_damage(transform, snapshot.bounds())),
        None => Rect::default(),
    };
    tree.set_geometry_transform(node, transform, extra_damage)
}

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
        assert_eq!(bounds.x0, (12.0 * scale).floor() as i32 - 1);
        assert_eq!(bounds.y0, (7.0 * scale).floor() as i32 - 1);
        assert_eq!(bounds.x1, (32.0 * scale).ceil() as i32 + 1);
        assert_eq!(bounds.y1, (17.0 * scale).ceil() as i32 + 1);
    }

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

    use crate::math::PhysicalRect;

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

    #[test]
    fn range_update_rebuilds_only_the_affected_chunk_lane() {
        let mut app = TestApp::new();
        let id = register_long(&mut app, 4096 + 10);
        let node = mount(&mut app, id);
        let before = chunk_lane_versions(&app, node);
        assert_eq!(before.len(), 2, "两块几何 = 两条块 lane");

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

        let idle = app.drain().expect("后续帧");
        assert!(!idle.rendered, "顺序已发布后的无变化帧必须空闲");
        assert_eq!(idle.stats.encoded_lanes, 0);
    }

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
        let update = app
            .app_mut()
            .update_geometry_indices(id, 0..0, &[])
            .expect("空区间合法");
        assert!(update.damaged.is_empty());
    }

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
        for window in [first, second] {
            match app.app_mut().frame(window).expect("窗口存在").expect("帧") {
                crate::window::FrameOutcome::Committed(_) => {}
                other => panic!("绑定几何的窗口 {window:?} 未被标脏：{other:?}"),
            }
        }

        let requests = app.app().frame_requests();
        let update = app
            .app_mut()
            .update_geometry_vertices(id, 0..0, &[])
            .expect("空区间合法");
        assert!(update.damaged.is_empty());
        assert_eq!(app.app().frame_requests(), requests, "空 range 不请求帧");
    }

    fn mount_canvas(app: &mut TestApp, geometries: &[GeometryId]) -> crate::node::id::NodeId {
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口存在").tree().root();
        let mut canvas = Canvas::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        for id in geometries {
            canvas.push_geometry(GeometryDraw::full(*id, Transform::IDENTITY, Color::WHITE));
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

    #[test]
    fn canvas_referencing_geometry_is_marked_when_the_geometry_data_updates() {
        let mut app = TestApp::new();
        let window = app.window_id();
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
        let encoded = app
            .app()
            .window(window)
            .expect("窗口存在")
            .encoded_geometry();
        let draw = encoded
            .iter()
            .find(|draw| draw.id == id)
            .expect("Canvas 内几何必须被编码成几何 op");
        assert!(
            draw.snapshot.revision() > 0,
            "Canvas 内几何必须携带真实快照（修复前是 missing_snapshot）：revision={}",
            draw.snapshot.revision()
        );
        assert!(
            !draw.snapshot.vertex_pages().is_empty() && !draw.snapshot.index_pages().is_empty(),
            "真实快照必须有页数据（空页会让 GPU 报 GeometryRegister）"
        );
    }

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

        let window = app.window_id();
        let mut canvas = Canvas::new(Rect::new(0.0, 0.0, 40.0, 40.0));
        canvas.push_geometry(GeometryDraw::full(
            second,
            Transform::IDENTITY,
            Color::WHITE,
        ));
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

        let requests = app.app().frame_requests();
        let update = app
            .app_mut()
            .truncate_geometry(id, 4096 * 3, 4096 * 3)
            .expect("等长截断是 no-op");
        assert!(update.damaged.is_empty());
        assert_eq!(app.app().frame_requests(), requests, "no-op 不请求帧");

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

    #[test]
    fn truncating_geometry_to_zero_keeps_the_decorative_lane_and_can_grow_back() {
        let mut app = TestApp::new();
        let id = register_long(&mut app, 4096 + 10);
        let node = mount(&mut app, id);
        let window = app.window_id();
        let snapshot = app.app().geometry().snapshot_arc(id).expect("快照");
        let first_bounds = snapshot.chunk(0).expect("块 0").bounds;
        let second_bounds = snapshot.chunk(1).expect("块 1").bounds;
        assert_eq!(snapshot.bounds(), Rect::new(0.0, 0.0, 64.0, 100.0));

        let _ = app
            .app_mut()
            .truncate_geometry(id, 0, 0)
            .expect("清空成 0 块");
        let record = app.drain().expect("清空帧");
        assert!(
            damage_hits(
                &record.damage,
                first_bounds.origin.x + 1.0,
                first_bounds.origin.y + 1.0
            ),
            "被移除的块 0 必须进 damage：{:?}",
            record.damage
        );
        assert!(
            damage_hits(&record.damage, 5.0, second_bounds.origin.y + 0.5),
            "被移除的块 1 必须进 damage：{:?}",
            record.damage
        );
        assert!(
            record.damage.iter().all(|rect| rect.y1 <= 70),
            "缺失块不得回退整个注册 bounds（高 100）：{:?}",
            record.damage
        );
        let window_state = app.app().window(window).expect("窗口存在");
        assert_eq!(
            window_state.tree().lane_count(node),
            1,
            "0 块时仍保留一条承载装饰的 lane"
        );
        assert!(
            window_state.encoded_geometry().is_empty(),
            "0 块时不得发射几何 op：{:?}",
            window_state.encoded_geometry()
        );

        append_triangle(&mut app, id, 100.0, 100.0);
        let record = app.drain().expect("追加帧");
        assert!(record.rendered, "追加必须提交");
        assert!(
            damage_hits(&record.damage, 100.0, 100.0),
            "追加的三角形必须进 damage：{:?}",
            record.damage
        );
        let window_state = app.app().window(window).expect("窗口存在");
        assert_eq!(
            window_state.tree().lane_count(node),
            1,
            "0 块上追加一个三角形后仍是 1 条块 lane"
        );
        assert!(
            !window_state.encoded_geometry().is_empty(),
            "追加后必须恢复几何 op"
        );
    }

    #[test]
    fn truncating_geometry_reencodes_the_canvas_that_references_it() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let id = register_long(&mut app, 4096 + 10);
        let node = mount_canvas(&mut app, &[id]);
        let _ = app.drain().expect("首帧");
        let ops_before = app
            .app()
            .window(window)
            .expect("窗口存在")
            .encoded_geometry()
            .iter()
            .filter(|draw| draw.id == id)
            .count();
        assert_eq!(ops_before, 2, "Canvas 内容按块展开：2 块 = 2 个几何 op");
        clear_dirty(&mut app);

        let requests = app.app().frame_requests();
        let update = app
            .app_mut()
            .truncate_geometry(id, 4096 * 3, 4096 * 3)
            .expect("截断成一块");
        assert!(!update.damaged.is_empty());
        assert!(
            is_paint_dirty(&app, node),
            "Canvas 引用的几何被截断后 Canvas 节点必须被标 PAINT"
        );
        assert!(
            app.app().frame_requests() > requests,
            "截断必须为 Canvas 请求一帧"
        );

        let _ = app.drain().expect("截断帧");
        let ops: Vec<_> = app
            .app()
            .window(window)
            .expect("窗口存在")
            .encoded_geometry()
            .iter()
            .filter(|draw| draw.id == id)
            .cloned()
            .collect();
        assert_eq!(ops.len(), 1, "截断后 Canvas 内几何只应展开 1 块：{ops:?}");
        assert_eq!(
            ops[0].range,
            0..12288,
            "整块几何绘制的绘制区间 = 截断后唯一的块（E4：op 携带裁剪后的 range）"
        );
        assert!(
            ops[0].snapshot.revision() > 0 && !ops[0].snapshot.vertex_pages().is_empty(),
            "Canvas 内几何 op 必须携带截断后的真实快照"
        );
    }

    #[test]
    fn canvas_geometry_range_change_reencodes_and_damages_locally() {
        let mut app = TestApp::new();
        let id = register_long(&mut app, 4096 + 10);
        let node = mount_canvas(&mut app, &[id]);
        let _ = app.drain().expect("首帧");
        let ranges = |app: &TestApp| -> Vec<std::ops::Range<u32>> {
            let window = app.window_id();
            app.app()
                .window(window)
                .expect("窗口存在")
                .encoded_geometry()
                .iter()
                .filter(|draw| draw.id == id)
                .map(|draw| draw.range.clone())
                .collect()
        };
        assert_eq!(
            ranges(&app),
            vec![0..12288, 12288..12318],
            "整份绘制的 Canvas 内几何按块展开（E4：每 op 携带本块的裁剪后区间）"
        );
        clear_dirty(&mut app);

        let window = app.window_id();
        let mut wide = Canvas::new(Rect::new(0.0, 0.0, 64.0, 100.0));
        wide.push_geometry(GeometryDraw::full(id, Transform::IDENTITY, Color::WHITE));
        assert!(
            app.app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .set_canvas(node, wide)
                .expect("窗口存在"),
            "Canvas 内容必须替换成功"
        );
        let record = app.drain().expect("画布扩展帧");
        assert!(record.rendered, "画布扩展必须提交一帧");
        clear_dirty(&mut app);

        let visible_bounds = Rect::new(0.0, 64.0, 10.0, 1.0);
        let mut canvas = Canvas::new(visible_bounds);
        canvas.push_geometry(GeometryDraw {
            id,
            transform: Transform::IDENTITY,
            color: Color::WHITE,
            range: 12288..12318,
        });
        assert!(
            app.app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .set_canvas(node, canvas)
                .expect("窗口存在"),
            "Canvas 内容必须替换成功"
        );
        let record = app.drain().expect("range 变更帧");
        assert!(record.rendered, "range 变更必须提交一帧");
        assert!(!record.damage_full, "子范围变更的 damage 不得整帧");
        assert!(
            damage_hits(&record.damage, 5.0, 64.0),
            "新 range 的可见范围必须进 damage（新画布 AABB）：{:?}",
            record.damage
        );
        assert!(
            damage_hits(&record.damage, 5.0, 50.0),
            "旧画布 AABB（新画布/节点盒之外的部分）必须进 damage（旧 ∪ 新）：{:?}",
            record.damage
        );
        assert_eq!(
            ranges(&app),
            vec![12288..12318],
            "只编码新 range 相交的块（不相交的块不产生 op）"
        );

        let mut canvas = Canvas::new(visible_bounds);
        canvas.push_geometry(GeometryDraw {
            id,
            transform: Transform::IDENTITY,
            color: Color::WHITE,
            range: 6..6,
        });
        assert!(
            app.app_mut()
                .window_mut(window)
                .expect("窗口存在")
                .set_canvas(node, canvas)
                .expect("窗口存在"),
            "Canvas 内容必须替换成功"
        );
        let record = app.drain().expect("空 range 帧");
        assert!(record.rendered, "旧 range 的像素必须重画（damage 非空）");
        assert!(!record.damage_full, "空 range 变更同样不得整帧");
        assert!(
            damage_hits(&record.damage, 5.0, 64.0),
            "上一帧 range 的旧像素（旧画布 AABB）必须进 damage：{:?}",
            record.damage
        );
        assert!(ranges(&app).is_empty(), "空 range 的 draw 不产生几何 op");
    }
}
