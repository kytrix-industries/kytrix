#[cfg(windows)]
use super::frame::surface_change_plan;
use super::{
    FrameIndex, FrameOutcome, Window, WindowId, WindowOptions, frame::frame_reaches_gpu,
    surface::covering_surface,
};
use crate::{
    math::{PhysicalRect, Scale, Size},
    node::dirty::DirtyFlags,
    paint::color::Color,
    render::compositor::{DamageRects, Plan},
};

fn window() -> Window {
    Window::new(WindowId::for_test(1), WindowOptions::default())
}

#[test]
fn new_window_has_root_and_empty_scene() {
    let window = window();

    assert_eq!(window.id(), WindowId::for_test(1));
    assert_eq!(window.tree().len(), 1, "初始只有 root");
    assert_eq!(window.scene().lane_count(), 0);
    assert!(window.scene().is_empty());
    assert_eq!(window.scale(), Scale::ONE);
}

#[test]
fn scene_starts_without_lanes() {
    let window = window();
    let scene = window.scene();

    assert_eq!(scene.lane_count(), 0);
    assert!(scene.order().is_empty());
    assert_eq!(scene.primitive_count(), 0);
    assert!(scene.is_empty());
}

#[test]
fn set_scale_marks_root_layout() {
    let mut window = window();
    window.tree_mut().clear_all_dirty_for_test();
    let scale = Scale::new(2.0).expect("合法比例");

    window.set_scale(scale);

    assert_eq!(window.scale(), scale);
    let root = window.tree().root();
    let flags = window.tree().get(root).expect("root 存在").dirty();
    assert!(flags.contains(DirtyFlags::LAYOUT));
    assert!(flags.contains(DirtyFlags::PAINT));
}

#[test]
fn window_options_default_values() {
    let options = WindowOptions::default();

    assert_eq!(options.title, "");
    assert_eq!(options.size.width, 800.0);
    assert_eq!(options.size.height, 600.0);
    assert_eq!(options.scale, Scale::ONE);
    assert_eq!(
        options.background,
        Color::BLACK,
        "默认背景必须是不透明黑（Color::default() 是透明）"
    );
}

#[test]
fn set_background_color_updates_options_and_ignores_the_same_value() {
    let mut window = window();
    assert_eq!(window.options().background, Color::BLACK);
    assert!(!window.full_damage_pending);

    window.set_background_color(Color::WHITE);
    assert_eq!(window.options().background, Color::WHITE);
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    assert!(
        window.full_damage_pending,
        "改色必须标一帧，否则背景色可能永不被 present"
    );

    // 同值 no-op：清掉 damage 之后再调同值，不得重新置位。
    window.full_damage_pending = false;
    window.set_background_color(Color::WHITE);
    assert_eq!(window.options().background, Color::WHITE);
    assert!(!window.full_damage_pending, "同值 no-op 不得标 damage");
}

/// 接线：`commit_frame` 的 frame 用真实 surface（不是 `size × scale` 的 covering）。
#[cfg(all(windows, feature = "test-support"))]
#[test]
fn a_committed_frame_uses_the_platform_surface_not_the_covering_size() {
    let (mut window, _node) = painted_window();
    // 800×600 的 covering 与 449×300 不同：记录里的整帧矩形能区分两者。
    window.set_surface((449, 300));

    let outcome = window.frame().expect("帧成功");
    assert!(matches!(outcome, FrameOutcome::Committed(_)));
    let record = window.record().expect("已提交帧必须有记录");
    assert!(record.damage_full, "set_surface 标全量重绘");
    assert_eq!(
        record.damage,
        vec![crate::math::PhysicalRect::new(0, 0, 449, 300)],
        "frame 必须用真实 surface，而不是 covering"
    );
}

/// Linux：`frame_rect` 用平台层写入的真实 Wayland buffer 尺寸（`set_surface`），
/// 不再是 `size × scale` 的 covering（文字按 buffer 像素 1:1）。
#[cfg(target_os = "linux")]
#[test]
fn frame_rect_uses_the_platform_buffer_on_linux() {
    let mut window = window();
    // 初值是 covering（800×600）；平台层随后在 event_loop 里写入 round 后的 buffer。
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 800, 600));
    window.set_surface((999, 777));
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 999, 777));

    // 平台已写入真值：`set_size` 不得把它重算成 covering（F-186 / V-122）。
    window.set_size(Size::new(256.8, 100.5));
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 999, 777));
    window.set_surface((449, 176));
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 449, 176));

    // 0 尺寸由 `set_surface` 钳到 1 px（不能给 swapchain 0 尺寸）。
    window.set_surface((0, 0));
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 1, 1));
}

