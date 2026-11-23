use super::App;
#[cfg(windows)]
use crate::render::AtlasArraySpec;
use crate::{
    math::{Rect, Size},
    node::{builder::NodeBuilder, dirty::DirtyFlags},
    paint::{
        atlas::{AtlasKind, AtlasTile},
        canvas::Canvas,
        color::Color,
        primitive::{Image, Primitive},
        style::PaintSpec,
    },
    resources::{DecodeState, ResourceImage},
    window::{WindowId, WindowOptions},
};

fn setup() -> (tokio::runtime::Runtime, App, WindowId) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let mut app = App::new(rt.handle().clone());
    let window = app.create_window(WindowOptions {
        title: "resources".into(),
        size: Size::new(200.0, 120.0),
        scale: crate::math::Scale::ONE,
        ..WindowOptions::default()
    });
    (rt, app, window)
}

fn png() -> Vec<u8> {
    png_rgba([10, 20, 30, 255])
}

fn png_rgba(rgba: [u8; 4]) -> Vec<u8> {
    let pixels: Vec<u8> = std::iter::repeat_n(rgba, 16).flatten().collect();
    let image = image::RgbaImage::from_raw(4, 4, pixels).expect("raw");
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode png");
    out.into_inner()
}

#[cfg(windows)]
fn glyph_spec(page: u32) -> AtlasArraySpec {
    AtlasArraySpec { page, layers: 1 }
}

fn tile_pixel(app: &App, image: crate::resources::ResourceImage) -> [u8; 4] {
    let (page, pixels) = app.resources().page_pixels().expect("image page");
    let page_width = page.0 as usize;
    let origin_x = (image.tile.u0 * page.0 as f32).round() as usize;
    let origin_y = (image.tile.v0 * page.0 as f32).round() as usize;
    let offset = origin_y * page_width * 4 + origin_x * 4;
    pixels[offset..offset + 4].try_into().expect("4 字节像素")
}

#[test]
fn resources_are_process_level() {
    let (_rt, mut app, window) = setup();
    let image = app.resources_mut().insert(&png()).expect("insert");

    assert_eq!(app.resources().len(), 1);
    assert_eq!(
        app.resources().page_pixels().expect("page").0,
        (2048, 2048),
        "图片页固定最大档（P1-2.7：页尺寸与插入顺序无关）"
    );
    let second = app.create_window(WindowOptions {
        title: "second".into(),
        size: Size::new(64.0, 64.0),
        scale: crate::math::Scale::ONE,
        ..WindowOptions::default()
    });
    assert_ne!(second, window);
    assert_eq!(app.resources().len(), 1);

    let (page, pixels) = app.resources().page_pixels().expect("page");
    let page_width = page.0 as usize;
    let origin_x = (image.tile.u0 * page.0 as f32).round() as usize;
    let origin_y = (image.tile.v0 * page.0 as f32).round() as usize;
    let offset = origin_y * page_width * 4 + origin_x * 4;
    assert_eq!(&pixels[offset..offset + 4], &[30, 20, 10, 255]);
}

#[test]
fn insert_does_not_touch_node_dirty_flags() {
    let (_rt, mut app, window) = setup();
    let root = app.window(window).expect("window").tree().root();
    let node = NodeBuilder::boxed()
        .layout_override(Rect::new(0.0, 0.0, 20.0, 20.0))
        .paint(PaintSpec {
            background: crate::paint::color::Color::WHITE,
            ..PaintSpec::default()
        })
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("box");
    let _ = app.frame(window).expect("frame").expect("ok");
    let before = app
        .window(window)
        .expect("window")
        .tree()
        .get(node)
        .expect("node")
        .dirty();

    let _ = app.resources_mut().insert(&png()).expect("insert");

    let after = app
        .window(window)
        .expect("window")
        .tree()
        .get(node)
        .expect("node")
        .dirty();
    assert_eq!(before, after, "插入资源不得改动任何节点的脏位");
}

#[test]
fn atlas_upload_without_renderers_keeps_windows_unsynced() {
    let (_rt, mut app, window) = setup();
    let _ = app.resources_mut().insert(&png()).expect("insert");

    app.upload_atlas_pages();
    app.upload_atlas_pages();
    let target = app.window(window).expect("window");
    assert!(!target.glyph_page_received());
    assert!(!target.image_page_received());
    assert_eq!(app.resources().len(), 1, "上传不影响缓存内容");
}

#[test]
fn atlas_upload_marks_only_windows_with_a_renderer() {
    let (_rt, mut app, first) = setup();
    let second = app.create_window(WindowOptions {
        title: "second".into(),
        size: Size::new(64.0, 64.0),
        scale: crate::math::Scale::ONE,
        ..WindowOptions::default()
    });
    app.upload_atlas_pages();

    for window in [first, second] {
        let target = app.window(window).expect("window");
        assert!(!target.has_renderer());
        assert!(
            !target.image_page_received(),
            "没有渲染器的窗口不得被记账为已同步"
        );
    }
}

#[cfg(windows)]
#[test]
fn atlas_pages_are_broadcast_to_every_window() {
    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, first) = setup();
    app.window_mut(first)
        .expect("window")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    let second = app.create_window(WindowOptions {
        title: "second".into(),
        size: Size::new(64.0, 64.0),
        scale: crate::math::Scale::ONE,
        ..WindowOptions::default()
    });
    app.window_mut(second)
        .expect("window")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");

    let _ = app.resources_mut().insert(&png()).expect("insert");
    app.upload_atlas_pages();

    for window in [first, second] {
        let target = app.window(window).expect("window");
        assert!(target.glyph_page_received(), "字形页必须广播到每个窗口");
        assert!(target.image_page_received(), "图片页必须广播到每个窗口");
    }

    app.upload_atlas_pages();
    for window in [first, second] {
        assert!(app.window(window).expect("window").image_page_received());
    }
}

