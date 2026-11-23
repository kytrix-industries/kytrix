use std::time::{Duration, Instant};

use bytes::Bytes;
use smol_str::SmolStr;

use crate::{
    a11y::{A11yAction, A11yDiagnostic, A11yDrawKind, A11yOverride, A11yRole},
    app::App,
    math::{DevicePixelRatio, LogicalRect, LogicalSize, px},
    node::{builder::NodeBuilder, id::NodeId, image::ImageFit, tree::NodeTree},
    paint::{
        color::{Color, ColorResolver},
        style::{PaintSpec, Style},
        style_builder::StyleBuilder,
    },
    test_support::{
        app::TestApp,
        record::{FrameRecord, PrimitiveKind, PrimitiveRecord},
    },
    window::{WindowId, WindowOptions},
};

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

fn fixed(width: f32, height: f32) -> Style {
    StyleBuilder::new().size(px(width), px(height)).build()
}

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
            size: LogicalSize::new(px(400.0), px(300.0)),
            scale: DevicePixelRatio::ONE,
            ..WindowOptions::default()
        });
        let mut fixture = Self { rt, app, window };
        fixture.column_root();
        fixture
    }

    fn column_root(&mut self) {
        let tree = self.tree_mut();
        let root = tree.root();
        let mut style = tree.get(root).expect("root").style().clone();
        style
            .flex_direction(taffy::style::FlexDirection::Column)
            .align_items(taffy::style::AlignItems::FLEX_START);
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

    fn bounds(&self, node: NodeId) -> LogicalRect {
        self.app
            .window(self.window)
            .expect("window")
            .tree()
            .get(node)
            .expect("node")
            .layout()
            .bounds
    }

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

    fn committed(&mut self) -> FrameRecord {
        let record = self.frame();
        assert!(record.rendered, "本帧必须提交：{record:?}");
        record
    }
}

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
        LogicalRect::new(px(0.0), px(0.0), px(40.0), px(20.0)),
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
        LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0)),
        "显式 Style 尺寸优先"
    );
    assert_eq!(image(&record, node).bounds, fixture.bounds(node));
}

#[test]
fn contain_letterboxes_and_cover_crops_the_source_uv() {
    let mut fixture = Fixture::new();
    let resource = fixture.insert(&png(40, 20));
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

    let contain_image = image(&record, contain);
    assert_eq!(
        contain_image.bounds,
        LogicalRect::new(px(0.0), px(4.0), px(16.0), px(8.0)),
        "Contain = 等比内嵌居中"
    );
    assert_eq!(contain_image.image_uv, Some(tile.uv()), "Contain 不裁源");

    let cover_image = image(&record, cover);
    assert_eq!(
        cover_image.bounds,
        LogicalRect::new(px(0.0), px(16.0), px(16.0), px(16.0)),
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
    assert_eq!(
        fixture.bounds(node),
        LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0))
    );
}

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

    let first = fixture.committed();
    assert!(images(&first, node).is_empty(), "未就绪时不得画图元");
    assert!(has_quad(&first, node), "占位 = 节点背景");

    let delivered = drive_until_delivered(&mut fixture);
    assert_eq!(delivered.len(), 1, "必须交付一项");
    assert!(
        matches!(delivered[0].1, crate::resources::DecodeState::Ready(_)),
        "解码必须成功：{:?}",
        delivered[0].1
    );

    let second = fixture.committed();
    let painted = image(&second, node);
    assert_eq!(painted.bounds, fixture.bounds(node));
    assert_eq!(
        fixture.bounds(node),
        LogicalRect::new(px(0.0), px(0.0), px(40.0), px(20.0))
    );
}

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
        LogicalRect::new(px(0.0), px(0.0), px(24.0), px(12.0)),
        "卸载不改变布局（内在尺寸保留）"
    );
}