/// macOS：`frame_rect` 用平台层写入的真实 backing surface，不再是 covering（文字 1:1）。
#[cfg(target_os = "macos")]
#[test]
fn frame_rect_uses_the_platform_backing_on_macos() {
    let mut window = window();
    // 初值是 covering（800×600）；平台层随后写入真实 backing。
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 800, 600));
    window.set_surface((1234, 777));
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 1234, 777));

    // 平台已写入真值：`set_size` 不得把它重算成 covering（F-186 / V-122）。
    window.set_size(Size::new(256.8, 100.5));
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 1234, 777));
    window.set_surface((449, 176));
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 449, 176));
}

#[test]
fn surface_starts_and_tracks_the_covering_size() {
    let mut window = Window::new(
        WindowId::for_test(1),
        WindowOptions {
            size: Size::new(800.0, 600.0),
            ..WindowOptions::default()
        },
    );
    assert_eq!(window.physical_surface(), (800, 600));

    // 125 / 150 / 175%：`set_scale` / `set_size` 先把 surface 重算成 covering（向外取整）；
    // 平台层随后用 `set_surface` 写真值（Linux 是 round 的 Wayland buffer）。
    for (factor, expected) in [
        (1.25, (1000, 750)),
        (1.5, (1200, 900)),
        (1.75, (1400, 1050)),
    ] {
        window.set_scale(Scale::new(factor).expect("合法比例"));
        assert_eq!(window.physical_surface(), expected, "scale {factor}");
        assert_eq!(
            window.physical_surface(),
            covering_surface(window.size(), window.scale()),
            "surface 必须与 covering 公式一致"
        );
    }

    // 亚像素：covering 向外取整（256.8 × 1.75 = 449.4 → 450），不是 round。
    window.set_size(Size::new(256.8, 100.5));
    assert_eq!(window.physical_surface(), (450, 176));
    assert_eq!(
        window.physical_surface(),
        covering_surface(window.size(), window.scale())
    );
    // 零尺寸至少 1 px（不能给 DXGI 0 尺寸的 swapchain）。
    window.set_size(Size::new(0.0, 0.0));
    assert_eq!(window.physical_surface(), (1, 1));
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[test]
fn set_surface_clamps_marks_full_damage_and_ignores_the_same_value() {
    let mut window = window();
    assert_eq!(window.physical_surface(), (800, 600));
    assert!(!window.full_damage_pending);
    window.tree_mut().clear_all_dirty_for_test();
    let epoch = window.scale_epoch();

    window.set_surface((0, 0));
    assert_eq!(window.physical_surface(), (1, 1), "零尺寸钳到 1 px");
    assert!(window.full_damage_pending, "surface 变化必须标全量 damage");
    let root = window.tree().root();
    assert!(
        !window
            .tree()
            .get(root)
            .expect("root")
            .dirty()
            .contains(DirtyFlags::LAYOUT),
        "set_surface 不标 layout（只是目标尺寸，不是布局变化）"
    );
    assert_eq!(window.scale_epoch(), epoch, "set_surface 不动 scale_epoch");

    window.full_damage_pending = false;
    window.set_surface((1, 1));
    assert!(!window.full_damage_pending, "同值 no-op 不得重复标全量");

    window.set_surface((10, 20));
    assert_eq!(window.physical_surface(), (10, 20));
    assert!(window.full_damage_pending);
}

/// Linux：整数回退的 buffer scale 默认 1（fractional 路径也固定 1），钳到 ≥1。
#[cfg(target_os = "linux")]
#[test]
fn buffer_scale_defaults_to_one_and_clamps() {
    let mut window = window();
    assert_eq!(window.buffer_scale, 1, "默认 1（fractional 路径恒 1）");

    window.set_buffer_scale(0);
    assert_eq!(
        window.buffer_scale, 1,
        "0 必须钳到 1（wl_surface 不接受 0）"
    );

    window.set_buffer_scale(2);
    assert_eq!(window.buffer_scale, 2, "整数回退写入 N");
}

/// F-186 / F-189 / V-122：平台写入真实目标之后，`set_size` / `set_scale` 只做布局 / DPI
/// 失效，不得把 `surface` 重置成 `size × scale` 的 covering。
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
#[test]
fn set_size_and_set_scale_keep_a_platform_written_surface() {
    let mut window = window();
    window.tree_mut().clear_all_dirty_for_test();
    window.set_surface((999, 777));
    window.full_damage_pending = false;
    let epoch = window.scale_epoch();
    let root = window.tree().root();

    window.set_size(Size::new(256.8, 100.5));
    assert_eq!(
        window.physical_surface(),
        (999, 777),
        "平台真值不得被 covering（257×101）覆盖"
    );
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 999, 777));
    assert!(window.full_damage_pending, "尺寸变化仍必须整帧重绘");
    assert!(
        window
            .tree()
            .get(root)
            .expect("root 存在")
            .dirty()
            .contains(DirtyFlags::LAYOUT),
        "尺寸变化仍必须标根 LAYOUT"
    );
    assert_eq!(window.scale_epoch(), epoch, "set_size 不动 scale_epoch");

    window.tree_mut().clear_all_dirty_for_test();
    window.full_damage_pending = false;
    window.set_scale(Scale::new(1.25).expect("合法比例"));
    assert_eq!(
        window.physical_surface(),
        (999, 777),
        "DPI 变化不得覆盖平台真值"
    );
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 999, 777));
    assert!(window.full_damage_pending, "DPI 变化仍必须整帧重绘");
    assert!(
        window
            .tree()
            .get(root)
            .expect("root 存在")
            .dirty()
            .contains(DirtyFlags::LAYOUT),
        "DPI 变化仍必须标根 LAYOUT"
    );
    assert_eq!(window.scale_epoch(), epoch + 1, "scale_epoch 照常递增");

    // 平台层下一次显式写入仍然生效。
    window.set_surface((449, 176));
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 449, 176));

    // 同值 `set_surface` 也必须把 `surface` 的所有权交给平台（flag 先于 early-return）：
    // 之后 `set_scale` 不得把它重算成 covering（1000×750）。
    let mut same_value = Window::new(WindowId::for_test(1), WindowOptions::default());
    same_value.set_surface(covering_surface(Size::new(800.0, 600.0), Scale::ONE));
    same_value.set_scale(Scale::new(1.25).expect("合法比例"));
    assert_eq!(
        same_value.physical_surface(),
        (800, 600),
        "同值 set_surface 之后的 set_scale 不得把 800×600 重算成 1000×750"
    );
}