#[cfg(windows)]
#[test]
fn a_busy_window_queues_its_rects_without_blocking_idle_windows() {
    use crate::{math::PhysicalRect, paint::atlas::AtlasKind, render::DirtyRects};

    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, first) = setup();
    let second = app.create_window(WindowOptions {
        title: "second".into(),
        size: Size::new(64.0, 64.0),
        scale: crate::math::Scale::ONE,
        ..WindowOptions::default()
    });
    for window in [first, second] {
        app.window_mut(window)
            .expect("window")
            .enable_offscreen_renderer()
            .expect("offscreen renderer");
    }
    let page = app.atlas.page().0;
    app.glyph_page_extent = Some(page);
    app.glyph_page = vec![0u8; (page * page) as usize];
    for window in [first, second] {
        app.window_mut(window)
            .expect("window")
            .mark_atlas_page_received(AtlasKind::Glyph, glyph_spec(page));
    }
    let full = DirtyRects::full();
    assert!(
        app.window_mut(second).expect("second").queue_atlas_upload(
            AtlasKind::Glyph,
            0,
            glyph_spec(page),
            &vec![0u8; (page * page) as usize],
            full,
        ),
        "offscreen 渲染器必须接受入队"
    );
    assert!(
        !app.window(second)
            .expect("second")
            .atlas_idle(AtlasKind::Glyph),
        "入队之后、录制之前必须是忙的"
    );

    let frame_dirty = PhysicalRect::new(0, 0, i32::MAX, 4);
    app.pending_dirty.mark(frame_dirty);
    app.upload_atlas_pages();

    assert!(
        app.window(first)
            .expect("first")
            .atlas_rects(AtlasKind::Glyph, 0)
            .is_empty(),
        "空闲窗口的矩形必须已经发出"
    );
    let second_rects = app
        .window(second)
        .expect("second")
        .atlas_rects(AtlasKind::Glyph, 0);
    assert_eq!(second_rects.rects().len(), 1);
    assert_eq!(
        second_rects.rects()[0],
        frame_dirty,
        "忙窗口必须把这一帧的脏矩形攒在自己的队列里"
    );
    assert_eq!(
        second_rects.byte_count(page, 1),
        4 * page as usize,
        "攒住的载荷只覆盖 4 行（页宽 × 4），不是整页"
    );
}

#[test]
fn a_page_change_rebuilds_the_glyph_page_buffer_from_zero() {
    let (_rt, mut app, _window) = setup();
    app.glyph_page = vec![0xFF; 512 * 512];
    app.glyph_page_extent = Some(512);
    assert!(app.atlas.grow());

    app.upload_atlas_pages();

    assert_eq!(app.glyph_page.len(), 1024 * 1024);
    assert!(
        app.glyph_page.iter().all(|byte| *byte == 0),
        "旧页字节不得残留在新页缓冲里"
    );
}

#[cfg(windows)]
#[test]
fn a_deferred_atlas_frame_requests_a_follow_up() {
    use crate::{math::PhysicalRect, paint::atlas::AtlasKind, render::DirtyRects};

    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, window) = setup();
    app.window_mut(window)
        .expect("window")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    let page = app.atlas.page().0;
    app.glyph_page_extent = Some(page);
    app.glyph_page = vec![0u8; (page * page) as usize];
    app.window_mut(window)
        .expect("window")
        .mark_atlas_page_received(AtlasKind::Glyph, glyph_spec(page));
    let full = DirtyRects::full();
    assert!(app.window_mut(window).expect("window").queue_atlas_upload(
        AtlasKind::Glyph,
        0,
        glyph_spec(page),
        &vec![0u8; (page * page) as usize],
        full,
    ));
    app.window_mut(window).expect("window").request_repaint();
    app.pending_dirty.mark(PhysicalRect::new(0, 0, i32::MAX, 4));

    let _ = app.frame(window).expect("frame").expect("ok");
    assert!(
        app.window(window).expect("window").atlas_rects_pending(),
        "忙窗口的图集矩形必须留在队列里"
    );
    assert!(
        app.dispatcher.take_frame_wake(),
        "补帧必须请求一次帧唤醒（否则静态窗口不会重试）"
    );

    let _ = app.frame(window).expect("follow-up").expect("ok");
    assert!(
        !app.window(window).expect("window").atlas_rects_pending(),
        "补帧之后图集矩形必须已经入队"
    );
}

#[cfg(windows)]
#[test]
fn a_late_renderer_receives_the_current_glyph_page() {
    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, window) = setup();
    let root = app.window(window).expect("window").tree().root();
    let _text = NodeBuilder::text("Ag", crate::text::TextStyle::new(14.0))
        .layout_override(Rect::new(0.0, 0.0, 120.0, 40.0))
        .build(app.window_mut(window).expect("window").tree_mut(), root)
        .expect("text");
    let _ = app.frame(window).expect("frame").expect("headless frame");

    app.window_mut(window)
        .expect("window")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    app.upload_atlas_pages();

    assert!(
        app.glyph_page.iter().any(|byte| *byte != 0),
        "晚挂上的渲染器必须拿到当前字形页，而不是空页"
    );
}

#[test]
fn image_atlas_page_is_lazy_until_the_first_insert() {
    let (_rt, mut app, _window) = setup();
    assert!(
        app.resources().page_pixels().is_none(),
        "没有插入图片时不得分配图片页（DEVIATIONS F-208）"
    );
    let _ = app.resources_mut().insert(&png()).expect("insert");
    let (page, pixels) = app.resources().page_pixels().expect("page");
    assert_eq!(
        page,
        (2048, 2048),
        "图片页固定最大档（P1-2.7：页尺寸与插入顺序无关）"
    );
    assert_eq!(pixels.len(), 2048 * 2048 * 4, "整页 = page² × BGRA8");
}

