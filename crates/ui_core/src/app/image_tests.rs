//! E7：`NodeKind::Image` 的集成用例（布局 / 绘制 / 资源生命周期 / DPI / a11y）。
//!
//! 覆盖验收面：
//! - 布局：内在尺寸（未给 `Style` 尺寸）、显式 `Style` 尺寸优先；
//! - 绘制：`Primitive::Image` 的 bounds / 调制 / `Cover` 的 UV 裁切（`FrameRecord`）；
//! - 资源：未就绪占位（无图元）、`poll_resources` 交付后自动失效重绘、释放后不再画旧 tile；
//! - DPI：按新 scale 重算内在尺寸 + `set_image_resource` 换变体；
//! - a11y：默认 `Image` 角色、可选标签、无标签 hidden、默认非交互；
//! - I3：稳定后不再出帧。

use std::time::{Duration, Instant};

use bytes::Bytes;
use smol_str::SmolStr;
use taffy::style::Dimension;

use crate::{
    a11y::{A11yAction, A11yOverride, A11yRole},
    app::App,
    math::{Rect, Scale, Size},
    node::{builder::NodeBuilder, id::NodeId, image::ImageFit, tree::NodeTree},
    paint::{
        color::{Color, ColorResolver},
        style::{PaintSpec, Style},
    },
    test_support::{
        app::TestApp,
        record::{FrameRecord, PrimitiveKind, PrimitiveRecord},
    },
    window::{WindowId, WindowOptions},
};

/// 生成指定尺寸的不透明 PNG（真实解码路径，不用伪造 tile）。
fn png(width: u32, height: u32) -> Vec<u8> {
    let pixels: Vec<u8> = std::iter::repeat_n([10u8, 20, 30, 255], (width * height) as usize)
        .flatten()
        .collect();
    let image = image::RgbaImage::from_raw(width, height, pixels).expect("raw");
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode png");
    out.into_inner()
}

fn white() -> PaintSpec {
    PaintSpec {
        background: Color::WHITE,
        ..PaintSpec::default()
    }
}

/// `Style` 的显式尺寸（taffy definite）。
fn fixed(width: f32, height: f32) -> Style {
    let mut style = Style::default();
    style.inner.size = taffy::geometry::Size {
        width: Dimension::length(width),
        height: Dimension::length(height),
    };
    style
}

/// headless App + 一个窗口（拥有自己的 current-thread runtime，便于驱动异步解码）。
struct Fixture {
    rt: tokio::runtime::Runtime,
    app: App,
    window: WindowId,
}

impl Fixture {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(rt.handle().clone());
        let window = app.create_window(WindowOptions {
            title: SmolStr::new_static("image"),
            size: Size::new(400.0, 300.0),
            scale: Scale::ONE,
            ..WindowOptions::default()
        });
        let mut fixture = Self { rt, app, window };
        fixture.column_root();
        fixture
    }

    /// 根改成「纵向 flex + 左对齐」：子节点因此按自身尺寸（内在/显式）排布，不被 stretch 拉满。
    fn column_root(&mut self) {
        let tree = self.tree_mut();
        let root = tree.root();
        let mut style = tree.get(root).expect("root").style().clone();
        style.inner.flex_direction = taffy::style::FlexDirection::Column;
        style.inner.align_items = taffy::style::AlignItems::FLEX_START;
        tree.set_style(root, style).expect("root style");
    }

    fn tree_mut(&mut self) -> &mut NodeTree {
        self.app.window_mut(self.window).expect("window").tree_mut()
    }

    fn root(&self) -> NodeId {
        self.app.window(self.window).expect("window").tree().root()
    }

    fn insert(&mut self, bytes: &[u8]) -> crate::resources::ResourceImage {
        self.app.resources_mut().insert(bytes).expect("insert")
    }

    fn mount(&mut self, build: impl FnOnce(&mut NodeTree, NodeId) -> NodeId) -> NodeId {
        let root = self.root();
        build(self.tree_mut(), root)
    }

    fn bounds(&self, node: NodeId) -> Rect {
        self.app
            .window(self.window)
            .expect("window")
            .tree()
            .get(node)
            .expect("node")
            .layout()
            .bounds
    }

    /// 推进一帧并返回帧记录（空闲帧也有记录）。
    fn frame(&mut self) -> FrameRecord {
        self.app
            .frame(self.window)
            .expect("window exists")
            .expect("frame ok");
        self.app
            .window(self.window)
            .expect("window")
            .record()
            .expect("frame record")
            .clone()
    }

    /// 推进一帧并断言提交（有像素工作）。
    fn committed(&mut self) -> FrameRecord {
        let record = self.frame();
        assert!(record.rendered, "本帧必须提交：{record:?}");
        record
    }
}