#[test]
fn closing_a_window_invalidates_images_referencing_released_resources() {
    let mut fixture = Fixture::new();
    let owner = fixture.app.create_window(WindowOptions {
        title: SmolStr::new_static("owner"),
        size: LogicalSize::new(px(64.0), px(64.0)),
        scale: DevicePixelRatio::ONE,
        ..WindowOptions::default()
    });
    let bytes = png(16, 16);
    let resource = fixture
        .app
        .resources_mut()
        .insert_owned(owner, &bytes)
        .expect("insert owned");

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

#[test]
fn dpi_change_reresolves_the_intrinsic_size_and_accepts_a_new_variant() {
    let mut fixture = Fixture::new();
    let bytes = png(80, 40);
    let one_x = fixture
        .app
        .resources_mut()
        .insert_at_size(&bytes, LogicalSize::new(px(40.0), px(20.0)))
        .expect("1x variant");
    let node = fixture.mount(|tree, root| {
        NodeBuilder::image(one_x.id, ImageFit::Fill)
            .build(tree, root)
            .expect("image")
    });
    let _ = fixture.committed();
    assert_eq!(
        fixture.bounds(node),
        LogicalRect::new(px(0.0), px(0.0), px(40.0), px(20.0))
    );

    fixture
        .app
        .window_mut(fixture.window)
        .expect("window")
        .set_scale(DevicePixelRatio::new(2.0).expect("finite scale"));
    let _ = fixture.committed();
    assert_eq!(
        fixture.bounds(node),
        LogicalRect::new(px(0.0), px(0.0), px(20.0), px(10.0)),
        "内在尺寸 = tile 物理像素 ÷ 窗口 scale"
    );

    let two_x = fixture
        .app
        .resources_mut()
        .insert_for_display(&bytes, LogicalSize::new(px(40.0), px(20.0)), 2.0)
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
        LogicalRect::new(px(0.0), px(0.0), px(40.0), px(20.0)),
        "按新 scale 选变体后逻辑尺寸恢复"
    );
    assert_eq!(
        image(&record, node).image_uv,
        Some(two_x.tile.uv()),
        "绘制必须用新变体的 tile"
    );
}

#[test]
fn set_image_resource_accepts_only_live_image_nodes() {
    let mut fixture = Fixture::new();
    let resource = fixture.insert(&png(8, 8));
    let other_resource = fixture.insert(&png(4, 4));
    let image = fixture.mount(|tree, root| {
        NodeBuilder::image(resource.id, ImageFit::Fill)
            .build(tree, root)
            .expect("image")
    });
    let other = fixture.mount(|tree, root| NodeBuilder::boxed().build(tree, root).expect("box"));
    let window = fixture.app.window_mut(fixture.window).expect("window");
    assert!(
        !window
            .set_image_resource(other, other_resource.id)
            .expect("节点存活"),
        "非图片节点必须返回 Ok(false)（与 set_canvas 同型）"
    );
    assert!(
        window
            .set_image_resource(image, other_resource.id)
            .expect("节点存活"),
        "图片节点必须接受新句柄"
    );
    let stale = NodeId::for_test(999, 1);
    assert!(
        matches!(
            window.set_image_resource(stale, resource.id),
            Err(crate::error::EngineError::StaleNode { node }) if node == stale
        ),
        "陈旧句柄必须返回 StaleNode"
    );
}

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
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0)));
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

    let diagnostics = app.app().a11y_diagnostics();
    assert!(
        diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            A11yDiagnostic::MissingAltText {
                node,
                kind: A11yDrawKind::Image
            } if *node == decorative
        )),
        "无 alt 的图片节点必须记 MissingAltText{{kind: Image}}，实际 {diagnostics:?}"
    );
    assert!(
        !diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            A11yDiagnostic::MissingAltText { node, .. } if *node == labelled
        )),
        "有 alt 的图片节点不得记 MissingAltText，实际 {diagnostics:?}"
    );
}

