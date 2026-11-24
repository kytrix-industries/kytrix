use std::time::{Duration, Instant};

use bytes::Bytes;
use smol_str::SmolStr;

use crate::{
    a11y::{A11yDiagnostic, A11yDrawKind, A11yOverride, A11yRole},
    app::App,
    error::EngineError,
    math::{DevicePixelRatio, DeviceSize, LogicalRect, LogicalSize, dp, px},
    node::{builder::NodeBuilder, id::NodeId, image::ImageFit, tree::NodeTree},
    paint::{
        atlas::AtlasTile,
        color::{Color, ColorResolver},
        style::{PaintSpec, Style},
        style_builder::StyleBuilder,
    },
    resources::ResourceImage,
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
        self.app.resources.insert(bytes).expect("insert")
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

fn native_tile(fixture: &Fixture, image: ResourceImage) -> AtlasTile {
    image
        .variant_for(fixture.app.resources(), DeviceSize::new(dp(4096), dp(4096)))
        .expect("位图的原生变体恒可用")
}

fn tile_for(fixture: &Fixture, image: ResourceImage, width: i32, height: i32) -> AtlasTile {
    image
        .variant_for(
            fixture.app.resources(),
            DeviceSize::new(dp(width), dp(height)),
        )
        .expect("目标物理尺寸的变体必须存在")
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
        NodeBuilder::image(resource.id(), ImageFit::Fill)
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
        Some(native_tile(&fixture, resource).uv()),
        "Fill 不裁源：UV 就是原生 tile 的完整 UV"
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
        NodeBuilder::image(resource.id(), ImageFit::Fill)
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
    let tile = native_tile(&fixture, resource);
    let build = |fit: ImageFit| {
        move |tree: &mut NodeTree, root: NodeId| {
            NodeBuilder::image(resource.id(), fit)
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
    assert_eq!(
        contain_image.image_uv,
        Some(tile_for(&fixture, resource, 16, 8).uv()),
        "Contain 不裁源（使用目标物理尺寸档位的 LOD tile 的完整 UV）"
    );

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
        NodeBuilder::image(resource.id(), ImageFit::Fill)
            .image_modulation(Color::BLACK)
            .style(fixed(8.0, 8.0))
            .build(tree, root)
            .expect("image")
    });
    let dynamic_node = fixture.mount(|tree, root| {
        NodeBuilder::image(resource.id(), ImageFit::Fill)
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
    let pending = fixture.app.resources.insert_async(Bytes::from(png(40, 20)));
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
        NodeBuilder::image(resource.id(), ImageFit::Fill)
            .paint(white())
            .build(tree, root)
            .expect("image")
    });
    let _ = fixture.committed();

    assert!(fixture.app.unload_resource(resource.id()), "资源存在");
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
        ..WindowOptions::default()
    });
    let bytes = png(16, 16);
    let resource = fixture
        .app
        .resources
        .insert_owned(owner, &bytes)
        .expect("insert owned");

    let node = fixture.mount(|tree, root| {
        NodeBuilder::image(resource.id(), ImageFit::Contain)
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

fn svg_asset(side: f32) -> Vec<u8> {
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{side}\" height=\"{side}\" \
         viewBox=\"0 0 {side} {side}\"><path d=\"M2 2H14V14H2Z\" fill=\"#FFFFFF\"/></svg>"
    )
    .into_bytes()
}

fn mounted_resource(fixture: &Fixture, node: NodeId) -> ResourceImage {
    let tree = fixture.app.window(fixture.window).expect("window").tree();
    let crate::node::tree::NodeKind::Image(content) = tree.get(node).expect("node").kind() else {
        panic!("expected an image node");
    };
    let id = content.source().resource().expect("ready resource");
    fixture.app.resources().image(id).expect("resource alive")
}

fn variant_targets(fixture: &Fixture, image: ResourceImage) -> Vec<DeviceSize> {
    fixture.app.resources().variant_targets(image.id())
}

fn tile_at(fixture: &Fixture, image: ResourceImage, side: i32) -> AtlasTile {
    image
        .variant_for(fixture.app.resources(), DeviceSize::new(dp(side), dp(side)))
        .expect("目标物理尺寸的变体必须存在")
}

fn tile_pixel(fixture: &Fixture, tile: AtlasTile) -> [u8; 4] {
    let (page, pixels) = fixture.app.resources().page_pixels().expect("image page");
    let page_width = page.0 as usize;
    let origin_x = (tile.u0 * page.0 as f32).round() as usize;
    let origin_y = (tile.v0 * page.0 as f32).round() as usize;
    let offset = origin_y * page_width * 4 + origin_x * 4;
    pixels[offset..offset + 4].try_into().expect("4 字节像素")
}

fn scaled(fixture: &mut Fixture, factor: f32) -> DevicePixelRatio {
    let dpr = DevicePixelRatio::new(factor).expect("finite scale");
    fixture
        .app
        .window_mut(fixture.window)
        .expect("window")
        .set_device_pixel_ratio(dpr);
    dpr
}

#[test]
fn bitmap_dpi_change_changes_only_the_intrinsic_size() {
    let mut fixture = Fixture::new();
    let resource = fixture.insert(&png(80, 40));
    let node = fixture.mount(|tree, root| {
        NodeBuilder::image(resource.id(), ImageFit::Fill)
            .build(tree, root)
            .expect("image")
    });
    let _ = fixture.committed();
    assert_eq!(
        fixture.bounds(node),
        LogicalRect::new(px(0.0), px(0.0), px(80.0), px(40.0))
    );
    let tile = native_tile(&fixture, resource);
    let pixel = tile_pixel(&fixture, tile);
    assert_eq!(
        variant_targets(&fixture, resource),
        vec![
            DeviceSize::new(dp(80), dp(40)),
            DeviceSize::new(dp(40), dp(20)),
            DeviceSize::new(dp(20), dp(10))
        ],
        "位图变体 = 原生 + 1/2 + 1/4"
    );

    let dpr = scaled(&mut fixture, 2.0);
    let _ = fixture.committed();
    assert_eq!(
        fixture.bounds(node),
        LogicalRect::new(px(0.0), px(0.0), px(40.0), px(20.0)),
        "DPI 变化改的是内在逻辑尺寸（原生物理像素 ÷ DPR）"
    );
    let after = mounted_resource(&fixture, node);
    assert_eq!(after.id(), resource.id(), "资源身份在 DPI 变化后保持稳定");
    assert_eq!(after.intrinsic(dpr), LogicalSize::new(px(40.0), px(20.0)));
    assert_eq!(
        native_tile(&fixture, after),
        tile,
        "原生 tile 不得因 DPI 变化而重解"
    );
    assert_eq!(tile_pixel(&fixture, tile), pixel, "原生像素逐字节不变");
    assert_eq!(
        variant_targets(&fixture, after),
        vec![
            DeviceSize::new(dp(80), dp(40)),
            DeviceSize::new(dp(40), dp(20)),
            DeviceSize::new(dp(20), dp(10))
        ],
        "DPI 变化不得给位图新增变体"
    );
}

#[test]
fn bitmap_cover_crops_the_native_tile() {
    let mut fixture = Fixture::new();
    let resource = fixture.insert(&png(40, 20));
    let tile = native_tile(&fixture, resource);
    let node = fixture.mount(|tree, root| {
        NodeBuilder::image(resource.id(), ImageFit::Cover)
            .style(fixed(16.0, 16.0))
            .build(tree, root)
            .expect("image")
    });
    let record = fixture.committed();
    let painted = image(&record, node);
    assert_eq!(
        painted.bounds,
        LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0)),
        "Cover 的图元 bounds 恒等于节点盒"
    );
    let (u0, v0, u1, v1) = (tile.u0, tile.v0, tile.u1, tile.v1);
    let uv = painted.image_uv.expect("image uv");
    assert!(
        (uv[0] - (u0 + (u1 - u0) * 0.25)).abs() < 1e-6,
        "Cover 必须裁原生 tile 的 UV：{uv:?}"
    );
    assert!(
        (uv[2] - (u1 - (u1 - u0) * 0.25)).abs() < 1e-6,
        "Cover 必须裁原生 tile 的 UV：{uv:?}"
    );
    assert!((uv[1] - v0).abs() < 1e-6 && (uv[3] - v1).abs() < 1e-6);
    assert_eq!(
        variant_targets(&fixture, resource),
        vec![
            DeviceSize::new(dp(40), dp(20)),
            DeviceSize::new(dp(20), dp(10)),
            DeviceSize::new(dp(10), dp(5))
        ],
        "Cover 不得在资源层按绘制目标下采样位图（只有固定 LOD）"
    );
}

