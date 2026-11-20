use super::App;
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
    render::AtlasArraySpec,
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

/// 4×4 的不透明 PNG（解码后逐字节可预测：BGRA = 源 RGB 交换）。
fn png() -> Vec<u8> {
    png_rgba([10, 20, 30, 255])
}

/// 指定颜色的 4×4 不透明 PNG。
fn png_rgba(rgba: [u8; 4]) -> Vec<u8> {
    let pixels: Vec<u8> = std::iter::repeat_n(rgba, 16).flatten().collect();
    let image = image::RgbaImage::from_raw(4, 4, pixels).expect("raw");
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode png");
    out.into_inner()
}

/// 字形图集的 array 规格（恒 1 层）。
fn glyph_spec(page: u32) -> AtlasArraySpec {
    AtlasArraySpec { page, layers: 1 }
}

/// 图集页内某个 tile 内容区左上角的 BGRA 字节。
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
    // 另一窗口共享同一份缓存（单 GPU 设备、进程级一份）。
    let second = app.create_window(WindowOptions {
        title: "second".into(),
        size: Size::new(64.0, 64.0),
        scale: crate::math::Scale::ONE,
        ..WindowOptions::default()
    });
    assert_ne!(second, window);
    assert_eq!(app.resources().len(), 1);

    // 内容区写进了页缓冲（BGR 顺序）。
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

    // headless：没有 renderer ⇒ 不上传，窗口保持「未收到整页」（换渲染器后会自动补整页）。
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

/// 评审阻塞 ②：`frame_all` 逐窗提交，图集页必须**广播**给每个窗口的渲染器，
/// 否则第一个窗口消费脏行后，其余窗口的纹理永远收不到内容。
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

    // 第二次调用：没有新写入 ⇒ 不重复上传（脏旗标已被统一消费）。
    app.upload_atlas_pages();
    for window in [first, second] {
        assert!(app.window(window).expect("window").image_page_received());
    }
}

/// 忙窗口不能拖住其它窗口：它的共享槽还没录制完时，本帧脏行攒在它自己的队列里，
/// 空闲窗口照常收到增量（DEVIATIONS F-211 的 BLOCKER 回归面）。
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
    // 图集缓冲已经按这一页尺寸建好（真实路径由上一帧的上传完成）。
    app.glyph_page_extent = Some(page);
    app.glyph_page = vec![0u8; (page * page) as usize];
    for window in [first, second] {
        app.window_mut(window)
            .expect("window")
            .mark_atlas_page_received(AtlasKind::Glyph, glyph_spec(page));
    }
    // 让 second 的共享槽停在「已入队、未录制」：这就是忙窗口。
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

    // 一帧的脏矩形：第 0..4 行、整页宽（等价于旧的行区间语义，但现在是矩形）。
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

/// 页尺寸变化必须重建并清零整页缓冲：旧页的行宽不同，残留字节会被当成新页内容上传
/// （1 纹素 padding 的线性采样会把它读进 tile 边缘）。
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

/// 图集行没赶上帧时，App 必须请求补帧：否则静态窗口会把缺字的画面一直留着。
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
    // 共享槽停在「已入队、未录制」：本帧的图集矩形只能攒着。
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

    // 补帧：上一帧的 publish 已经消费了共享槽，这一帧必须把矩形入队并清掉。
    let _ = app.frame(window).expect("follow-up").expect("ok");
    assert!(
        !app.window(window).expect("window").atlas_rects_pending(),
        "补帧之后图集矩形必须已经入队"
    );
}

/// 平台层在窗口创建之后才挂渲染器：这一帧没有新脏行，但整页必须包含**已有**字形
/// （否则新窗口永远拿到一张空页，且记账为已同步）。
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
    // 没有渲染器的帧：字形记录进 `GlyphAtlas`，整页缓冲此时还不能代表它。
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

/// D9-c：图片图集是多层 texture array（层数 = 打开的页数），逐层独立上传。
///
/// 用 16² 的小图片页让「第 2 页」在 3 张 4×4 图后必然出现；再在不丢矩形的前提下验证：
/// - 新页的写入只攒到 **layer 1**（layer 0 不得被整页重传）；
/// - 同页增量只带 AOI 矩形（字节数 << 整页）；
/// - 忙槽空闲后矩形只入队一次并清空。
#[cfg(windows)]
#[test]
fn image_pages_upload_to_their_own_texture_array_layers() {
    use crate::{paint::atlas::AtlasKind, render::DirtyRects};

    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, window) = setup();
    // 16² 的图片页：每个 4×4 tile（含 padding 6×6）一页只放得下 2 个（etagere 装箱实测）。
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
    // 让首帧的整层上传真正录制进 GPU 槽（离屏渲染器在下一帧提交时消费待上传层）。
    app.window_mut(window).expect("window").request_repaint();
    let _ = app.frame(window).expect("frame").expect("ok");
    assert!(
        app.window(window)
            .expect("window")
            .atlas_idle(AtlasKind::Image),
        "首帧上传必须已经录制，图片图集槽才能再次入队"
    );

    // 让图片图集的共享槽停在「已入队、未录制」：后续写入只能攒在该窗口的层队列里。
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

    // 页 0 已满 ⇒ 这张新图落在第 2 页（layer 1）。
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

    // 忙槽在下一帧被录制；随后的上传只发 layer 1 并清掉它的矩形。
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

