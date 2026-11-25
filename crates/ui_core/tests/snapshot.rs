use ui_core::{
    math::DevicePixelRatio,
    test_support::snapshot::{
        GoldenError, SnapshotCase, SnapshotImage, SnapshotScene, SnapshotTheme, Tolerance,
        assert_golden, capture, compare, gpu_guard,
    },
};

fn case(scene: SnapshotScene, theme: SnapshotTheme, scale: f32) -> SnapshotCase {
    SnapshotCase {
        scene,
        theme,
        size: scene.size(),
        scale: DevicePixelRatio::new(scale).expect("golden scale must be positive"),
    }
}

fn run_with(case: SnapshotCase, tolerance: Tolerance) {
    let _guard = gpu_guard();
    match assert_golden(case, tolerance) {
        Ok(diff) => eprintln!(
            "{} max={} avg={:.3} mismatch={}/{} (allowed {})",
            case.golden_name(),
            diff.max_channel_diff,
            diff.avg_channel_diff,
            diff.mismatched_pixels,
            diff.compared_pixels,
            tolerance.allowed_mismatches(diff.compared_pixels)
        ),
        Err(error) => panic!("golden {}: {error}\n\n{error:?}", case.golden_name()),
    }
}

fn run(case: SnapshotCase) {
    run_with(case, Tolerance::default());
}

#[test]
fn golden_shapes() {
    for scale in [1.0, 1.25, 2.0] {
        for theme in [SnapshotTheme::Light, SnapshotTheme::Dark] {
            run(case(SnapshotScene::Shapes, theme, scale));
        }
    }
}

#[test]
fn golden_shadow() {
    run(case(SnapshotScene::Shadow, SnapshotTheme::Light, 1.0));
}

#[test]
fn golden_gradient() {
    run(case(SnapshotScene::Gradient, SnapshotTheme::Light, 1.0));
}

#[test]
fn golden_colors() {
    for theme in [SnapshotTheme::Light, SnapshotTheme::Dark] {
        run(case(SnapshotScene::Colors, theme, 1.0));
    }
}

#[test]
fn golden_image() {
    for scale in [1.0, 2.0] {
        run(case(SnapshotScene::Image, SnapshotTheme::Light, scale));
    }
}

#[test]
fn golden_color_emoji() {
    let case = case(SnapshotScene::ColorEmoji, SnapshotTheme::Light, 1.0);
    if std::env::var_os("UI_CORE_BLESS").is_none() && !case.golden_path().exists() {
        eprintln!(
            "skip golden {}: no golden for this device (reason: emoji font/GPU not provisioned)",
            case.golden_name()
        );
        return;
    }
    run(case);
}

fn gpu_pixel_exempt() -> bool {
    std::env::var_os("KYTRIX_TEST_SKIP_GPU_PIXELS").is_some()
}

fn skip_gpu_pixel_assertions(reason: &str) {
    assert!(
        gpu_pixel_exempt(),
        "彩色 emoji 像素断言无法执行：{reason}；如需在无彩色 emoji 字体/无呈现后端的环境显式豁免，设置 KYTRIX_TEST_SKIP_GPU_PIXELS=1"
    );
    eprintln!(
        "skip color emoji pixel assertions: {reason} (explicitly exempted by KYTRIX_TEST_SKIP_GPU_PIXELS=1)"
    );
}