#[test]
fn window_size_is_root_available_space() {
    use crate::node::builder::NodeBuilder;

    let mut window = Window::new(
        WindowId::for_test(1),
        WindowOptions {
            size: Size::new(120.0, 60.0),
            ..WindowOptions::default()
        },
    );
    let root = window.tree().root();
    let child = NodeBuilder::boxed()
        .build(window.tree_mut(), root)
        .expect("挂载成功");

    let report = window.compute_layout().expect("布局成功");

    assert_eq!(window.size(), Size::new(120.0, 60.0));
    assert_eq!(
        window.tree().get(root).expect("root").layout().bounds.size,
        Size::new(120.0, 60.0),
        "根 100% × 100% 撑满窗口"
    );
    assert!(report.laid_out >= 2, "根与子节点都要布局：{report:?}");
    let _ = child;
}

#[test]
fn dpi_scale_does_not_change_logical_boxes() {
    use crate::{
        math::Rect,
        node::builder::NodeBuilder,
        paint::style::{Clip, Style},
    };

    fn fixed(width: f32, height: f32) -> Style {
        let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(width),
            height: taffy::style::Dimension::length(height),
        };
        Style {
            inner,
            clip: Clip::None,
        }
    }

    let mut window = Window::new(
        WindowId::for_test(1),
        WindowOptions {
            size: Size::new(120.0, 60.0),
            ..WindowOptions::default()
        },
    );
    let root = window.tree().root();
    let child = NodeBuilder::boxed()
        .style(fixed(10.5, 3.25))
        .build(window.tree_mut(), root)
        .expect("挂载成功");
    let _ = window.compute_layout().expect("布局成功");
    let before = window.tree().get(child).expect("存在").layout().bounds;

    window.set_scale(Scale::new(1.5).expect("合法比例"));
    let _ = window.compute_layout().expect("布局成功");

    assert_eq!(
        window.tree().get(child).expect("存在").layout().bounds,
        before,
        "逻辑布局盒与 scale 无关（物理化在 P06）"
    );
    assert_eq!(before, Rect::new(0.0, 0.0, 10.5, 3.25));
}