/// 某个节点的全部 `Primitive::Image` 快照（按绘制序）。
fn images(record: &FrameRecord, node: NodeId) -> Vec<PrimitiveRecord> {
    record
        .frame
        .as_ref()
        .map(|frame| {
            frame
                .primitives
                .iter()
                .filter(|primitive| {
                    primitive.node == node && primitive.kind == PrimitiveKind::Image
                })
                .copied()
                .collect()
        })
        .unwrap_or_default()
}

/// 某个节点的唯一 `Primitive::Image`。
fn image(record: &FrameRecord, node: NodeId) -> PrimitiveRecord {
    let mut found = images(record, node);
    assert_eq!(found.len(), 1, "必须恰好一个图片图元：{found:?}");
    found.pop().expect("checked len")
}

fn has_quad(record: &FrameRecord, node: NodeId) -> bool {
    record
        .frame
        .as_ref()
        .is_some_and(|frame| frame.quads.iter().any(|quad| quad.node == node))
}

// ── 布局：内在尺寸 / 显式尺寸 ───────────────────────────────────────────────

/// 未给 `Style` 尺寸 ⇒ 用资源内在尺寸（tile 物理像素 ÷ 窗口 scale = 1）。
#[test]
fn intrinsic_size_drives_layout_and_fill_paint() {
    let mut fixture = Fixture::new();
    let resource = fixture.insert(&png(40, 20));
    let node = fixture.mount(|tree, root| {
        NodeBuilder::image(resource.id, ImageFit::Fill)
            .paint(white())
            .build(tree, root)
            .expect("image")
    });

    let record = fixture.committed();
    assert_eq!(
        fixture.bounds(node),
        Rect::new(0.0, 0.0, 40.0, 20.0),
        "未给尺寸时布局盒 = 资源内在尺寸"
    );
    let painted = image(&record, node);
    assert_eq!(painted.bounds, fixture.bounds(node), "Fill = 拉伸到节点盒");
    assert_eq!(
        painted.image_uv,
        Some(resource.tile.uv()),
        "Fill 不裁源：UV 就是资源 tile 的完整 UV"
    );
    assert_eq!(
        painted.image_modulation,
        Some(ColorResolver.resolve(Color::WHITE)),
        "默认调制 = 白色（不调制）"
    );
}

/// 显式 `Style` 尺寸优先于资源内在尺寸。
#[test]
fn explicit_style_size_wins_over_the_intrinsic_size() {
    let mut fixture = Fixture::new();
    let resource = fixture.insert(&png(40, 20));
    let node = fixture.mount(|tree, root| {
        NodeBuilder::image(resource.id, ImageFit::Fill)
            .style(fixed(16.0, 16.0))
            .build(tree, root)
            .expect("image")
    });

    let record = fixture.committed();
    assert_eq!(
        fixture.bounds(node),
        Rect::new(0.0, 0.0, 16.0, 16.0),
        "显式 Style 尺寸优先"
    );
    assert_eq!(image(&record, node).bounds, fixture.bounds(node));
}

// ── 绘制：Contain / Cover 几何 + 调制 ──────────────────────────────────────