#[test]
fn color_emoji_glyphs_render_in_color() {
    let _guard = gpu_guard();
    let case = case(SnapshotScene::ColorEmoji, SnapshotTheme::Light, 1.0);
    let image = match capture(case) {
        Ok(image) => image,
        Err(error) => {
            skip_gpu_pixel_assertions(&format!("离屏读回失败：{error:?}"));
            return;
        }
    };
    let mut colored = 0usize;
    let mut globe_ocean = 0usize;
    let mut globe_land = 0usize;
    let mut flag_band_red = 0usize;
    let mut flag_band_saturated = 0usize;
    let mut flag_band_ink = 0usize;
    let width = image.width as usize;
    for (index, pixel) in image.rgba8.as_chunks::<4>().0.iter().enumerate() {
        let [r, g, b, a] = *pixel;
        if a == 0 {
            continue;
        }
        let span = r.max(g).max(b) - r.min(g).min(b);
        if span >= 60 {
            colored += 1;
        }
        if b > 120 && b > r.saturating_add(40) {
            globe_ocean += 1;
        }
        if g > 120 && g > r.saturating_add(40) {
            globe_land += 1;
        }
        let y = index / width;
        if FLAG_BAND.contains(&y) {
            if r > 150 && g < 100 && b < 110 {
                flag_band_red += 1;
            }
            if span >= 60 {
                flag_band_saturated += 1;
            }
            if r < 100 && g < 100 && b < 100 {
                flag_band_ink += 1;
            }
        }
    }
    eprintln!(
        "color emoji pixels: colored={colored} globe_ocean={globe_ocean} globe_land={globe_land} flag_band_red={flag_band_red} flag_band_saturated={flag_band_saturated} flag_band_ink={flag_band_ink}"
    );
    if colored == 0 {
        skip_gpu_pixel_assertions("宿主没有彩色 emoji 字体（colored=0）");
        return;
    }
    assert!(
        colored > 300,
        "彩色字形必须产生足够的饱和像素（实测 {colored}，说明走的是单色 R8 路径）"
    );
    match ui_core::test_support::snapshot::device_dir() {
        "warp" => {
            assert!(
                globe_ocean > 20 && globe_land > 20,
                "🌍 必须同时有海洋蓝与陆地绿（ocean={globe_ocean}, land={globe_land}）"
            );
            assert_eq!(
                flag_band_saturated, 0,
                "Segoe UI Emoji 没有国旗字形：🇨🇳 行必须由单色字母渲染（实测 {flag_band_saturated} 个饱和像素）"
            );
            assert!(
                flag_band_ink > 20,
                "🇨🇳 行必须画出深色字母 CN（实测 {flag_band_ink} 个暗像素）"
            );
        }
        "metal-real" => {
            assert!(
                globe_ocean > 20 && globe_land > 20,
                "🌍 必须同时有海洋蓝与陆地绿（ocean={globe_ocean}, land={globe_land}）"
            );
            assert!(
                flag_band_red > 20,
                "Apple Color Emoji 必须为 🇨🇳 在旗帜行带内提供红色旗面（实测 {flag_band_red} 个像素）"
            );
        }
        "lavapipe" => {
            eprintln!(
                "note: 🌍 在 Linux 上由 DejaVu Sans 的单色轮廓字形接管（font stack 未列 emoji 家族），\
                 彩色证据来自 Noto Color Emoji 的国旗/家庭 ZWJ 字形"
            );
            assert!(
                flag_band_red > 20,
                "Noto Color Emoji 必须为 🇨🇳 在旗帜行带内提供红色旗面（实测 {flag_band_red} 个像素）"
            );
        }
        other => panic!("color emoji 用例缺少设备目录 {other} 的期望"),
    }
}

const FLAG_BAND: core::ops::Range<usize> = 112..160;

#[test]
fn golden_clip() {
    run_with(
        case(SnapshotScene::Clip, SnapshotTheme::Light, 1.0),
        Tolerance::GEOMETRY,
    );
}

#[test]
fn golden_clip_subpixel() {
    run_with(
        case(SnapshotScene::ClipSubpixel, SnapshotTheme::Light, 1.0),
        Tolerance::GEOMETRY,
    );
}

fn pixel(image: &SnapshotImage, x: u32, y: u32) -> [u8; 4] {
    let offset = ((y as usize) * (image.width as usize) + (x as usize)) * 4;
    image.rgba8[offset..offset + 4]
        .try_into()
        .expect("4 bytes per pixel")
}

fn channel_span(a: [u8; 4], b: [u8; 4]) -> u8 {
    a.iter()
        .zip(b.iter())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0)
}

#[test]
fn group_opacity_is_a_composite_multiplier_in_the_readback() {
    use ui_core::{
        math::px,
        node::builder::NodeBuilder,
        paint::{
            color::Color,
            style::{PaintSpec, Style, StylePatch},
            style_builder::StyleBuilder,
        },
        test_support::app::TestApp,
    };

    fn sized(width: f32, height: f32) -> Style {
        StyleBuilder::new().size(px(width), px(height)).build()
    }
    fn white() -> PaintSpec {
        PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        }
    }

    let _guard = gpu_guard();
    let mut app = TestApp::new();
    let window = app.window_id();
    let root = app.app().window(window).expect("窗口").tree().root();
    {
        let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
        let group = NodeBuilder::boxed()
            .style(sized(100.0, 100.0))
            .paint(white())
            .bind_style(|_cx| StylePatch {
                opacity: Some(0.5),
                ..StylePatch::default()
            })
            .build(tree, root)
            .expect("group");
        NodeBuilder::boxed()
            .style(sized(50.0, 50.0))
            .paint(white())
            .bind_style(|_cx| StylePatch {
                opacity: Some(0.5),
                ..StylePatch::default()
            })
            .build(tree, group)
            .expect("child");
    }
    app.drain().expect("一帧");
    let image = app.read_pixels().expect("WARP / Metal 离屏读回");

    let child_alpha = pixel(&image, 25, 25)[3];
    let group_only_alpha = pixel(&image, 75, 75)[3];
    let outside_alpha = pixel(&image, 300, 300)[3];

    assert!(
        (i32::from(child_alpha) - 159).abs() <= 6,
        "子区域 alpha 应为 0.625×255≈159（组 0.5 叠子 0.25），实测 {child_alpha}"
    );
    assert!(
        (i32::from(group_only_alpha) - 128).abs() <= 6,
        "只有组的区域 alpha 应为 0.5×255≈128，实测 {group_only_alpha}"
    );
    assert_eq!(outside_alpha, 0, "组外必须全透明（没有多画出任何区域）");
    let child_pixel = pixel(&image, 25, 25);
    assert!(
        child_pixel[..3]
            .iter()
            .all(|channel| i32::from(*channel) > 100),
        "预乘 RGB 必须随 alpha 一起缩放（白 quad 的 rgb 不应被剪成 0）：{child_pixel:?}"
    );
}