#[test]
fn resize_relayouts_percent_children() {
    use crate::{
        node::builder::NodeBuilder,
        paint::style::{Clip, Style},
    };

    let mut window = Window::new(
        WindowId::for_test(1),
        WindowOptions {
            size: Size::new(100.0, 100.0),
            ..WindowOptions::default()
        },
    );
    let root = window.tree().root();
    let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
    inner.size = taffy::geometry::Size {
        width: taffy::style::Dimension::percent(1.0),
        height: taffy::style::Dimension::percent(1.0),
    };
    let child = NodeBuilder::boxed()
        .style(Style {
            inner,
            clip: Clip::None,
        })
        .build(window.tree_mut(), root)
        .expect("挂载成功");
    let _ = window.compute_layout().expect("布局成功");
    assert_eq!(
        window.tree().get(child).expect("存在").layout().bounds.size,
        Size::new(100.0, 100.0)
    );

    window.set_size(Size::new(300.0, 50.0));
    let _ = window.compute_layout().expect("布局成功");

    assert_eq!(
        window.tree().get(child).expect("存在").layout().bounds.size,
        Size::new(300.0, 50.0),
        "百分比尺寸随根可用空间变化"
    );
}

#[test]
fn set_size_marks_root_layout_and_clears_cache() {
    use crate::node::builder::NodeBuilder;

    let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
    let root = window.tree().root();
    let _ = NodeBuilder::boxed()
        .build(window.tree_mut(), root)
        .expect("挂载成功");
    let _ = window.compute_layout().expect("布局成功");

    window.set_size(Size::new(640.0, 480.0));

    let flags = window.tree().get(root).expect("root 存在").dirty();
    assert!(
        flags.contains(DirtyFlags::LAYOUT),
        "尺寸变化必须标根 LAYOUT（实际 {flags:?}）"
    );
    assert_eq!(window.size(), Size::new(640.0, 480.0));
}
#[test]
fn scale_epoch_bumps_on_dpi_change() {
    let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
    assert_eq!(window.scale_epoch(), 0);

    window.set_scale(Scale::new(1.5).expect("finite scale"));
    assert_eq!(window.scale_epoch(), 1);

    window.set_scale(Scale::new(2.0).expect("finite scale"));
    assert_eq!(window.scale_epoch(), 2);
    assert_eq!(window.scale().factor(), 2.0);
}

// ── P06：帧推进 ──

use crate::{
    node::builder::NodeBuilder,
    paint::style::{PaintSpec, StylePatch},
};

fn painted_window() -> (Window, crate::node::id::NodeId) {
    let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
    let root = window.tree().root();
    let node = NodeBuilder::boxed()
        .layout_override(crate::math::Rect::new(0.0, 0.0, 40.0, 16.0))
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .build(window.tree_mut(), root)
        .expect("挂载成功");
    (window, node)
}

/// V-122 ⑧：提交判据的纯函数（不碰 GPU）。Full/Partial 有工作；Idle 只在目标尺寸变化时
/// 才有工作；没有 renderer 的 headless 帧恒有工作。
#[test]
fn frame_reaches_gpu_tracks_plan_and_target_size() {
    let mut rects = DamageRects::new();
    assert!(rects.push(PhysicalRect::new(0, 0, 4, 4)));
    let partial = Plan::Partial(rects);
    let surface = (100, 50);

    assert!(
        frame_reaches_gpu(Plan::Full, Some(surface), surface),
        "Full 一定有 GPU 工作"
    );
    assert!(
        frame_reaches_gpu(partial, Some(surface), surface),
        "Partial 一定有 GPU 工作"
    );
    assert!(
        frame_reaches_gpu(Plan::Idle, None, surface),
        "没有 renderer（headless / 纯 CPU 帧）：仍按 Committed 结算"
    );
    assert!(
        !frame_reaches_gpu(Plan::Idle, Some(surface), surface),
        "Idle 且目标尺寸没变 ⇒ 不 publish"
    );
    assert!(
        frame_reaches_gpu(Plan::Idle, Some((99, 50)), surface),
        "Idle 但目标尺寸变了 ⇒ GPU 线程仍要重建目标"
    );
}

