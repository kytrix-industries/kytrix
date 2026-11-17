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
    window::WindowId,
};

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
    let extra_damage = match geometry.desc_arc(id) {
        Some(desc) => transformed_damage(previous, desc.bounds)
            .union(transformed_damage(transform, desc.bounds)),
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

    /// 注册一块 20×10 的几何（bounds 已知 ⇒ damage / op 范围可精确断言）。
    fn register(app: &mut TestApp) -> GeometryId {
        let desc = GeometryDesc {
            vertex_bytes: bytes::Bytes::from_static(&[0u8; 12 * 3]),
            index_bytes: bytes::Bytes::from_static(&[0u8; 2]),
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
}