#[test]
fn svg_variants_follow_the_draw_box_at_each_dpr() {
    let mut fixture = Fixture::new();
    let intrinsic = LogicalSize::new(px(16.0), px(16.0));
    let svg = fixture
        .app
        .insert_svg_for_window(fixture.window, &svg_asset(16.0), intrinsic)
        .expect("1x svg");
    assert_eq!(svg.intrinsic(DevicePixelRatio::ONE), intrinsic);
    let node = fixture.mount(|tree, root| {
        NodeBuilder::image(svg.id(), ImageFit::Fill)
            .build(tree, root)
            .expect("image")
    });
    let _ = fixture.committed();
    assert_eq!(
        fixture.bounds(node),
        LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0))
    );
    assert_eq!(
        variant_targets(&fixture, svg),
        vec![DeviceSize::new(dp(16), dp(16))]
    );
    let one_x = tile_at(&fixture, svg, 16);

    let dpr = scaled(&mut fixture, 1.5);
    let record = fixture.committed();
    assert_eq!(svg.intrinsic(dpr), intrinsic, "SVG 内在逻辑尺寸不随 DPI 变");
    assert_eq!(
        fixture.bounds(node),
        LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0)),
        "SVG intrinsic 不变 ⇒ DPI 变化不重排"
    );
    assert_eq!(
        variant_targets(&fixture, svg),
        vec![
            DeviceSize::new(dp(16), dp(16)),
            DeviceSize::new(dp(24), dp(24))
        ],
        "1.5× 必须按绘制盒生成 24×24 物理变体"
    );
    let one_and_half = tile_at(&fixture, svg, 24);
    assert_ne!(one_and_half, one_x, "不同物理尺寸必须是不同栅格");
    assert_eq!(
        image(&record, node).image_uv,
        Some(one_and_half.uv()),
        "绘制必须用 1.5× 的新变体 tile"
    );

    scaled(&mut fixture, 2.0);
    let record = fixture.committed();
    assert_eq!(
        variant_targets(&fixture, svg),
        vec![
            DeviceSize::new(dp(16), dp(16)),
            DeviceSize::new(dp(24), dp(24)),
            DeviceSize::new(dp(32), dp(32))
        ],
        "2× 必须生成 32×32 物理变体"
    );
    let two_x = tile_at(&fixture, svg, 32);
    assert_ne!(two_x, one_x);
    assert_ne!(two_x, one_and_half);
    assert_eq!(
        image(&record, node).image_uv,
        Some(two_x.uv()),
        "绘制必须用 2× 的新变体 tile"
    );

    scaled(&mut fixture, 1.0);
    let record = fixture.committed();
    assert_eq!(
        variant_targets(&fixture, svg).len(),
        3,
        "切回 1× 必须复用已有变体而不是重解"
    );
    assert_eq!(tile_at(&fixture, svg, 16), one_x);
    assert_eq!(
        image(&record, node).image_uv,
        Some(one_x.uv()),
        "切回 1× 后必须画回 1× 变体"
    );
}