#[cfg(windows)]
#[test]
fn image_pages_upload_to_their_own_texture_array_layers() {
    use crate::{paint::atlas::AtlasKind, render::DirtyRects};

    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, window) = setup();
    app.resources = crate::resources::ResourceCache::with_page_for_test((16, 16));
    for index in 0..3u8 {
        let _ = app
            .resources_mut()
            .insert(&png_rgba([index + 1, 20, 30, 255]))
            .expect("insert");
    }
    assert_eq!(app.resources().page_count(), 2, "第 3 张必须开第 2 页");
    app.window_mut(window)
        .expect("window")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    app.upload_atlas_pages();

    let extent = app.resources().page_extent();
    {
        let target = app.window(window).expect("window");
        assert!(target.image_page_received(), "数组 extent 必须记账");
        assert!(target.image_layer_received(0), "layer 0 首帧整层上传");
        assert!(target.image_layer_received(1), "layer 1 首帧整层上传");
        assert!(
            !target.image_layer_received(2),
            "没有内容的层不得记账为已初始化"
        );
        assert!(!target.atlas_rects_pending(), "首帧上传后不得留待传矩形");
    }
    app.window_mut(window).expect("window").request_repaint();
    let _ = app.frame(window).expect("frame").expect("ok");
    assert!(
        app.window(window)
            .expect("window")
            .atlas_idle(AtlasKind::Image),
        "首帧上传必须已经录制，图片图集槽才能再次入队"
    );

    let page_bytes = (extent as usize) * (extent as usize) * 4;
    let layers = app.resources().page_count() as u32;
    assert!(
        app.window_mut(window).expect("window").queue_atlas_upload(
            AtlasKind::Image,
            0,
            AtlasArraySpec {
                page: extent,
                layers,
            },
            &vec![0u8; page_bytes],
            DirtyRects::full()
        ),
        "offscreen 渲染器必须接受入队"
    );
    assert!(
        !app.window(window)
            .expect("window")
            .atlas_idle(AtlasKind::Image),
        "入队之后、录制之前图片图集必须是忙的"
    );

    let inserted = app
        .resources_mut()
        .insert(&png_rgba([200, 1, 2, 255]))
        .expect("insert");
    assert_eq!(inserted.tile.page, 1, "页 0 已满，新图必须落在 layer 1");
    app.upload_atlas_pages();

    {
        let target = app.window(window).expect("window");
        assert_eq!(
            target.atlas_rects(AtlasKind::Image, 0).rects().len(),
            0,
            "layer 0 没有新写入 ⇒ 不得攒 layer 0 的待传矩形（更不得整页重传）"
        );
        let layer1 = target.atlas_rects(AtlasKind::Image, 1);
        assert_eq!(
            layer1.rects().len(),
            1,
            "layer 1 必须攒下新 tile 的 AOI 矩形"
        );
        assert!(
            layer1.byte_count(extent, 4) < page_bytes,
            "同页增量只传 AOI（{} B < 整页 {page_bytes} B）",
            layer1.byte_count(extent, 4)
        );
    }

    app.window_mut(window).expect("window").request_repaint();
    let _ = app.frame(window).expect("frame").expect("ok");
    app.upload_atlas_pages();
    let target = app.window(window).expect("window");
    assert!(
        !target.atlas_rects_pending(),
        "layer 1 的矩形必须已经入队并清空"
    );
    assert_eq!(target.atlas_rects(AtlasKind::Image, 0).rects().len(), 0);
}

#[cfg(windows)]
#[test]
fn growing_the_image_array_reuploads_every_open_layer() {
    use crate::{paint::atlas::AtlasKind, render::AtlasArraySpec};

    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, window) = setup();
    app.resources = crate::resources::ResourceCache::with_page_for_test((16, 16));
    for index in 0..2u8 {
        let _ = app
            .resources_mut()
            .insert(&png_rgba([index + 1, 20, 30, 255]))
            .expect("insert");
    }
    assert_eq!(app.resources().page_count(), 1);
    app.window_mut(window)
        .expect("window")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");

    app.upload_atlas_pages();
    assert_eq!(
        app.window(window).expect("window").image_array_spec(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 1
        }),
        "1 页 ⇒ 1 层数组"
    );
    assert_eq!(
        app.window(window)
            .expect("window")
            .atlas_pending_layers(AtlasKind::Image),
        Some(vec![0]),
        "首帧只上传已打开的 layer 0"
    );
    app.window_mut(window).expect("window").request_repaint();
    let _ = app.frame(window).expect("frame").expect("ok");

    let _ = app
        .resources_mut()
        .insert(&png_rgba([200, 1, 2, 255]))
        .expect("insert");
    assert_eq!(app.resources().page_count(), 2);
    app.upload_atlas_pages();
    assert_eq!(
        app.window(window).expect("window").image_array_spec(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 2
        }),
        "2 页 ⇒ 数组记账必须前进到 2 层"
    );
    assert_eq!(
        app.window(window)
            .expect("window")
            .atlas_pending_layers(AtlasKind::Image),
        Some(vec![0, 1]),
        "扩层帧必须把 layer 0 也整页重传（增长不能丢已上传的图）"
    );
    assert!(app.window(window).expect("window").image_layer_received(0));
    assert!(app.window(window).expect("window").image_layer_received(1));
}

#[cfg(feature = "test-support")]
#[test]
fn images_on_layers_without_an_established_array_are_not_drawn() {
    let (_rt, mut app, window) = setup();
    let root = app.window(window).expect("窗口存在").tree().root();
    let canvas_node = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 64.0, 32.0)))
        .layout_override(Rect::new(0.0, 0.0, 64.0, 32.0))
        .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
        .expect("canvas 节点");
    let tile = |page: u16| AtlasTile {
        atlas: AtlasKind::Image,
        page,
        u0: 0.25,
        v0: 0.25,
        u1: 0.5,
        v1: 0.5,
        generation: 0,
        slot: 0,
        slot_epoch: 0,
    };
    let mut canvas = Canvas::new(Rect::new(0.0, 0.0, 64.0, 32.0));
    canvas.push_image(Image {
        bounds: Rect::new(0.0, 0.0, 32.0, 32.0),
        tile: tile(0),
        modulation: Color::WHITE,
    });
    canvas.push_image(Image {
        bounds: Rect::new(32.0, 0.0, 32.0, 32.0),
        tile: tile(1),
        modulation: Color::WHITE,
    });
    app.window_mut(window)
        .expect("窗口存在")
        .set_canvas(canvas_node, canvas)
        .expect("节点存活");

    app.window_mut(window)
        .expect("窗口存在")
        .set_image_drawable_layers(1);
    app.window_mut(window).expect("窗口存在").request_repaint();
    let _ = app.frame(window).expect("帧").expect("首帧不报错");
    let buffer = app
        .window(window)
        .expect("窗口存在")
        .instance_buffer()
        .expect("inline 合成器");
    assert_eq!(
        buffer.image_layers(),
        vec![0],
        "层 1 还没有建立 ⇒ 它的 Image 图元不得产生实例；层 0 照画"
    );

    app.window_mut(window)
        .expect("窗口存在")
        .set_image_drawable_layers(2);
    app.window_mut(window).expect("窗口存在").request_repaint();
    let _ = app.frame(window).expect("帧").expect("次帧不报错");
    let buffer = app
        .window(window)
        .expect("窗口存在")
        .instance_buffer()
        .expect("inline 合成器");
    assert_eq!(
        buffer.image_layers(),
        vec![0, 1],
        "数组建立之后两层的图元都必须上屏"
    );
}

