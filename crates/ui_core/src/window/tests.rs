#[cfg(windows)]
use super::frame::surface_change_plan;
use super::{
    Decorations, FrameIndex, FrameOutcome, FramePresentation, Window, WindowId, WindowOptions,
    WindowState, frame::frame_reaches_gpu, surface::covering_surface,
};
use crate::{
    app::App,
    error::EngineError,
    math::{PhysicalRect, Point, Rect, Scale, Size},
    node::{builder::NodeBuilder, dirty::DirtyFlags, id::NodeId},
    paint::{canvas::Canvas, color::Color, style::Style, style_builder::StyleBuilder},
    render::compositor::{DamageRects, Plan},
    text::{InlineAlign, InlineBoxSpec, TextAlign, TextContent, TextStyle},
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
fn dpi_change_does_not_invalidate_layout() {
    let mut window = window();
    let root = window.tree().root();
    let _ = window.compute_layout().expect("布局成功");
    window.tree_mut().clear_all_dirty_for_test();
    let bounds_before = window.tree().get(root).expect("root 存在").layout().bounds;
    let epoch = window.scale_epoch();

    window.set_scale(Scale::new(2.0).expect("合法比例"));

    let flags = window.tree().get(root).expect("root 存在").dirty();
    assert!(
        !flags.contains(DirtyFlags::LAYOUT),
        "DPI 变化不得标根 LAYOUT（实际 {flags:?}）"
    );
    assert!(
        !window.tree().has_layout_work(),
        "DPI 变化不得留下待布局工作"
    );
    assert_eq!(
        window.tree().get(root).expect("root 存在").layout().bounds,
        bounds_before,
        "DPI 变化不得改变 layout bounds"
    );
    assert!(window.full_damage_pending, "DPI 变化必须标整帧重绘");
    assert_eq!(window.scale_epoch(), epoch + 1, "scale_epoch 精确递增 1");
}

#[test]
fn window_options_default_values() {
    let options = WindowOptions::default();

    assert_eq!(options.title, "");
    assert_eq!(options.size.width, 800.0);
    assert_eq!(options.size.height, 600.0);
    assert_eq!(
        options.position, None,
        "默认位置由平台决定（Windows 屏幕中央附近 / macOS 级联 / Wayland compositor）"
    );
    assert_eq!(options.min_size, None, "默认无最小尺寸约束");
    assert_eq!(options.max_size, None, "默认无最大尺寸约束");
    assert_eq!(options.owner, None, "默认没有 owner（独立顶层窗口）");
    assert!(!options.modal, "默认不是模态窗口");
    assert!(!options.always_on_top, "默认不置顶");
    assert_eq!(options.scale, Scale::ONE);
    assert_eq!(
        options.background,
        Color::BLACK,
        "默认背景必须是不透明黑（Color::default() 是透明）"
    );
}

#[test]
fn drag_regions_are_stored_and_cleared_by_an_empty_slice() {
    let mut window = window();
    assert!(window.drag_regions().is_empty(), "默认没有拖动区");

    let strip = Rect::new(0.0, 0.0, 800.0, 38.0);
    let handle = Rect::new(0.0, 38.0, 800.0, 8.0);
    window.set_drag_regions(&[strip, handle]);
    assert_eq!(
        window.drag_regions(),
        &[strip, handle],
        "区域按逻辑窗口坐标原样保存（含顺序）"
    );

    window.set_drag_regions(&[strip, handle]);
    assert_eq!(window.drag_regions(), &[strip, handle]);

    window.set_drag_regions(&[]);
    assert!(window.drag_regions().is_empty(), "空切片 = 清空拖动区");
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

    window.full_damage_pending = false;
    window.set_background_color(Color::WHITE);
    assert_eq!(window.options().background, Color::WHITE);
    assert!(!window.full_damage_pending, "同值 no-op 不得标 damage");
}

#[cfg(all(windows, feature = "test-support"))]
#[test]
fn a_committed_frame_uses_the_platform_surface_not_the_covering_size() {
    let (mut window, _node) = painted_window();
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

#[cfg(target_os = "linux")]
#[test]
fn frame_rect_uses_the_platform_buffer_on_linux() {
    let mut window = window();
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 800, 600));
    window.set_surface((999, 777));
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 999, 777));

    window.set_size(Size::new(256.8, 100.5));
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 999, 777));
    window.set_surface((449, 176));
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 449, 176));

    window.set_surface((0, 0));
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 1, 1));
}

