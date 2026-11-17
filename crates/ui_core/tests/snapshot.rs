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
        assert_golden, compare, gpu_guard,
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

/// 跑一个 case：任何失败都 panic（带 golden 名）。
fn run(case: SnapshotCase) {
    let _guard = gpu_guard();
    match assert_golden(case, Tolerance::default()) {
        Ok(diff) => eprintln!(
            "{} max={} avg={:.3} mismatch={}/{}",
            case.golden_name(),
            diff.max_channel_diff,
            diff.avg_channel_diff,
            diff.mismatched_pixels,
            diff.compared_pixels
        ),
        Err(error) => panic!("golden {}: {error}\n\n{error:?}", case.golden_name()),
    }
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
fn golden_clip() {
    run(case(SnapshotScene::Clip, SnapshotTheme::Light, 1.0));
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