#[cfg(windows)]
#[test]
fn a_busy_upload_slot_defers_the_image_array_growth() {
    use crate::{
        paint::atlas::AtlasKind,
        render::{AtlasArraySpec, DirtyRects},
    };

    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, window) = setup();
    app.resources = crate::resources::ResourceCache::with_page_for_test((16, 16));
    for index in 0..2u8 {
        let _ = app
            .resources_mut()
            .insert(&png_rgba([index + 1, 20, 30, 255]))
            .expect("insert");
    }
    assert_eq!(app.resources().page_count(), 1);
    app.window_mut(window)
        .expect("window")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    app.upload_atlas_pages();
    assert_eq!(
        app.window(window).expect("window").image_array_spec(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 1
        })
    );
    app.window_mut(window).expect("window").request_repaint();
    let _ = app.frame(window).expect("frame").expect("ok");

    let _ = app
        .resources_mut()
        .insert(&png_rgba([200, 1, 2, 255]))
        .expect("insert");
    assert_eq!(app.resources().page_count(), 2);
    let page_bytes = 16usize * 16 * 4;
    assert!(
        app.window_mut(window).expect("window").queue_atlas_upload(
            AtlasKind::Image,
            0,
            AtlasArraySpec {
                page: 16,
                layers: 1
            },
            &vec![0u8; page_bytes],
            DirtyRects::full(),
        ),
        "offscreen 渲染器必须接受入队"
    );

    app.upload_atlas_pages();
    let target = app.window(window).expect("window");
    assert_eq!(
        target.image_array_spec(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 1
        }),
        "槽忙 ⇒ 这一帧不得扩层（数组仍是旧 spec）"
    );
    assert_eq!(
        target.image_drawable_layers(),
        1,
        "新层不可画（编码期过滤）、旧层照画"
    );
    assert_eq!(
        target.atlas_pending_layers(AtlasKind::Image),
        Some(vec![0]),
        "不得在扩层前把 layer 1 入队（否则设备侧会拒绝重建）"
    );
    assert!(
        !target.atlas_rects(AtlasKind::Image, 1).is_empty(),
        "新页的写入必须留在该窗口的待传队列里，不能丢"
    );

    app.window_mut(window).expect("window").request_repaint();
    let _ = app.frame(window).expect("frame").expect("ok");
    app.upload_atlas_pages();
    let target = app.window(window).expect("window");
    assert_eq!(
        target.image_array_spec(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 2
        }),
        "槽空闲后的下一帧才扩层"
    );
    assert_eq!(target.image_drawable_layers(), 2);
    assert_eq!(
        target.atlas_pending_layers(AtlasKind::Image),
        Some(vec![0, 1]),
        "扩层帧必须整组重传（layer 0 也要）"
    );
}
#[cfg(windows)]
#[test]
fn growing_the_image_array_waits_until_every_window_can_take_the_whole_array() {
    use crate::{
        paint::atlas::AtlasKind,
        render::{AtlasArraySpec, DirtyRects},
    };

    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, first) = setup();
    app.resources = crate::resources::ResourceCache::with_page_for_test((16, 16));
    for index in 0..2u8 {
        let _ = app
            .resources_mut()
            .insert(&png_rgba([index + 1, 20, 30, 255]))
            .expect("insert");
    }
    assert_eq!(app.resources().page_count(), 1);
    app.window_mut(first)
        .expect("first")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    app.upload_atlas_pages();
    assert_eq!(
        app.window(first).expect("first").image_array_spec(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 1
        })
    );

    let second = app.create_window(WindowOptions {
        title: "second".into(),
        size: Size::new(64.0, 64.0),
        scale: crate::math::Scale::ONE,
        ..WindowOptions::default()
    });
    app.window_mut(second)
        .expect("second")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    app.upload_atlas_pages();
    assert_eq!(
        app.window(second).expect("second").image_array_spec(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 1
        })
    );
    for window in [first, second] {
        app.window_mut(window).expect("window").request_repaint();
        let _ = app.frame(window).expect("frame").expect("ok");
        assert!(
            app.window(window)
                .expect("window")
                .atlas_idle(AtlasKind::Image),
            "录制之后共享槽必须空闲"
        );
    }

    let _ = app
        .resources_mut()
        .insert(&png_rgba([200, 1, 2, 255]))
        .expect("insert");
    assert_eq!(app.resources().page_count(), 2);
    let page_bytes = 16usize * 16 * 4;
    assert!(
        app.window_mut(second).expect("second").queue_atlas_upload(
            AtlasKind::Image,
            0,
            AtlasArraySpec {
                page: 16,
                layers: 1
            },
            &vec![0u8; page_bytes],
            DirtyRects::full(),
        ),
        "offscreen 渲染器必须接受入队"
    );
    assert!(
        !app.window(second)
            .expect("second")
            .atlas_idle(AtlasKind::Image)
    );

    app.upload_atlas_pages();
    for window in [first, second] {
        assert_eq!(
            app.window(window).expect("window").image_array_spec(),
            Some(AtlasArraySpec {
                page: 16,
                layers: 1
            }),
            "任一窗口的槽忙 ⇒ 这一帧不得扩层（不变量 2）"
        );
        assert_eq!(
            app.window(window).expect("window").image_drawable_layers(),
            1,
            "旧层照画、新层不可画（不变量 3 / 1）"
        );
    }

    for window in [second, first] {
        app.window_mut(window).expect("window").request_repaint();
        let _ = app.frame(window).expect("frame").expect("ok");
    }
    app.upload_atlas_pages();
    for window in [first, second] {
        assert_eq!(
            app.window(window).expect("window").image_array_spec(),
            Some(AtlasArraySpec {
                page: 16,
                layers: 2
            }),
            "所有窗口都就绪之后数组才前进到 2 层"
        );
        assert_eq!(
            app.window(window).expect("window").image_drawable_layers(),
            2,
            "扩层完成后新层可画"
        );
    }
    assert_eq!(
        app.window(second)
            .expect("second")
            .atlas_pending_layers(AtlasKind::Image),
        Some(vec![0, 1]),
        "扩层帧必须对每个已打开层整页重传"
    );
}
#[cfg(windows)]
#[test]
fn a_partial_layer_enqueue_does_not_advance_the_image_array() {
    use crate::{paint::atlas::AtlasKind, render::AtlasArraySpec};

    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, first) = setup();
    app.resources = crate::resources::ResourceCache::with_page_for_test((16, 16));
    for index in 0..2u8 {
        let _ = app
            .resources_mut()
            .insert(&png_rgba([index + 1, 20, 30, 255]))
            .expect("insert");
    }
    app.window_mut(first)
        .expect("first")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    app.upload_atlas_pages();
    assert_eq!(
        app.image_array_established_for_test(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 1
        })
    );
    app.window_mut(first).expect("first").request_repaint();
    let _ = app.frame(first).expect("frame").expect("ok");

    let _ = app
        .resources_mut()
        .insert(&png_rgba([200, 1, 2, 255]))
        .expect("insert");
    assert_eq!(app.resources().page_count(), 2);
    app.window_mut(first)
        .expect("first")
        .set_atlas_upload_failure_for_test(Some(1));
    app.upload_atlas_pages();
    assert_eq!(
        app.image_array_established_for_test(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 1
        }),
        "部分层入队失败不得推进 App 记账的图片数组"
    );
    assert_eq!(
        app.window(first).expect("first").image_array_spec(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 1
        }),
        "该窗口仍只有旧层可用"
    );
    assert_eq!(
        app.window(first).expect("first").image_drawable_layers(),
        0,
        "可画层不得抬起（本帧一层都不可画是 fail-closed 的代价）"
    );
    assert_eq!(
        app.window(first)
            .expect("first")
            .atlas_pending_layers(AtlasKind::Image),
        Some(vec![0]),
        "只有第 0 层被入队（部分层），设备侧不应收到完整扩层请求"
    );
    app.upload_atlas_pages();
    assert_eq!(
        app.image_array_established_for_test(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 1
        }),
        "重复遇到部分层入队失败也不得推进"
    );
}