/// `Contain` = 等比内嵌居中；`Cover` = 等比铺满 + 源 UV 居中裁切。
#[test]
fn contain_letterboxes_and_cover_crops_the_source_uv() {
    let mut fixture = Fixture::new();
    let resource = fixture.insert(&png(40, 20)); // 2:1
    let tile = resource.tile;
    let build = |fit: ImageFit| {
        move |tree: &mut NodeTree, root: NodeId| {
            NodeBuilder::image(resource.id, fit)
                .style(fixed(16.0, 16.0))
                .build(tree, root)
                .expect("image")
        }
    };
    let contain = fixture.mount(build(ImageFit::Contain));
    let cover = fixture.mount(build(ImageFit::Cover));

    let record = fixture.committed();

    // Contain：scale = min(16/40, 16/20) = 0.4 ⇒ 16×8，垂直居中（y = 4）。
    let contain_image = image(&record, contain);
    assert_eq!(
        contain_image.bounds,
        Rect::new(0.0, 4.0, 16.0, 8.0),
        "Contain = 等比内嵌居中"
    );
    assert_eq!(contain_image.image_uv, Some(tile.uv()), "Contain 不裁源");

    // Cover：scale = max(0.4, 0.8) = 0.8 ⇒ 显示 32×16，水平裁中间一半、垂直完整。
    // 图元 bounds 恒等于节点盒（源区域裁在 UV 上，不溢出、不需要 clip）。
    let cover_image = image(&record, cover);
    assert_eq!(
        cover_image.bounds,
        Rect::new(0.0, 16.0, 16.0, 16.0),
        "Cover 的图元 bounds = 节点盒"
    );
    let (u0, v0, u1, v1) = (tile.u0, tile.v0, tile.u1, tile.v1);
    let expected_u0 = u0 + (u1 - u0) * 0.25;
    let expected_u1 = u1 - (u1 - u0) * 0.25;
    let uv = cover_image.image_uv.expect("image uv");
    assert!(
        (uv[0] - expected_u0).abs() < 1e-6,
        "u0 裁掉 25% 源宽：{uv:?}"
    );
    assert!(
        (uv[2] - expected_u1).abs() < 1e-6,
        "u1 裁掉 25% 源宽：{uv:?}"
    );
    assert!((uv[1] - v0).abs() < 1e-6, "垂直方向不裁：{uv:?}");
    assert!((uv[3] - v1).abs() < 1e-6, "垂直方向不裁：{uv:?}");
}

/// 静态调制直接进 `Primitive::Image`；`bind_style` 的 `foreground` 动态覆盖（文本同通道）。
#[test]
fn static_and_dynamic_modulation_reach_the_image_primitive() {
    let mut fixture = Fixture::new();
    let resource = fixture.insert(&png(8, 8));
    let static_node = fixture.mount(|tree, root| {
        NodeBuilder::image(resource.id, ImageFit::Fill)
            .image_modulation(Color::BLACK)
            .style(fixed(8.0, 8.0))
            .build(tree, root)
            .expect("image")
    });
    let dynamic_node = fixture.mount(|tree, root| {
        NodeBuilder::image(resource.id, ImageFit::Fill)
            .image_modulation(Color::BLACK)
            .bind_style(|_| crate::paint::style::StylePatch {
                foreground: Some(Color::WHITE),
                ..crate::paint::style::StylePatch::default()
            })
            .style(fixed(8.0, 8.0))
            .build(tree, root)
            .expect("image")
    });

    let record = fixture.committed();
    assert_eq!(
        image(&record, static_node).image_modulation,
        Some(ColorResolver.resolve(Color::BLACK)),
        "静态调制"
    );
    assert_eq!(
        image(&record, dynamic_node).image_modulation,
        Some(ColorResolver.resolve(Color::WHITE)),
        "bind_style.foreground 覆盖静态调制"
    );
}

/// 资源不存活（陈旧 / 已释放句柄）⇒ 不画图元、不 panic（占位 = 节点背景 / 透明）。
#[test]
fn missing_resource_paints_no_image_primitive() {
    let mut fixture = Fixture::new();
    let stale = crate::resources::ResourceId::for_test(99, 1);
    let node = fixture.mount(|tree, root| {
        NodeBuilder::image(stale, ImageFit::Fill)
            .paint(white())
            .style(fixed(16.0, 16.0))
            .build(tree, root)
            .expect("image")
    });

    let record = fixture.committed();
    assert!(images(&record, node).is_empty(), "没有资源不得画图片图元");
    assert!(has_quad(&record, node), "占位 = 节点自己的背景 Quad");
    assert_eq!(fixture.bounds(node), Rect::new(0.0, 0.0, 16.0, 16.0));
}

// ── 资源生命周期：未就绪 → 到达 → 自动重绘；释放 / 关窗 ──────────────────