/// **P2-B3 核心语义**：图片 array 的层数 = 当前打开的图集页数。页数 1 → 2 时 App 必须
/// 扩层（数组重建为 2 层），并且对**每个已打开的层**整页重传 —— 只补新层会让重建后的数组
/// 丢掉 layer 0 的图。
///
/// 变异：App 的「数组缺失」判据忽略层数（只比页边长）⇒ 扩层帧只入队 layer 1 ⇒ 本测试的
/// 待上传层断言失败。
#[cfg(windows)]
#[test]
fn growing_the_image_array_reuploads_every_open_layer() {
    use crate::{paint::atlas::AtlasKind, render::AtlasArraySpec};

    let _guard = crate::render::windows::device::gpu_test_lock();
    let (_rt, mut app, window) = setup();
    // 16² 图片页：每页放 2 个 4×4 tile（6² 含 padding）⇒ 2 张图占一页，第 3 张开第 2 页。
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

    // 一页：数组 1 层，首帧只上传 layer 0。
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
    // 让离屏渲染器录制首帧，槽位回到空闲。
    app.window_mut(window).expect("window").request_repaint();
    let _ = app.frame(window).expect("frame").expect("ok");

    // 第 3 张图 ⇒ 第 2 页。扩层帧：两个已打开的层都必须整页重传。
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

/// D9-c：晚挂上的窗口必须补齐**每一个已打开页**的整层内容（texture array 每层独立）。
///
/// 新建窗口时进程级资源页可能早就有内容、而脏矩形早已被旧窗口消费 —— 这时只能按
/// `page_pixels_at(layer)` 逐层整页上传；只补 layer 0 会让 layer 1 的 tile 采样到未定义内容。
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

    // 再跑一次：没有任何新写入，两边都不得重复上传。
    app.upload_atlas_pages();
    for window in [first, second] {
        assert!(!app.window(window).expect("window").atlas_rects_pending());
    }
}

/// D-7：关窗必须归还该窗口 `insert_owned` 的引用；去重共享的内容不得被误回收。
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
    // 右窗口的 tile 仍有效：去重命中说明条目与内容索引都还在。
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

/// D-7：重复 close 幂等，且不得释放其它窗口的引用（窗口不存在时提前返回）。
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
    // 右窗口的资源仍可用（内容索引与 tile 都还在）。
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

/// D-6 测试用 runtime：`insert_async` 现在先 spawn 一个 async 任务等并发许可再投 blocking 解码，
/// `current_thread` runtime 需要被 `block_on` 驱动；这里用单 worker 的 multi_thread 让任务在
/// 后台 worker 上跑，测试线程可以同步轮询 `run_frame_hook`。
fn decode_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("multi-thread runtime")
}

/// 轮询直到 `predicate` 为真（上限 `attempts` × `sleep`）。
fn wait_until(attempts: usize, mut predicate: impl FnMut() -> bool) -> bool {
    for _ in 0..attempts {
        if predicate() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    false
}

/// D-6：`insert_async` 的结果必须由帧钩子（`run_frame_hook` → `poll`）交付，且只交付一次。
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
    // 再跑一轮钩子也不得重复交付 / 开新帧。
    let requests = app.frame_requests();
    app.run_frame_hook();
    assert!(app.take_resource_results().is_empty());
    assert_eq!(app.frame_requests(), requests, "没有新交付就不会每轮请求帧");
}

/// D-6：解码完成必须唤醒平台泵（否则空闲时泵阻塞在 GetMessageW/poll，`poll` 永不发生）。
#[test]
fn async_decode_wakes_the_pump() {
    let rt = decode_runtime();
    let mut app = App::new(rt.handle().clone());
    let _window = app.create_window(WindowOptions::default());
    // 清掉构造期可能留下的唤醒位。
    let _ = app.dispatcher().take_frame_wake();
    let _pending = app
        .resources_mut()
        .insert_async(bytes::Bytes::from(png_rgba([1, 2, 3, 255])));

    assert!(
        wait_until(400, || app.dispatcher().take_frame_wake()),
        "解码完成必须唤醒泵（Dispatcher 电平位）"
    );
    // 收尾：把结果交付掉，避免 pending 残留影响其它断言。
    let mut delivered = Vec::new();
    assert!(wait_until(200, || {
        app.run_frame_hook();
        delivered = app.take_resource_results();
        !delivered.is_empty()
    }));
    assert_eq!(delivered.len(), 1);
}

/// D-6：`Window::set_canvas` 替换 Canvas 节点内容 ⇒ 标子树 PAINT、下一帧新图元上屏。
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

/// D-6：`Window::set_canvas` 的类型/生命周期契约。
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

/// D-6 完整闭环：异步入库的图片 → 帧钩子交付 Ready → `set_canvas` 把新 tile 应用到已有节点
/// → 下一帧上屏（素材/节点生命周期由调用方持有 `ResourceImage` 的引用决定）。
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