#[test]
fn svg_variants_are_quantized_and_reused() {
    let mut fixture = Fixture::new();
    let intrinsic = LogicalSize::new(px(16.0), px(16.0));
    let svg = fixture
        .app
        .insert_svg_for_window(fixture.window, &svg_asset(16.0), intrinsic)
        .expect("svg");
    let _node = fixture.mount(|tree, root| {
        NodeBuilder::image(svg.id(), ImageFit::Fill)
            .build(tree, root)
            .expect("image")
    });
    let _ = fixture.committed();
    scaled(&mut fixture, 1.25);
    let _ = fixture.committed();
    assert_eq!(
        variant_targets(&fixture, svg),
        vec![
            DeviceSize::new(dp(16), dp(16)),
            DeviceSize::new(dp(20), dp(20))
        ],
        "1.25× × 16 = 20 物理像素（量化后仍为 20）"
    );
    let quantized = tile_at(&fixture, svg, 20);

    scaled(&mut fixture, 1.5);
    let _ = fixture.committed();
    assert_eq!(variant_targets(&fixture, svg).len(), 3);

    scaled(&mut fixture, 1.25);
    let _ = fixture.committed();
    assert_eq!(
        variant_targets(&fixture, svg).len(),
        3,
        "回到 1.25× 必须复用 20×20 变体"
    );
    assert_eq!(tile_at(&fixture, svg, 20), quantized);
}