/// P2-4：Windows 的目标尺寸变化必须变成非 `Idle` 计划 —— `set_content_size` 在 buffer 内
/// 只是 CPU 记账，`Idle` 发布会让 GPU 线程消费 waitable token 却不 present。
#[cfg(windows)]
#[test]
fn a_windows_surface_change_never_publishes_an_idle_plan() {
    let surface = (100, 50);
    assert_eq!(
        surface_change_plan(Plan::Idle, Some((99, 50)), surface),
        Plan::Full,
        "目标尺寸变了 ⇒ 整帧重画（新暴露的像素没有 damage）"
    );
    assert_eq!(
        surface_change_plan(Plan::Idle, Some(surface), surface),
        Plan::Idle,
        "尺寸没变 ⇒ 保持 Idle（不凭空产生 GPU 工作）"
    );
    assert_eq!(
        surface_change_plan(Plan::Idle, None, surface),
        Plan::Idle,
        "没有渲染器时由 frame_reaches_gpu 决定，不在这里造 Full"
    );

    let mut rects = DamageRects::new();
    assert!(rects.push(PhysicalRect::new(0, 0, 4, 4)));
    assert_eq!(
        surface_change_plan(Plan::Partial(rects), Some((99, 50)), surface),
        Plan::Full,
        "目标尺寸变了：Partial 的 damage 是按旧目标算的，必须整帧重画"
    );
    assert_eq!(
        surface_change_plan(Plan::Partial(rects), Some(surface), surface),
        Plan::Partial(rects),
        "尺寸没变：Partial 原样保留"
    );
    assert_eq!(
        surface_change_plan(Plan::Partial(rects), None, surface),
        Plan::Partial(rects),
        "没有渲染器：不在这里造 Full"
    );
    assert!(frame_reaches_gpu(
        surface_change_plan(Plan::Idle, Some((99, 50)), surface),
        Some((99, 50)),
        surface
    ));
}

/// P2-1 / V-122 ⑧（端到端）：真实 renderer 下「有像素但 `plan == Plan::Idle`、目标尺寸
/// 未变」的帧不 publish、不递增 `FrameIndex`，但 paint 已消费 lane 与 damage，必须像已提交
/// 帧一样 `settle_frame` 复位节点状态（否则后续 over-damage / a11y 多标）。
///
/// 构造：节点从一开始就在 800×600 目标之外（900,900 起）；先跑一帧结算 root 的初始布局
/// damage（首帧必定是全帧，会掩掉被测的 `Plan::Idle`），再只改节点颜色（PAINT-only，
/// 不冒泡到 root）。被测帧的 damage 只来自越界节点，被 `plan` 全部裁空 ⇒ `Plan::Idle`；
/// 离屏渲染器目标仍是 800×600 ⇒ 没有 GPU 工作。
///
/// 回归保护：旧实现只有 `!has_pixels` 一道门禁，本帧 `has_pixels == true`（paint 重建了
/// lane 并产出 damage），因此会返回 `Committed(FrameIndex(2))` 并推进序号 —— 本测试的
/// `Idle` / `FrameIndex(1)` 断言会失败。
#[cfg(windows)]
#[test]
fn an_idle_plan_with_a_real_renderer_settles_node_damage() {
    let _guard = crate::render::windows::device::gpu_test_lock();
    let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
    window
        .enable_offscreen_renderer()
        .expect("offscreen renderer");
    let root = window.tree().root();
    let node = NodeBuilder::boxed()
        .layout_override(crate::math::Rect::new(900.0, 900.0, 40.0, 16.0))
        .paint(PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        })
        .build(window.tree_mut(), root)
        .expect("挂载成功");
    let warmup = window.frame().expect("预热帧成功");
    assert_eq!(
        warmup,
        FrameOutcome::Committed(FrameIndex(1)),
        "预热帧用于结算 root 的初始 damage"
    );
    assert!(
        window
            .tree()
            .get(node)
            .expect("节点存在")
            .damage()
            .is_empty(),
        "预热帧提交后节点 damage 已复位"
    );

    // 只改颜色：PAINT-only，不触发 root 布局，因此本帧 damage 只来自越界节点。
    let patch = StylePatch {
        background: Some(Color::BLACK),
        ..StylePatch::default()
    };
    assert!(window.tree_mut().set_style_patch(node, patch));
    assert!(window.tree_mut().mark_paint(node));
    assert!(
        !window
            .tree()
            .get(node)
            .expect("节点存在")
            .damage()
            .is_empty(),
        "PAINT 变更必须让节点累积 damage（否则下面的复位断言没有意义）"
    );

    let outcome = window.frame().expect("帧成功");

    assert_eq!(outcome, FrameOutcome::Idle, "有像素但到不了 GPU ⇒ Idle");
    assert_eq!(
        window.frame_index(),
        FrameIndex(1),
        "Idle 不推进 FrameIndex"
    );
    assert!(
        window
            .tree()
            .get(node)
            .expect("节点存在")
            .damage()
            .is_empty(),
        "paint 已消费 damage：Idle 也必须结算节点（P2-1）"
    );
}