#[cfg(windows)]
#[test]
fn a_new_window_without_an_array_gets_zero_drawable_layers_while_growth_is_blocked() {
    use crate::{
        paint::atlas::AtlasKind,
        render::{AtlasArraySpec, DirtyRects},
    };

    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, first) = setup();
    app.resources = crate::resources::ResourceCache::with_page_for_test((16, 16));
    for index in 0..2u8 {
        let _ = app
            .resources_mut()
            .insert(&png_rgba([index + 1, 20, 30, 255]))
            .expect("insert");
    }
    app.window_mut(first)
        .expect("first")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    app.upload_atlas_pages();
    assert_eq!(
        app.window(first).expect("first").image_array_spec(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 1
        })
    );
    app.window_mut(first).expect("first").request_repaint();
    let _ = app.frame(first).expect("frame").expect("ok");

    let second = app.create_window(WindowOptions {
        title: "second".into(),
        size: Size::new(64.0, 64.0),
        scale: crate::math::Scale::ONE,
        ..WindowOptions::default()
    });
    app.window_mut(second)
        .expect("second")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    assert_eq!(app.window(second).expect("second").image_array_spec(), None);

    let _ = app
        .resources_mut()
        .insert(&png_rgba([200, 1, 2, 255]))
        .expect("insert");
    assert_eq!(app.resources().page_count(), 2);
    let page_bytes = 16usize * 16 * 4;
    assert!(app.window_mut(first).expect("first").queue_atlas_upload(
        AtlasKind::Image,
        0,
        AtlasArraySpec {
            page: 16,
            layers: 1
        },
        &vec![0u8; page_bytes],
        DirtyRects::full(),
    ));
    app.upload_atlas_pages();
    assert_eq!(
        app.window(first).expect("first").image_array_spec(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 1
        }),
        "被挡帧不扩层"
    );
    assert_eq!(
        app.window(first).expect("first").image_drawable_layers(),
        1,
        "已同步窗口按自己的 spec 照画旧层"
    );
    assert_eq!(
        app.window(second).expect("second").image_drawable_layers(),
        0,
        "未同步窗口（数组还是占位）一层都不能画（RBT-09：上限按窗口收敛）"
    );

    app.window_mut(first).expect("first").request_repaint();
    let _ = app.frame(first).expect("frame").expect("ok");
    app.upload_atlas_pages();
    for window in [first, second] {
        assert_eq!(
            app.window(window).expect("window").image_array_spec(),
            Some(AtlasArraySpec {
                page: 16,
                layers: 2
            }),
            "扩层后两个窗口都必须同步到 2 层"
        );
        assert_eq!(
            app.window(window).expect("window").image_drawable_layers(),
            2
        );
    }
}

