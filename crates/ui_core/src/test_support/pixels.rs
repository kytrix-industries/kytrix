use crate::{
    app::App,
    error::{EngineError, PixelHarnessCause},
    math::{DevicePixelRatio, DeviceRect, LogicalPoint, LogicalRect, dp},
    paint::atlas::AtlasKind,
    render::{
        DirtyRects,
        backend::compositor::GpuRenderer,
        compositor::{FramePlan, Plan, plan},
        damage::DamageSummary,
        delta::{ScrollDelta, ScrollTable},
        frame::FramePayload,
        timer::FrameTimings,
    },
    test_support::{
        record::FrameRecord,
        snapshot::{SnapshotError, SnapshotImage},
    },
    window::{FrameIndex, Window, WindowId},
};

pub type ScrollSlot = (LogicalPoint, DeviceRect);

pub fn render_offscreen(
    app: &App,
    window: &Window,
    size: (u32, u32),
    scale: DevicePixelRatio,
    scroll: &[ScrollSlot],
) -> Result<SnapshotImage, EngineError> {
    let (offsets, clip) = scroll_payload(scroll, scale);
    let mut renderer = offscreen_gpu(size)?;
    upload_atlases(app, &mut renderer);
    let buffer = inline_buffer(window)?;
    let payload = FramePayload::full(buffer, offsets, clip);
    let mut frame = FramePlan::new(Plan::Full, true);
    let mut timings = FrameTimings::new();
    renderer.submit_snapshot(FrameIndex::from_raw(1), &payload, &mut frame, &mut timings)?;
    readback(&mut renderer)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PixelFrame {
    Idle,
    Full(SnapshotImage),
    Incremental {
        pixels: SnapshotImage,
        damage: Vec<DeviceRect>,
    },
}

pub struct PersistentPixels {
    renderer: GpuRenderer,
    surface: (u32, u32),
    bounds: DeviceRect,
}

impl PersistentPixels {
    pub fn new(app: &App, window: WindowId) -> Result<Self, EngineError> {
        let target = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let surface = covering_size(target)?;
        Ok(Self {
            renderer: offscreen_gpu(surface)?,
            surface,
            bounds: surface_bounds(surface),
        })
    }

    pub fn present(&mut self, app: &App, window: WindowId) -> Result<PixelFrame, EngineError> {
        let target = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        let record = target.record().ok_or(EngineError::PixelHarness {
            cause: PixelHarnessCause::NoCommittedFrame,
        })?;
        self.present_record(app, window, record)
    }

    pub fn present_record(
        &mut self,
        app: &App,
        window: WindowId,
        record: &FrameRecord,
    ) -> Result<PixelFrame, EngineError> {
        let target = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?;
        if !record.rendered {
            return Ok(PixelFrame::Idle);
        }
        if covering_size(target)? != self.surface {
            return Err(EngineError::PixelHarness {
                cause: PixelHarnessCause::SurfaceResized,
            });
        }
        let buffer = inline_buffer(target)?;
        let damage = record.damage.clone();
        let plan = if record.damage_full {
            Plan::Full
        } else {
            plan(
                DamageSummary {
                    full: false,
                    rects: damage.len(),
                },
                &damage,
                self.bounds,
            )
        };
        upload_atlases(app, &mut self.renderer);
        let (offsets, clip) = scroll_payload(target.scroll_slots(), target.device_pixel_ratio());
        let payload = FramePayload::full(buffer, offsets, clip);
        let fresh = self.renderer.is_fresh();
        let mut frame = FramePlan::new(plan, fresh);
        let mut timings = FrameTimings::new();
        self.renderer
            .submit_snapshot(record.frame_index, &payload, &mut frame, &mut timings)?;
        let pixels = readback(&mut self.renderer)?;
        Ok(match (fresh, plan) {
            (false, Plan::Partial(_)) => PixelFrame::Incremental { pixels, damage },
            _ => PixelFrame::Full(pixels),
        })
    }

    pub fn readback(&mut self) -> Result<SnapshotImage, EngineError> {
        readback(&mut self.renderer)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PixelDiff {
    pub span: u8,
    pub at: (u32, u32),
}

#[must_use]
pub fn compare_pixels(
    reference: &SnapshotImage,
    actual: &SnapshotImage,
    excluded: &[DeviceRect],
) -> PixelDiff {
    assert_eq!(
        (reference.width, reference.height),
        (actual.width, actual.height),
        "pixel comparison needs matching readback sizes"
    );
    let excluded: Vec<(u32, u32, u32, u32)> = excluded
        .iter()
        .map(|rect| {
            (
                rect.x0.value().max(0) as u32,
                rect.y0.value().max(0) as u32,
                rect.x1.value().max(0) as u32,
                rect.y1.value().max(0) as u32,
            )
        })
        .collect();
    let mut diff = PixelDiff::default();
    for y in 0..actual.height {
        for x in 0..actual.width {
            if excluded
                .iter()
                .any(|(x0, y0, x1, y1)| x >= *x0 && x < *x1 && y >= *y0 && y < *y1)
            {
                continue;
            }
            let span = channel_span(pixel(reference, x, y), pixel(actual, x, y));
            if span > diff.span {
                diff = PixelDiff { span, at: (x, y) };
            }
        }
    }
    diff
}

#[must_use]
pub fn changed_pixels(
    before: &SnapshotImage,
    after: &SnapshotImage,
    damage: &[DeviceRect],
    padding: u32,
) -> u32 {
    assert_eq!(
        (before.width, before.height),
        (after.width, after.height),
        "changed-pixel accounting needs matching readback sizes"
    );
    let damaged: Vec<(u32, u32, u32, u32)> = damage
        .iter()
        .map(|rect| {
            (
                rect.x0.value().max(0) as u32,
                rect.y0.value().max(0) as u32,
                rect.x1.value().max(0) as u32,
                rect.y1.value().max(0) as u32,
            )
        })
        .collect();
    let mut outside = 0;
    for y in 0..after.height {
        for x in 0..after.width {
            if pixel(before, x, y) == pixel(after, x, y) {
                continue;
            }
            let covered = damaged.iter().any(|(x0, y0, x1, y1)| {
                x.saturating_add(padding) >= *x0
                    && y.saturating_add(padding) >= *y0
                    && x < x1.saturating_add(padding)
                    && y < y1.saturating_add(padding)
            });
            if !covered {
                outside += 1;
            }
        }
    }
    outside
}

fn inline_buffer(window: &Window) -> Result<&crate::paint::instance::InstanceBuffer, EngineError> {
    window.instance_buffer().ok_or(EngineError::PixelHarness {
        cause: PixelHarnessCause::NotInlineWindow,
    })
}

fn covering_size(window: &Window) -> Result<(u32, u32), EngineError> {
    let scale = window.device_pixel_ratio();
    let surface = scale.to_covering_device_rect(LogicalRect::from_origin_size(
        LogicalPoint::ZERO,
        window.size(),
    ));
    let width = surface.width().max(dp(1)).value();
    let height = surface.height().max(dp(1)).value();
    u32::try_from(width)
        .ok()
        .zip(u32::try_from(height).ok())
        .ok_or(EngineError::PixelHarness {
            cause: PixelHarnessCause::UnrepresentableSurface,
        })
}

fn surface_bounds(surface: (u32, u32)) -> DeviceRect {
    DeviceRect::new(dp(0), dp(0), dp(surface.0 as i32), dp(surface.1 as i32))
}

fn readback(renderer: &mut GpuRenderer) -> Result<SnapshotImage, EngineError> {
    let image = renderer.readback()?;
    Ok(SnapshotImage {
        width: image.width,
        height: image.height,
        rgba8: image.rgba8,
    })
}

fn upload_atlases(app: &App, renderer: &mut GpuRenderer) {
    let (glyph_page, glyph_pixels) = app.glyph_page_pixels();
    renderer.queue_atlas_upload(
        AtlasKind::Glyph,
        0,
        crate::render::AtlasArraySpec {
            page: glyph_page.0,
            layers: 1,
        },
        glyph_pixels,
        DirtyRects::full(),
    );
    if let Some((color_page, color_pixels)) = app.color_glyph_page_pixels() {
        renderer.queue_atlas_upload(
            AtlasKind::ColorGlyph,
            0,
            crate::render::AtlasArraySpec {
                page: color_page.0,
                layers: 1,
            },
            color_pixels,
            DirtyRects::full(),
        );
    }
    let image_pages = app.resource_page_pixels_all();
    let image_layers = u32::try_from(image_pages.len()).expect("image page count fits u32");
    for (layer, (image_page, image_pixels)) in image_pages.into_iter().enumerate() {
        renderer.queue_atlas_upload(
            AtlasKind::Image,
            layer as u16,
            crate::render::AtlasArraySpec {
                page: image_page.0,
                layers: image_layers,
            },
            image_pixels.to_vec(),
            DirtyRects::full(),
        );
    }
}

fn offscreen_gpu(size: (u32, u32)) -> Result<GpuRenderer, EngineError> {
    #[cfg(windows)]
    {
        GpuRenderer::new_warp_offscreen(size)
    }
    #[cfg(not(windows))]
    {
        GpuRenderer::new_offscreen(size)
    }
}

fn scroll_payload(
    scroll: &[ScrollSlot],
    scale: DevicePixelRatio,
) -> (Vec<[f32; 2]>, Vec<[i32; 4]>) {
    let mut table = ScrollTable::new();
    let deltas: Vec<ScrollDelta> = scroll
        .iter()
        .enumerate()
        .map(|(slot, (offset, viewport))| ScrollDelta {
            slot: slot as u32,
            offset: *offset,
            viewport: *viewport,
        })
        .collect();
    table.apply(&deltas, scale);
    let mut offsets = Vec::with_capacity(scroll.len());
    let mut clip = Vec::with_capacity(scroll.len());
    table.to_payload(scale, &mut offsets);
    table.viewports_to_payload(&mut clip);
    (offsets, clip)
}

#[must_use]
pub fn pixel(image: &SnapshotImage, x: u32, y: u32) -> [u8; 4] {
    assert!(
        x < image.width && y < image.height,
        "pixel ({x},{y}) out of bounds ({}x{})",
        image.width,
        image.height
    );
    let offset = ((y as usize) * (image.width as usize) + (x as usize)) * 4;
    image.rgba8[offset..offset + 4]
        .try_into()
        .expect("4 bytes per pixel")
}

#[must_use]
pub fn channel_span(a: [u8; 4], b: [u8; 4]) -> u8 {
    a.iter()
        .zip(b.iter())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0)
}

#[must_use]
pub fn region_max_span(
    image: &SnapshotImage,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    expected: [u8; 4],
) -> u8 {
    let mut span = 0;
    for y in y0..=y1 {
        for x in x0..=x1 {
            span = span.max(channel_span(pixel(image, x, y), expected));
        }
    }
    span
}

#[must_use]
pub fn region_diff(
    before: &SnapshotImage,
    after: &SnapshotImage,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
) -> u8 {
    assert_eq!(
        (before.width, before.height),
        (after.width, after.height),
        "readback sizes must not drift between frames"
    );
    let mut span = 0;
    for y in y0..=y1 {
        for x in x0..=x1 {
            span = span.max(channel_span(pixel(before, x, y), pixel(after, x, y)));
        }
    }
    span
}

#[must_use]
pub fn count_pixels(
    image: &SnapshotImage,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    predicate: impl Fn([u8; 4]) -> bool,
) -> u32 {
    let mut count = 0;
    for y in y0..=y1 {
        for x in x0..=x1 {
            if predicate(pixel(image, x, y)) {
                count += 1;
            }
        }
    }
    count
}

#[must_use]
pub fn describe_error(error: &SnapshotError) -> String {
    match error {
        SnapshotError::NoRenderer => String::from("no renderer in this build"),
        SnapshotError::Gpu { detail } => format!("gpu: {detail}"),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{PersistentPixels, PixelFrame, changed_pixels, compare_pixels, pixel};
    use crate::{
        math::{DeviceRect, LogicalPoint, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId},
        paint::{
            canvas::Canvas,
            color::Color,
            geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
            primitive::Path,
            style::{PaintSpec, ShadowSpec, Shadows},
            style_builder::StyleBuilder,
        },
        test_support::{
            app::TestApp,
            record::FrameRecord,
            snapshot::{SnapshotImage, gpu_guard},
        },
    };

    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];
    const RED: [u8; 4] = [255, 0, 0, 255];
    const DAMAGE_PAD: u32 = 1;

    fn white() -> PaintSpec {
        PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        }
    }

    fn blue() -> PaintSpec {
        PaintSpec {
            background: Color::srgb(0.0, 0.0, 1.0, 1.0),
            ..PaintSpec::default()
        }
    }

    fn backdrop(app: &mut TestApp) -> NodeId {
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let size = app.app().window(window).expect("窗口").size();
        let bounds = LogicalRect::from_origin_size(LogicalPoint::ZERO, size);
        mount(app, root, bounds, white())
    }

    fn mount(app: &mut TestApp, parent: NodeId, bounds: LogicalRect, paint: PaintSpec) -> NodeId {
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
        NodeBuilder::boxed()
            .layout_override(bounds)
            .paint(paint)
            .build(tree, parent)
            .expect("节点")
    }

    fn remove(app: &mut TestApp, node: NodeId) {
        let window = app.window_id();
        app.app_mut()
            .window_mut(window)
            .expect("窗口")
            .tree_mut()
            .remove(node)
            .expect("移除节点");
    }

    fn drain(app: &mut TestApp) -> FrameRecord {
        app.drain().expect("帧").clone()
    }

    fn idle(app: &mut TestApp) -> bool {
        !drain(app).rendered
    }

    fn device_rect(app: &TestApp, bounds: LogicalRect) -> DeviceRect {
        let window = app.app().window(app.window_id()).expect("窗口");
        window.device_pixel_ratio().to_covering_device_rect(bounds)
    }

    fn damage_covers(damage: &[DeviceRect], area: DeviceRect) -> bool {
        damage.iter().any(|rect| {
            rect.x0 <= area.x0 && rect.y0 <= area.y0 && rect.x1 >= area.x1 && rect.y1 >= area.y1
        })
    }

    fn baseline(app: &TestApp) -> SnapshotImage {
        app.read_pixels().expect("WARP / Metal / lavapipe 离屏读回")
    }

    fn harness(app: &TestApp) -> PersistentPixels {
        PersistentPixels::new(app.app(), app.window_id()).expect("持久像素目标")
    }

    fn replay(app: &TestApp, pixels: &mut PersistentPixels) -> PixelFrame {
        pixels
            .present(app.app(), app.window_id())
            .expect("持久增量帧")
    }

    fn replay_record(
        app: &TestApp,
        pixels: &mut PersistentPixels,
        record: &FrameRecord,
    ) -> PixelFrame {
        pixels
            .present_record(app.app(), app.window_id(), record)
            .expect("重放帧记录")
    }

    fn pixels_of(frame: &PixelFrame) -> &SnapshotImage {
        match frame {
            PixelFrame::Full(pixels) | PixelFrame::Incremental { pixels, .. } => pixels,
            PixelFrame::Idle => panic!("空闲帧没有像素"),
        }
    }

    fn assert_incremental(frame: &PixelFrame) {
        assert!(
            matches!(frame, PixelFrame::Incremental { .. }),
            "该帧必须走持久目标的 damage scissor 增量通路"
        );
    }

    fn assert_matches_baseline(frame: &PixelFrame, app: &TestApp, excluded: &[DeviceRect]) {
        let diff = compare_pixels(&baseline(app), pixels_of(frame), excluded);
        assert!(
            diff.span <= DAMAGE_PAD as u8,
            "增量像素必须与整帧基线一致：span={} at {:?}（排除 {} 个矩形）",
            diff.span,
            diff.at,
            excluded.len()
        );
    }

    fn assert_baseline_opaque(app: &TestApp) {
        let baseline = baseline(app);
        let width = baseline.width;
        let height = baseline.height;
        let rgba = baseline.rgba8.as_chunks::<4>().0;
        let translucent: Vec<(u32, u32, u8)> = rgba
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry[3] < 255)
            .map(|(index, entry)| (index as u32 % width, index as u32 / width, entry[3]))
            .filter(|(x, y, _)| *x + 1 < width && *y + 1 < height)
            .collect();
        assert!(
            translucent.is_empty(),
            "基线内容区必须完全不透明（半透明像素 {:?}）：否则增量重绘在覆盖边缘会累积 alpha",
            translucent
        );
    }

    fn point_inside(bounds: LogicalRect) -> (u32, u32) {
        (
            (bounds.origin.x + bounds.size.width / 2.0).value() as u32,
            (bounds.origin.y + bounds.size.height / 2.0).value() as u32,
        )
    }

    fn pixel_at(pixels: &SnapshotImage, point: (u32, u32)) -> [u8; 4] {
        pixel(pixels, point.0, point.1)
    }

    fn page_corner(app: &TestApp) -> LogicalRect {
        let window = app.app().window(app.window_id()).expect("窗口");
        let size = window.size();
        LogicalRect::new(
            px(size.width.value() - 40.0),
            px(size.height.value() - 40.0),
            px(20.0),
            px(20.0),
        )
    }

    fn register_triangle_geometry(app: &mut TestApp, bounds: LogicalRect) -> GeometryId {
        let points = [[0.0f32, 0.0, -1.0], [20.0, 0.0, -1.0], [0.0, 10.0, -1.0]];
        let vertex_bytes: Vec<u8> = points
            .iter()
            .flat_map(|point| point.iter().flat_map(|value| value.to_ne_bytes()))
            .collect();
        app.app_mut()
            .geometry_mut()
            .insert(GeometryDesc {
                vertex_bytes: bytes::Bytes::from(vertex_bytes),
                index_bytes: bytes::Bytes::from_static(&[0u8, 0, 1, 0, 2, 0]),
                vertex_stride: 12,
                index_type: IndexType::U16,
                topology: Topology::TriangleList,
                bounds,
                pipeline: GeometryPipeline::Ui,
            })
            .expect("注册三角形几何")
    }

    fn assert_changes_within_damage(before: &SnapshotImage, frame: &PixelFrame) {
        let PixelFrame::Incremental { pixels, damage } = frame else {
            panic!("只有增量帧才能断言「变化像素 ⊆ damage」");
        };
        let outside = changed_pixels(before, pixels, damage, DAMAGE_PAD);
        assert_eq!(
            outside, 0,
            "变化像素必须全部落在 damage 内（damage={damage:?}）"
        );
    }

    #[test]
    fn the_persistent_target_keeps_pixels_outside_a_narrow_damage_set() {
        let _guard = gpu_guard();
        let mut app = TestApp::new();
        backdrop(&mut app);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let box_bounds = LogicalRect::new(px(40.0), px(40.0), px(40.0), px(40.0));
        let box_node = mount(&mut app, root, box_bounds, blue());
        let record = drain(&mut app);
        let mut pixels = harness(&app);
        let first = replay_record(&app, &mut pixels, &record);
        assert!(
            matches!(first, PixelFrame::Full(_)),
            "首帧必须整帧建立持久目标"
        );
        assert_baseline_opaque(&app);
        assert_eq!(
            pixel_at(pixels_of(&first), point_inside(box_bounds)),
            BLUE,
            "方块必须画进持久目标"
        );

        remove(&mut app, box_node);
        let removed = drain(&mut app);
        assert!(!removed.damage_full, "移除单节点不得整帧");
        let old_area = device_rect(&app, box_bounds);
        assert!(
            damage_covers(&removed.damage, old_area),
            "移除节点的 damage 必须覆盖旧面积：{:?}",
            removed.damage
        );

        let mut narrowed = removed.clone();
        narrowed.damage = vec![device_rect(&app, page_corner(&app))];
        let stale = replay_record(&app, &mut pixels, &narrowed);
        assert_incremental(&stale);
        assert_eq!(
            pixel_at(pixels_of(&stale), point_inside(box_bounds)),
            BLUE,
            "damage 未覆盖时旧像素必须残留在持久目标上（scissor 之外不重绘）"
        );
        assert!(
            compare_pixels(&baseline(&app), pixels_of(&stale), &[]).span > 128,
            "漏覆盖的帧必须与整帧基线明显不一致"
        );

        let healed = replay_record(&app, &mut pixels, &removed);
        assert_incremental(&healed);
        assert_eq!(
            pixel_at(pixels_of(&healed), point_inside(box_bounds)),
            WHITE,
            "damage 覆盖旧面积后必须重绘成背景"
        );
        assert_matches_baseline(&healed, &app, &[]);
    }

    #[test]
    fn a_moved_node_keeps_the_persistent_target_free_of_stale_pixels() {
        let _guard = gpu_guard();
        let mut app = TestApp::new();
        backdrop(&mut app);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let old = LogicalRect::new(px(100.0), px(100.0), px(120.0), px(80.0));
        let new = LogicalRect::new(px(400.0), px(200.0), px(120.0), px(80.0));
        let panel = mount(&mut app, root, old, blue());
        let record = drain(&mut app);
        let mut pixels = harness(&app);
        let first = replay_record(&app, &mut pixels, &record);
        assert!(matches!(first, PixelFrame::Full(_)));
        assert_baseline_opaque(&app);
        let before_move = pixels.readback().expect("读回");

        app.app_mut()
            .window_mut(window)
            .expect("窗口")
            .tree_mut()
            .set_layout_override(panel, Some(crate::layout::LayoutOverride::new(new)))
            .expect("移动面板");
        let moved = drain(&mut app);
        assert!(!moved.damage_full, "移动单节点不得整帧");
        assert!(
            damage_covers(&moved.damage, device_rect(&app, old)),
            "移动必须覆盖旧位置：{:?}",
            moved.damage
        );
        assert!(
            damage_covers(&moved.damage, device_rect(&app, new)),
            "移动必须覆盖新位置：{:?}",
            moved.damage
        );
        let opened = replay(&app, &mut pixels);
        assert_incremental(&opened);
        assert_changes_within_damage(&before_move, &opened);
        assert_matches_baseline(&opened, &app, &[]);
        assert_eq!(
            pixel_at(pixels_of(&opened), point_inside(old)),
            WHITE,
            "旧位置必须回到背景色"
        );
        assert_eq!(
            pixel_at(pixels_of(&opened), point_inside(new)),
            BLUE,
            "新位置必须画出面板"
        );

        for _ in 0..12 {
            app.advance(Duration::from_millis(16));
            assert!(idle(&mut app), "稳态不得持续出帧");
            assert_eq!(
                replay(&app, &mut pixels),
                PixelFrame::Idle,
                "空转帧不得触碰持久目标"
            );
        }
        assert_eq!(
            pixels.readback().expect("读回"),
            pixels_of(&opened).clone(),
            "空转期间持久目标像素必须逐字节不变"
        );
    }

    #[test]
    fn opening_settling_closing_and_reopening_a_panel_leaves_no_residue() {
        let _guard = gpu_guard();
        let mut app = TestApp::new();
        backdrop(&mut app);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let places = [
            LogicalRect::new(px(420.0), px(120.0), px(284.0), px(300.0)),
            LogicalRect::new(px(140.0), px(220.0), px(284.0), px(300.0)),
            LogicalRect::new(px(430.0), px(90.0), px(284.0), px(300.0)),
            LogicalRect::new(px(180.0), px(150.0), px(284.0), px(300.0)),
        ];
        let record = drain(&mut app);
        let mut pixels = harness(&app);
        assert!(matches!(
            replay_record(&app, &mut pixels, &record),
            PixelFrame::Full(_)
        ));
        assert_baseline_opaque(&app);
        for (round, place) in places.into_iter().enumerate() {
            let before = pixels.readback().expect("读回");
            let popup = mount(&mut app, root, place, blue());
            let opened_record = drain(&mut app);
            assert!(!opened_record.damage_full, "第 {round} 次打开不得整帧");
            let opened = replay(&app, &mut pixels);
            assert_incremental(&opened);
            assert_changes_within_damage(&before, &opened);
            assert_matches_baseline(&opened, &app, &[]);
            let center = (
                (place.origin.x + place.size.width / 2.0).value() as u32,
                (place.origin.y + place.size.height / 2.0).value() as u32,
            );
            assert_eq!(pixel(pixels_of(&opened), center.0, center.1), BLUE);

            for _ in 0..10 {
                app.advance(Duration::from_millis(16));
                assert!(idle(&mut app), "空转帧不得出帧");
                assert_eq!(replay(&app, &mut pixels), PixelFrame::Idle);
            }

            let before_close = pixels.readback().expect("读回");
            remove(&mut app, popup);
            let closed_record = drain(&mut app);
            assert!(!closed_record.damage_full, "第 {round} 次关闭不得整帧");
            assert!(
                damage_covers(&closed_record.damage, device_rect(&app, place)),
                "关闭必须覆盖旧面板面积：{:?}",
                closed_record.damage
            );
            let closed = replay(&app, &mut pixels);
            assert_incremental(&closed);
            assert_changes_within_damage(&before_close, &closed);
            assert_matches_baseline(&closed, &app, &[]);
            assert_eq!(
                pixel(pixels_of(&closed), center.0, center.1),
                WHITE,
                "关闭后旧面板中心必须回到背景色（第 {round} 次）"
            );
        }
        for _ in 0..4 {
            app.advance(Duration::from_millis(16));
            assert!(idle(&mut app));
        }
        assert_eq!(
            pixels.readback().expect("读回"),
            baseline(&app),
            "关闭 + 空转后持久目标必须与整帧基线逐字节一致"
        );
    }

    fn shadowed() -> PaintSpec {
        PaintSpec {
            background: Color::WHITE,
            shadows: Shadows::new([ShadowSpec {
                offset: LogicalPoint::new(px(0.0), px(4.0)),
                blur: px(12.0),
                spread: px(2.0),
                color: Color::BLACK.with_opacity(0.5),
            }]),
            ..PaintSpec::default()
        }
    }

    fn shadow_extent(bounds: LogicalRect) -> LogicalRect {
        let sigma = (12.0 * crate::paint::instance::SHADOW_SIGMA_SCALE)
            .max(crate::paint::instance::SHADOW_SIGMA_MIN);
        let pad = px(crate::paint::instance::SHADOW_PAD_SIGMAS * sigma + 1.0);
        bounds
            .outset(px(2.0))
            .translate(LogicalPoint::new(px(0.0), px(4.0)))
            .outset(pad)
    }

    fn absolute_at(bounds: LogicalRect) -> crate::paint::style::Style {
        StyleBuilder::new()
            .absolute()
            .left(bounds.origin.x)
            .top(bounds.origin.y)
            .size(bounds.size.width, bounds.size.height)
            .build()
    }

    fn mount_shadowed_canvas_subtree(
        app: &mut TestApp,
        parent: NodeId,
        bounds: LogicalRect,
    ) -> NodeId {
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
        let group = NodeBuilder::boxed()
            .style(absolute_at(bounds))
            .paint(shadowed())
            .build(tree, parent)
            .expect("浮层子树");
        let local = LogicalRect::new(px(0.0), px(0.0), px(180.0), px(120.0));
        let mut canvas = crate::paint::canvas::Canvas::new(local);
        canvas.push_quad(crate::paint::primitive::Quad {
            bounds: LogicalRect::new(px(10.0), px(10.0), px(60.0), px(40.0)),
            corner_radius: [px(0.0); 4],
            paint: crate::paint::primitive::QuadPaint::Solid {
                fill: Color::srgb(1.0, 0.0, 0.0, 1.0),
                border: None,
            },
        });
        NodeBuilder::canvas(canvas)
            .layout_override(LogicalRect::new(px(10.0), px(20.0), px(180.0), px(120.0)))
            .build(tree, group)
            .expect("画布子节点");
        group
    }

    #[test]
    fn adding_and_removing_a_shadowed_canvas_subtree_stays_incremental() {
        let _guard = gpu_guard();
        let mut app = TestApp::new();
        backdrop(&mut app);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let probe = mount(
            &mut app,
            root,
            LogicalRect::new(px(40.0), px(40.0), px(60.0), px(40.0)),
            blue(),
        );
        let record = drain(&mut app);
        let mut pixels = harness(&app);
        assert!(matches!(
            replay_record(&app, &mut pixels, &record),
            PixelFrame::Full(_)
        ));
        assert_baseline_opaque(&app);

        let bounds = LogicalRect::new(px(300.0), px(140.0), px(200.0), px(160.0));
        let subtree = mount_shadowed_canvas_subtree(&mut app, root, bounds);
        let added = drain(&mut app);
        assert!(!added.damage_full, "新增带阴影/Canvas 的子树不得整帧");
        assert!(
            damage_covers(&added.damage, device_rect(&app, shadow_extent(bounds))),
            "新增必须把绘制外延（含阴影 pad）放进 damage：{:?}",
            added.damage
        );
        let before_add = pixels.readback().expect("读回");
        let opened = replay(&app, &mut pixels);
        assert_incremental(&opened);
        assert_changes_within_damage(&before_add, &opened);
        assert_matches_baseline(&opened, &app, &[]);

        app.app_mut()
            .window_mut(window)
            .expect("窗口")
            .tree_mut()
            .set_layout_override(
                probe,
                Some(crate::layout::LayoutOverride::new(LogicalRect::new(
                    px(120.0),
                    px(40.0),
                    px(60.0),
                    px(40.0),
                ))),
            )
            .expect("移动探针");
        let reuse = drain(&mut app);
        assert!(!reuse.damage_full, "新增后的局部改动不得整帧");
        assert!(
            reuse.stats.reused_segments > 0,
            "新增子树之后必须复用未变化 lane 的实例段：{:?}",
            reuse.stats
        );
        let reuse_before = pixels.readback().expect("读回");
        let moved = replay(&app, &mut pixels);
        assert_incremental(&moved);
        assert_changes_within_damage(&reuse_before, &moved);
        assert_matches_baseline(&moved, &app, &[]);

        for _ in 0..8 {
            app.advance(Duration::from_millis(16));
            assert!(idle(&mut app));
            assert_eq!(replay(&app, &mut pixels), PixelFrame::Idle);
        }

        let before_remove = pixels.readback().expect("读回");
        remove(&mut app, subtree);
        let removed = drain(&mut app);
        assert!(!removed.damage_full, "移除带阴影/Canvas 的子树不得整帧");
        assert!(
            damage_covers(&removed.damage, device_rect(&app, shadow_extent(bounds))),
            "移除必须覆盖旧绘制外延（含阴影 pad）：{:?}",
            removed.damage
        );
        let closed = replay(&app, &mut pixels);
        assert_incremental(&closed);
        assert_changes_within_damage(&before_remove, &closed);
        assert_matches_baseline(&closed, &app, &[]);
        assert_eq!(
            pixel_at(pixels_of(&closed), point_inside(bounds)),
            WHITE,
            "移除后旧浮层中心必须回到背景"
        );

        app.advance(Duration::from_millis(16));
        assert!(idle(&mut app));
        assert_eq!(
            pixels.readback().expect("读回"),
            baseline(&app),
            "新增→复用→移除全程结束后持久目标必须与整帧基线逐字节一致"
        );
    }

    fn scroll_slots(app: &TestApp) -> Vec<super::ScrollSlot> {
        let window = app.app().window(app.window_id()).expect("窗口");
        window.scroll_slots().to_vec()
    }

    fn baseline_scrolled(app: &TestApp) -> SnapshotImage {
        app.read_pixels_with_scroll(&scroll_slots(app))
            .expect("WARP / Metal / lavapipe 离屏读回")
    }

    #[test]
    fn a_scrolling_container_replays_incrementally_against_the_scroll_table() {
        let _guard = gpu_guard();
        let mut app = TestApp::new();
        backdrop(&mut app);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let viewport = {
            let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .absolute()
                        .left(px(100.0))
                        .top(px(80.0))
                        .size(px(240.0), px(200.0))
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("滚动容器")
        };
        let handle = app
            .app_mut()
            .make_scroll(
                window,
                viewport,
                crate::interaction::ScrollConfig::default(),
            )
            .expect("滚动容器登记");
        app.app_mut()
            .set_scroll_content(&handle, LogicalSize::new(px(240.0), px(1200.0)))
            .expect("内容尺寸");
        let moving = {
            let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(20.0), px(320.0), px(100.0), px(40.0)))
                .paint(blue())
                .build(tree, viewport)
                .expect("滚动内容")
        };
        let record = drain(&mut app);
        let mut pixels = harness(&app);
        let scroll_before = scroll_slots(&app);
        assert!(matches!(
            replay_record(&app, &mut pixels, &record),
            PixelFrame::Full(_)
        ));
        assert_baseline_opaque(&app);

        let before_scroll = pixels.readback().expect("读回");
        app.app_mut()
            .scroll_to(&handle, LogicalPoint::new(px(0.0), px(300.0)))
            .expect("滚动");
        let scrolled = drain(&mut app);
        assert!(!scrolled.damage_full, "滚动容器滚动不得整帧");
        let scroll_after = scroll_slots(&app);
        assert_ne!(scroll_before, scroll_after, "滚动后槽位偏移必须变化");
        let scrolled_frame = replay(&app, &mut pixels);
        assert_incremental(&scrolled_frame);
        assert_changes_within_damage(&before_scroll, &scrolled_frame);
        let diff = compare_pixels(&baseline_scrolled(&app), pixels_of(&scrolled_frame), &[]);
        assert!(
            diff.span <= DAMAGE_PAD as u8,
            "滚动增量帧必须与整帧基线一致：span={} at {:?}",
            diff.span,
            diff.at
        );

        let before_move = pixels.readback().expect("读回");
        app.app_mut()
            .window_mut(window)
            .expect("窗口")
            .tree_mut()
            .set_layout_override(
                moving,
                Some(crate::layout::LayoutOverride::new(LogicalRect::new(
                    px(20.0),
                    px(400.0),
                    px(100.0),
                    px(40.0),
                ))),
            )
            .expect("移动滚动内容");
        let moved = drain(&mut app);
        assert!(!moved.damage_full, "滚动内容移动不得整帧");
        let old_screen = LogicalRect::new(px(120.0), px(100.0), px(100.0), px(40.0));
        let new_screen = LogicalRect::new(px(120.0), px(180.0), px(100.0), px(40.0));
        assert!(
            damage_covers(&moved.damage, device_rect(&app, old_screen)),
            "滚动内容移动必须覆盖旧屏幕位置：{:?}",
            moved.damage
        );
        assert!(
            damage_covers(&moved.damage, device_rect(&app, new_screen)),
            "滚动内容移动必须覆盖新屏幕位置：{:?}",
            moved.damage
        );
        let moved_frame = replay(&app, &mut pixels);
        assert_incremental(&moved_frame);
        assert_changes_within_damage(&before_move, &moved_frame);
        assert_eq!(
            pixel_at(pixels_of(&moved_frame), point_inside(old_screen)),
            WHITE,
            "滚动内容移动后旧屏幕位置必须回到背景"
        );
        assert_eq!(
            pixel_at(pixels_of(&moved_frame), point_inside(new_screen)),
            BLUE,
            "滚动内容移动后新屏幕位置必须绘制"
        );
        let diff = compare_pixels(&baseline_scrolled(&app), pixels_of(&moved_frame), &[]);
        assert!(
            diff.span <= DAMAGE_PAD as u8,
            "滚动内容移动后的增量帧必须与整帧基线一致：span={} at {:?}",
            diff.span,
            diff.at
        );
    }

    fn mount_scrolled_viewport(app: &mut TestApp) -> (NodeId, crate::interaction::ScrollHandle) {
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let viewport = {
            let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .absolute()
                        .left(px(100.0))
                        .top(px(80.0))
                        .size(px(240.0), px(200.0))
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("滚动容器")
        };
        let handle = app
            .app_mut()
            .make_scroll(
                window,
                viewport,
                crate::interaction::ScrollConfig::default(),
            )
            .expect("滚动容器登记");
        app.app_mut()
            .set_scroll_content(&handle, LogicalSize::new(px(240.0), px(1200.0)))
            .expect("内容尺寸");
        (viewport, handle)
    }

    #[test]
    fn removing_a_direct_child_of_a_scrolled_container_damages_its_screen_position() {
        let _guard = gpu_guard();
        let mut app = TestApp::new();
        backdrop(&mut app);
        let window = app.window_id();
        let (viewport, handle) = mount_scrolled_viewport(&mut app);
        let row_bounds = LogicalRect::new(px(20.0), px(320.0), px(100.0), px(40.0));
        let screen_bounds = LogicalRect::new(px(120.0), px(100.0), px(100.0), px(40.0));
        let row = {
            let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
            NodeBuilder::boxed()
                .layout_override(row_bounds)
                .paint(blue())
                .build(tree, viewport)
                .expect("滚动行")
        };
        app.app_mut()
            .scroll_to(&handle, LogicalPoint::new(px(0.0), px(300.0)))
            .expect("滚动");
        let record = drain(&mut app);
        let mut pixels = harness(&app);
        assert!(matches!(
            replay_record(&app, &mut pixels, &record),
            PixelFrame::Full(_)
        ));
        assert_baseline_opaque(&app);
        let before_remove = pixels.readback().expect("读回");
        assert_eq!(
            pixel_at(&before_remove, point_inside(screen_bounds)),
            BLUE,
            "前置条件：滚动 offset=300 时该行必须出现在屏幕 (120,100)"
        );

        remove(&mut app, row);
        let removed = drain(&mut app);
        assert!(!removed.damage_full, "删除滚动容器直接子节点不得整帧");
        assert!(
            damage_covers(&removed.damage, device_rect(&app, screen_bounds)),
            "删除滚动容器直接子节点必须覆盖其旧屏幕位置（含容器自身 scroll offset）：{:?}",
            removed.damage
        );
        let closed = replay(&app, &mut pixels);
        assert_incremental(&closed);
        assert_changes_within_damage(&before_remove, &closed);
        assert_matches_baseline(&closed, &app, &[]);
        assert_eq!(
            pixel_at(pixels_of(&closed), point_inside(screen_bounds)),
            WHITE,
            "删除后旧行位置必须回到背景"
        );
    }

    #[test]
    fn a_path_canvas_on_an_incremental_frame_renders_inside_the_damage() {
        let _guard = gpu_guard();
        let mut app = TestApp::new();
        backdrop(&mut app);
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let record = drain(&mut app);
        let mut pixels = harness(&app);
        assert!(matches!(
            replay_record(&app, &mut pixels, &record),
            PixelFrame::Full(_)
        ));
        assert_baseline_opaque(&app);

        let local = LogicalRect::new(px(0.0), px(0.0), px(20.0), px(10.0));
        let geometry = register_triangle_geometry(&mut app, local);
        let mut canvas = Canvas::new(local);
        canvas.push_path(Path {
            bounds: local,
            geometry,
            fill: Color::srgb(1.0, 0.0, 0.0, 1.0),
        });
        let place = LogicalRect::new(px(300.0), px(200.0), px(20.0), px(10.0));
        {
            let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
            NodeBuilder::canvas(canvas)
                .layout_override(place)
                .build(tree, root)
                .expect("路径画布");
        }
        let before_add = pixels.readback().expect("读回");
        let added = drain(&mut app);
        assert!(!added.damage_full, "新增路径画布不得整帧");
        assert!(
            damage_covers(&added.damage, device_rect(&app, place)),
            "新增路径画布的 damage 必须覆盖其位置：{:?}",
            added.damage
        );
        let frame = replay(&app, &mut pixels);
        assert_incremental(&frame);
        assert_changes_within_damage(&before_add, &frame);
        assert_matches_baseline(&frame, &app, &[]);
        assert_eq!(
            pixel_at(
                pixels_of(&frame),
                (
                    (place.origin.x + px(5.0)).value() as u32,
                    (place.origin.y + px(2.0)).value() as u32
                )
            ),
            RED,
            "局部 damage 帧内画布中的 Path 必须真正绘制出来"
        );
    }
}