#[test]
fn clip_content_never_leaks_past_the_rect_clip_boundaries() {
    let _guard = gpu_guard();
    let image = capture(case(SnapshotScene::Clip, SnapshotTheme::Light, 1.0))
        .expect("WARP/lavapipe/metal 必须能离屏读回 clip 场景");

    let backdrop = pixel(&image, 200, 100);
    let ink = pixel(&image, 144, 30);
    let accent = pixel(&image, 120, 90);
    assert!(
        channel_span(backdrop, ink) > 128 && channel_span(backdrop, accent) > 128,
        "参照色必须可区分：backdrop={backdrop:?} ink={ink:?} accent={accent:?}"
    );

    for y in 18..=37 {
        assert!(
            channel_span(pixel(&image, 147, y), ink) <= 2,
            "x=147,y={y} 必须是 ink（右边界内侧最后一列）：{:?}",
            pixel(&image, 147, y)
        );
    }
    for x in 108..=147 {
        assert!(
            channel_span(pixel(&image, x, 97), accent) <= 2,
            "x={x},y=97 必须是 accent（底边界内侧最后一行）：{:?}",
            pixel(&image, x, 97)
        );
    }

    for y in 8..98 {
        for x in 148..image.width {
            assert!(
                channel_span(pixel(&image, x, y), backdrop) <= 2,
                "x={x},y={y} 越过右边界 x=148（P1-N1 的越界列）：{:?}",
                pixel(&image, x, y)
            );
        }
    }
    for y in 98..image.height {
        for x in 108..148 {
            assert!(
                channel_span(pixel(&image, x, y), backdrop) <= 2,
                "x={x},y={y} 越过底边界 y=98：{:?}",
                pixel(&image, x, y)
            );
        }
    }
    for y in 8..98 {
        for x in 0..8 {
            assert!(
                channel_span(pixel(&image, x, y), backdrop) <= 2,
                "x={x},y={y} 越过左边界 x=8：{:?}",
                pixel(&image, x, y)
            );
        }
    }
    for y in 0..8 {
        for x in 8..148 {
            assert!(
                channel_span(pixel(&image, x, y), backdrop) <= 2,
                "x={x},y={y} 越过上边界 y=8：{:?}",
                pixel(&image, x, y)
            );
        }
    }
}

#[test]
fn compare_reports_stats_without_a_device() {
    let actual = SnapshotImage {
        width: 2,
        height: 1,
        rgba8: vec![10, 20, 30, 255, 40, 50, 60, 255],
    };
    let mut golden = actual.clone();
    golden.rgba8[0] = 12;
    golden.rgba8[5] = 53;

    let diff = compare(&actual, &golden, Tolerance::default()).expect("compare");
    assert_eq!(diff.max_channel_diff, 3);
    assert_eq!(diff.mismatched_pixels, 1);
    assert_eq!(diff.first_mismatch, Some((1, 0)));
    assert_eq!(
        diff.mismatch_bounds,
        Some(ui_core::test_support::snapshot::PixelBounds {
            min_x: 1,
            min_y: 0,
            max_x: 1,
            max_y: 0,
        })
    );
    assert_eq!(diff.compared_pixels, 2);
    assert!(diff.avg_channel_diff > 0.0 && diff.avg_channel_diff < 1.0);

    let exact = compare(&actual, &golden, Tolerance::EXACT).expect("compare");
    assert_eq!(exact.mismatched_pixels, 2);
    assert_eq!(exact.first_mismatch, Some((0, 0)));
    assert_eq!(
        exact.mismatch_bounds.map(|bounds| bounds.to_string()),
        Some("(0,0)-(1,0)".to_string())
    );
    assert_eq!(exact.compared_pixels, 2);
}

#[test]
fn compare_rejects_dimension_mismatch() {
    let actual = SnapshotImage {
        width: 1,
        height: 1,
        rgba8: vec![0, 0, 0, 255],
    };
    let golden = SnapshotImage {
        width: 1,
        height: 2,
        rgba8: vec![0; 8],
    };
    assert_eq!(
        compare(&actual, &golden, Tolerance::default()),
        Err(GoldenError::Dimensions {
            actual: (1, 1),
            golden: (1, 2)
        })
    );
}