#[cfg(windows)]
#[test]
fn initial_array_establishment_ignores_participation() {
    use crate::render::AtlasArraySpec;

    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, window) = setup();
    app.resources = crate::resources::ResourceCache::with_page_for_test((16, 16));
    let _ = app
        .resources_mut()
        .insert(&png_rgba([7, 8, 9, 255]))
        .expect("insert");
    app.window_mut(window)
        .expect("window")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    app.window_mut(window)
        .expect("window")
        .set_atlas_participating_for_test(false);

    app.upload_atlas_pages();
    assert_eq!(
        app.window(window).expect("window").image_array_spec(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 1
        }),
        "首次建数组必须先于参与状态：hidden 窗口不能挡住初始化"
    );
    assert_eq!(
        app.window(window).expect("window").image_drawable_layers(),
        1,
        "初始化完成后窗口按已建立层数可画"
    );
}

#[cfg(windows)]
#[test]
fn an_occluded_window_does_not_block_growth_and_resyncs_when_it_returns() {
    use crate::{
        paint::atlas::AtlasKind,
        render::{AtlasArraySpec, DirtyRects},
    };

    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, first) = setup();
    app.resources = crate::resources::ResourceCache::with_page_for_test((16, 16));
    for index in 0..2u8 {
        let _ = app
            .resources_mut()
            .insert(&png_rgba([index + 1, 20, 30, 255]))
            .expect("insert");
    }
    app.window_mut(first)
        .expect("first")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    app.upload_atlas_pages();
    let second = app.create_window(WindowOptions {
        title: "second".into(),
        size: Size::new(64.0, 64.0),
        scale: crate::math::Scale::ONE,
        ..WindowOptions::default()
    });
    app.window_mut(second)
        .expect("second")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    app.upload_atlas_pages();
    for window in [first, second] {
        app.window_mut(window).expect("window").request_repaint();
        let _ = app.frame(window).expect("frame").expect("ok");
    }

    let _ = app
        .resources_mut()
        .insert(&png_rgba([200, 1, 2, 255]))
        .expect("insert");
    let page_bytes = 16usize * 16 * 4;
    assert!(app.window_mut(second).expect("second").queue_atlas_upload(
        AtlasKind::Image,
        0,
        AtlasArraySpec {
            page: 16,
            layers: 1
        },
        &vec![0u8; page_bytes],
        DirtyRects::full(),
    ));
    app.window_mut(second)
        .expect("second")
        .set_atlas_participating_for_test(false);
    app.upload_atlas_pages();
    assert_eq!(
        app.window(first).expect("first").image_array_spec(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 2
        }),
        "不参与渲染的窗口不得挡住其它窗口的扩层（RBT-10）"
    );
    assert_eq!(app.window(first).expect("first").image_drawable_layers(), 2);
    assert_eq!(
        app.window(second).expect("second").image_drawable_layers(),
        1,
        "不参与渲染的窗口保留自己的已建立层数（不清零；恢复后按 received != target 重传）"
    );

    app.window_mut(second)
        .expect("second")
        .set_atlas_participating_for_test(true);
    app.window_mut(second).expect("second").request_repaint();
    let _ = app.frame(second).expect("frame").expect("ok");
    app.upload_atlas_pages();
    let second_window = app.window(second).expect("second");
    assert_eq!(
        second_window.image_array_spec(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 2
        }),
        "恢复后必须整组重传到当前 spec"
    );
    assert_eq!(second_window.image_drawable_layers(), 2);
    assert_eq!(
        second_window.atlas_pending_layers(AtlasKind::Image),
        Some(vec![0, 1]),
        "恢复后的重传必须是每个已打开层的整页上传"
    );
}

#[cfg(windows)]
#[test]
fn growth_degrades_after_a_window_stops_consuming_uploads() {
    use crate::{
        paint::atlas::AtlasKind,
        render::{AtlasArraySpec, DirtyRects},
    };

    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, first) = setup();
    app.resources = crate::resources::ResourceCache::with_page_for_test((16, 16));
    for index in 0..2u8 {
        let _ = app
            .resources_mut()
            .insert(&png_rgba([index + 1, 20, 30, 255]))
            .expect("insert");
    }
    app.window_mut(first)
        .expect("first")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    app.upload_atlas_pages();
    let second = app.create_window(WindowOptions {
        title: "second".into(),
        size: Size::new(64.0, 64.0),
        scale: crate::math::Scale::ONE,
        ..WindowOptions::default()
    });
    app.window_mut(second)
        .expect("second")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    app.upload_atlas_pages();
    for window in [first, second] {
        app.window_mut(window).expect("window").request_repaint();
        let _ = app.frame(window).expect("frame").expect("ok");
    }

    let _ = app
        .resources_mut()
        .insert(&png_rgba([200, 1, 2, 255]))
        .expect("insert");
    let page_bytes = 16usize * 16 * 4;
    assert!(app.window_mut(second).expect("second").queue_atlas_upload(
        AtlasKind::Image,
        0,
        AtlasArraySpec {
            page: 16,
            layers: 1
        },
        &vec![0u8; page_bytes],
        DirtyRects::full(),
    ));

    for _ in 0..7 {
        app.upload_atlas_pages();
        assert_eq!(
            app.window(first).expect("first").image_array_spec(),
            Some(AtlasArraySpec {
                page: 16,
                layers: 1
            }),
            "阈值之前必须保持 fail-closed（不扩层）"
        );
    }
    app.upload_atlas_pages();
    assert_eq!(
        app.window(first).expect("first").image_array_spec(),
        Some(AtlasArraySpec {
            page: 16,
            layers: 2
        }),
        "连续被挡到阈值后必须降级推进（有界兜底）"
    );
    assert_eq!(
        app.window(second).expect("second").image_array_spec(),
        None,
        "降级时长期不消费上传的窗口记账作废（恢复后整组重传）"
    );
    assert_eq!(
        app.window(second).expect("second").image_drawable_layers(),
        0
    );
}