#[test]
fn async_bitmap_resources_are_owned_by_the_asking_window() {
    let mut fixture = Fixture::new();
    let pending = fixture
        .app
        .insert_bitmap_async_for_window(fixture.window, Bytes::from(png(16, 16)))
        .expect("async bitmap");
    let delivered = drive_until_delivered(&mut fixture);
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, pending);
    assert!(
        matches!(delivered[0].1, crate::resources::DecodeState::Ready(_)),
        "异步位图必须解码成功：{:?}",
        delivered[0].1
    );
    assert_eq!(fixture.app.resources().len(), 1);
    assert!(fixture.app.close_window(fixture.window));
    assert_eq!(
        fixture.app.resources().len(),
        0,
        "关窗必须回收异步插入的 owner 引用"
    );
}

#[test]
fn async_svg_resources_deliver_the_intrinsic_and_register_the_owner() {
    let mut fixture = Fixture::new();
    let dpr = scaled(&mut fixture, 2.0);
    let intrinsic = LogicalSize::new(px(16.0), px(16.0));
    let pending = fixture
        .app
        .insert_svg_async_for_window(fixture.window, Bytes::from(svg_asset(16.0)), intrinsic)
        .expect("async svg");
    let delivered = drive_until_delivered(&mut fixture);
    let state = delivered
        .into_iter()
        .find_map(|(handle, state)| (handle == pending).then_some(state))
        .expect("异步 SVG 必须交付");
    let crate::resources::DecodeState::Ready(image) = state else {
        panic!("异步 SVG 必须成功：{state:?}");
    };
    assert_eq!(
        image.intrinsic(dpr),
        intrinsic,
        "SVG intrinsic 由调用方给出"
    );
    assert!(
        matches!(
            image.kind(),
            crate::resources::ImageResourceKind::Vector { .. }
        ),
        "SVG 资源必须是矢量来源"
    );
    assert_eq!(fixture.app.resources().len(), 1);
    assert!(fixture.app.close_window(fixture.window));
    assert_eq!(fixture.app.resources().len(), 0);
}

#[test]
fn svg_insert_validates_the_intrinsic_and_the_bytes() {
    let mut fixture = Fixture::new();
    let error = fixture
        .app
        .insert_svg_for_window(
            fixture.window,
            &svg_asset(8.0),
            LogicalSize::new(px(0.0), px(16.0)),
        )
        .expect_err("零内在尺寸必须拒绝");
    assert!(
        matches!(error, EngineError::InvalidImageIntrinsic { .. }),
        "{error:?}"
    );

    let error = fixture
        .app
        .insert_svg_for_window(fixture.window, b"<svg", LogicalSize::new(px(8.0), px(8.0)))
        .expect_err("损坏的 SVG 字节必须拒绝");
    assert!(
        matches!(
            error,
            EngineError::ResourceDecode {
                cause: crate::resources::DecodeCause::Corrupt { ref format, .. }
            } if format == "svg"
        ),
        "{error:?}"
    );
    assert_eq!(fixture.app.resources().len(), 0, "失败不得留下资源");
}

