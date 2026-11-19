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
            .mark_atlas_page_received(AtlasKind::Glyph, page);
    }
    // 让 second 的共享槽停在「已入队、未录制」：这就是忙窗口。
    let full = DirtyRects::full();
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

    // 一帧的脏矩形：第 0..4 行、整页宽（等价于旧的行区间语义，但现在是矩形）。
    let frame_dirty = PhysicalRect::new(0, 0, i32::MAX, 4);
    app.pending_dirty.mark(frame_dirty);
    app.upload_atlas_pages();

    assert!(
        app.window(first)
            .expect("first")
            .atlas_rects(AtlasKind::Glyph)
            .is_empty(),
        "空闲窗口的矩形必须已经发出"
    );
    let second_rects = app
        .window(second)
        .expect("second")
        .atlas_rects(AtlasKind::Glyph);
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
        .mark_atlas_page_received(AtlasKind::Glyph, page);
    // 共享槽停在「已入队、未录制」：本帧的图集矩形只能攒着。
    let full = DirtyRects::full();
    assert!(app.window_mut(window).expect("window").queue_atlas_upload(
        AtlasKind::Glyph,
        page,
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
