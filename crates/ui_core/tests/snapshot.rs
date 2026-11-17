//! L3 集成测试：像素 golden（Windows WARP / Linux lavapipe / macOS Metal）。
//!
//! 运行：`cargo test -p ui_core --features test-support --test snapshot`
//! 生成/更新：`$env:UI_CORE_BLESS='1'`（PowerShell）或 `UI_CORE_BLESS=1`（bash）后重跑同一条命令；
//! 只写**当前平台**目录，每个被覆盖的文件会打印一行 `blessed by UI_CORE_BLESS`。
//!
//! golden 按设备类型分目录（D14），每个平台只和自己的基线比；GPU 失败是**测试失败**，不是跳过。
//! 容差语义见 `tests/golden/README.md`（逐通道 2 + 不匹配像素 ≤ 0.1%）。

use ui_core::{
    math::Scale,
    test_support::snapshot::{
        GoldenError, SnapshotCase, SnapshotImage, SnapshotScene, SnapshotTheme, Tolerance,
        assert_golden, capture, compare, gpu_guard,
    },
};

/// 一个 case（尺寸取场景的固有尺寸，见 `SnapshotScene::size`）。
fn case(scene: SnapshotScene, theme: SnapshotTheme, scale: f32) -> SnapshotCase {
    SnapshotCase {
        scene,
        theme,
        size: scene.size(),
        scale: Scale::new(scale).expect("golden scale must be positive"),
    }
}

/// 跑一个 case（指定容差）：任何失败都 panic（带 golden 名）。
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

/// 跑一个 case（默认容差）。
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

/// 几何边界 case 用 **0 不匹配像素** 的容差（AR2b P2-N4）：默认的 0.1% 比例容差允许 38 px，
/// 整列 20 px 的越界/漏裁会被静默放过（P1-N1 就是这样被 bless 进基线的）。
#[test]
fn golden_clip() {
    run_with(
        case(SnapshotScene::Clip, SnapshotTheme::Light, 1.0),
        Tolerance::GEOMETRY,
    );
}

/// AR2b P2-N4：分数坐标边界的矩形 clip（容器边界落在 1/4 与 1/2 像素上）。吸附方向错
/// （向内 / round）会让边界列/行各差 1 像素 —— 只有 0 容差抓得住。
#[test]
fn golden_clip_subpixel() {
    run_with(
        case(SnapshotScene::ClipSubpixel, SnapshotTheme::Light, 1.0),
        Tolerance::GEOMETRY,
    );
}

/// 读回图的一个像素（RGBA8，预乘 sRGB）。
fn pixel(image: &SnapshotImage, x: u32, y: u32) -> [u8; 4] {
    let offset = ((y as usize) * (image.width as usize) + (x as usize)) * 4;
    image.rgba8[offset..offset + 4]
        .try_into()
        .expect("4 bytes per pixel")
}

/// 两个颜色在一次断言里的最大单通道差（含 alpha；预乘空间）。
fn channel_span(a: [u8; 4], b: [u8; 4]) -> u8 {
    a.iter()
        .zip(b.iter())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0)
}

/// P1-N1 的**不依赖 golden** 的回归闸门：矩形 clip 必须逐列/逐行精确裁掉框外像素。
///
/// 这条测试读**本机 GPU 的读回像素**（WARP / lavapipe / metal 各自跑），断言的是语义而不是
/// 某张基线：
///
/// - `clip.outer` = 逻辑 `(8,8,140×90)` ⇒ 物理右边界 `x = 148`、底边界 `y = 98`；矩形 clip
///   是 Skia `roundOut` 语义（向外吸附，与矩形有交的像素全保留），所以 `x = 147` / `y = 97`
///   必须是内容，`x ≥ 148` / `y ≥ 98` 必须是背景。
/// - 旧实现（`world_px ± 0.5` 浮点比较）在 lavapipe 上把 `x = 148, y = 18..37` 判成「有交」
///   ⇒ 一列 20 px 满不透明 ink 越出裁剪框（P1-N1），而 warp/metal 在同一坐标是背景。
/// - 参照色从**同一张图**里取（背景取画布右侧空白，ink/accent 取图元内部），并且先断言参照色
///   彼此可区分 —— 否则「什么都没画」也能让断言通过。
#[test]
fn clip_content_never_leaks_past_the_rect_clip_boundaries() {
    let _guard = gpu_guard();
    let image = capture(case(SnapshotScene::Clip, SnapshotTheme::Light, 1.0))
        .expect("WARP/lavapipe/metal 必须能离屏读回 clip 场景");

    // 参照色：(200,100) 在 `clip.outer` 之外 ⇒ 纯背景；(144,30) 在溢出子矩形内部 ⇒ ink。
    let backdrop = pixel(&image, 200, 100);
    let ink = pixel(&image, 144, 30);
    let accent = pixel(&image, 120, 90);
    assert!(
        channel_span(backdrop, ink) > 128 && channel_span(backdrop, accent) > 128,
        "参照色必须可区分：backdrop={backdrop:?} ink={ink:?} accent={accent:?}"
    );

    // ① 内容确实存在（防止「整块没画」把下面几条变成空断言）。
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

    // ② 右边界：`clip.outer` 的整个 y 范围内，x ≥ 148 一行都不许有内容。
    for y in 8..98 {
        for x in 148..image.width {
            assert!(
                channel_span(pixel(&image, x, y), backdrop) <= 2,
                "x={x},y={y} 越过右边界 x=148（P1-N1 的越界列）：{:?}",
                pixel(&image, x, y)
            );
        }
    }
    // ③ 底边界：y ≥ 98 一列都不许有内容（`clip.outer.overflow` 向下溢出）。
    for y in 98..image.height {
        for x in 108..148 {
            assert!(
                channel_span(pixel(&image, x, y), backdrop) <= 2,
                "x={x},y={y} 越过底边界 y=98：{:?}",
                pixel(&image, x, y)
            );
        }
    }
    // ④ 左/上边界同理（`x < 8`、`y < 8` 不许有内容）。
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
    // 第 0 个像素红通道差 2（= `per_channel`，在容差内）；第 1 个像素绿通道差 3（不匹配）。
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

    // `EXACT` 收紧到逐字节：两处都不匹配，包围盒覆盖整行。
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