#[cfg(target_os = "macos")]
#[test]
fn frame_rect_uses_the_platform_backing_on_macos() {
    let mut window = window();
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 800, 600));
    window.set_surface((1234, 777));
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 1234, 777));

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

    window.set_size(Size::new(256.8, 100.5));
    assert_eq!(window.physical_surface(), (450, 176));
    assert_eq!(
        window.physical_surface(),
        covering_surface(window.size(), window.scale())
    );
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
        !window
            .tree()
            .get(root)
            .expect("root 存在")
            .dirty()
            .contains(DirtyFlags::LAYOUT),
        "DPI 变化不得标根 LAYOUT"
    );
    assert_eq!(window.scale_epoch(), epoch + 1, "scale_epoch 照常递增");

    window.set_surface((449, 176));
    assert_eq!(window.frame_rect(), PhysicalRect::new(0, 0, 449, 176));

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
fn dpi_change_does_not_move_logical_boxes() {
    use crate::{
        math::Rect,
        node::builder::NodeBuilder,
        paint::{style::Style, style_builder::StyleBuilder},
    };

    fn fixed(width: f32, height: f32) -> Style {
        StyleBuilder::new().size(width, height).build()
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
    assert_eq!(before, Rect::new(0.0, 0.0, 10.5, 3.25));

    for factor in [1.5f32, 2.0] {
        window.set_scale(Scale::new(factor).expect("合法比例"));
        assert!(
            window.tree_mut().invalidate_layout(root),
            "前置：必须真的请求重排，否则缓存命中下测不到 DPI 相关 measure"
        );
        let report = window.compute_layout().expect("布局成功");
        assert!(report.laid_out > 0, "DPI {factor} 下必须真的重排过");

        let after = window.tree().get(child).expect("存在").layout().bounds;
        assert_eq!(
            after, before,
            "DPI {factor} 下 layout bounds 必须逐位不变（{after:?} vs {before:?}）"
        );
    }
}

#[test]
fn layout_is_identical_at_every_device_scale() {
    use crate::{
        node::builder::NodeBuilder,
        paint::{style::Style, style_builder::StyleBuilder},
    };

    fn fixed(width: f32, height: f32) -> Style {
        StyleBuilder::new().size(width, height).build()
    }

    let mut window = Window::new(
        WindowId::for_test(1),
        WindowOptions {
            size: Size::new(120.0, 60.0),
            ..WindowOptions::default()
        },
    );
    let root = window.tree().root();
    let mut column = fixed(100.0, 50.0);
    column.flex_direction(taffy::style::FlexDirection::Column);
    let column = NodeBuilder::boxed()
        .style(column)
        .build(window.tree_mut(), root)
        .expect("挂载成功");
    let child = NodeBuilder::boxed()
        .style(fixed(10.5, 3.25))
        .build(window.tree_mut(), column)
        .expect("挂载成功");
    let _ = window.compute_layout().expect("布局成功");

    let snapshot = |window: &Window| -> Vec<crate::node::tree::LayoutBox> {
        [root, column, child]
            .map(|node| window.tree().get(node).expect("存在").layout())
            .to_vec()
    };
    let reference = snapshot(&window);

    for factor in [1.0f32, 1.25, 1.5, 1.75, 2.0, 3.0] {
        window.set_scale(Scale::new(factor).expect("合法比例"));
        let root = window.tree().root();
        assert!(
            window.tree_mut().invalidate_layout(root),
            "前置：必须真的请求重排，否则测不到 DPI 相关的 measure"
        );
        let report = window.compute_layout().expect("布局成功");
        assert!(report.laid_out > 0, "DPI {factor} 下必须真的重排过");

        assert_eq!(
            snapshot(&window),
            reference,
            "DPI {factor} 下 LayoutBox 必须逐位相同"
        );
    }
}

#[test]
fn resize_relayouts_percent_children() {
    use crate::{node::builder::NodeBuilder, paint::style_builder::StyleBuilder};

    let mut window = Window::new(
        WindowId::for_test(1),
        WindowOptions {
            size: Size::new(100.0, 100.0),
            ..WindowOptions::default()
        },
    );
    let root = window.tree().root();
    let style = StyleBuilder::new().size_full().build();
    let child = NodeBuilder::boxed()
        .style(style)
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

use crate::paint::style::{PaintSpec, StylePatch};

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

#[test]
fn window_frame_reports_skipped_inline_box_images() {
    use crate::text::{
        InlineAlign, InlineBoxPaint, InlineBoxSpec, ShapedInlineBox, ShapedLine, ShapedTextItem,
        ShapedTextLine, TextContent, TextDiagnostic, TextStyle,
    };

    let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
    let spec = InlineBoxSpec {
        id: 21,
        index: 1,
        size: Size::new(8.0, 8.0),
        baseline: None,
        align: InlineAlign::Baseline,
        paint: Some(InlineBoxPaint::Image {
            resource: crate::resources::ResourceId::for_test(4242, 1),
            modulation: Color::WHITE,
        }),
    };
    let content = TextContent::new("a b", TextStyle::new(16.0)).with_inline_boxes([spec]);
    let shaped_line = ShapedLine {
        text: smol_str::SmolStr::new_static("a b"),
        lines: vec![ShapedTextLine {
            text_range: 0..3,
            baseline: 12.0,
            line_height: 16.0,
            advance: 20.0,
            items: vec![ShapedTextItem::InlineBox(ShapedInlineBox {
                id: 21,
                rect: Rect::new(2.0, 2.0, 8.0, 8.0),
                baseline: None,
            })],
            decorations: Vec::new(),
        }],
        width: 20.0,
        height: 16.0,
        first_baseline: Some(12.0),
        atlas_generation: 0,
        font_stack_version: 1,
        scale: 1.0,
    };
    let node = {
        let root = window.tree().root();
        let node = NodeBuilder::text("a b", TextStyle::new(16.0))
            .layout_override(Rect::new(0.0, 0.0, 40.0, 16.0))
            .build(window.tree_mut(), root)
            .expect("挂载成功");
        let tree = window.tree_mut();
        assert!(tree.set_text_content(node, content.clone()), "文本载荷");
        assert!(
            tree.install_text_line_for_test(node, &content, std::rc::Rc::new(shaped_line), 40.0),
            "注入行快照"
        );
        node
    };
    let outcome = window.frame().expect("帧成功");
    assert!(matches!(outcome, FrameOutcome::Committed(_)));
    assert_eq!(
        window.inline_box_image_diagnostics(),
        [TextDiagnostic::InlineBoxImageMissing { id: 21 }],
        "Window::frame 路径的图片跳过必须可观测（D17）"
    );
    let _ = node;
}

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

#[test]
fn frame_presentations_are_recorded_and_bounded() {
    let mut window = window();
    assert!(
        window.last_frame_presentation().is_none(),
        "没有完成帧时为空"
    );
    let base = std::time::Instant::now();
    for value in 1..=(super::FRAME_PRESENTATION_HISTORY as u32 + 3) {
        window.note_frame_presentation(FramePresentation {
            index: FrameIndex::from_raw(value),
            published_at: base,
            completed_at: base + std::time::Duration::from_millis(1),
            presented_at: Some(base + std::time::Duration::from_millis(3)),
            latency: std::time::Duration::from_millis(1),
        });
    }
    let recent: Vec<u32> = window
        .recent_frame_presentations()
        .map(|presentation| presentation.index.get())
        .collect();
    assert_eq!(recent.len(), super::FRAME_PRESENTATION_HISTORY, "历史有界");
    assert_eq!(recent.last().copied(), Some(11), "保留最新帧");
    assert_eq!(
        window
            .last_frame_presentation()
            .expect("最新快照")
            .publish_to_presented(),
        Some(std::time::Duration::from_millis(3)),
        "publish → presented（下界）"
    );
    window.note_frame_presented(
        FrameIndex::from_raw(10),
        Some(base + std::time::Duration::from_millis(9)),
    );
    let updated = window
        .recent_frame_presentations()
        .find(|presentation| presentation.index.get() == 10)
        .expect("10 仍在历史里");
    assert_eq!(
        updated.completion_to_presented(),
        Some(std::time::Duration::from_millis(8))
    );
    window.note_frame_presented(FrameIndex::from_raw(1), Some(base));
    assert!(
        window
            .recent_frame_presentations()
            .all(|presentation| presentation.index.get() != 1),
        "被历史挤掉的帧不会被复活"
    );
}

#[test]
fn decorations_drive_the_initial_client_decoration_state() {
    let native = window();
    assert_eq!(
        native.options().decorations,
        Decorations::Native,
        "WindowOptions 默认必须是 Native（既有行为）"
    );
    assert!(!native.state().client_decoration, "Native ⇒ 系统装饰");
    assert!(!native.state().maximized);
    assert!(!native.state().fullscreen);
    assert!(native.state().can_maximize);
    assert!(native.state().can_minimize);
    if cfg!(windows) {
        assert!(
            !native.state().can_fullscreen,
            "Windows 没有全屏控制：初值就必须是 false（首轮泵不产生校正性状态变化）"
        );
    } else {
        assert!(native.state().can_fullscreen, "macOS/Linux 原生支持全屏");
    }

    let frameless = Window::new(
        WindowId::for_test(2),
        WindowOptions {
            decorations: Decorations::None,
            ..WindowOptions::default()
        },
    );
    if cfg!(target_os = "macos") {
        assert!(
            !frameless.state().client_decoration,
            "macOS 恒由系统装饰（原生交通灯）：Decorations::None 是文档化降级"
        );
    } else {
        assert!(frameless.state().client_decoration, "None ⇒ 应用自绘装饰");
    }
}

#[test]
fn set_state_is_idempotent_and_marks_the_root_for_paint_when_it_changes() {
    let mut window = window();
    let root = window.tree().root();
    window.tree_mut().clear_paint(root);
    assert!(!window.tree().has_paint_work(), "前置：根节点已清脏");

    let unchanged = window.state();
    assert!(!window.set_state(unchanged), "同值必须是 no-op");
    assert!(!window.tree().has_paint_work(), "同值不得标 PAINT");

    let changed = WindowState {
        maximized: true,
        ..unchanged
    };
    assert!(window.set_state(changed), "状态变化必须报告 true");
    assert!(
        window.tree().has_paint_work(),
        "状态变化必须给根节点标 PAINT（I3：走与应用写信号同一条失效路径）"
    );
    assert!(window.state().maximized);
}

#[test]
fn window_options_carry_an_optional_icon() {
    assert_eq!(WindowOptions::default().icon, None);
    let options = WindowOptions {
        icon: Some(bytes::Bytes::from_static(&[1, 2, 3])),
        ..WindowOptions::default()
    };
    assert_eq!(options.clone(), options);
    assert!(options.icon.is_some());
}

fn text_style(width: f32) -> Style {
    let mut style = StyleBuilder::new()
        .padding_sides(7.0, 0.0, 0.0, 9.0)
        .build();
    style
        .align_self(taffy::style::AlignSelf::START)
        .width(taffy::style::Dimension::length(width))
        .border_sides(3.0, 0.0, 0.0, 2.0);
    style
}

fn text_window(text: &str, width: f32) -> (tokio::runtime::Runtime, App, WindowId, NodeId) {
    text_window_with_style(text, width, TextStyle::new(16.0))
}

fn text_window_with_style(
    text: &str,
    width: f32,
    content_style: TextStyle,
) -> (tokio::runtime::Runtime, App, WindowId, NodeId) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime");
    let mut app = App::new(rt.handle().clone());
    let window = app.create_window(WindowOptions::default());
    let node = {
        let w = app.window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        NodeBuilder::text(text, content_style)
            .style(text_style(width))
            .build(w.tree_mut(), root)
            .expect("挂载文本节点")
    };
    (rt, app, window, node)
}

fn commit_frame(app: &mut App, window: WindowId) {
    let outcome = app.frame(window).expect("窗口存在").expect("帧成功");
    assert!(matches!(outcome, FrameOutcome::Committed(_)), "{outcome:?}");
}

fn assert_caret_round_trip(app: &App, window: WindowId, node: NodeId, byte_index: usize) {
    let window = app.window(window).expect("窗口存在");
    let caret = window
        .text_caret_rect(node, byte_index)
        .expect("caret 矩形");
    let point = Point::new(
        caret.origin.x + caret.size.width * 0.5,
        caret.origin.y + caret.size.height * 0.5,
    );
    let hit = window.tree().hit_test(point).expect("caret 点必须命中");
    assert_eq!(hit.node, node, "fixture 必须让文本节点成为最上层命中目标");
    let index = window
        .text_byte_index_at(node, hit.local_point)
        .expect("点 → 字节索引");
    assert_eq!(
        index, byte_index,
        "caret({byte_index}) 的窗口点 {hit:?} 往返后必须是 {byte_index}"
    );
}

#[test]
fn text_byte_index_at_round_trips_with_caret_rect() {
    let (_rt, mut app, window, node) = text_window("hello world", 200.0);
    commit_frame(&mut app, window);
    let window_ref = app.window(window).expect("窗口存在");

    let bounds = window_ref.tree().get(node).expect("节点").layout().bounds;
    let content_offset = window_ref.tree().content_offset(node);
    assert!(content_offset.x > 0.0 && content_offset.y > 0.0);
    let caret = window_ref.text_caret_rect(node, 0).expect("caret");
    assert!(
        (caret.origin.x - bounds.origin.x - content_offset.x).abs() < 0.01,
        "caret 绝对 x 必须含 border + padding：{caret:?} vs {content_offset:?}"
    );
    assert_eq!(
        window_ref
            .text_byte_index_at(node, Point::new(content_offset.x, content_offset.y))
            .expect("盒首"),
        0,
        "文本盒左上角 → 0"
    );

    for index in [0usize, 1, 5, 11] {
        assert_caret_round_trip(&app, window, node, index);
    }
}

#[test]
fn text_byte_index_at_round_trips_the_numeric_fast_path() {
    let (_rt, mut app, window, node) = text_window("1234567890", 200.0);
    let report = app
        .refresh_text(window)
        .expect("窗口存在")
        .expect("文本 pass");
    assert!(
        report.fast_path_shaped >= 1,
        "fixture 必须命中数字快速路径（报告：{report:?}）"
    );
    commit_frame(&mut app, window);
    let window_ref = app.window(window).expect("窗口存在");

    for index in [0usize, 1, 5, 10] {
        assert_caret_round_trip(&app, window, node, index);
    }

    let bounds = window_ref.tree().get(node).expect("节点").layout().bounds;
    let caret = window_ref.text_caret_rect(node, 0).expect("caret");
    let line_center_y = (caret.origin.y - bounds.origin.y) + caret.size.height * 0.5;
    assert_eq!(
        window_ref
            .text_byte_index_at(node, Point::new(0.0, -1.0))
            .expect("盒上方"),
        0
    );
    assert_eq!(
        window_ref
            .text_byte_index_at(node, Point::new(0.0, bounds.size.height + 50.0))
            .expect("盒下方"),
        "1234567890".len()
    );
    assert_eq!(
        window_ref
            .text_byte_index_at(node, Point::new(bounds.size.width + 100.0, line_center_y))
            .expect("行右越界"),
        "1234567890".len()
    );
}

#[test]
fn text_byte_index_at_round_trips_every_line_of_wrapped_text() {
    let (_rt, mut app, window, node) = text_window("alpha beta gamma delta", 70.0);
    commit_frame(&mut app, window);
    let window_ref = app.window(window).expect("窗口存在");
    let line_count = window_ref
        .tree()
        .text_line(node)
        .expect("文本 pass 必须已经建好 ShapedLine")
        .lines
        .len();
    assert!(line_count >= 3, "fixture 必须折成多行，实际 {line_count}");

    let mut seen_lines: Vec<i32> = Vec::new();
    let bounds_origin_y = window_ref
        .tree()
        .get(node)
        .expect("节点")
        .layout()
        .bounds
        .origin
        .y;
    for index in 0..="alpha beta gamma delta".len() {
        let caret = window_ref.text_caret_rect(node, index).expect("caret");
        let line_key = (caret.origin.y - bounds_origin_y).round() as i32;
        if seen_lines.contains(&line_key) {
            continue;
        }
        seen_lines.push(line_key);
        assert_caret_round_trip(&app, window, node, index);
    }
    assert_eq!(seen_lines.len(), line_count, "每行至少要抽样一个行首");
}

#[test]
fn text_byte_index_at_clamps_in_border_box_local_coordinates() {
    let (_rt, mut app, window, node) = text_window("abcdef", 200.0);
    commit_frame(&mut app, window);
    let window_ref = app.window(window).expect("窗口存在");
    let bounds = window_ref.tree().get(node).expect("节点").layout().bounds;
    let content_offset = window_ref.tree().content_offset(node);
    let caret = window_ref.text_caret_rect(node, 0).expect("caret");
    let line_center_y = content_offset.y + caret.size.height * 0.5;

    assert_eq!(
        window_ref
            .text_byte_index_at(node, Point::new(0.0, 0.0))
            .expect("盒上方"),
        0,
        "border box 顶部在文本盒上方 ⇒ 0"
    );
    assert_eq!(
        window_ref
            .text_byte_index_at(node, Point::new(0.0, bounds.size.height + 50.0))
            .expect("盒下方"),
        6,
        "border box 下方 ⇒ text.len()"
    );
    assert_eq!(
        window_ref
            .text_byte_index_at(node, Point::new(-100.0, line_center_y))
            .expect("行左越界"),
        0
    );
    assert_eq!(
        window_ref
            .text_byte_index_at(node, Point::new(bounds.size.width + 100.0, line_center_y))
            .expect("行右越界"),
        6
    );
    assert_eq!(
        window_ref
            .text_byte_index_at(node, Point::new(-100.0, -100.0))
            .expect("负坐标"),
        0
    );
}

#[test]
fn text_byte_index_at_never_splits_cjk_or_emoji_clusters() {
    let text = "a中b🌍";
    let (_rt, mut app, window, node) = text_window(text, 200.0);
    commit_frame(&mut app, window);
    let window_ref = app.window(window).expect("窗口存在");
    let bounds = window_ref.tree().get(node).expect("节点").layout().bounds;
    let caret_zero = window_ref.text_caret_rect(node, 0).expect("caret");
    let line_center_y = (caret_zero.origin.y - bounds.origin.y) + caret_zero.size.height * 0.5;
    let local_x = |index: usize| {
        window_ref
            .text_caret_rect(node, index)
            .expect("caret")
            .origin
            .x
            - bounds.origin.x
    };

    for index in [0usize, 1, 4, 5, 9] {
        assert_caret_round_trip(&app, window, node, index);
    }

    let cjk_middle = Point::new((local_x(1) + local_x(4)) * 0.5, line_center_y);
    let cjk_index = window_ref
        .text_byte_index_at(node, cjk_middle)
        .expect("CJK 中点");
    assert!(
        cjk_index == 1 || cjk_index == 4,
        "CJK 簇中点必须吸附到边界，实际 {cjk_index}"
    );
    let emoji_middle = Point::new((local_x(5) + local_x(9)) * 0.5, line_center_y);
    let emoji_index = window_ref
        .text_byte_index_at(node, emoji_middle)
        .expect("emoji 中点");
    assert!(
        emoji_index == 5 || emoji_index == 9,
        "emoji 簇中点必须吸附到边界，实际 {emoji_index}"
    );
    for index in [cjk_index, emoji_index] {
        assert!(text.is_char_boundary(index), "返回 {index} 不是合法边界");
    }
}

#[test]
fn text_byte_index_at_rejects_non_text_and_unshaped_nodes() {
    let (_rt, mut app, window, node) = text_window("abc", 100.0);
    let point = Point::new(1.0, 1.0);

    assert!(matches!(
        app.window(window)
            .expect("窗口存在")
            .text_byte_index_at(node, point),
        Err(EngineError::StaleNode { node: reported }) if reported == node
    ));

    commit_frame(&mut app, window);
    let root = app.window(window).expect("窗口").tree().root();
    assert!(matches!(
        app.window(window)
            .expect("窗口存在")
            .text_byte_index_at(root, point),
        Err(EngineError::StaleNode { node: reported }) if reported == root
    ));

    let (canvas, removed) = {
        let w = app.window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        let canvas = NodeBuilder::canvas(Canvas::default())
            .build(w.tree_mut(), root)
            .expect("挂载 Canvas");
        let removed = NodeBuilder::boxed()
            .build(w.tree_mut(), root)
            .expect("挂载盒子");
        assert!(w.tree_mut().remove(removed).is_ok(), "卸载盒子");
        (canvas, removed)
    };
    let window_ref = app.window(window).expect("窗口存在");
    assert!(matches!(
        window_ref.text_byte_index_at(canvas, point),
        Err(EngineError::StaleNode { node: reported }) if reported == canvas
    ));
    assert!(matches!(
        window_ref.text_byte_index_at(removed, point),
        Err(EngineError::StaleNode { node: reported }) if reported == removed
    ));
}

#[test]
fn text_byte_index_at_is_read_only() {
    let (_rt, mut app, window, node) = text_window("read only", 120.0);
    commit_frame(&mut app, window);
    let requests_before = app.frame_requests();
    let dirty_before = app
        .window(window)
        .expect("窗口存在")
        .tree()
        .get(node)
        .expect("节点")
        .dirty();

    {
        let window_ref = app.window(window).expect("窗口存在");
        for _ in 0..4 {
            let index = window_ref
                .text_byte_index_at(node, Point::new(5.0, 12.0))
                .expect("命中");
            assert!(index <= "read only".len());
        }
    }

    assert_eq!(
        app.frame_requests(),
        requests_before,
        "I6：点 → 字节索引不得请求帧"
    );
    let tree = app.window(window).expect("窗口存在").tree();
    assert_eq!(
        tree.get(node).expect("节点").dirty(),
        dirty_before,
        "I6：点 → 字节索引不得改脏位"
    );
    assert!(!tree.has_paint_work(), "I6：不得留下待绘制工作");
}

fn assert_stale_byte_index_at(app: &App, window: WindowId, node: NodeId) {
    let window_ref = app.window(window).expect("窗口存在");
    assert!(
        matches!(
            window_ref.text_byte_index_at(node, Point::new(1.0, 1.0)),
            Err(EngineError::StaleNode { node: reported }) if reported == node
        ),
        "文本 pass 之后输入变了却返回旧快照索引（必须 StaleNode）"
    );
}

fn commit_frame_after_repaint(app: &mut App, window: WindowId) {
    app.request_repaint(window).expect("窗口存在");
    commit_frame(app, window);
}

#[test]
fn text_byte_index_at_rejects_text_changed_after_the_text_pass() {
    let (_rt, mut app, window, node) = text_window("abcdef", 200.0);
    commit_frame(&mut app, window);
    {
        let w = app.window_mut(window).expect("窗口存在");
        assert!(
            w.tree_mut()
                .set_text_content(node, TextContent::new("abc", TextStyle::new(16.0))),
            "替换文本载荷"
        );
    }
    assert_stale_byte_index_at(&app, window, node);

    commit_frame_after_repaint(&mut app, window);
    let index = app
        .window(window)
        .expect("窗口存在")
        .text_byte_index_at(node, Point::new(1.0, 1.0))
        .expect("新文本 pass 后必须可命中");
    assert!(index <= 3, "命中不得越过新文本长度，实际 {index}");
}

#[test]
fn text_byte_index_at_rejects_style_changed_after_the_text_pass() {
    let (_rt, mut app, window, node) = text_window("abcdef", 200.0);
    commit_frame(&mut app, window);
    {
        let w = app.window_mut(window).expect("窗口存在");
        assert!(
            w.tree_mut()
                .set_text_content(node, TextContent::new("abcdef", TextStyle::new(24.0))),
            "替换文本样式"
        );
    }
    assert_stale_byte_index_at(&app, window, node);

    commit_frame_after_repaint(&mut app, window);
    assert!(
        app.window(window)
            .expect("窗口存在")
            .text_byte_index_at(node, Point::new(1.0, 1.0))
            .is_ok(),
        "新样式完成文本 pass 后必须可命中"
    );
}

#[test]
fn text_byte_index_at_rejects_inline_boxes_changed_after_the_text_pass() {
    let (_rt, mut app, window, node) = text_window("abcdef", 200.0);
    commit_frame(&mut app, window);
    let spec = InlineBoxSpec {
        id: 31,
        index: 1,
        size: Size::new(8.0, 8.0),
        baseline: None,
        align: InlineAlign::Baseline,
        paint: None,
    };
    {
        let w = app.window_mut(window).expect("窗口存在");
        assert!(
            w.tree_mut().set_text_content(
                node,
                TextContent::new("abcdef", TextStyle::new(16.0)).with_inline_boxes([spec]),
            ),
            "追加行内盒"
        );
    }
    assert_stale_byte_index_at(&app, window, node);

    commit_frame_after_repaint(&mut app, window);
    assert!(
        app.window(window)
            .expect("窗口存在")
            .text_byte_index_at(node, Point::new(1.0, 1.0))
            .is_ok(),
        "行内盒完成文本 pass 后必须可命中"
    );
}

#[test]
fn text_byte_index_at_rejects_layout_width_changed_after_the_text_pass() {
    let (_rt, mut app, window, node) = text_window("abcdef", 200.0);
    commit_frame(&mut app, window);
    {
        let w = app.window_mut(window).expect("窗口存在");
        assert!(
            w.tree_mut().set_style(node, text_style(120.0)).is_ok(),
            "改窄文本节点"
        );
        w.compute_layout().expect("布局");
    }
    assert_stale_byte_index_at(&app, window, node);

    commit_frame_after_repaint(&mut app, window);
    assert!(
        app.window(window)
            .expect("窗口存在")
            .text_byte_index_at(node, Point::new(1.0, 1.0))
            .is_ok(),
        "按新宽度重排后必须可命中"
    );
}

#[test]
fn text_byte_index_at_guards_the_numeric_fast_path_alignment_offset() {
    for alignment in [TextAlign::Center, TextAlign::Right] {
        let (_rt, mut app, window, node) = text_window_with_style(
            "1234567890",
            200.0,
            TextStyle::new(16.0).with_alignment(alignment),
        );
        let report = app
            .refresh_text(window)
            .expect("窗口存在")
            .expect("文本 pass");
        assert!(
            report.fast_path_shaped >= 1,
            "{alignment:?} fixture 必须命中数字快速路径（报告：{report:?}）"
        );
        commit_frame(&mut app, window);
        let window_ref = app.window(window).expect("窗口存在");
        let bounds = window_ref.tree().get(node).expect("节点").layout().bounds;
        let content_offset = window_ref.tree().content_offset(node);
        let caret = window_ref.text_caret_rect(node, 0).expect("caret");
        assert!(
            caret.origin.x - bounds.origin.x - content_offset.x > 1.0,
            "{alignment:?} 的快速路径对齐偏移必须非零（否则本用例测不到 offset）"
        );

        let last = if matches!(alignment, TextAlign::Right) {
            9
        } else {
            10
        };
        for index in 0..=last {
            assert_caret_round_trip(&app, window, node, index);
        }
    }
}

#[test]
fn text_byte_index_at_returns_zero_for_empty_text_at_window_level() {
    let (_rt, mut app, window, node) = text_window("", 100.0);
    commit_frame(&mut app, window);
    let window_ref = app.window(window).expect("窗口存在");
    for point in [
        Point::new(0.0, 0.0),
        Point::new(-5.0, -5.0),
        Point::new(500.0, 500.0),
        Point::new(1.0, 1.0),
    ] {
        assert_eq!(
            window_ref
                .text_byte_index_at(node, point)
                .expect("空文本必须可命中"),
            0,
            "{point:?}"
        );
    }
}