/// 未就绪占位 → 异步解码交付 → 引擎自动改就绪、重排重绘（应用不重建节点）。
#[test]
fn pending_image_placeholder_is_replaced_when_the_resource_arrives() {
    let mut fixture = Fixture::new();
    let pending = fixture
        .app
        .resources_mut()
        .insert_async(Bytes::from(png(40, 20)));
    let node = fixture.mount(|tree, root| {
        NodeBuilder::image_pending(pending, ImageFit::Fill)
            .paint(white())
            .style(fixed(40.0, 20.0))
            .build(tree, root)
            .expect("image")
    });

    // 第 1 帧：current-thread runtime 没有被驱动，解码任务还没跑 ⇒ 资源未就绪。
    let first = fixture.committed();
    assert!(images(&first, node).is_empty(), "未就绪时不得画图元");
    assert!(has_quad(&first, node), "占位 = 节点背景");

    // 驱动 runtime 让解码完成，再轮询交付（等价于平台泵的 poll_resources）。
    let delivered = drive_until_delivered(&mut fixture);
    assert_eq!(delivered.len(), 1, "必须交付一项");
    assert!(
        matches!(delivered[0].1, crate::resources::DecodeState::Ready(_)),
        "解码必须成功：{:?}",
        delivered[0].1
    );

    // 交付已把节点改成 Ready 并标 LAYOUT | PAINT ⇒ 下一帧出图。
    let second = fixture.committed();
    let painted = image(&second, node);
    assert_eq!(painted.bounds, fixture.bounds(node));
    assert_eq!(fixture.bounds(node), Rect::new(0.0, 0.0, 40.0, 20.0));
}

/// 释放资源 ⇒ 引擎把引用它的图片节点标脏；下一帧不画旧 tile、布局不变。
#[test]
fn unloading_a_resource_repaints_without_stale_images() {
    let mut fixture = Fixture::new();
    let resource = fixture.insert(&png(24, 12));
    let node = fixture.mount(|tree, root| {
        NodeBuilder::image(resource.id, ImageFit::Fill)
            .paint(white())
            .build(tree, root)
            .expect("image")
    });
    let _ = fixture.committed();

    assert!(fixture.app.unload_resource(resource.id), "资源存在");
    let record = fixture.committed();
    assert!(
        images(&record, node).is_empty(),
        "资源释放后不得继续画旧 tile（lane 必须重建）"
    );
    assert_eq!(
        fixture.bounds(node),
        Rect::new(0.0, 0.0, 24.0, 12.0),
        "卸载不改变布局（内在尺寸保留）"
    );
}

/// 关窗释放 `insert_owned` 的引用后，**其它窗口**里引用同一句柄的图片节点不得继续画旧 tile。
#[test]
fn closing_a_window_invalidates_images_referencing_released_resources() {
    let mut fixture = Fixture::new();
    let owner = fixture.app.create_window(WindowOptions {
        title: SmolStr::new_static("owner"),
        size: Size::new(64.0, 64.0),
        scale: Scale::ONE,
        ..WindowOptions::default()
    });
    let bytes = png(16, 16);
    let resource = fixture
        .app
        .resources_mut()
        .insert_owned(owner, &bytes)
        .expect("insert owned");

    // 共享句柄挂在另一个窗口的图片节点上（跨窗口共享是允许的）。
    let node = fixture.mount(|tree, root| {
        NodeBuilder::image(resource.id, ImageFit::Contain)
            .paint(white())
            .build(tree, root)
            .expect("image")
    });
    let first = fixture.committed();
    assert!(!images(&first, node).is_empty(), "关窗前必须画出来");

    assert!(fixture.app.close_window(owner), "owner 窗口存在");
    assert_eq!(fixture.app.resources().len(), 0, "关窗后引用必须归还");
    let record = fixture.committed();
    assert!(
        images(&record, node).is_empty(),
        "资源已释放：其它窗口不得悬垂引用旧 tile"
    );
}

/// 驱动 runtime 并反复轮询，直到 `poll_resources` 交付至少一项（超时 panic）。
fn drive_until_delivered(
    fixture: &mut Fixture,
) -> Vec<(
    crate::resources::PendingResource,
    crate::resources::DecodeState,
)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        fixture
            .rt
            .block_on(async { tokio::time::sleep(Duration::from_millis(10)).await });
        fixture.app.poll_resources();
        let delivered = fixture.app.take_resource_results();
        if !delivered.is_empty() {
            return delivered;
        }
        assert!(
            Instant::now() < deadline,
            "异步解码在 5s 内没有交付（测试基础设施故障）"
        );
    }
}

// ── DPI：内在尺寸重算 + 变体切换 ───────────────────────────────────────────