#[cfg(windows)]
#[test]
fn a_new_window_receives_every_open_image_layer() {
    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, first) = setup();
    app.resources = crate::resources::ResourceCache::with_page_for_test((16, 16));
    for index in 0..3u8 {
        let _ = app
            .resources_mut()
            .insert(&png_rgba([index + 10, 40, 50, 255]))
            .expect("insert");
    }
    assert_eq!(app.resources().page_count(), 2);
    app.window_mut(first)
        .expect("first")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    app.upload_atlas_pages();

    let second = app.create_window(WindowOptions {
        title: "second".into(),
        size: Size::new(64.0, 64.0),
        scale: crate::math::Scale::ONE,
        ..WindowOptions::default()
    });
    app.window_mut(second)
        .expect("second")
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    app.upload_atlas_pages();

    let target = app.window(second).expect("second");
    assert!(target.image_page_received(), "新窗口必须收到数组 extent");
    assert!(target.image_layer_received(0), "新窗口必须补 layer 0");
    assert!(target.image_layer_received(1), "新窗口必须补 layer 1");
    assert!(!target.atlas_rects_pending());
    {
        let first = app.window(first).expect("first");
        assert!(
            !first.atlas_rects_pending() && first.image_layer_received(0),
            "已同步窗口不得被新窗口的补齐拖成整页重传"
        );
    }

    app.upload_atlas_pages();
    for window in [first, second] {
        assert!(!app.window(window).expect("window").atlas_rects_pending());
    }
}

#[test]
fn closing_a_window_releases_its_owned_resources() {
    let (_rt, mut app, left) = setup();
    let right = app.create_window(WindowOptions {
        title: "right".into(),
        size: Size::new(64.0, 64.0),
        scale: crate::math::Scale::ONE,
        ..WindowOptions::default()
    });
    let bytes = png();
    let left_image = app
        .resources_mut()
        .insert_owned(left, &bytes)
        .expect("left insert");
    let right_image = app
        .resources_mut()
        .insert_owned(right, &bytes)
        .expect("right insert");
    assert_eq!(left_image, right_image, "同一内容两个窗口共享同一 tile");
    assert_eq!(app.resources().len(), 1, "去重后只有一条条目");
    assert_eq!(app.resources().stats().used_pixels, 16, "4×4 内容区");

    assert!(app.close_window(left), "左窗口存在");
    assert_eq!(app.resources().len(), 1, "右窗口仍持有引用，不得回收");
    let still_alive = app
        .resources_mut()
        .insert(&bytes)
        .expect("共享内容必须仍可去重命中");
    assert_eq!(still_alive, right_image, "右窗口的 tile 不得失效");
    assert_eq!(
        tile_pixel(&app, right_image),
        [30, 20, 10, 255],
        "右窗口 tile 的页内像素必须仍是原内容"
    );
    assert!(app.resources_mut().unload(still_alive.id), "还掉临时引用");

    assert!(app.close_window(right), "右窗口存在");
    assert_eq!(app.resources().len(), 0, "两个窗口关完后条目必须归零");
    let stats = app.resources().stats();
    assert_eq!(stats.used_pixels, 0);
    assert_eq!(
        stats.used_bytes,
        2048 * 2048 * 4,
        "回收后只剩常驻图片页字节（源字节也归还）"
    );
}

#[test]
fn closing_a_window_is_idempotent_with_resources() {
    let (_rt, mut app, left) = setup();
    let right = app.create_window(WindowOptions {
        title: "right".into(),
        size: Size::new(64.0, 64.0),
        scale: crate::math::Scale::ONE,
        ..WindowOptions::default()
    });
    let left_bytes = png_rgba([1, 2, 3, 255]);
    let right_bytes = png_rgba([4, 5, 6, 255]);
    let _left_image = app
        .resources_mut()
        .insert_owned(left, &left_bytes)
        .expect("left insert");
    let right_image = app
        .resources_mut()
        .insert_owned(right, &right_bytes)
        .expect("right insert");
    assert_eq!(app.resources().len(), 2, "两种内容两条条目");

    assert!(app.close_window(left));
    assert_eq!(app.resources().len(), 1, "只释放左窗口的引用");
    assert!(!app.close_window(left), "已关闭窗口的重复 close 返回 false");
    assert_eq!(
        app.resources().len(),
        1,
        "重复 close 不得释放其它窗口的引用"
    );
    let still_alive = app
        .resources_mut()
        .insert(&right_bytes)
        .expect("right content still alive");
    assert_eq!(still_alive, right_image);
    assert_eq!(tile_pixel(&app, right_image), [6, 5, 4, 255]);
    assert!(app.resources_mut().unload(still_alive.id));

    assert!(app.close_window(right));
    assert_eq!(app.resources().len(), 0);
}

fn decode_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("multi-thread runtime")
}

fn wait_until(attempts: usize, mut predicate: impl FnMut() -> bool) -> bool {
    for _ in 0..attempts {
        if predicate() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    false
}

#[test]
fn async_decode_is_delivered_by_the_frame_hook() {
    let rt = decode_runtime();
    let mut app = App::new(rt.handle().clone());
    let _window = app.create_window(WindowOptions::default());
    let pending = app
        .resources_mut()
        .insert_async(bytes::Bytes::from(png_rgba([10, 20, 30, 255])));

    let requests = app.frame_requests();
    let mut delivered = Vec::new();
    assert!(
        wait_until(400, || {
            app.run_frame_hook();
            delivered = app.take_resource_results();
            !delivered.is_empty()
        }),
        "异步解码结果必须在 ≈2s 内由帧钩子交付"
    );
    assert!(
        app.frame_requests() > requests,
        "交付必须有新帧请求（否则应用看不到结果）"
    );
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, pending, "必须带原 PendingResource 句柄");
    assert!(
        matches!(&delivered[0].1, DecodeState::Ready(image) if (image.width, image.height) == (4, 4)),
        "必须交付 Ready(ResourceImage)：{:?}",
        delivered[0].1
    );
    assert_eq!(app.resources().len(), 1, "交付后条目已装箱");
    assert!(app.take_resource_results().is_empty(), "每项只交付一次");
    let requests = app.frame_requests();
    app.run_frame_hook();
    assert!(app.take_resource_results().is_empty());
    assert_eq!(app.frame_requests(), requests, "没有新交付就不会每轮请求帧");
}