#[test]
fn explicitly_decorative_images_have_no_missing_alt_diagnostic() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let resource = app
        .app_mut()
        .resources_mut()
        .insert(&png(4, 4))
        .expect("insert");
    let window = app.window_id();
    let root = app.app().window(window).expect("window").tree().root();
    let mount = |app: &mut TestApp, over: A11yOverride| {
        NodeBuilder::image(resource.id, ImageFit::Contain)
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0)))
            .a11y(over)
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                root,
            )
            .expect("image")
    };
    let decorative = mount(
        &mut app,
        A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        },
    );
    let unnamed = mount(&mut app, A11yOverride::default());
    let hidden_labelled = mount(
        &mut app,
        A11yOverride {
            hidden: true,
            label: Some(SmolStr::new_static("隐藏但有名字")),
            ..A11yOverride::default()
        },
    );
    let _ = app.drain().expect("frame");

    let diagnostics = app.app().a11y_diagnostics();
    let missing_alt = |node: NodeId| {
        diagnostics.iter().any(|diagnostic| {
            matches!(
                diagnostic,
                A11yDiagnostic::MissingAltText {
                    node: target,
                    kind: A11yDrawKind::Image
                } if *target == node
            )
        })
    };
    assert!(
        !missing_alt(decorative),
        "显式 hidden 的装饰图不得记 MissingAltText，实际 {diagnostics:?}"
    );
    assert!(
        missing_alt(unnamed),
        "没有显式 hidden / label 的图片必须记 MissingAltText，实际 {diagnostics:?}"
    );
    assert!(
        !missing_alt(hidden_labelled),
        "有 label 的图片不得记 MissingAltText，实际 {diagnostics:?}"
    );

    let snapshot = app.a11y().expect("a11y 交付");
    let record = snapshot.node_for(decorative).expect("装饰图节点");
    assert_eq!(record.role, A11yRole::Image);
    assert!(record.hidden, "装饰图仍然 hidden（只是免诊断）");
}

fn svg_asset(side: f32) -> Vec<u8> {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{side}\" height=\"{side}\" \
         viewBox=\"0 0 {side} {side}\"><path d=\"M2 2H14V14H2Z\" fill=\"#FFFFFF\"/></svg>"
    )
    .into_bytes()
}

fn mounted_resource(fixture: &Fixture, node: NodeId) -> crate::resources::ResourceImage {
    let tree = fixture.app.window(fixture.window).expect("window").tree();
    let crate::node::tree::NodeKind::Image(content) = tree.get(node).expect("node").kind() else {
        panic!("expected an image node");
    };
    let id = content.source().resource().expect("ready resource");
    fixture.app.resources().image(id).expect("resource alive")
}

#[test]
fn an_svg_display_variant_is_rerasterized_when_the_scale_changes() {
    let mut fixture = Fixture::new();
    let bytes = svg_asset(16.0);
    let one_x = fixture
        .app
        .resources_mut()
        .insert_svg_for_display(&bytes, LogicalSize::new(px(16.0), px(16.0)), 1.0)
        .expect("1× 变体");
    assert_eq!((one_x.width, one_x.height), (16, 16));
    let node = fixture.mount(|tree, root| {
        NodeBuilder::image(one_x.id, ImageFit::Fill)
            .build(tree, root)
            .expect("image")
    });
    let _ = fixture.committed();
    assert_eq!(
        fixture.bounds(node),
        LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0))
    );

    fixture
        .app
        .window_mut(fixture.window)
        .expect("window")
        .set_scale(DevicePixelRatio::new(2.0).expect("finite scale"));
    let _ = fixture.committed();
    let two_x = mounted_resource(&fixture, node);
    assert_eq!(
        (two_x.width, two_x.height),
        (32, 32),
        "scale 变化必须把节点自动切到新物理尺寸的栅格"
    );
    assert_ne!(two_x.id, one_x.id, "换的是变体，不是同一张栅格");
    assert_eq!(
        fixture.bounds(node),
        LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0)),
        "物理栅格变了，逻辑内在尺寸不变（32 ÷ 2）"
    );
}

#[test]
fn a_bitmap_display_variant_is_rerasterized_when_the_scale_changes() {
    let mut fixture = Fixture::new();
    let bytes = png(80, 40);
    let one_x = fixture
        .app
        .resources_mut()
        .insert_for_display(&bytes, LogicalSize::new(px(40.0), px(20.0)), 1.0)
        .expect("1× 变体");
    let node = fixture.mount(|tree, root| {
        NodeBuilder::image(one_x.id, ImageFit::Fill)
            .build(tree, root)
            .expect("image")
    });
    let _ = fixture.committed();

    fixture
        .app
        .window_mut(fixture.window)
        .expect("window")
        .set_scale(DevicePixelRatio::new(2.0).expect("finite scale"));
    let _ = fixture.committed();
    let two_x = mounted_resource(&fixture, node);
    assert_eq!(
        (two_x.width, two_x.height),
        (80, 40),
        "位图的 for_display 变体同样按新 scale 重解"
    );
    assert_eq!(
        fixture.bounds(node),
        LogicalRect::new(px(0.0), px(0.0), px(40.0), px(20.0))
    );
}