#[test]
fn window_resource_apis_reject_unknown_windows() {
    let mut fixture = Fixture::new();
    let missing = WindowId::for_test(999);
    let intrinsic = LogicalSize::new(px(8.0), px(8.0));
    let bitmap = png(4, 4);
    assert!(matches!(
        fixture.app.insert_bitmap_for_window(missing, &bitmap),
        Err(EngineError::UnknownWindow { window }) if window == missing
    ));
    assert!(matches!(
        fixture
            .app
            .insert_svg_for_window(missing, &svg_asset(8.0), intrinsic),
        Err(EngineError::UnknownWindow { window }) if window == missing
    ));
    assert!(matches!(
        fixture
            .app
            .insert_bitmap_async_for_window(missing, Bytes::from(bitmap)),
        Err(EngineError::UnknownWindow { window }) if window == missing
    ));
    assert!(matches!(
        fixture
            .app
            .insert_svg_async_for_window(missing, Bytes::from(svg_asset(8.0)), intrinsic),
        Err(EngineError::UnknownWindow { window }) if window == missing
    ));
    assert_eq!(fixture.app.resources().len(), 0, "未知窗口不得留下资源");
}

#[test]
fn synchronous_resource_inserts_are_owned_by_the_window() {
    let mut fixture = Fixture::new();
    let intrinsic = LogicalSize::new(px(16.0), px(16.0));
    fixture
        .app
        .insert_bitmap_for_window(fixture.window, &png(16, 16))
        .expect("bitmap");
    fixture
        .app
        .insert_svg_for_window(fixture.window, &svg_asset(16.0), intrinsic)
        .expect("svg");
    assert_eq!(fixture.app.resources().len(), 2);

    assert!(fixture.app.close_window(fixture.window));
    assert_eq!(
        fixture.app.resources().len(),
        0,
        "关窗必须归还同步资源的 owner 引用"
    );
}