#[test]
fn frame_commits_once_and_increments_index() {
    let (mut window, _node) = painted_window();
    assert_eq!(window.frame_index().get(), 0);

    let outcome = window.frame().expect("帧成功");
    assert_eq!(outcome, FrameOutcome::Committed(FrameIndex(1)));
    assert_eq!(window.frame_index().get(), 1);
}

#[test]
fn idle_frame_does_not_commit_or_increment() {
    let (mut window, _node) = painted_window();
    let _ = window.frame().expect("首帧");
    assert_eq!(window.frame_index().get(), 1);

    let outcome = window.frame().expect("第二帧");
    assert_eq!(outcome, FrameOutcome::Idle);
    assert_eq!(window.frame_index().get(), 1, "空闲帧不得递增序号");
}

#[test]
fn paint_only_frame_skips_layout_and_reuses_lanes() {
    let (mut window, node) = painted_window();
    let _ = window.frame().expect("首帧");

    let patch = StylePatch {
        background: Some(Color::BLACK),
        ..StylePatch::default()
    };
    assert!(window.tree_mut().set_style_patch(node, patch));
    let _ = window.tree_mut().mark_paint(node);

    assert!(
        window.layout_if_dirty().expect("无错误").is_none(),
        "只改颜色不应触发布局"
    );
    let outcome = window.frame().expect("第二帧");
    assert!(matches!(outcome, FrameOutcome::Committed(_)));
}

/// P1-2.4 回归：帧内 panic 之后窗口必须还能出帧。
///
/// 旧实现里 `FrameTicket` 没有 `Drop`，panic 展开只是把它丢掉，`FrameScheduler` 永久停在
/// `Draining` —— `request_frame()` 之后恒 false，`App::frame` 永远返回 `Idle`，窗口冻结且
/// 无 API 可复位。这里用一个会 panic 的 measure 闭包复现，然后断言第二帧仍能提交。
#[test]
fn a_panicking_frame_does_not_freeze_the_scheduler() {
    let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
    let root = window.tree().root();
    let node = NodeBuilder::boxed()
        .measure(Box::new(
            |_: taffy::Size<Option<f32>>, _: taffy::Size<taffy::AvailableSpace>| {
                panic!("user measure closure exploded");
            },
        ))
        .build(window.tree_mut(), root)
        .expect("挂载成功");

    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| window.frame()));
    assert!(caught.is_err(), "闭包 panic 必须冒泡出 frame");
    assert_eq!(window.frame_index().get(), 0, "panic 帧不得递增序号");

    // 关键断言：调度器没有卡在 `Draining`。移除会 panic 的 measure 之后必须仍能出帧。
    let mut healthy = NodeBuilder::boxed();
    window.tree_mut().remove(node).expect("移除会 panic 的节点");
    healthy = healthy.paint(crate::paint::style::PaintSpec {
        background: crate::paint::color::Color::WHITE,
        ..crate::paint::style::PaintSpec::default()
    });
    let _ = healthy.build(window.tree_mut(), root).expect("健康节点");
    let outcome = window.frame();
    assert!(
        outcome.is_ok(),
        "panic 之后必须仍能出帧（否则窗口永久冻结）：{outcome:?}"
    );
}

#[test]
fn frame_aborts_on_layout_error() {
    let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
    let root = window.tree().root();
    let _ = NodeBuilder::boxed()
        .measure(Box::new(
            |_: taffy::Size<Option<f32>>, _: taffy::Size<taffy::AvailableSpace>| {
                Size::new(f32::NAN, 10.0)
            },
        ))
        .build(window.tree_mut(), root)
        .expect("挂载成功");

    let outcome = window.frame();
    assert!(matches!(
        outcome,
        Err(crate::error::EngineError::InvalidMeasure { .. })
    ));
    assert_eq!(window.frame_index().get(), 0, "中止不得递增序号");
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "use App::frame")]
fn frame_rejects_bound_nodes_without_styles() {
    let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
    let root = window.tree().root();
    let _ = NodeBuilder::boxed()
        .layout_override(crate::math::Rect::new(0.0, 0.0, 10.0, 10.0))
        .bind_style(|_cx| StylePatch::default())
        .build(window.tree_mut(), root)
        .expect("挂载成功");

    let _ = window.frame();
}
