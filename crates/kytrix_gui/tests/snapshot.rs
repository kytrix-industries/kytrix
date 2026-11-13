//! L3 集成测试：像素 golden（Windows WARP）。
//!
//! 运行：`cargo test -p kytrix_gui --features test-support --test snapshot`
//! 生成/更新：`$env:UPDATE_GOLDEN='1'`（PowerShell）或 `UPDATE_GOLDEN=1`（bash）后重跑同一条命令。
//!
//! macOS / Linux：`skip_reason()` 非空 ⇒ 打印原因并返回（跨设备比 warp golden 必然失败，
//! 见 `design/phase-13-resources-snapshot.md` §9.6）。Windows 上的 GPU 失败是**测试失败**，不是跳过。

use kytrix_gui::{
    math::Scale,
    test_support::snapshot::{
        GoldenError, SnapshotCase, SnapshotImage, SnapshotScene, SnapshotTheme, Tolerance,
        assert_golden, compare, gpu_guard, skip_reason,
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

/// 跑一个 case：非 Windows 跳过；Windows 上任何失败都 panic（带 golden 名）。
fn run(case: SnapshotCase) {
    if let Some(reason) = skip_reason() {
        eprintln!("skip {}: {reason}", case.golden_name());
        return;
    }
    let _guard = gpu_guard();
    match assert_golden(case, Tolerance::default()) {
        Ok(diff) => eprintln!(
            "{} max={} avg={:.3} over={}/{}",
            case.golden_name(),
            diff.max_channel_diff,
            diff.avg_channel_diff,
            diff.over_tolerance_pixels,
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
    golden.rgba8[0] = 12; // 第 0 个像素的红通道差 2

    let diff = compare(&actual, &golden, Tolerance::default()).expect("compare");
    assert_eq!(diff.max_channel_diff, 2);
    assert_eq!(diff.over_tolerance_pixels, 1);
    assert_eq!(diff.first_over_tolerance, Some((0, 0)));
    assert_eq!(diff.compared_pixels, 2);
    assert!(diff.avg_channel_diff > 0.0 && diff.avg_channel_diff < 1.0);

    // `EXACT` 只收紧阈值（`per_channel = 0`），超差像素数不变。
    let exact = compare(&actual, &golden, Tolerance::EXACT).expect("compare");
    assert_eq!(exact.over_tolerance_pixels, 1);
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
