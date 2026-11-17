use super::App;
use crate::{
    math::{Rect, Size},
    node::builder::NodeBuilder,
    paint::style::PaintSpec,
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
    let pixels: Vec<u8> = std::iter::repeat_n([10u8, 20, 30, 255], 16)
        .flatten()
        .collect();
    let image = image::RgbaImage::from_raw(4, 4, pixels).expect("raw");
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("encode png");
    out.into_inner()
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
fn a_busy_window_queues_its_rows_without_blocking_idle_windows() {
    use crate::{paint::atlas::AtlasKind, render::DirtyRows};

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
            .mark_atlas_page_received(AtlasKind::Glyph, page);
    }
    // 让 second 的共享槽停在「已入队、未录制」：这就是忙窗口。
    let mut full = DirtyRows::default();
    full.mark(0, u32::MAX);
    assert!(
        app.window_mut(second).expect("second").queue_atlas_upload(
            AtlasKind::Glyph,
            page,
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

    app.pending_dirty.mark(0, 4);
    app.upload_atlas_pages();

    assert!(
        app.window(first)
            .expect("first")
            .atlas_rows(AtlasKind::Glyph)
            .is_empty(),
        "空闲窗口的行必须已经发出"
    );
    let second_rows = app
        .window(second)
        .expect("second")
        .atlas_rows(AtlasKind::Glyph);
    assert_eq!(second_rows.spans().len(), 1);
    assert_eq!(
        second_rows.spans()[0],
        0..4,
        "忙窗口必须把这一帧的行攒在自己的队列里"
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
    use crate::{paint::atlas::AtlasKind, render::DirtyRows};

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
        .mark_atlas_page_received(AtlasKind::Glyph, page);
    // 共享槽停在「已入队、未录制」：本帧的图集行只能攒着。
    let mut full = DirtyRows::default();
    full.mark(0, u32::MAX);
    assert!(app.window_mut(window).expect("window").queue_atlas_upload(
        AtlasKind::Glyph,
        page,
        &vec![0u8; (page * page) as usize],
        full,
    ));
    app.window_mut(window).expect("window").request_repaint();
    app.pending_dirty.mark(0, 4);

    let _ = app.frame(window).expect("frame").expect("ok");
    assert!(
        app.window(window).expect("window").atlas_rows_pending(),
        "忙窗口的图集行必须留在队列里"
    );
    assert!(
        app.dispatcher.take_frame_wake(),
        "补帧必须请求一次帧唤醒（否则静态窗口不会重试）"
    );

    // 补帧：上一帧的 publish 已经消费了共享槽，这一帧必须把行入队并清掉。
    let _ = app.frame(window).expect("follow-up").expect("ok");
    assert!(
        !app.window(window).expect("window").atlas_rows_pending(),
        "补帧之后图集行必须已经入队"
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