#[test]
fn set_image_resource_accepts_only_live_image_nodes() {
    let mut fixture = Fixture::new();
    let resource = fixture.insert(&png(8, 8));
    let other_resource = fixture.insert(&png(4, 4));
    let image = fixture.mount(|tree, root| {
        NodeBuilder::image(resource.id(), ImageFit::Fill)
            .build(tree, root)
            .expect("image")
    });
    let other = fixture.mount(|tree, root| NodeBuilder::boxed().build(tree, root).expect("box"));
    let window = fixture.app.window_mut(fixture.window).expect("window");
    assert!(
        !window
            .set_image_resource(other, other_resource.id())
            .expect("节点存活"),
        "非图片节点必须返回 Ok(false)（与 set_canvas 同型）"
    );
    assert!(
        window
            .set_image_resource(image, other_resource.id())
            .expect("节点存活"),
        "图片节点必须接受新句柄"
    );
    let stale = NodeId::for_test(999, 1);
    assert!(
        matches!(
            window.set_image_resource(stale, resource.id()),
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
        NodeBuilder::image(resource.id(), ImageFit::Fill)
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
    let resource = app.app_mut().resources.insert(&png(4, 4)).expect("insert");
    let window = app.window_id();
    let root = app.app().window(window).expect("window").tree().root();
    let mount = |app: &mut TestApp, label: Option<&'static str>| {
        let mut builder = NodeBuilder::image(resource.id(), ImageFit::Contain)
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(16.0), px(16.0)));
        if let Some(label) = label {
            builder = builder.a11y(A11yOverride {
                label: Some(smol_str::SmolStr::new_static(label)),
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
}

#[test]
fn explicitly_decorative_images_have_no_missing_alt_diagnostic() {
    let mut app = TestApp::new();
    app.enable_a11y_capture();
    let resource = app.app_mut().resources.insert(&png(4, 4)).expect("insert");
    let window = app.window_id();
    let root = app.app().window(window).expect("window").tree().root();
    let mount = |app: &mut TestApp, over: A11yOverride| {
        NodeBuilder::image(resource.id(), ImageFit::Contain)
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
            label: Some(smol_str::SmolStr::new_static("隐藏但有名字")),
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

#[test]
fn failed_svg_variant_is_reported_once_and_falls_back() {
    let mut fixture = Fixture::new();
    fixture.app.resources = crate::resources::ResourceCache::with_page_for_test((16, 16));
    let intrinsic = LogicalSize::new(px(16.0), px(16.0));
    let svg = fixture
        .app
        .insert_svg_for_window(fixture.window, &svg_asset(16.0), intrinsic)
        .expect("svg");
    let failing = fixture.mount(|tree, root| {
        NodeBuilder::image(svg.id(), ImageFit::Fill)
            .build(tree, root)
            .expect("image")
    });
    let first = fixture.frame();
    assert!(images(&first, failing).is_empty(), "16×16 放不进 16 页");
    let diagnostics = fixture.app.image_diagnostics();
    assert_eq!(diagnostics.len(), 1, "失败必须记录一次性诊断");
    assert_eq!(diagnostics[0].resource(), svg.id());
    assert!(matches!(
        diagnostics[0],
        crate::resources::ImageDiagnostic::VariantUnavailable {
            cause: crate::resources::DecodeCause::Capacity {
                cause: crate::resources::CapacityCause::PageFull,
                ..
            },
            ..
        }
    ));
    let second = fixture.frame();
    assert!(images(&second, failing).is_empty());
    assert_eq!(
        fixture.app.image_diagnostics().len(),
        1,
        "同一失败不得每帧重复记录"
    );

    let fallback = fixture.mount(|tree, root| {
        NodeBuilder::image(svg.id(), ImageFit::Fill)
            .style(fixed(8.0, 8.0))
            .build(tree, root)
            .expect("image")
    });
    let record = fixture.committed();
    assert!(
        fixture.app.image_diagnostics().is_empty(),
        "变体生成恢复后必须清掉诊断"
    );
    assert_eq!(
        variant_targets(&fixture, svg),
        vec![DeviceSize::new(dp(8), dp(8))],
        "只有放得下的 8×8 变体"
    );
    assert_eq!(
        image(&record, failing).image_uv,
        Some(tile_at(&fixture, svg, 8).uv()),
        "生成失败的节点回退到最近的可用变体（≤ 目标）"
    );
    assert_eq!(
        image(&record, fallback).image_uv,
        Some(tile_at(&fixture, svg, 8).uv())
    );
}

#[test]
fn prepare_image_variant_serves_canvas_and_inline_images() {
    let mut fixture = Fixture::new();
    let intrinsic = LogicalSize::new(px(16.0), px(16.0));
    let svg = fixture
        .app
        .insert_svg_for_window(fixture.window, &svg_asset(16.0), intrinsic)
        .expect("svg");
    let target = LogicalSize::new(px(20.0), px(20.0));
    let tile = fixture
        .app
        .prepare_image_variant(fixture.window, svg, target, ImageFit::Fill)
        .expect("20×20 变体");
    assert_eq!(
        variant_targets(&fixture, svg),
        vec![DeviceSize::new(dp(20), dp(20))]
    );
    assert_eq!(
        fixture
            .app
            .prepare_image_variant(fixture.window, svg, target, ImageFit::Fill),
        Some(tile),
        "同一目标必须复用同一变体"
    );
    let cover = fixture
        .app
        .prepare_image_variant(
            fixture.window,
            svg,
            LogicalSize::new(px(20.0), px(10.0)),
            ImageFit::Contain,
        )
        .expect("Contain 目标变体");
    assert_ne!(cover, tile, "不同目标物理尺寸必须是不同变体");
    assert_eq!(
        fixture
            .app
            .prepare_image_variant(WindowId::for_test(999), svg, target, ImageFit::Fill),
        None,
        "未知窗口返回 None"
    );
    assert!(fixture.app.close_window(fixture.window));
    assert_eq!(
        fixture
            .app
            .prepare_image_variant(fixture.window, svg, target, ImageFit::Fill),
        None,
        "关窗后资源已回收"
    );
}