/// DPI 变化 ⇒ 内在尺寸 = tile / 新 scale；应用换 2× 变体后逻辑尺寸恢复。
#[test]
fn dpi_change_reresolves_the_intrinsic_size_and_accepts_a_new_variant() {
    let mut fixture = Fixture::new();
    // 80×40 的 2× 资产：1× 屏取 40×20 变体（逻辑尺寸 40×20），2× 屏取 80×40 变体。
    let bytes = png(80, 40);
    let one_x = fixture
        .app
        .resources_mut()
        .insert_at_size(&bytes, Size::new(40.0, 20.0))
        .expect("1x variant");
    let node = fixture.mount(|tree, root| {
        NodeBuilder::image(one_x.id, ImageFit::Fill)
            .build(tree, root)
            .expect("image")
    });
    let _ = fixture.committed();
    assert_eq!(fixture.bounds(node), Rect::new(0.0, 0.0, 40.0, 20.0));

    // 1× → 2×：tile 仍是 40×20 物理像素 ⇒ 逻辑内在尺寸减半（应用尚未换变体）。
    fixture
        .app
        .window_mut(fixture.window)
        .expect("window")
        .set_scale(Scale::new(2.0).expect("finite scale"));
    let _ = fixture.committed();
    assert_eq!(
        fixture.bounds(node),
        Rect::new(0.0, 0.0, 20.0, 10.0),
        "内在尺寸 = tile 物理像素 ÷ 窗口 scale"
    );

    // 应用按新 scale 取 2× 变体并换到节点上：下一帧布局前重解析 ⇒ 逻辑尺寸恢复。
    let two_x = fixture
        .app
        .resources_mut()
        .insert_for_display(&bytes, Size::new(40.0, 20.0), 2.0)
        .expect("2x variant");
    assert_eq!(
        (two_x.width, two_x.height),
        (80, 40),
        "2× 变体必须是 80×40 物理像素"
    );
    assert!(
        fixture
            .app
            .window_mut(fixture.window)
            .expect("window")
            .set_image_resource(node, two_x.id)
            .expect("node alive"),
        "图片节点接受新资源句柄"
    );
    let record = fixture.committed();
    assert_eq!(
        fixture.bounds(node),
        Rect::new(0.0, 0.0, 40.0, 20.0),
        "按新 scale 选变体后逻辑尺寸恢复"
    );
    assert_eq!(
        image(&record, node).image_uv,
        Some(two_x.tile.uv()),
        "绘制必须用新变体的 tile"
    );
}

/// I3：稳定后的图片节点不再产生帧。
#[test]
fn a_steady_image_node_does_not_request_frames() {
    let mut fixture = Fixture::new();
    let resource = fixture.insert(&png(8, 8));
    let _node = fixture.mount(|tree, root| {
        NodeBuilder::image(resource.id, ImageFit::Fill)
            .build(tree, root)
            .expect("image")
    });
    let _ = fixture.committed();
    let idle = fixture.frame();
    assert!(!idle.rendered, "无变化的图片节点必须 Idle（I3）");
}

// ── a11y ────────────────────────────────────────────────────────────────────

/// 默认 `Image` 角色 + 可选标签；无标签按装饰图 hidden；默认非交互（无动作）。
#[test]
fn image_nodes_map_to_the_image_role_with_optional_label() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let resource = app
        .app_mut()
        .resources_mut()
        .insert(&png(4, 4))
        .expect("insert");
    let window = app.window_id();
    let root = app.app().window(window).expect("window").tree().root();
    let mount = |app: &mut TestApp, label: Option<&'static str>| {
        let mut builder = NodeBuilder::image(resource.id, ImageFit::Contain)
            .layout_override(Rect::new(0.0, 0.0, 16.0, 16.0));
        if let Some(label) = label {
            builder = builder.a11y(A11yOverride {
                label: Some(SmolStr::new_static(label)),
                ..A11yOverride::default()
            });
        }
        builder
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                root,
            )
            .expect("image")
    };
    let labelled = mount(&mut app, Some("品牌图标"));
    let decorative = mount(&mut app, None);
    let _ = app.drain().expect("frame");

    let snapshot = app.a11y().expect("a11y 交付");
    let labelled_node = snapshot.node_for(labelled).expect("labelled");
    assert_eq!(labelled_node.role, A11yRole::Image);
    assert_eq!(labelled_node.label_text(), Some("品牌图标"));
    assert!(!labelled_node.hidden, "有 alt 的图片节点不得隐藏");
    assert!(
        labelled_node.actions.is_empty(),
        "图片默认非交互：不得有动作，实际 {:?}",
        labelled_node.actions
    );
    assert!(!labelled_node.actions.contains(&A11yAction::Click));

    let decorative_node = snapshot.node_for(decorative).expect("decorative");
    assert_eq!(decorative_node.role, A11yRole::Image);
    assert!(
        decorative_node.hidden,
        "没有 alt 的图片节点按装饰图 hidden（不静默播报空节点）"
    );
}