#[test]
fn async_decode_wakes_the_pump() {
    let rt = decode_runtime();
    let mut app = App::new(rt.handle().clone());
    let _window = app.create_window(WindowOptions::default());
    let _ = app.dispatcher().take_frame_wake();
    let _pending = app
        .resources_mut()
        .insert_async(bytes::Bytes::from(png_rgba([1, 2, 3, 255])));

    assert!(
        wait_until(400, || app.dispatcher().take_frame_wake()),
        "解码完成必须唤醒泵（Dispatcher 电平位）"
    );
    let mut delivered = Vec::new();
    assert!(wait_until(200, || {
        app.run_frame_hook();
        delivered = app.take_resource_results();
        !delivered.is_empty()
    }));
    assert_eq!(delivered.len(), 1);
}

#[test]
fn set_canvas_replaces_canvas_content_and_marks_subtree_paint() {
    let (_rt, mut app, window) = setup();
    let root = app.window(window).expect("窗口存在").tree().root();
    let node = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 32.0, 32.0)))
        .layout_override(Rect::new(0.0, 0.0, 32.0, 32.0))
        .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
        .expect("canvas 节点");
    let _ = app.frame(window).expect("帧").expect("首帧不报错");

    let tile = AtlasTile {
        atlas: AtlasKind::Image,
        page: 0,
        u0: 0.0,
        v0: 0.0,
        u1: 0.25,
        v1: 0.25,
        generation: 1,
        slot: 0,
        slot_epoch: 0,
    };
    let mut canvas = Canvas::new(Rect::new(0.0, 0.0, 32.0, 32.0));
    canvas.push_image(Image {
        bounds: Rect::new(0.0, 0.0, 32.0, 32.0),
        tile,
        modulation: Color::WHITE,
    });
    let replaced = app
        .window_mut(window)
        .expect("窗口存在")
        .set_canvas(node, canvas)
        .expect("节点存活");
    assert!(replaced, "Canvas 节点必须接受替换");
    let dirty = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .get(node)
        .expect("节点")
        .dirty();
    assert!(
        dirty.contains(DirtyFlags::PAINT),
        "替换画布必须标 PAINT（重建 lane）：{dirty:?}"
    );

    let _ = app.frame(window).expect("帧").expect("次帧不报错");
    let target = app.window(window).expect("窗口存在");
    let lane = target.tree().lane_of(node).expect("lane");
    let primitives = target
        .scene()
        .lane_primitives(lane)
        .expect("lane primitives");
    assert!(
        primitives.iter().any(|primitive| matches!(
            primitive,
            Primitive::Canvas(canvas)
                if canvas
                    .primitives()
                    .iter()
                    .any(|inner| matches!(inner, Primitive::Image(image) if image.tile == tile)),
        )),
        "新画布的 Image 图元必须上屏：{primitives:?}"
    );
}

#[test]
fn set_canvas_rejects_non_canvas_nodes_and_stale_handles() {
    let (_rt, mut app, window) = setup();
    let root = app.window(window).expect("窗口存在").tree().root();
    let text = NodeBuilder::text("not a canvas", crate::text::TextStyle::new(16.0))
        .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
        .expect("text 节点");
    assert!(
        !app.window_mut(window)
            .expect("窗口存在")
            .set_canvas(text, Canvas::new(Rect::new(0.0, 0.0, 8.0, 8.0)))
            .expect("节点存活"),
        "非 Canvas 节点必须返回 Ok(false) 且不改状态"
    );

    let canvas = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 16.0, 16.0)))
        .layout_override(Rect::new(0.0, 0.0, 16.0, 16.0))
        .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
        .expect("canvas 节点");
    app.window_mut(window)
        .expect("窗口存在")
        .tree_mut()
        .remove(canvas)
        .expect("移除成功");
    let error = app
        .window_mut(window)
        .expect("窗口存在")
        .set_canvas(canvas, Canvas::new(Rect::new(0.0, 0.0, 8.0, 8.0)))
        .expect_err("陈旧节点必须报错");
    assert!(
        matches!(error, crate::error::EngineError::StaleNode { .. }),
        "{error:?}"
    );
}

#[test]
fn async_ready_tile_can_be_applied_to_an_existing_node() {
    let rt = decode_runtime();
    let mut app = App::new(rt.handle().clone());
    let window = app.create_window(WindowOptions::default());
    let root = app.window(window).expect("窗口存在").tree().root();
    let node = NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 32.0, 32.0)))
        .layout_override(Rect::new(0.0, 0.0, 32.0, 32.0))
        .build(app.window_mut(window).expect("窗口存在").tree_mut(), root)
        .expect("canvas 节点");
    let _ = app.frame(window).expect("帧").expect("首帧不报错");

    let _pending = app
        .resources_mut()
        .insert_async(bytes::Bytes::from(png_rgba([40, 50, 60, 255])));
    let mut image: Option<ResourceImage> = None;
    assert!(wait_until(400, || {
        app.run_frame_hook();
        for (_pending, state) in app.take_resource_results() {
            if let DecodeState::Ready(ready) = state {
                image = Some(ready);
            }
        }
        image.is_some()
    }));
    let image = image.expect("必须交付 Ready");
    assert_eq!((image.width, image.height), (4, 4));

    let mut canvas = Canvas::new(Rect::new(0.0, 0.0, 32.0, 32.0));
    canvas.push_image(Image {
        bounds: Rect::new(0.0, 0.0, 4.0, 4.0),
        tile: image.tile,
        modulation: Color::WHITE,
    });
    assert!(
        app.window_mut(window)
            .expect("窗口存在")
            .set_canvas(node, canvas)
            .expect("节点存活")
    );
    let _ = app.frame(window).expect("帧").expect("次帧不报错");
    let target = app.window(window).expect("窗口存在");
    let lane = target.tree().lane_of(node).expect("lane");
    let primitives = target.scene().lane_primitives(lane).expect("prims");
    assert!(
        primitives.iter().any(|primitive| match primitive {
            Primitive::Canvas(canvas) => canvas.primitives().iter().any(
                |inner| matches!(inner, Primitive::Image(primed) if primed.tile == image.tile)
            ),
            _ => false,
        }),
        "Ready tile 必须被 set_canvas 应用到节点并上屏：{primitives:?}"
    );
}
