use std::time::{Duration, Instant};

use super::{
    a11y::ShadowTree,
    probe::Probe,
    record::{FrameRecord, RenderedFrame},
    snapshot::{SnapshotCase, SnapshotError, SnapshotImage},
};
use crate::{
    app::{App, AppHandle},
    error::EngineError,
    event::{Event, InputEvent, InputOutcome, Modifiers},
    math::{DevicePixelRatio, LogicalPoint, dp},
    window::{FrameOutcome, WindowId, WindowOptions},
};

pub struct TestApp {
    _runtime: tokio::runtime::Runtime,
    app: App,
    window: WindowId,
    last_rendered: Option<RenderedFrame>,
    a11y_capture: bool,
    last_a11y_update: Option<accesskit::TreeUpdate>,
    a11y_shadow: ShadowTree,
    a11y_contract_error: Option<String>,
    tasks_run: usize,
}

impl Default for TestApp {
    fn default() -> Self {
        Self::new()
    }
}

impl TestApp {
    #[must_use]
    pub fn new() -> Self {
        Self::with_options(WindowOptions::default())
    }

    #[must_use]
    pub fn with_options(options: WindowOptions) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let mut app = App::new(runtime.handle().clone());
        app.use_engine_clock();
        let window = app.create_window(options);
        Self {
            _runtime: runtime,
            app,
            window,
            last_rendered: None,
            a11y_capture: false,
            last_a11y_update: None,
            a11y_shadow: ShadowTree::new(),
            a11y_contract_error: None,
            tasks_run: 0,
        }
    }

    #[must_use]
    pub fn with_scale(dpr: DevicePixelRatio) -> Self {
        let mut app = Self::with_options(WindowOptions::default());
        app.set_dpr(dpr);
        app
    }

    pub fn set_dpr(&mut self, dpr: DevicePixelRatio) {
        if let Some(window) = self.app.window_mut(self.window) {
            window.set_device_pixel_ratio(dpr);
        }
    }

    #[must_use]
    pub fn window_id(&self) -> WindowId {
        self.window
    }

    #[must_use]
    pub fn app(&self) -> &App {
        &self.app
    }

    #[must_use]
    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    pub fn drain(&mut self) -> Result<&FrameRecord, EngineError> {
        self.step(A11yGate::Immediate)
    }

    pub fn drain_pump(&mut self) -> Result<&FrameRecord, EngineError> {
        self.step(A11yGate::Pump)
    }

    fn step(&mut self, gate: A11yGate) -> Result<&FrameRecord, EngineError> {
        let outcome = self
            .app
            .frame(self.window)
            .expect("TestApp's window must exist");
        outcome?;
        let deliver = self.a11y_capture
            && match gate {
                A11yGate::Immediate => true,
                A11yGate::Pump => self.app.a11y_pending(self.window),
            };
        self.last_a11y_update = None;
        self.a11y_contract_error = None;
        if deliver && let Some(snapshot) = self.app.a11y_take(self.window) {
            let update = crate::a11y::adapter::to_tree_update(&snapshot);
            if let Err(error) = self.a11y_shadow.apply(&update) {
                self.a11y_contract_error = Some(error);
            }
            self.last_a11y_update = Some(update);
            self.app.attach_a11y(self.window, snapshot);
        }
        self.pump_tasks();
        if let Some(frame) = self.frame_record().frame.clone() {
            self.last_rendered = Some(frame);
        }
        Ok(self.frame_record())
    }

    fn pump_tasks(&mut self) {
        let dispatcher = self.app.dispatcher();
        for task in dispatcher.drain_tasks() {
            self.tasks_run += 1;
            task.run(&mut self.app);
        }
    }

    #[must_use]
    pub fn handle(&self) -> AppHandle {
        self.app.handle()
    }

    #[must_use]
    pub fn tasks_run(&self) -> usize {
        self.tasks_run
    }

    #[must_use]
    pub fn frame_record(&self) -> &FrameRecord {
        self.app
            .window(self.window)
            .expect("TestApp's window must exist")
            .record()
            .expect("drain() must have produced a frame record")
    }

    #[must_use]
    pub fn rendered(&self) -> Option<&RenderedFrame> {
        self.frame_record().frame.as_ref()
    }

    #[must_use]
    pub fn probe(&self) -> Probe<'_> {
        let window = self
            .app
            .window(self.window)
            .expect("TestApp's window must exist");
        Probe::new(window.tree())
    }

    pub fn enable_a11y_capture(&mut self) {
        self.a11y_capture = true;
        self.app.set_a11y_capture(self.window, true);
    }

    #[must_use]
    pub fn a11y(&self) -> Option<&crate::a11y::A11yTreeSnapshot> {
        self.frame_record().a11y.as_ref()
    }

    #[must_use]
    pub fn a11y_update(&self) -> Option<&accesskit::TreeUpdate> {
        self.last_a11y_update.as_ref()
    }

    #[must_use]
    pub fn a11y_contract_error(&self) -> Option<&str> {
        self.a11y_contract_error.as_deref()
    }

    pub fn assert_a11y_contract(&self) {
        if let Some(error) = &self.a11y_contract_error {
            panic!("TreeUpdate violates the AccessKit contract: {error}");
        }
    }

    #[must_use]
    pub fn a11y_shadow_summaries(&self) -> ahash::HashMap<accesskit::NodeId, String> {
        self.a11y_shadow.summaries()
    }

    pub fn a11y_oracle_diff(&mut self) -> Option<String> {
        assert!(
            self.a11y_capture,
            "a11y oracle requires enable_a11y_capture()"
        );
        let _ = self.drain().expect("incremental a11y frame");
        let incremental = self.a11y_shadow.summaries();
        let incremental_structure = self.a11y_shadow.structure();
        self.app
            .window_mut(self.window)
            .expect("TestApp's window must exist")
            .a11y_force_full();
        let _ = self.drain().expect("full a11y frame");
        let Some(update) = self.last_a11y_update.clone() else {
            return Some(String::from(
                "forced full a11y delivery produced no TreeUpdate",
            ));
        };
        let mut full = ShadowTree::new();
        if let Err(error) = full.apply(&update) {
            return Some(format!(
                "forced full TreeUpdate violates the contract: {error}"
            ));
        }
        let complete = full.summaries();
        if let Some(error) = self.a11y_contract_error.as_ref() {
            return Some(format!(
                "incremental TreeUpdate violates the contract: {error}"
            ));
        }
        if incremental.len() != complete.len() {
            return Some(format!(
                "incremental shadow has {} node(s), full rebuild has {}",
                incremental.len(),
                complete.len()
            ));
        }
        let mut ids: Vec<&accesskit::NodeId> = complete.keys().collect();
        ids.sort_by_key(|id| id.0);
        for id in ids {
            match incremental.get(id) {
                None => {
                    return Some(format!(
                        "node {id:?} exists in the full rebuild but not in the incremental tree"
                    ));
                }
                Some(actual) if actual != &complete[id] => {
                    return Some(format!(
                        "node {id:?} differs:\n  incremental: {actual}\n  full:        {}",
                        complete[id]
                    ));
                }
                Some(_) => {}
            }
        }
        if incremental_structure != full.structure() {
            return Some(String::from(
                "parent/children structure differs between incremental and full delivery",
            ));
        }
        None
    }

    pub fn assert_a11y_oracle(&mut self) {
        if let Some(diff) = self.a11y_oracle_diff() {
            panic!("inconsistent a11y tree: {diff}");
        }
    }

    pub fn inject(&mut self, event: InputEvent) -> InputOutcome {
        let stamp = self.app.clock();
        self.app
            .dispatch_input(self.window, event.with_timestamp(stamp))
    }

    pub fn dispatch_event(&mut self, event: Event) {
        let stamp = self.app.clock();
        let event = match event {
            Event::Input(input) => Event::Input(input.with_timestamp(stamp)),
            other => other,
        };
        self.app.dispatch(self.window, event);
    }

    #[must_use]
    pub fn pointer_position(&self) -> Option<LogicalPoint> {
        self.app.pointer_position(self.window)
    }

    pub fn move_pointer(&mut self, position: LogicalPoint) -> InputOutcome {
        self.inject(InputEvent::pointer_move(position, Modifiers::empty()))
    }

    pub fn enter_pointer(&mut self, position: LogicalPoint) -> InputOutcome {
        self.inject(InputEvent::PointerEnter {
            position,
            modifiers: Modifiers::empty(),
            timestamp: Instant::now(),
        })
    }

    pub fn leave_pointer(&mut self, position: LogicalPoint) -> InputOutcome {
        self.inject(InputEvent::PointerLeave {
            position,
            modifiers: Modifiers::empty(),
            timestamp: Instant::now(),
        })
    }

    pub fn advance(&mut self, by: Duration) {
        self.app.advance_clock(by);
    }

    pub fn invalidate_all_lanes(&mut self) {
        if let Some(window) = self.app.window_mut(self.window) {
            window.force_full_repaint();
        }
    }

    pub fn oracle_diff(&mut self) -> Option<String> {
        self.drain().expect("incremental frame");
        let incremental = self
            .rendered()
            .cloned()
            .or_else(|| self.last_rendered.clone())
            .expect("incremental frame must have committed pixels before");
        self.invalidate_all_lanes();
        self.drain().expect("full frame");
        let full = self
            .rendered()
            .cloned()
            .expect("full frame must commit pixels");
        if incremental.primitives != full.primitives {
            return Some(format!(
                "primitive sequence differs: incremental {} vs full {} entries",
                incremental.primitives.len(),
                full.primitives.len()
            ));
        }
        if incremental.quads != full.quads {
            return Some(format!(
                "quad sequence differs: incremental {} vs full {} entries",
                incremental.quads.len(),
                full.quads.len()
            ));
        }
        None
    }

    pub fn assert_oracle(&mut self) {
        if let Some(diff) = self.oracle_diff() {
            panic!("stale drawing: {diff}");
        }
    }

    pub fn snapshot(&self, case: SnapshotCase) -> Result<SnapshotImage, SnapshotError> {
        debug_assert!(case.scale.factor() > 0.0, "snapshot scale must be positive");
        let window = self.app.window(self.window).ok_or(SnapshotError::Gpu {
            detail: smol_str::SmolStr::new_static("snapshot: window was closed"),
        })?;
        let surface =
            case.scale
                .to_covering_device_rect(crate::math::LogicalRect::from_origin_size(
                    LogicalPoint::ZERO,
                    case.size,
                ));
        let size = (
            surface.width().max(dp(1)).value() as u32,
            surface.height().max(dp(1)).value() as u32,
        );
        let (glyph_page, glyph_pixels) = self.app.glyph_page_pixels();
        let mut renderer = Self::offscreen_renderer(size).map_err(gpu_error)?;
        renderer.queue_atlas_upload(
            crate::paint::atlas::AtlasKind::Glyph,
            0,
            crate::render::AtlasArraySpec {
                page: glyph_page.0,
                layers: 1,
            },
            &glyph_pixels,
            crate::render::DirtyRects::full(),
        );
        if let Some((color_page, color_pixels)) = self.app.color_glyph_page_pixels() {
            renderer.queue_atlas_upload(
                crate::paint::atlas::AtlasKind::ColorGlyph,
                0,
                crate::render::AtlasArraySpec {
                    page: color_page.0,
                    layers: 1,
                },
                &color_pixels,
                crate::render::DirtyRects::full(),
            );
        }
        let image_pages = self.app.resource_page_pixels_all();
        let image_layers = u32::try_from(image_pages.len()).expect("image page count fits u32");
        for (layer, (image_page, image_pixels)) in image_pages.into_iter().enumerate() {
            renderer.queue_atlas_upload(
                crate::paint::atlas::AtlasKind::Image,
                layer as u16,
                crate::render::AtlasArraySpec {
                    page: image_page.0,
                    layers: image_layers,
                },
                image_pixels,
                crate::render::DirtyRects::full(),
            );
        }
        let image = renderer
            .render_snapshot(
                window
                    .instance_buffer()
                    .expect("render_snapshot requires an inline (headless) window"),
            )
            .map_err(gpu_error)?;
        Ok(SnapshotImage {
            width: image.width,
            height: image.height,
            rgba8: image.rgba8,
        })
    }

    #[cfg(windows)]
    fn offscreen_renderer(
        size: (u32, u32),
    ) -> Result<crate::render::backend::Renderer, EngineError> {
        crate::render::backend::Renderer::new_warp_offscreen(size)
    }

    #[cfg(not(windows))]
    fn offscreen_renderer(
        size: (u32, u32),
    ) -> Result<crate::render::backend::Renderer, EngineError> {
        crate::render::backend::Renderer::new_offscreen(size)
    }

    fn readback_surface(&self) -> ((u32, u32), crate::math::DevicePixelRatio) {
        let window = self
            .app
            .window(self.window)
            .expect("TestApp's window must exist");
        let scale = window.device_pixel_ratio();
        let size = window.size();
        let surface = scale.to_covering_device_rect(crate::math::LogicalRect::from_origin_size(
            LogicalPoint::ZERO,
            size,
        ));
        (
            (
                surface.width().max(dp(1)).value() as u32,
                surface.height().max(dp(1)).value() as u32,
            ),
            scale,
        )
    }

    pub fn read_pixels(&self) -> Result<SnapshotImage, SnapshotError> {
        self.read_pixels_with_scroll(&[])
    }

    pub fn read_pixels_with_scroll(
        &self,
        scroll: &[super::pixels::ScrollSlot],
    ) -> Result<SnapshotImage, SnapshotError> {
        let (size, scale) = self.readback_surface();
        let window = self
            .app
            .window(self.window)
            .ok_or_else(|| SnapshotError::Gpu {
                detail: smol_str::SmolStr::new_static("readback: window was closed"),
            })?;
        super::pixels::render_offscreen(&self.app, window, size, scale, scroll).map_err(gpu_error)
    }

    pub fn window(&mut self) -> TestWindow<'_> {
        TestWindow {
            app: &mut self.app,
            id: self.window,
        }
    }
}

fn gpu_error(error: EngineError) -> SnapshotError {
    SnapshotError::Gpu {
        detail: smol_str::SmolStr::new(error.to_string()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum A11yGate {
    Immediate,
    Pump,
}

pub struct TestWindow<'a> {
    app: &'a mut App,
    id: WindowId,
}

impl TestWindow<'_> {
    pub fn drain(&mut self) -> Result<FrameOutcome, EngineError> {
        match self.app.frame(self.id) {
            Some(result) => result,
            None => Ok(FrameOutcome::Idle),
        }
    }

    #[must_use]
    pub fn frame_record(&self) -> &FrameRecord {
        self.app
            .window(self.id)
            .expect("window must exist")
            .record()
            .expect("drain() must have produced a frame record")
    }

    #[must_use]
    pub fn probe(&self) -> Probe<'_> {
        let window = self.app.window(self.id).expect("window must exist");
        Probe::new(window.tree())
    }
}

#[cfg(test)]
mod tests {
    use super::TestApp;
    #[cfg(all(not(windows), not(target_os = "macos")))]
    use crate::test_support::snapshot::SnapshotError;
    use crate::{
        event::Event,
        math::{DevicePixelRatio, LogicalPoint, LogicalRect, LogicalSize, px},
        node::builder::NodeBuilder,
        paint::{
            canvas::Canvas,
            color::{Color, ColorResolver, LinearPremulRgba},
            style::{PaintSpec, StylePatch},
            style_builder::StyleBuilder,
        },
        test_support::{
            probe::Selector,
            record::{LaneOutcome, PrimitiveKind},
            snapshot::SnapshotCase,
        },
        text::{TextContent, TextStyle},
    };

    const RECT: LogicalRect = LogicalRect::from_origin_size(
        LogicalPoint::new(px(4.0), px(6.0)),
        crate::math::LogicalSize::new(px(40.0), px(16.0)),
    );

    fn white() -> PaintSpec {
        PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        }
    }

    fn mount_painted(app: &mut TestApp, id: &'static str) -> crate::node::id::NodeId {
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        NodeBuilder::boxed()
            .layout_override(RECT)
            .paint(white())
            .id(id)
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("挂载成功")
    }

    #[test]
    fn probe_by_id_and_bounds() {
        let mut app = TestApp::new();
        let node = mount_painted(&mut app, "panel");
        let _ = app.drain().expect("帧");
        let probe = app.probe();
        assert_eq!(probe.by_selector(Selector::Id("panel")), Some(node));
        assert!(probe.exists(Selector::Id("panel")));
        assert_eq!(probe.bounds(node), Some(RECT));
        assert!(probe.lane_of(node).is_some());
    }

    #[test]
    fn probe_by_text_and_path() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let panel = NodeBuilder::boxed()
            .id("panel")
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("挂载");
        let _label = NodeBuilder::text("price", TextStyle::new(px(12.0)))
            .layout_override(RECT)
            .id("price-label")
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), panel)
            .expect("挂载");
        let _ = app.drain().expect("帧");

        let probe = app.probe();
        assert!(probe.exists(Selector::Text("price")));
        assert_eq!(probe.count(Selector::Text("price")), 1);
        assert_eq!(
            probe.by_selector(Selector::Path(&["panel", "price-label"])),
            probe.by_selector(Selector::Id("price-label"))
        );
        assert_eq!(probe.by_selector(Selector::Path(&["missing"])), None);
    }

    #[test]
    fn probe_missing_and_hit() {
        let mut app = TestApp::new();
        let node = mount_painted(&mut app, "panel");
        let _ = app.drain().expect("帧");
        let probe = app.probe();
        assert_eq!(probe.by_selector(Selector::Id("nope")), None);
        let hit = probe
            .hit(LogicalPoint::new(px(10.0), px(10.0)))
            .expect("命中");
        assert_eq!(hit.node, node);
        assert!(probe.hit(LogicalPoint::new(px(-5.0), px(-5.0))).is_none());
    }

    #[test]
    fn drain_records_quad_fields() {
        let mut app = TestApp::new();
        let _node = mount_painted(&mut app, "panel");
        let _ = app.drain().expect("帧");
        let rendered = app.rendered().expect("已提交");
        assert_eq!(rendered.frame_index, app.frame_record().frame_index);
        assert_eq!(rendered.quads.len(), 1);
        let quad = rendered.quads[0];
        assert_eq!(quad.bounds, RECT);
        let expected: LinearPremulRgba = ColorResolver.resolve(Color::WHITE);
        assert_eq!(quad.fill, expected);
        assert!(quad.border.is_none());
        assert!(
            rendered
                .primitives
                .iter()
                .any(|primitive| primitive.kind == PrimitiveKind::Quad)
        );
        assert!(!app.frame_record().damage.is_empty());
    }

    #[test]
    fn second_drain_visits_only_the_changed_closure() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let color = app.app_mut().signal(Color::WHITE);
        {
            let _ = NodeBuilder::boxed()
                .layout_override(RECT)
                .bind_style(move |cx| StylePatch {
                    background: Some(*cx.read(&color)),
                    ..StylePatch::default()
                })
                .id("a")
                .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
                .expect("挂载");
        }
        let static_node = mount_painted(&mut app, "b");
        let _ = app.drain().expect("首帧");

        app.app_mut().write(&color, Color::BLACK);
        let record = app.drain().expect("第二帧").clone();
        assert!(record.rendered);
        assert!(
            !record
                .paint_log
                .lanes
                .iter()
                .any(|entry| entry.node == static_node),
            "未变化的兄弟节点不得被访问（脏子树剪枝）：{:?}",
            record.paint_log
        );
        assert!(
            !record.dirty_nodes.contains(&static_node),
            "未变化的兄弟节点不得被重建：{:?}",
            record.dirty_nodes
        );
        assert!(
            record
                .paint_log
                .lanes
                .iter()
                .all(|entry| entry.outcome == LaneOutcome::Reused
                    || record.dirty_nodes.contains(&entry.node)),
            "被访问的 lane 中，未重建者必须整桶复用：{:?}",
            record.paint_log
        );
    }

    #[test]
    fn oracle_matches_full_refresh() {
        let mut app = TestApp::new();
        let _ = mount_painted(&mut app, "panel");
        assert!(app.oracle_diff().is_none());
    }

    #[test]
    fn snapshot_result_is_typed_per_platform() {
        let app = TestApp::new();
        let result = app.snapshot(SnapshotCase {
            scene: crate::test_support::snapshot::SnapshotScene::Shapes,
            theme: crate::test_support::snapshot::SnapshotTheme::Light,
            size: crate::math::LogicalSize::new(px(64.0), px(64.0)),
            scale: DevicePixelRatio::new(1.0).expect("scale"),
        });
        #[cfg(windows)]
        {
            let image = result.expect("WARP snapshot");
            assert_eq!((image.width, image.height), (64, 64));
            assert_eq!(image.rgba8.len(), 64 * 64 * 4);
            assert!(image.rgba8.iter().all(|byte| *byte == 0));
        }
        #[cfg(target_os = "macos")]
        {
            let image = result.expect("Metal offscreen snapshot");
            assert_eq!((image.width, image.height), (64, 64));
            assert_eq!(image.rgba8.len(), 64 * 64 * 4);
        }
        #[cfg(target_os = "linux")]
        {
            match result {
                Ok(image) => {
                    assert_eq!((image.width, image.height), (64, 64));
                    assert_eq!(image.rgba8.len(), 64 * 64 * 4);
                    assert!(
                        image.rgba8.iter().all(|byte| *byte == 0),
                        "empty scene must read back fully transparent"
                    );
                }
                Err(SnapshotError::Gpu { detail }) => {
                    eprintln!("skip: no usable Vulkan device for the offscreen renderer: {detail}");
                }
                Err(other) => panic!("unexpected snapshot error: {other:?}"),
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn rendered_text_produces_visible_glyph_pixels() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut app = TestApp::with_options(crate::window::WindowOptions {
            title: "text pixels".into(),
            size: crate::math::LogicalSize::new(px(200.0), px(60.0)),
            ..crate::window::WindowOptions::default()
        });
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let _backplate = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(200.0), px(60.0)))
            .paint(PaintSpec {
                background: Color::BLACK,
                ..PaintSpec::default()
            })
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                root,
            )
            .expect("backplate");
        let _text = NodeBuilder::text(
            "WWWW",
            TextStyle {
                size: px(24.0),
                color: Color::WHITE,
                ..TextStyle::default()
            },
        )
        .layout_override(LogicalRect::new(px(10.0), px(8.0), px(180.0), px(40.0)))
        .build(
            app.app_mut().window_mut(window).expect("窗口").tree_mut(),
            root,
        )
        .expect("text");
        let _ = app.drain().expect("text frame");

        let image = app
            .snapshot(SnapshotCase {
                scene: crate::test_support::snapshot::SnapshotScene::Shapes,
                theme: crate::test_support::snapshot::SnapshotTheme::Light,
                size: crate::math::LogicalSize::new(px(200.0), px(60.0)),
                scale: DevicePixelRatio::ONE,
            })
            .expect("WARP snapshot");
        let bright = image
            .rgba8
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[0] > 128 && pixel[1] > 128 && pixel[2] > 128)
            .count();
        let coverage = bright as f32 / f32::from((image.width * image.height) as u16);
        assert!(bright > 0, "text must produce non-background pixels");
        assert!(
            coverage > 0.001 && coverage < 0.5,
            "glyph coverage {coverage} is implausible (0 or whole screen)"
        );
    }

    #[test]
    fn resize_forces_full_damage() {
        let mut app = TestApp::new();
        let _node = mount_painted(&mut app, "panel");
        let _ = app.drain().expect("首帧");

        let window = app.window_id();
        app.app_mut()
            .window_mut(window)
            .expect("窗口")
            .set_size(crate::math::LogicalSize::new(px(400.0), px(300.0)));
        let record = app.drain().expect("resize 帧").clone();
        assert!(record.damage_full, "resize 必须退化为全帧");
        let expected = DevicePixelRatio::ONE.to_covering_device_rect(LogicalRect::new(
            px(0.0),
            px(0.0),
            px(400.0),
            px(300.0),
        ));
        assert_eq!(record.damage, vec![expected]);
    }

    #[test]
    fn with_scale_injects_the_device_pixel_ratio() {
        let dpr = DevicePixelRatio::new(2.0).expect("2x is valid");
        let mut app = TestApp::with_scale(dpr);
        let window = app.window_id();
        let _node = mount_painted(&mut app, "panel");
        let _ = app.drain().expect("首帧");

        let target = app.app().window(window).expect("窗口");
        assert_eq!(target.device_pixel_ratio(), dpr);
        assert_eq!(target.physical_surface(), (1600, 1200));
        let dump = crate::test_support::diagnostics::frame_dump(&app);
        assert_eq!(dump.scale, 2.0);
        assert_eq!(
            dump.viewport,
            dpr.to_covering_device_rect(LogicalRect::from_origin_size(
                LogicalPoint::ZERO,
                LogicalSize::new(px(800.0), px(600.0)),
            ))
        );
        assert!(
            dump.instances
                .iter()
                .any(|instance| instance.bounds == [8.0, 12.0, 80.0, 32.0]),
            "注入的 DPR 必须把逻辑图元落到物理像素：{:?}",
            dump.instances
                .iter()
                .map(|instance| instance.bounds)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn with_scale_rasterizes_canvas_text_at_the_injected_ratio() {
        let dpr = DevicePixelRatio::new(2.0).expect("2x is valid");
        let mut app = TestApp::with_scale(dpr);
        let window = app.window_id();
        let bounds = LogicalRect::from_origin_size(
            LogicalPoint::ZERO,
            LogicalSize::new(px(120.0), px(40.0)),
        );
        let mut canvas = Canvas::new(bounds);
        assert!(canvas.push_text(
            TextContent::new("A", TextStyle::new(px(16.0))),
            LogicalPoint::ZERO,
        ));
        let node = {
            let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
            let root = tree.root();
            NodeBuilder::canvas(canvas)
                .layout_override(bounds)
                .build(tree, root)
                .expect("canvas 挂载")
        };
        let _ = app.drain().expect("帧");
        let resolved = app
            .app()
            .window(window)
            .expect("窗口")
            .canvas_text()
            .get(&node)
            .expect("canvas 文本必须已解析");
        assert_eq!(
            resolved.lines[0].scale, dpr,
            "注入的 DPR 必须决定文本光栅变体"
        );
    }

    #[test]
    fn a_dpi_change_repaints_without_relayout_and_the_event_is_diagnostic_only() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let node = mount_painted(&mut app, "panel");
        let _ = app.drain().expect("首帧");
        let bounds = app.probe().bounds(node).expect("bounds");

        app.dispatch_event(Event::DpiChanged { dpi: 192 });
        let _ = app.drain().expect("事件帧");
        assert_eq!(
            app.app().window(window).expect("窗口").device_pixel_ratio(),
            DevicePixelRatio::ONE,
            "DpiChanged 只是诊断事件，不得改变窗口 DPR"
        );
        assert_eq!(app.probe().bounds(node).expect("bounds"), bounds);

        let dpr = DevicePixelRatio::new(2.0).expect("2x is valid");
        app.set_dpr(dpr);
        let record = app.drain().expect("DPI 帧").clone();
        assert!(record.rendered, "DPI 变化必须重绘");
        assert!(record.damage_full, "DPI 变化必须退化为全帧");
        assert_eq!(
            app.app().window(window).expect("窗口").device_pixel_ratio(),
            dpr
        );
        assert_eq!(
            app.probe().bounds(node).expect("bounds"),
            bounds,
            "DPI 变化不得重排布局"
        );
    }

    #[test]
    fn text_glyphs_use_border_and_padding_offset() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let style = StyleBuilder::new()
            .size(px(80.0), px(40.0))
            .padding(px(4.0))
            .build();
        let node = NodeBuilder::text("MMM", TextStyle::new(px(12.0)))
            .style(style)
            .id("label")
            .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
            .expect("挂载");

        let _ = app.drain().expect("帧");
        assert_eq!(
            app.app()
                .window(window)
                .expect("窗口")
                .tree()
                .content_offset(node),
            LogicalPoint::new(px(4.0), px(4.0)),
            "content_offset = border + padding"
        );
        let rendered = app.rendered().expect("已提交");
        let glyph_bounds = rendered
            .primitives
            .iter()
            .find(|primitive| primitive.kind == PrimitiveKind::GlyphRun)
            .map(|primitive| primitive.bounds)
            .expect("必须有字形");
        assert!(
            glyph_bounds.origin.x >= px(3.0),
            "字形绝对坐标必须加上 padding 偏移（实际 {:?}）",
            glyph_bounds
        );
    }
}

#[cfg(test)]
mod p10_tests {
    use super::TestApp;
    use crate::{
        event::{Action, InputEvent, KeyCode, Modifiers, PointerButton},
        math::{LogicalPoint, LogicalRect, dp, px},
        node::builder::NodeBuilder,
        paint::{
            style::{Clip, PaintSpec, StylePatch},
            style_builder::StyleBuilder,
        },
    };

    fn app_with_button() -> (TestApp, crate::node::id::NodeId) {
        let mut app = TestApp::new();
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(10.0), px(10.0), px(60.0), px(40.0)))
            .paint(PaintSpec {
                background: crate::paint::color::Color::from_rgba8([10, 20, 30, 255]),
                ..PaintSpec::default()
            })
            .on_click(Action::custom("press"))
            .build(tree, root)
            .expect("mount");
        app.app_mut().on_action(Action::custom("press"), |_ctx| {});
        app.drain().expect("first frame");
        (app, node)
    }

    #[test]
    fn inject_click_reports_the_action_and_the_target() {
        let (mut app, node) = app_with_button();
        app.inject(InputEvent::pointer_down(
            LogicalPoint::new(px(20.0), px(20.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let outcome = app.inject(InputEvent::pointer_up(
            LogicalPoint::new(px(20.0), px(20.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        ));

        assert_eq!(outcome.hit.map(|hit| hit.node), Some(node));
        assert_eq!(outcome.action, Some(Action::custom("press")));
        assert_eq!(outcome.target, Some(node));
        assert_eq!(outcome.click_count, 1);
    }

    #[test]
    fn inject_scroll_only_damages_the_viewport() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        let mut style = StyleBuilder::new().size(px(100.0), px(100.0)).build();
        style.clip = Clip::Bounds;
        let container = NodeBuilder::boxed()
            .style(style)
            .build(tree, root)
            .expect("mount");
        for index in 0..4 {
            let _ = NodeBuilder::boxed()
                .layout_override(LogicalRect::new(
                    px(0.0),
                    px(index as f32 * 40.0),
                    px(100.0),
                    px(40.0),
                ))
                .paint(PaintSpec {
                    background: crate::paint::color::Color::from_rgba8([
                        index as u8 * 10 + 1,
                        0,
                        0,
                        255,
                    ]),
                    ..PaintSpec::default()
                })
                .build(tree, container);
        }
        app.drain().expect("first frame");
        let handle = app
            .app_mut()
            .make_scroll(
                window,
                container,
                crate::interaction::ScrollConfig::default(),
            )
            .expect("scroll");
        app.app_mut()
            .set_scroll_content(&handle, crate::math::LogicalSize::new(px(100.0), px(160.0)))
            .expect("content");
        app.app_mut()
            .scroll_to(&handle, LogicalPoint::new(px(0.0), px(40.0)))
            .expect("scroll");
        let record = app.drain().expect("scroll frame");

        let allowed = crate::math::DeviceRect::new(dp(-1), dp(-1), dp(101), dp(101));
        assert!(!record.damage.is_empty(), "滚动必须产生 damage");
        for rect in &record.damage {
            assert!(
                rect.intersect(allowed) == *rect,
                "damage 必须落在视口（含 1px AA 外扩）内：{rect:?}"
            );
        }
    }

    #[test]
    fn inject_double_click_reports_two() {
        let (mut app, node) = app_with_button();
        let click = |app: &mut TestApp| {
            app.inject(InputEvent::pointer_down(
                LogicalPoint::new(px(20.0), px(20.0)),
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            app.inject(InputEvent::pointer_up(
                LogicalPoint::new(px(20.0), px(20.0)),
                PointerButton::Primary,
                Modifiers::empty(),
            ))
        };

        assert_eq!(click(&mut app).click_count, 1);
        app.advance(std::time::Duration::from_millis(200));
        assert_eq!(click(&mut app).click_count, 2, "200ms 内同点 = 双击");
        app.advance(std::time::Duration::from_millis(600));
        assert_eq!(click(&mut app).click_count, 1, "超时后重新计数");
        let _ = node;
    }

    #[test]
    fn animation_reaches_idle_after_the_last_frame() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let transient = app.app_mut().transient(0.0_f32);
        app.app_mut()
            .animate(
                window,
                crate::interaction::AnimatedValue::Scalar {
                    target: transient,
                    to: 1.0,
                },
                &[],
                crate::interaction::AnimationSpec {
                    duration: std::time::Duration::from_millis(50),
                    easing: crate::interaction::Easing::Linear,
                },
            )
            .expect("animate");
        app.drain().expect("animation frame");
        app.advance(std::time::Duration::from_millis(60));
        app.drain().expect("final frame");

        assert_eq!(*app.app().read_transient(&transient), 1.0);
        assert!(!app.app().animations_running(window));
        let before = app.app().window(window).expect("window").frame_index();
        app.drain().expect("idle frame");
        let after = app.app().window(window).expect("window").frame_index();
        assert_eq!(before, after, "动画结束后必须回到空闲零帧");
    }

    #[test]
    fn tab_focus_is_visible_through_the_probe() {
        let (_app_unused, _node_unused) = app_with_button();
        let (mut app, node) = app_with_button();
        app.inject(InputEvent::key_down(KeyCode::Tab, Modifiers::empty()));
        assert_eq!(app.app().focused(app.window_id()), Some(node));
        assert!(
            !app.app()
                .window(app.window_id())
                .expect("window")
                .tree()
                .get(node)
                .expect("node")
                .dirty()
                .contains(crate::node::dirty::DirtyFlags::PAINT)
        );
        let _ = StylePatch::default();
    }
    #[test]
    fn nested_scroll_damage_is_in_screen_coordinates() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        let mut outer_style = StyleBuilder::new().size(px(200.0), px(200.0)).build();
        outer_style.clip = Clip::Bounds;
        let outer = NodeBuilder::boxed()
            .style(outer_style)
            .build(tree, root)
            .expect("outer");
        let mut inner_style = StyleBuilder::new()
            .size(px(200.0), px(200.0))
            .margin_sides(px(100.0), px(0.0), px(0.0), px(0.0))
            .build();
        inner_style.clip = Clip::Bounds;
        let inner = NodeBuilder::boxed()
            .style(inner_style)
            .build(tree, outer)
            .expect("inner");
        for index in 0..6 {
            let _ = NodeBuilder::boxed()
                .layout_override(LogicalRect::new(
                    px(0.0),
                    px(index as f32 * 40.0),
                    px(200.0),
                    px(40.0),
                ))
                .paint(PaintSpec {
                    background: crate::paint::color::Color::from_rgba8([
                        index as u8 + 1,
                        0,
                        0,
                        255,
                    ]),
                    ..PaintSpec::default()
                })
                .build(tree, inner);
        }
        app.drain().expect("first frame");

        let outer_handle = app
            .app_mut()
            .make_scroll(window, outer, crate::interaction::ScrollConfig::default())
            .expect("outer scroll");
        app.app_mut()
            .set_scroll_content(
                &outer_handle,
                crate::math::LogicalSize::new(px(200.0), px(400.0)),
            )
            .expect("outer content");
        let inner_handle = app
            .app_mut()
            .make_scroll(window, inner, crate::interaction::ScrollConfig::default())
            .expect("inner scroll");
        app.app_mut()
            .set_scroll_content(
                &inner_handle,
                crate::math::LogicalSize::new(px(200.0), px(240.0)),
            )
            .expect("inner content");
        app.app_mut()
            .scroll_to(&outer_handle, LogicalPoint::new(px(0.0), px(30.0)))
            .expect("outer scroll");
        let _ = app.drain().expect("outer frame");

        app.app_mut()
            .scroll_to(&inner_handle, LogicalPoint::new(px(0.0), px(10.0)))
            .expect("inner scroll");
        let record = app.drain().expect("inner frame");

        let allowed = crate::math::DeviceRect::new(dp(-1), dp(69), dp(201), dp(271));
        assert!(!record.damage.is_empty(), "内层滚动必须产生 damage");
        for rect in &record.damage {
            assert!(
                rect.intersect(allowed) == *rect,
                "嵌套滚动的 damage 必须在**屏幕**坐标内：{rect:?}"
            );
        }
        assert!(
            record
                .damage
                .iter()
                .any(|rect| (69..=71).contains(&rect.y0.value())),
            "damage 顶部必须落在内层视口的屏幕位置：{:?}",
            record.damage
        );
    }

    #[test]
    fn clearing_focus_damages_the_ring_outset() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        let ringed = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(10.0), px(10.0), px(40.0), px(40.0)))
            .paint(PaintSpec {
                background: crate::paint::color::Color::from_rgba8([9, 9, 9, 255]),
                ..PaintSpec::default()
            })
            .on_click(Action::custom("a"))
            .focus_ring(crate::paint::style::FocusRing::new(
                px(2.0),
                px(2.0),
                crate::paint::color::Color::from_rgba8([0, 120, 255, 255]),
            ))
            .build(tree, root)
            .expect("ringed");
        let other = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(100.0), px(10.0), px(40.0), px(40.0)))
            .on_click(Action::custom("b"))
            .build(tree, root)
            .expect("other");
        app.drain().expect("first frame");
        app.app_mut()
            .set_focus(window, Some(ringed))
            .then_some(())
            .expect("focus");
        let _ = app.drain().expect("focus frame");

        app.app_mut()
            .set_focus(window, Some(other))
            .then_some(())
            .expect("move focus");
        let record = app.drain().expect("unfocus frame");

        assert!(
            record
                .damage
                .iter()
                .any(|rect| rect.x0 <= 6 && rect.y0 <= 6 && rect.x1 >= 54 && rect.y1 >= 54),
            "清焦点时必须把环的外扩并入 damage（否则残留 1px）：{:?}",
            record.damage
        );
    }
}

#[cfg(test)]
mod p11_tests {
    use smol_str::SmolStr;

    use super::TestApp;
    use crate::{
        a11y::{A11yAction, A11yDiagnostic, A11yOverride, A11yPatch, A11yRole, A11yTreeSnapshot},
        event::Action,
        math::{DevicePixelRatio, LogicalPoint, LogicalPx, LogicalRect, px},
        node::{builder::NodeBuilder, dirty::DirtyFlags, id::NodeId},
        paint::{
            canvas::Canvas,
            color::Color,
            style::{Clip, PaintSpec, Style, StylePatch},
            style_builder::StyleBuilder,
        },
        text::TextStyle,
        text_edit::EditableSpec,
        window::WindowOptions,
    };

    fn white() -> PaintSpec {
        PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        }
    }

    fn mount(
        app: &mut TestApp,
        build: impl FnOnce(&mut crate::node::tree::NodeTree, NodeId) -> NodeId,
    ) -> NodeId {
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        build(tree, root)
    }

    fn snapshot(app: &TestApp) -> &A11yTreeSnapshot {
        app.a11y().expect("本帧必须有一笔 a11y 交付")
    }

    fn scroll_container_style(width: LogicalPx, height: LogicalPx) -> Style {
        let mut style = StyleBuilder::new().relative().size(width, height).build();
        style.clip = Clip::Bounds;
        style
    }

    fn fixed_size_style(width: LogicalPx, height: LogicalPx) -> Style {
        StyleBuilder::new().size(width, height).build()
    }

    fn child_of(app: &TestApp, parent: NodeId) -> NodeId {
        let window = app.window_id();
        app.app()
            .window(window)
            .expect("window")
            .tree()
            .get(parent)
            .expect("parent")
            .children()
            .first()
            .copied()
            .expect("parent has a child")
    }

    #[test]
    fn a11y_roles_labels_and_values() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let root = {
            let window = app.window_id();
            app.app().window(window).expect("window").tree().root()
        };
        let label = mount(&mut app, |tree, root| {
            NodeBuilder::text("价格", TextStyle::new(px(12.0)))
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(40.0), px(20.0)))
                .id("price")
                .build(tree, root)
                .expect("text")
        });
        let button = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(24.0), px(60.0), px(24.0)))
                .paint(white())
                .on_click(Action::custom("submit"))
                .a11y(A11yOverride {
                    label: Some(SmolStr::new_static("提交")),
                    ..A11yOverride::default()
                })
                .id("submit")
                .build(tree, root)
                .expect("button")
        });
        let canvas = mount(&mut app, |tree, root| {
            NodeBuilder::canvas(Canvas::new(LogicalRect::new(
                px(0.0),
                px(0.0),
                px(60.0),
                px(30.0),
            )))
            .a11y(A11yOverride {
                label: Some(SmolStr::new_static("K 线图")),
                ..A11yOverride::default()
            })
            .id("chart")
            .build(tree, root)
            .expect("canvas")
        });
        let _ = app.drain().expect("frame");

        let snap = snapshot(&app);
        assert!(snap.full, "首笔必须是全树");
        let root_node = snap.node_for(root).expect("root");
        assert_eq!(root_node.role, A11yRole::Window);
        assert_eq!(root_node.label_text(), None, "默认窗口没有标题 ⇒ 根无名");
        let label_node = snap.node_for(label).expect("label");
        assert_eq!(label_node.role, A11yRole::Label);
        assert_eq!(
            label_node.value.as_deref(),
            Some("价格"),
            "文本走 value（Role::Label）"
        );
        let button_node = snap.node_for(button).expect("button");
        assert_eq!(button_node.role, A11yRole::Button);
        assert_eq!(button_node.label_text(), Some("提交"));
        assert!(button_node.actions.contains(&A11yAction::Click));
        assert!(button_node.actions.contains(&A11yAction::Focus));
        let canvas_node = snap.node_for(canvas).expect("canvas");
        assert_eq!(canvas_node.role, A11yRole::Image);
        assert_eq!(canvas_node.label_text(), Some("K 线图"));
        assert!(!canvas_node.hidden);
    }

    #[test]
    fn a11y_box_is_a_transparent_container() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let panel = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .id("panel")
                .build(tree, root)
                .expect("panel")
        });
        let _ = app.drain().expect("frame");
        assert_eq!(
            snapshot(&app).node_for(panel).expect("panel").role,
            A11yRole::GenericContainer,
            "没有语义的 Box 必须是 GenericContainer（AccessKit 侧透明）"
        );
    }

    #[test]
    fn a11y_increment_contains_only_the_change_and_its_ancestors() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let text = app.app_mut().signal(SmolStr::new_static("旧的"));
        let label_signal = text;
        let button = mount(&mut app, move |tree, root| {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(60.0), px(24.0)))
                .paint(white())
                .on_click(Action::custom("press"))
                .bind_a11y(move |cx| A11yPatch {
                    label: Some(cx.read(&label_signal).clone()),
                    ..A11yPatch::default()
                })
                .id("press")
                .build(tree, root)
                .expect("button")
        });
        let _ = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(30.0), px(60.0), px(24.0)))
                .id("sibling")
                .build(tree, root)
                .expect("sibling")
        });
        let _ = app.drain().expect("first");
        assert!(snapshot(&app).full);

        app.app_mut().write(&text, SmolStr::new_static("新的"));
        let _ = app.drain().expect("second");
        let snap = snapshot(&app);
        assert!(!snap.full, "第二笔必须是增量");
        let ids: Vec<NodeId> = snap.nodes().iter().map(|node| node.engine).collect();
        assert_eq!(
            ids,
            vec![root, button],
            "增量 = 变更节点 + 祖先链（不含兄弟）"
        );
        assert_eq!(
            snap.node_for(button).expect("button").label_text(),
            Some("新的")
        );
    }

    #[test]
    fn a11y_focus_is_always_present_even_on_an_idle_frame() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let button = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(60.0), px(24.0)))
                .on_click(Action::custom("press"))
                .id("press")
                .build(tree, root)
                .expect("button")
        });
        let _ = app.drain().expect("first");

        app.app_mut().set_focus(window, Some(button));
        let record = app.drain().expect("focus frame");
        assert!(!record.rendered, "纯 A11Y 变化必须让本帧保持 Idle");
        let snap = record.a11y.as_ref().expect("Idle 帧也要有 a11y 交付");
        assert_eq!(
            snap.focus(),
            snap.id_for_engine(button).expect("焦点节点的稳定 id"),
            "TreeUpdate.focus 必须是稳定 id"
        );
        assert_eq!(snap.focus_node().engine, button, "焦点节点必须在快照里");
    }

    #[test]
    fn a11y_structure_matches_the_tree() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let panel = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .id("panel")
                .build(tree, root)
                .expect("panel")
        });
        let _ = app.drain().expect("first");

        let child = mount(&mut app, |tree, _root| {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(20.0), px(20.0)))
                .id("child")
                .build(tree, panel)
                .expect("child")
        });
        let _ = app.drain().expect("second");
        let snap = snapshot(&app);
        let panel_id = snap.id_for_engine(panel).expect("panel 的稳定 id");
        let child_id = snap.node_for(child).expect("新子节点必须出现").id;
        assert_eq!(
            snap.children_of(panel_id),
            vec![child_id],
            "父的 children 必须与节点树一致（稳定 id）"
        );
        assert_eq!(
            snap.node_for(panel).expect("panel").children,
            vec![child_id]
        );
    }

    #[test]
    fn a11y_bounds_equal_probe_times_scale() {
        let scale = DevicePixelRatio::new(1.25).expect("scale");
        let mut app = TestApp::with_options(WindowOptions {
            title: SmolStr::new_static("scale"),
            size: crate::math::LogicalSize::new(px(400.0), px(300.0)),
            ..WindowOptions::default()
        });
        app.set_dpr(scale);
        app.enable_a11y_capture();
        let node = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(8.0), px(12.0), px(40.0), px(24.0)))
                .paint(white())
                .id("box")
                .build(tree, root)
                .expect("box")
        });
        let _ = app.drain().expect("frame");
        let logical = app.probe().bounds(node).expect("bounds");
        let expected = scale.to_covering_device_rect(logical);
        let snap = snapshot(&app);
        assert_eq!(
            snap.node_for(node).expect("box").bounds,
            expected,
            "a11y bounds 必须是 逻辑矩形 × scale 的 floor/ceil 外扩（覆盖整块逻辑矩形）"
        );
    }

    #[test]
    fn a11y_bind_a11y_disabled_is_not_layout() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let disabled = app.app_mut().signal(false);
        let signal = disabled;
        let button = mount(&mut app, move |tree, root| {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(60.0), px(24.0)))
                .paint(white())
                .on_click(Action::custom("press"))
                .bind_a11y(move |cx| A11yPatch {
                    disabled: Some(*cx.read(&signal)),
                    ..A11yPatch::default()
                })
                .id("press")
                .build(tree, root)
                .expect("button")
        });
        let _ = app.drain().expect("first");
        assert!(!snapshot(&app).node_for(button).expect("button").disabled);

        app.app_mut().write(&disabled, true);
        let _ = app.drain().expect("second");
        let snap = snapshot(&app);
        assert!(snap.node_for(button).expect("button").disabled);
        let flags = app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(button)
            .expect("button")
            .dirty();
        assert!(
            !flags.contains(DirtyFlags::LAYOUT),
            "`bind_a11y` 的禁用态不得触发重排（A11Y 不产生视觉 damage）"
        );
    }

    #[test]
    fn a11y_canvas_requires_alt_text() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let canvas = mount(&mut app, |tree, root| {
            NodeBuilder::canvas(Canvas::new(LogicalRect::new(
                px(0.0),
                px(0.0),
                px(60.0),
                px(30.0),
            )))
            .id("chart")
            .build(tree, root)
            .expect("canvas")
        });
        let _ = app.drain().expect("frame");
        let snap = snapshot(&app);
        assert!(
            snap.node_for(canvas).expect("canvas").hidden,
            "缺替代文本的自绘节点必须 hidden（不静默丢弃）"
        );
        assert!(
            app.app()
                .a11y_diagnostics()
                .iter()
                .any(|diagnostic| matches!(
                    diagnostic,
                    A11yDiagnostic::MissingAltText { node, .. } if *node == canvas
                )),
            "必须记 MissingAltText 诊断"
        );
    }

    #[test]
    fn a11y_hidden_nodes_are_excluded() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let hidden = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(20.0), px(20.0)))
                .id("hidden")
                .build(tree, root)
                .expect("hidden")
        });
        let _ = app.drain().expect("first");

        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .set_style_patch(
                hidden,
                StylePatch {
                    visible: Some(false),
                    ..StylePatch::default()
                },
            );
        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_a11y(hidden);
        let _ = app.drain().expect("second");
        let snap = snapshot(&app);
        assert!(
            snap.node_for(hidden).expect("hidden").hidden,
            "`visible: false` 必须传播到 a11y 的 hidden"
        );
        assert!(
            snap.visible_nodes_in_batch()
                .all(|node| node.engine != hidden),
            "hidden 节点不得出现在可见集合里"
        );
    }

    #[test]
    fn a11y_scale_change_resends_the_full_tree() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let node = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(20.0), px(20.0)))
                .paint(white())
                .build(tree, root)
                .expect("box")
        });
        let _ = app.drain().expect("first");
        assert!(snapshot(&app).full);

        let window = app.window_id();
        app.app_mut()
            .window_mut(window)
            .expect("window")
            .set_device_pixel_ratio(DevicePixelRatio::new(1.5).expect("scale"));
        let _ = app.drain().expect("second");
        let snap = snapshot(&app);
        assert!(snap.full, "DPI 变化必须整棵重发（bounds 全是物理像素）");
        assert_eq!(
            snap.node_for(node).expect("box").bounds,
            DevicePixelRatio::new(1.5)
                .expect("scale")
                .to_device_rect(LogicalRect::new(px(0.0), px(0.0), px(20.0), px(20.0)))
        );
    }

    #[test]
    fn a11y_virtual_list_exposes_only_mounted_rows() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let mut rows_style = StyleBuilder::new()
            .relative()
            .size(px(100.0), px(80.0))
            .build();
        rows_style.clip = Clip::Bounds;
        let container = mount(&mut app, move |tree, root| {
            NodeBuilder::boxed()
                .style(rows_style)
                .id("rows")
                .build(tree, root)
                .expect("container")
        });
        let list = app
            .app_mut()
            .make_virtual_list(
                window,
                container,
                crate::interaction::VirtualListConfig {
                    extent: px(20.0),
                    extents: None,
                    overscan: 1,
                },
            )
            .expect("virtual list");
        app.app_mut()
            .set_virtual_items(&list, 100, |index| {
                NodeBuilder::boxed()
                    .a11y(A11yOverride {
                        label: Some(smol_str::format_smolstr!("row {index}")),
                        ..A11yOverride::default()
                    })
                    .id("row")
            })
            .expect("items");
        let _ = app.drain().expect("frame");

        let snap = snapshot(&app);
        let rows: Vec<&crate::a11y::A11yNode> = snap
            .nodes()
            .iter()
            .filter(|node| node.role == A11yRole::ListBoxOption)
            .collect();
        let mounted = app.app().virtual_list_state(&list).expect("state").mounted;
        assert_eq!(rows.len(), mounted, "a11y 只暴露已挂载的行（可见窗口）");
        assert_eq!(snap.node_for(container).expect("list").role, A11yRole::List);
        assert!(mounted < 100, "100 行的列表不得整表挂载");
        let _ = LogicalPoint::ZERO;
    }

    #[test]
    fn a11y_revealing_an_ancestor_refreshes_descendant_focus_incrementally() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let visible = app.app_mut().signal(false);
        let panel = mount(&mut app, move |tree, root| {
            let panel = NodeBuilder::boxed()
                .style(fixed_size_style(px(120.0), px(40.0)))
                .bind_style(move |cx| StylePatch {
                    visible: Some(*cx.read(&visible)),
                    ..StylePatch::default()
                })
                .id("panel")
                .build(tree, root)
                .expect("panel");
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(60.0), px(24.0)))
                .tab_stop(true)
                .on_click(Action::custom("tab"))
                .id("tab")
                .build(tree, panel)
                .expect("tab");
            panel
        });
        let tab = child_of(&app, panel);
        let _ = app.drain().expect("seed");
        let seeded = snapshot(&app);
        let record = seeded.node_for(tab).expect("tab 必须在首笔全树里");
        assert!(
            record.actions.contains(&A11yAction::Click),
            "有 on_click ⇒ Click"
        );
        assert!(
            !record.actions.contains(&A11yAction::Focus),
            "祖先不可见 ⇒ 后代不可聚焦、不得宣告 Focus：{:?}",
            record.actions
        );

        app.app_mut().write(&visible, true);
        let _ = app.drain().expect("reveal");
        let revealed = snapshot(&app);
        assert!(!revealed.full, "揭示面板必须走增量（不是全树）");
        let record = revealed
            .node_for(tab)
            .expect("揭示后后代必须重发（旧实现只标祖先 ⇒ 增量笔丢 Focus）");
        assert!(
            record.actions.contains(&A11yAction::Focus),
            "祖先变为可见后，后代的隐式 Focus 必须出现在增量笔里：{:?}",
            record.actions
        );
        assert_eq!(
            record.parent,
            revealed.id_for_engine(panel),
            "重发记录仍挂原 a11y 父"
        );
        app.assert_a11y_oracle();
    }

    #[test]
    fn a11y_enabling_an_ancestor_refreshes_descendant_actions() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let enabled = app.app_mut().signal(false);
        let panel = mount(&mut app, move |tree, root| {
            let panel = NodeBuilder::boxed()
                .style(fixed_size_style(px(120.0), px(40.0)))
                .bind_style(move |cx| StylePatch {
                    interactive: Some(*cx.read(&enabled)),
                    ..StylePatch::default()
                })
                .id("panel")
                .build(tree, root)
                .expect("panel");
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(60.0), px(24.0)))
                .tab_stop(true)
                .on_click(Action::custom("buy"))
                .a11y(A11yOverride {
                    role: Some(A11yRole::Button),
                    label: Some(SmolStr::new_static("买入")),
                    ..A11yOverride::default()
                })
                .id("buy")
                .build(tree, panel)
                .expect("button");
            panel
        });
        let button = child_of(&app, panel);
        let _ = app.drain().expect("seed");
        let seeded = snapshot(&app);
        let record = seeded.node_for(button).expect("button");
        assert!(
            record.disabled,
            "祖先 interactive=false ⇒ 后代继承 disabled"
        );
        assert!(
            !record.actions.contains(&A11yAction::Click)
                && !record.actions.contains(&A11yAction::Focus),
            "禁用子树不得宣告状态改变动作：{:?}",
            record.actions
        );

        app.app_mut().write(&enabled, true);
        let _ = app.drain().expect("enable");
        let snap = snapshot(&app);
        assert!(!snap.full);
        let record = snap
            .node_for(button)
            .expect("启用后后代必须重发（旧实现只标祖先）");
        assert!(!record.disabled, "祖先可交互后后代不再是 disabled");
        assert!(
            record.actions.contains(&A11yAction::Click)
                && record.actions.contains(&A11yAction::Focus),
            "动作能力必须随祖先一起恢复：{:?}",
            record.actions
        );
        app.assert_a11y_oracle();
    }

    #[test]
    fn a11y_tab_switch_refreshes_selection() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let active = app.app_mut().signal(0usize);
        let tabs = mount(&mut app, move |tree, root| {
            let tab_list = NodeBuilder::boxed()
                .style(fixed_size_style(px(200.0), px(32.0)))
                .a11y(A11yOverride {
                    role: Some(A11yRole::TabList),
                    label: Some(SmolStr::new_static("页面")),
                    ..A11yOverride::default()
                })
                .id("tabs")
                .build(tree, root)
                .expect("tab list");
            let mut nodes = Vec::with_capacity(2);
            let actions = [Action::custom("tab.0"), Action::custom("tab.1")];
            for (index, action) in actions.into_iter().enumerate() {
                nodes.push(
                    NodeBuilder::boxed()
                        .layout_override(LogicalRect::new(
                            px(index as f32 * 100.0),
                            px(0.0),
                            px(100.0),
                            px(32.0),
                        ))
                        .tab_stop(true)
                        .on_click(action)
                        .a11y(A11yOverride {
                            role: Some(A11yRole::Tab),
                            label: Some(smol_str::format_smolstr!("页面 {index}")),
                            ..A11yOverride::default()
                        })
                        .bind_a11y(move |cx| A11yPatch {
                            selected: Some(*cx.read(&active) == index),
                            pos_in_set: Some(index),
                            size_of_set: Some(2),
                            ..A11yPatch::default()
                        })
                        .id(smol_str::format_smolstr!("tab.{index}"))
                        .build(tree, tab_list)
                        .expect("tab"),
                );
            }
            tab_list
        });
        let tab_nodes: Vec<NodeId> = {
            let window = app.window_id();
            app.app()
                .window(window)
                .expect("window")
                .tree()
                .get(tabs)
                .expect("tabs")
                .children()
                .to_vec()
        };
        let _ = app.drain().expect("seed");
        let seeded = snapshot(&app);
        assert_eq!(
            seeded.node_for(tab_nodes[0]).expect("tab 0").selected,
            Some(true)
        );
        assert_eq!(
            seeded.node_for(tab_nodes[1]).expect("tab 1").selected,
            Some(false)
        );
        for tab in &tab_nodes {
            assert!(
                seeded
                    .node_for(*tab)
                    .expect("tab")
                    .actions
                    .contains(&A11yAction::Focus),
                "Tab 是可聚焦控件，必须宣告 Focus"
            );
        }

        app.app_mut().write(&active, 1);
        let _ = app.drain().expect("switch");
        let snap = snapshot(&app);
        assert!(!snap.full);
        assert_eq!(
            snap.node_for(tab_nodes[0]).expect("tab 0").selected,
            Some(false)
        );
        assert_eq!(
            snap.node_for(tab_nodes[1]).expect("tab 1").selected,
            Some(true)
        );
        for tab in &tab_nodes {
            assert!(
                snap.node_for(*tab)
                    .expect("切换后两个 Tab 都必须重发")
                    .actions
                    .contains(&A11yAction::Focus),
                "切换不得丢 Focus 能力"
            );
        }
        app.assert_a11y_oracle();
    }

    #[test]
    fn a11y_role_change_refreshes_descendant_derived_roles() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let as_input = app.app_mut().signal(false);
        let panel = mount(&mut app, move |tree, root| {
            let panel = NodeBuilder::boxed()
                .style(fixed_size_style(px(120.0), px(40.0)))
                .a11y(A11yOverride {
                    label: Some(SmolStr::new_static("价格")),
                    ..A11yOverride::default()
                })
                .bind_a11y(move |cx| A11yPatch {
                    role: Some(if *cx.read(&as_input) {
                        A11yRole::TextInput
                    } else {
                        A11yRole::Group
                    }),
                    ..A11yPatch::default()
                })
                .id("panel")
                .build(tree, root)
                .expect("panel");
            NodeBuilder::text("42150", TextStyle::new(px(12.0)))
                .id("value")
                .build(tree, panel)
                .expect("text");
            panel
        });
        let text = child_of(&app, panel);
        let _ = app.drain().expect("seed");
        let seeded = snapshot(&app);
        assert_eq!(
            seeded.node_for(text).expect("text").role,
            A11yRole::Label,
            "普通容器里的文本是 Label"
        );

        app.app_mut().write(&as_input, true);
        let _ = app.drain().expect("role change");
        let snap = snapshot(&app);
        assert!(!snap.full);
        let record = snap
            .node_for(text)
            .expect("祖先角色变化后文本子节点必须重发（affects_descendants）");
        assert_eq!(
            record.role,
            A11yRole::TextRun,
            "父角色变成 TextInput 后，文本子节点必须被推导成 TextRun"
        );
        assert_eq!(
            crate::a11y::adapter::to_accesskit(record).role(),
            accesskit::Role::TextRun
        );
        app.assert_a11y_oracle();
    }

    #[test]
    fn a11y_role_change_through_the_style_pass_refreshes_descendant_roles() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let as_input = app.app_mut().signal(false);
        let panel = mount(&mut app, move |tree, root| {
            let panel = NodeBuilder::boxed()
                .style(fixed_size_style(px(120.0), px(40.0)))
                .a11y(A11yOverride {
                    label: Some(SmolStr::new_static("价格")),
                    ..A11yOverride::default()
                })
                .bind_style(move |cx| StylePatch {
                    opacity: Some(if *cx.read(&as_input) { 0.5 } else { 1.0 }),
                    ..StylePatch::default()
                })
                .bind_a11y(move |cx| A11yPatch {
                    role: Some(if *cx.read(&as_input) {
                        A11yRole::TextInput
                    } else {
                        A11yRole::Group
                    }),
                    ..A11yPatch::default()
                })
                .id("panel")
                .build(tree, root)
                .expect("panel");
            NodeBuilder::text("42150", TextStyle::new(px(12.0)))
                .id("value")
                .build(tree, panel)
                .expect("text");
            panel
        });
        let text = child_of(&app, panel);
        let _ = app.drain().expect("seed");
        assert_eq!(
            snapshot(&app).node_for(text).expect("text").role,
            A11yRole::Label,
            "普通容器里的文本是 Label"
        );

        app.app_mut().write(&as_input, true);
        let _ = app.drain().expect("style pass frame");
        assert!(
            !app.frame_record().a11y_only,
            "本帧有 bind_style 工作，必须走普通 style pass（不是 a11y-only 快路径）"
        );
        let snap = snapshot(&app);
        assert!(!snap.full, "角色变化必须是增量笔");
        let record = snap
            .node_for(text)
            .expect("style pass 的 bind_a11y 写点也必须重发后代（F2）");
        assert_eq!(
            record.role,
            A11yRole::TextRun,
            "父角色变成 TextInput 后文本子节点必须被推导成 TextRun"
        );
        app.assert_a11y_oracle();
    }

    #[test]
    fn a11y_set_patch_refreshes_descendant_derived_roles() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let panel = mount(&mut app, |tree, root| {
            let panel = NodeBuilder::boxed()
                .style(fixed_size_style(px(120.0), px(40.0)))
                .a11y(A11yOverride {
                    label: Some(SmolStr::new_static("价格")),
                    ..A11yOverride::default()
                })
                .id("panel")
                .build(tree, root)
                .expect("panel");
            NodeBuilder::text("42150", TextStyle::new(px(12.0)))
                .id("value")
                .build(tree, panel)
                .expect("text");
            panel
        });
        let text = child_of(&app, panel);
        let _ = app.drain().expect("seed");
        assert_eq!(
            snapshot(&app).node_for(text).expect("text").role,
            A11yRole::Label
        );

        app.app_mut()
            .set_a11y_patch(
                window,
                panel,
                A11yPatch {
                    role: Some(A11yRole::TextInput),
                    ..A11yPatch::default()
                },
            )
            .expect("explicit a11y patch");
        let _ = app.drain().expect("patch frame");
        let snap = snapshot(&app);
        assert!(!snap.full, "显式补丁必须是增量笔");
        let record = snap
            .node_for(text)
            .expect("set_a11y_patch 通道也必须重发后代（F3）");
        assert_eq!(
            record.role,
            A11yRole::TextRun,
            "显式补丁与 bind_a11y 同通道：role → 后代 TextRun"
        );
        app.assert_a11y_oracle();
    }

    #[test]
    fn a11y_hidden_change_refreshes_adopted_descendant_hidden() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let hidden = app.app_mut().signal(false);
        let table = mount(&mut app, move |tree, root| {
            let table = NodeBuilder::boxed()
                .style(fixed_size_style(px(200.0), px(60.0)))
                .a11y(A11yOverride {
                    role: Some(A11yRole::Table),
                    label: Some(SmolStr::new_static("行情")),
                    ..A11yOverride::default()
                })
                .id("table")
                .build(tree, root)
                .expect("table");
            let wrapper = NodeBuilder::boxed()
                .style(fixed_size_style(px(200.0), px(20.0)))
                .bind_a11y(move |cx| A11yPatch {
                    hidden: Some(*cx.read(&hidden)),
                    ..A11yPatch::default()
                })
                .id("wrapper")
                .build(tree, table)
                .expect("wrapper");
            NodeBuilder::boxed()
                .style(fixed_size_style(px(200.0), px(20.0)))
                .a11y(A11yOverride {
                    role: Some(A11yRole::Row),
                    ..A11yOverride::default()
                })
                .id("row")
                .build(tree, wrapper)
                .expect("row");
            table
        });
        let wrapper = child_of(&app, table);
        let row = child_of(&app, wrapper);
        let _ = app.drain().expect("seed");
        let seeded = snapshot(&app);
        assert!(!seeded.node_for(row).expect("row").hidden, "初始不隐藏");
        assert_eq!(
            seeded.node_for(row).expect("row").parent,
            Some(seeded.node_for(table).expect("table").id),
            "行被领养到 Table 下（物理父是包裹层）"
        );

        app.app_mut().write(&hidden, true);
        let _ = app.drain().expect("hidden change");
        let snap = snapshot(&app);
        assert!(!snap.full);
        assert!(
            snap.node_for(row)
                .expect("被领养行必须重发（affects_descendants 的 hidden 分支）")
                .hidden,
            "跳过的物理祖先声明 hidden ⇒ 被领养的后代必须隐藏"
        );
        app.assert_a11y_oracle();
    }

    #[test]
    fn a11y_virtual_rows_refresh_while_the_panel_stays_open() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let open = app.app_mut().signal(false);
        let container = mount(&mut app, move |tree, root| {
            let panel = NodeBuilder::boxed()
                .style(fixed_size_style(px(100.0), px(80.0)))
                .bind_style(move |cx| StylePatch {
                    visible: Some(*cx.read(&open)),
                    ..StylePatch::default()
                })
                .id("panel")
                .build(tree, root)
                .expect("panel");
            NodeBuilder::boxed()
                .style(scroll_container_style(px(100.0), px(80.0)))
                .id("rows")
                .build(tree, panel)
                .expect("container")
        });
        let list = app
            .app_mut()
            .make_virtual_list(
                window,
                container,
                crate::interaction::VirtualListConfig {
                    extent: px(20.0),
                    extents: None,
                    overscan: 0,
                },
            )
            .expect("virtual list");
        app.app_mut()
            .set_virtual_items(&list, 3, |index| {
                NodeBuilder::boxed()
                    .a11y(A11yOverride {
                        label: Some(smol_str::format_smolstr!("row {index}")),
                        ..A11yOverride::default()
                    })
                    .id("row")
            })
            .expect("items");
        let _ = app.drain().expect("seed");
        app.app_mut().write(&open, true);
        let _ = app.drain().expect("open panel");
        let rows: Vec<NodeId> = app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .virtual_slot(container)
            .expect("virtual slot")
            .mounted
            .iter()
            .map(|(_, row)| *row)
            .collect();
        assert!(!rows.is_empty(), "首窗必须挂载行");

        for (index, row) in rows.iter().enumerate() {
            app.app_mut()
                .set_a11y_patch(
                    window,
                    *row,
                    A11yPatch {
                        selected: Some(true),
                        checked: Some(true),
                        pos_in_set: Some(index),
                        ..A11yPatch::default()
                    },
                )
                .expect("dynamic row a11y");
        }
        let _ = app.drain().expect("row refresh");
        let snap = snapshot(&app);
        assert!(!snap.full, "行选中刷新必须是增量（不得整表重建）");
        for row in &rows {
            let record = snap
                .node_for(*row)
                .expect("被刷新的行必须在本笔增量里（静态快照的失败面）");
            assert_eq!(record.selected, Some(true), "selected 必须刷新");
            assert_eq!(record.checked, Some(true), "多选勾选态必须刷新");
        }
        assert_eq!(
            app.app()
                .window(window)
                .expect("window")
                .tree()
                .virtual_slot(container)
                .expect("slot")
                .mounted
                .len(),
            rows.len(),
            "刷新期不得重建行（同一批句柄仍在挂载）"
        );
        app.assert_a11y_oracle();
    }

    #[test]
    fn a11y_virtual_rows_follow_a_selection_signal() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let active = app.app_mut().signal(0usize);
        let container = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .style(scroll_container_style(px(100.0), px(80.0)))
                .id("rows")
                .build(tree, root)
                .expect("container")
        });
        let list = app
            .app_mut()
            .make_virtual_list(
                window,
                container,
                crate::interaction::VirtualListConfig {
                    extent: px(20.0),
                    extents: None,
                    overscan: 0,
                },
            )
            .expect("virtual list");
        app.app_mut()
            .set_virtual_items(&list, 2, move |index| {
                NodeBuilder::boxed()
                    .bind_a11y(move |cx| A11yPatch {
                        selected: Some(*cx.read(&active) == index),
                        checked: Some(*cx.read(&active) == index),
                        pos_in_set: Some(index),
                        size_of_set: Some(2),
                        ..A11yPatch::default()
                    })
                    .id("row")
            })
            .expect("items");
        let _ = app.drain().expect("settle");
        app.enable_a11y_capture();
        let _ = app.drain().expect("seed");
        let rows: Vec<NodeId> = app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .virtual_slot(container)
            .expect("virtual slot")
            .mounted
            .iter()
            .map(|(_, row)| *row)
            .collect();
        let seeded = snapshot(&app);
        assert_eq!(
            seeded.node_for(rows[0]).expect("row 0").selected,
            Some(true)
        );
        assert_eq!(
            seeded.node_for(rows[1]).expect("row 1").selected,
            Some(false)
        );

        app.app_mut().write(&active, 1);
        let _ = app.drain().expect("select");
        let snap = snapshot(&app);
        assert!(!snap.full, "选中集变化必须走增量（bind_a11y 信号依赖）");
        assert_eq!(
            snap.node_for(rows[0]).expect("row 0").selected,
            Some(false),
            "读写同一个信号 ⇒ 两行都要随依赖失效重发"
        );
        assert_eq!(snap.node_for(rows[1]).expect("row 1").checked, Some(true));
        app.assert_a11y_oracle();
    }

    fn editable_a11y_spec() -> EditableSpec {
        EditableSpec {
            role: A11yRole::TextInput,
            label: smol_str::SmolStr::new_static("搜索"),
            placeholder: smol_str::SmolStr::new_static(""),
            multiline: false,
            masked: false,
            readonly: false,
            disabled: false,
            max_length: None,
            submit: None,
            text_style: TextStyle::new(px(16.0)),
            caret_color: Color::BLACK,
            selection_color: Color::BLACK,
            placeholder_color: Color::BLACK,
            composition_color: Color::BLACK,
        }
    }

    #[test]
    fn a11y_force_full_keeps_a_focused_editable_text_run_attached() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        for _ in 0..2100 {
            NodeBuilder::boxed()
                .build(
                    app.app_mut().window_mut(window).expect("window").tree_mut(),
                    root,
                )
                .expect("box");
        }
        let editable = NodeBuilder::editable_text(editable_a11y_spec())
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                root,
            )
            .expect("editable input");
        assert!(
            app.app_mut().set_focus(window, Some(editable)),
            "可编辑输入必须可聚焦"
        );
        app.app_mut()
            .window_mut(window)
            .expect("window")
            .a11y_force_full();
        let _ = app.drain().expect("frame");
        assert!(
            app.a11y_contract_error().is_none(),
            "强制全量交付必须合法：{:?}",
            app.a11y_contract_error()
        );
        let snapshot = snapshot(&app);
        assert!(snapshot.full, "强制全量后的首笔必须是全树");
        let record = &snapshot.focus_node;
        assert_eq!(
            record.engine, editable,
            "2100 个兄弟后的焦点输入必须走 focus 回退"
        );
        let run = snapshot
            .focus_text_run
            .as_ref()
            .expect("焦点回退必须附带虚拟 TextRun");
        assert_eq!(record.text_run, Some(run.id));
        assert!(
            record.children.contains(&run.id),
            "焦点回退的 children 必须保留本节点自己的 TextRun：{:?}",
            record.children
        );
        let tree_update = app.a11y_update().expect("TreeUpdate 必须被捕获");
        assert!(
            tree_update
                .nodes
                .iter()
                .any(|(id, _)| *id == accesskit::NodeId(run.id.get())),
            "TextRun 必须与输入同笔交付"
        );
    }
}

#[cfg(test)]
mod p11_fix_tests {
    use smol_str::SmolStr;

    use super::TestApp;
    use crate::{
        a11y::{A11yOverride, A11yPatch, A11yRole},
        event::Action,
        interaction::ScrollConfig,
        layout::LayoutOverride,
        math::{DevicePixelRatio, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, dirty::DirtyFlags, id::NodeId},
        paint::{
            color::Color,
            style::{Clip, PaintSpec, Style},
            style_builder::StyleBuilder,
        },
        text::{TextAlign, TextDirection, TextStyle},
        window::FrameOutcome,
    };

    fn white() -> PaintSpec {
        PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        }
    }

    fn percent_style() -> Style {
        StyleBuilder::new().size_full().build()
    }

    fn mount(
        app: &mut TestApp,
        build: impl FnOnce(&mut crate::node::tree::NodeTree, NodeId) -> NodeId,
    ) -> NodeId {
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        build(tree, root)
    }

    #[test]
    fn a11y_bounds_follow_window_resize() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let child = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .style(percent_style())
                .paint(white())
                .id("child")
                .build(tree, root)
                .expect("child")
        });
        let _ = app.drain().expect("first");
        let before = app.probe().bounds(child).expect("bounds");

        app.app_mut()
            .window_mut(window)
            .expect("window")
            .set_size(LogicalSize::new(px(400.0), px(300.0)));
        let _ = app.drain().expect("second");

        let after = app.probe().bounds(child).expect("bounds");
        assert_ne!(before, after, "resize 必须真的改变子节点的布局盒");
        let snap = app.a11y().expect("resize 后必须有 a11y 交付");
        assert_eq!(
            snap.node_for(child)
                .expect("布局盒变了的子节点必须重发")
                .bounds,
            app.app()
                .window(window)
                .expect("window")
                .device_pixel_ratio()
                .to_covering_device_rect(after),
            "a11y bounds 必须覆盖整个逻辑矩形（floor/ceil 外扩）"
        );
    }

    #[test]
    fn a11y_bounds_follow_layout_override() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let node = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(20.0), px(20.0)))
                .paint(white())
                .id("box")
                .build(tree, root)
                .expect("box")
        });
        let _ = app.drain().expect("first");

        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .set_layout_override(
                node,
                Some(LayoutOverride::new(LogicalRect::new(
                    px(0.0),
                    px(0.0),
                    px(44.0),
                    px(24.0),
                ))),
            )
            .expect("override");
        let _ = app.drain().expect("second");

        let logical = app.probe().bounds(node).expect("bounds");
        assert_eq!(
            logical.size,
            LogicalSize::new(px(44.0), px(24.0)),
            "覆盖必须生效"
        );
        let snap = app.a11y().expect("覆盖后必须有 a11y 交付");
        assert_eq!(
            snap.node_for(node).expect("必须重发").bounds,
            DevicePixelRatio::ONE.to_covering_device_rect(logical)
        );
    }

    #[test]
    fn a11y_bounds_follow_scroll() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let mut viewport_style = StyleBuilder::new()
            .relative()
            .size(px(200.0), px(100.0))
            .build();
        viewport_style.clip = Clip::Bounds;
        let container = mount(&mut app, move |tree, root| {
            NodeBuilder::boxed()
                .style(viewport_style)
                .id("viewport")
                .build(tree, root)
                .expect("container")
        });
        let row = mount(&mut app, |tree, _root| {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(100.0), px(40.0)))
                .paint(white())
                .id("row")
                .build(tree, container)
                .expect("row")
        });
        let handle = app
            .app_mut()
            .make_scroll(window, container, ScrollConfig::default())
            .expect("scroll");
        app.app_mut()
            .set_scroll_content(&handle, LogicalSize::new(px(200.0), px(400.0)))
            .expect("content");
        let _ = app.drain().expect("first");
        let before = app
            .a11y()
            .expect("first a11y")
            .node_for(row)
            .expect("row")
            .bounds;

        app.app_mut()
            .scroll_by(&handle, crate::math::LogicalPoint::new(px(0.0), px(60.0)))
            .expect("scroll");
        let _ = app.drain().expect("second");

        let logical = app.probe().bounds(row).expect("bounds");
        let snap = app.a11y().expect("滚动后必须有 a11y 交付");
        let after = snap.node_for(row).expect("行必须重发").bounds;
        assert_ne!(before, after, "滚动必须改变行的 a11y 位置");
        assert_eq!(
            after,
            DevicePixelRatio::ONE.to_covering_device_rect(crate::interaction::scroll::to_screen(
                app.app().window(window).expect("window").tree(),
                row,
                logical
            ))
        );
    }

    #[test]
    fn a11y_scroll_during_a_batched_first_tree_is_not_lost() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let mut viewport_style = StyleBuilder::new()
            .relative()
            .size(px(200.0), px(100.0))
            .build();
        viewport_style.clip = Clip::Bounds;
        let container = mount(&mut app, move |tree, root| {
            NodeBuilder::boxed()
                .style(viewport_style)
                .id("viewport")
                .build(tree, root)
                .expect("container")
        });
        let rows: Vec<NodeId> = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            (0..3000)
                .map(|index| {
                    NodeBuilder::boxed()
                        .layout_override(LogicalRect::new(
                            px(0.0),
                            px(index as f32),
                            px(50.0),
                            px(10.0),
                        ))
                        .id(smol_str::format_smolstr!("row-{index}"))
                        .build(tree, container)
                        .expect("row")
                })
                .collect()
        };
        let handle = app
            .app_mut()
            .make_scroll(window, container, ScrollConfig::default())
            .expect("scroll");
        app.app_mut()
            .set_scroll_content(&handle, LogicalSize::new(px(200.0), px(4000.0)))
            .expect("content");

        let _ = app.drain().expect("首帧");
        let first = app.a11y().expect("首笔 a11y").clone();
        assert!(first.full, "首笔必须是全树");
        let before = first.node_for(rows[0]).expect("行 0 必须已交付").bounds;

        app.app_mut()
            .scroll_to(&handle, crate::math::LogicalPoint::new(px(0.0), px(120.0)))
            .expect("scroll to");
        let _ = app.drain().expect("滚动帧");
        let snapshot = app.a11y().expect("滚动后必须有 a11y 交付");
        let after = snapshot
            .node_for(rows[0])
            .expect("已交付行必须在滚动后重发")
            .bounds;
        assert_ne!(
            before, after,
            "滚动后已交付行的 bounds 必须更新（修前它永久停在旧位置）"
        );
        app.assert_a11y_contract();
    }

    #[test]
    fn a11y_bind_a11y_write_is_idle_without_damage() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let disabled = app.app_mut().signal(false);
        let signal = disabled;
        let button = mount(&mut app, move |tree, root| {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(60.0), px(24.0)))
                .paint(white())
                .on_click(Action::custom("press"))
                .bind_a11y(move |cx| A11yPatch {
                    disabled: Some(*cx.read(&signal)),
                    ..A11yPatch::default()
                })
                .id("press")
                .build(tree, root)
                .expect("button")
        });
        let first = app.drain().expect("first");
        assert!(first.rendered, "首帧必须提交（有背景要画）");
        assert!(
            !app.a11y()
                .expect("first a11y")
                .node_for(button)
                .expect("button")
                .disabled
        );

        app.app_mut().write(&disabled, true);
        let record = app.drain().expect("second");
        assert!(
            !record.rendered,
            "只有 a11y 变化 ⇒ 帧末门禁必须退回 Idle（FrameOutcome::{:?}）",
            FrameOutcome::Idle
        );
        assert!(record.damage.is_empty(), "a11y 更新不产生 damage");
        let flags = app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(button)
            .expect("button")
            .dirty();
        assert!(
            !flags.intersects(DirtyFlags::LAYOUT | DirtyFlags::PAINT),
            "`bind_a11y` 不得标 LAYOUT/PAINT：{flags:?}"
        );
        assert!(
            app.a11y()
                .expect("a11y 必须交付")
                .node_for(button)
                .expect("button")
                .disabled,
            "补丁照常生效"
        );
    }

    #[test]
    fn a_consumer_epoch_reset_delivers_a_full_tree_without_advancing_the_clock() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let button = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(60.0), px(24.0)))
                .paint(white())
                .on_click(Action::custom("press"))
                .id("press")
                .build(tree, root)
                .expect("button")
        });
        let _ = app.drain_pump().expect("首帧");
        assert!(
            app.a11y_update().expect("首笔").tree.is_some(),
            "首笔必须全树"
        );
        app.assert_a11y_contract();

        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_a11y(button);
        assert!(
            !app.app().a11y_pending(window),
            "限流窗口内的普通脏位不得立即交付"
        );

        let epoch = app
            .app()
            .window(window)
            .expect("window")
            .a11y_consumer_epoch();
        epoch.bump();
        let _ = app.drain_pump().expect("重置后的泵轮");
        let update = app
            .a11y_update()
            .expect("消费端重置后必须交付（不推进时钟、不依赖脏位）")
            .clone();
        assert!(
            update.tree.is_some(),
            "重置后的第一笔必须携带 TreeInfo（否则 consumer panic / AT 空树）"
        );
        app.assert_a11y_contract();
        let mut fresh = crate::test_support::a11y::ShadowTree::new();
        fresh
            .apply(&update)
            .expect("AT 重新接入时全树笔必须能初始化一棵空树");
        assert!(fresh.len() >= 2, "全树笔必须覆盖根 + 按钮");
    }

    fn text_run_fixture(app: &mut TestApp, text: &'static str, style: TextStyle) -> NodeId {
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let input_style = StyleBuilder::new().size(px(240.0), px(24.0)).build();
        let input = NodeBuilder::boxed()
            .style(input_style)
            .a11y(A11yOverride {
                role: Some(A11yRole::TextInput),
                label: Some(SmolStr::new_static("搜索")),
                ..A11yOverride::default()
            })
            .id("input")
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                root,
            )
            .expect("input");
        NodeBuilder::text(text, style)
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(160.0), px(24.0)))
            .id("run")
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                input,
            )
            .expect("text run")
    }

    #[test]
    fn text_run_exposes_character_metrics_for_a_single_ltr_line() {
        for (text, expected_lengths, caret_byte, position_index) in [
            ("价🌍ab", vec![3_u8, 4, 1, 1], 3_usize, 1_usize),
            ("42150", vec![1_u8, 1, 1, 1, 1], 2, 2),
        ] {
            let mut app = TestApp::new();
            app.enable_a11y_capture();
            let window = app.window_id();
            let run = text_run_fixture(
                &mut app,
                text,
                TextStyle {
                    direction: TextDirection::Ltr,
                    ..TextStyle::new(px(20.0))
                },
            );
            let _ = app.drain().expect("frame");
            app.assert_a11y_contract();

            let snapshot = app.a11y().expect("a11y delivery");
            let record = snapshot.node_for(run).expect("text run record");
            assert_eq!(record.role, A11yRole::TextRun);
            assert_eq!(record.character_lengths, expected_lengths, "{text:?}");
            assert_eq!(
                record.character_positions.len(),
                expected_lengths.len(),
                "{text:?}：positions 必须与 character_lengths 等长"
            );
            assert_eq!(
                record.character_widths.len(),
                expected_lengths.len(),
                "{text:?}：widths 必须与 character_lengths 等长"
            );
            assert!(
                record
                    .character_positions
                    .windows(2)
                    .all(|pair| pair[0] <= pair[1]),
                "{text:?}：LTR 位置必须单调：{:?}",
                record.character_positions
            );
            assert!(
                record.character_widths.iter().all(|width| *width > 0.0),
                "{text:?}：每个 grapheme 的 advance 必须为正：{:?}",
                record.character_widths
            );

            let update = app.a11y_update().expect("TreeUpdate").clone();
            let ak = update
                .nodes
                .iter()
                .find(|(id, _)| *id == accesskit::NodeId(record.id.get()))
                .map(|(_, node)| node)
                .expect("accesskit node");
            assert_eq!(
                ak.text_direction(),
                Some(accesskit::TextDirection::LeftToRight),
                "{text:?}"
            );
            assert_eq!(
                ak.character_positions(),
                Some(record.character_positions.as_slice()),
                "{text:?}"
            );
            assert_eq!(
                ak.character_widths(),
                Some(record.character_widths.as_slice()),
                "{text:?}"
            );

            let caret = app
                .app()
                .window(window)
                .expect("window")
                .text_caret_rect(run, caret_byte)
                .expect("caret");
            let bounds = app.probe().bounds(run).expect("bounds");
            let scale = app
                .app()
                .window(window)
                .expect("window")
                .device_pixel_ratio()
                .factor();
            let expected = (caret.min().x - bounds.origin.x).value() * scale;
            let actual = record.character_positions[position_index];
            assert!(
                (actual - expected).abs() <= 1.0,
                "{text:?}：positions[{position_index}]={actual} 必须与真实 caret x={expected} 一致"
            );
        }
    }

    #[test]
    fn multi_line_text_run_omits_character_metrics() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let run = text_run_fixture(
            &mut app,
            "价🌍ab\n第二行",
            TextStyle {
                direction: TextDirection::Ltr,
                ..TextStyle::new(px(20.0))
            },
        );
        let _ = app.drain().expect("frame");
        app.assert_a11y_contract();
        let snapshot = app.a11y().expect("a11y delivery");
        let record = snapshot.node_for(run).expect("text run record");
        assert!(
            !record.character_lengths.is_empty(),
            "多行文本仍必须有 character_lengths（文本接口本身可用）"
        );
        assert!(
            record.character_positions.is_empty() && record.character_widths.is_empty(),
            "多行文本必须省略 character_positions/widths（否则 caret 会命中错行）"
        );
        let update = app.a11y_update().expect("TreeUpdate").clone();
        let ak = update
            .nodes
            .iter()
            .find(|(id, _)| *id == accesskit::NodeId(record.id.get()))
            .map(|(_, node)| node)
            .expect("accesskit node");
        assert!(
            ak.character_positions().is_none()
                && ak.character_widths().is_none()
                && ak.text_direction().is_none(),
            "省略时不得写 character_positions/widths/text_direction（AT 回退路径）"
        );
    }

    #[test]
    fn auto_direction_text_run_exposes_character_metrics_for_ltr_text() {
        for (text, caret_byte, position_index) in [("Hello", 2_usize, 2_usize), ("42150", 3, 3)] {
            let mut app = TestApp::new();
            app.enable_a11y_capture();
            let window = app.window_id();
            let run = text_run_fixture(&mut app, text, TextStyle::new(px(20.0)));
            let _ = app.drain().expect("frame");
            app.assert_a11y_contract();

            let snapshot = app.a11y().expect("a11y delivery");
            let record = snapshot.node_for(run).expect("text run record");
            assert_eq!(record.role, A11yRole::TextRun);
            assert_eq!(record.character_lengths, vec![1_u8, 1, 1, 1, 1], "{text:?}");
            assert_eq!(
                record.character_positions.len(),
                record.character_lengths.len(),
                "{text:?}：Auto + 纯 LTR 必须输出 character_positions"
            );
            assert_eq!(
                record.character_widths.len(),
                record.character_lengths.len(),
                "{text:?}：Auto + 纯 LTR 必须输出 character_widths"
            );
            assert!(
                record
                    .character_positions
                    .windows(2)
                    .all(|pair| pair[0] <= pair[1]),
                "{text:?}：LTR 位置必须单调：{:?}",
                record.character_positions
            );
            assert!(
                record.character_widths.iter().all(|width| *width > 0.0),
                "{text:?}：每个 grapheme 的 advance 必须为正：{:?}",
                record.character_widths
            );

            let update = app.a11y_update().expect("TreeUpdate").clone();
            let ak = update
                .nodes
                .iter()
                .find(|(id, _)| *id == accesskit::NodeId(record.id.get()))
                .map(|(_, node)| node)
                .expect("accesskit node");
            assert_eq!(
                ak.text_direction(),
                Some(accesskit::TextDirection::LeftToRight),
                "{text:?}：解析后是 LTR ⇒ text_direction 必须是 LeftToRight"
            );
            assert_eq!(
                ak.character_positions(),
                Some(record.character_positions.as_slice()),
                "{text:?}"
            );

            let caret = app
                .app()
                .window(window)
                .expect("window")
                .text_caret_rect(run, caret_byte)
                .expect("caret");
            let bounds = app.probe().bounds(run).expect("bounds");
            let scale = app
                .app()
                .window(window)
                .expect("window")
                .device_pixel_ratio()
                .factor();
            let expected = (caret.min().x - bounds.origin.x).value() * scale;
            let actual = record.character_positions[position_index];
            assert!(
                (actual - expected).abs() <= 1.0,
                "{text:?}：positions[{position_index}]={actual} 必须与真实 caret x={expected} 一致"
            );
        }
    }

    #[test]
    fn auto_direction_cjk_text_run_exposes_character_metrics() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let run = text_run_fixture(&mut app, "价🌍ab", TextStyle::new(px(20.0)));
        let _ = app.drain().expect("frame");
        app.assert_a11y_contract();

        let snapshot = app.a11y().expect("a11y delivery");
        let record = snapshot.node_for(run).expect("text run record");
        assert_eq!(
            record.character_lengths,
            vec![3_u8, 4, 1, 1],
            "CJK/emoji 仍按 grapheme 计字节长度"
        );
        assert_eq!(
            record.character_positions.len(),
            4,
            "CJK 是 LTR 方向 ⇒ 必须输出 character_positions：{:?}",
            record.character_positions
        );
        assert_eq!(
            record.character_widths.len(),
            4,
            "CJK 是 LTR 方向 ⇒ 必须输出 character_widths：{:?}",
            record.character_widths
        );
        assert!(
            record
                .character_positions
                .windows(2)
                .all(|pair| pair[0] <= pair[1]),
            "LTR 位置必须单调：{:?}",
            record.character_positions
        );
        assert!(
            record.character_widths.iter().all(|width| *width > 0.0),
            "CJK/emoji 的 advance 必须为正：{:?}",
            record.character_widths
        );
        let caret = app
            .app()
            .window(window)
            .expect("window")
            .text_caret_rect(run, 3)
            .expect("caret");
        let bounds = app.probe().bounds(run).expect("bounds");
        let scale = app
            .app()
            .window(window)
            .expect("window")
            .device_pixel_ratio()
            .factor();
        let expected = (caret.min().x - bounds.origin.x).value() * scale;
        assert!(
            (record.character_positions[1] - expected).abs() <= 1.0,
            "positions[1]={} 必须与真实 caret x={expected} 一致",
            record.character_positions[1]
        );
    }

    #[test]
    fn auto_direction_text_run_omits_character_metrics_when_a_run_is_rtl() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let run = text_run_fixture(&mut app, "abc אבג", TextStyle::new(px(20.0)));
        let _ = app.drain().expect("frame");
        app.assert_a11y_contract();
        let snapshot = app.a11y().expect("a11y delivery");
        let record = snapshot.node_for(run).expect("text run record");
        assert!(
            !record.character_lengths.is_empty(),
            "文本接口本身仍可用（character_lengths 与方向无关）"
        );
        assert!(
            record.character_positions.is_empty() && record.character_widths.is_empty(),
            "解析后含 RTL run ⇒ 必须省略位置/宽度（不猜单轴、不发错轴的值）：{:?} / {:?}",
            record.character_positions,
            record.character_widths
        );
        let update = app.a11y_update().expect("TreeUpdate").clone();
        let ak = update
            .nodes
            .iter()
            .find(|(id, _)| *id == accesskit::NodeId(record.id.get()))
            .map(|(_, node)| node)
            .expect("accesskit node");
        assert!(
            ak.character_positions().is_none()
                && ak.character_widths().is_none()
                && ak.text_direction().is_none(),
            "省略时不得写 character_positions/widths/text_direction（AT 回退路径）"
        );
    }

    #[test]
    fn rtl_direction_text_run_omits_character_metrics() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let run = text_run_fixture(
            &mut app,
            "אבג",
            TextStyle {
                direction: TextDirection::Rtl,
                ..TextStyle::new(px(20.0))
            },
        );
        let _ = app.drain().expect("frame");
        app.assert_a11y_contract();
        let snapshot = app.a11y().expect("a11y delivery");
        let record = snapshot.node_for(run).expect("text run record");
        assert_eq!(
            record.character_lengths,
            vec![2_u8, 2, 2],
            "希伯来字母每个 2 字节，仍是文本接口的输入"
        );
        assert!(
            record.character_positions.is_empty() && record.character_widths.is_empty(),
            "显式 RTL 必须省略位置/宽度：{:?} / {:?}",
            record.character_positions,
            record.character_widths
        );
    }

    #[test]
    fn a_value_override_that_disagrees_with_the_shaped_text_omits_character_metrics() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let run = text_run_fixture(
            &mut app,
            "ab",
            TextStyle {
                direction: TextDirection::Ltr,
                ..TextStyle::new(px(20.0))
            },
        );
        let window = app.window_id();
        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .set_a11y_patch(
                run,
                A11yPatch {
                    value: Some(SmolStr::new_static("ABCDEFGH")),
                    ..A11yPatch::default()
                },
            );
        let _ = app.drain().expect("frame");
        app.assert_a11y_contract();

        let snapshot = app.a11y().expect("a11y delivery");
        let record = snapshot.node_for(run).expect("text run record");
        assert_eq!(
            record.value.as_deref(),
            Some("ABCDEFGH"),
            "value 覆盖本身仍然生效"
        );
        assert_eq!(
            record.character_lengths.len(),
            8,
            "character_lengths 仍描述 value 串（AccessKit 的 Σ == value.len() 约束成立）"
        );
        assert!(
            record.character_positions.is_empty() && record.character_widths.is_empty(),
            "value 与真实文本不一致 ⇒ 必须省略位置/宽度（否则 caret 表错位）：{:?} / {:?}",
            record.character_positions,
            record.character_widths
        );
        let update = app.a11y_update().expect("TreeUpdate").clone();
        let ak = update
            .nodes
            .iter()
            .find(|(id, _)| *id == accesskit::NodeId(record.id.get()))
            .map(|(_, node)| node)
            .expect("accesskit node");
        assert!(
            ak.character_positions().is_none()
                && ak.character_widths().is_none()
                && ak.text_direction().is_none(),
            "RE-02：口径不一致时不得写 character_positions/widths/text_direction"
        );
    }

    #[test]
    fn an_equal_length_value_override_that_differs_from_the_shaped_text_omits_character_metrics() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let run = text_run_fixture(
            &mut app,
            "ab",
            TextStyle {
                direction: TextDirection::Ltr,
                ..TextStyle::new(px(20.0))
            },
        );
        let window = app.window_id();
        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .set_a11y_patch(
                run,
                A11yPatch {
                    value: Some(SmolStr::new_static("xy")),
                    ..A11yPatch::default()
                },
            );
        let _ = app.drain().expect("frame");
        app.assert_a11y_contract();

        let snapshot = app.a11y().expect("a11y delivery");
        let record = snapshot.node_for(run).expect("text run record");
        assert_eq!(
            record.value.as_deref(),
            Some("xy"),
            "value 覆盖本身仍然生效"
        );
        assert_eq!(
            record.character_lengths,
            vec![1_u8, 1],
            "character_lengths 仍按 value 串划分（AccessKit 的 Σ == value.len() 约束不变）"
        );
        assert!(
            record.character_positions.is_empty() && record.character_widths.is_empty(),
            "等长但内容不同的 value 覆盖同样必须省略位置/宽度（旧守卫只比字节总长会漏掉）：{:?} / {:?}",
            record.character_positions,
            record.character_widths
        );
        let update = app.a11y_update().expect("TreeUpdate").clone();
        let ak = update
            .nodes
            .iter()
            .find(|(id, _)| *id == accesskit::NodeId(record.id.get()))
            .map(|(_, node)| node)
            .expect("accesskit node");
        assert!(
            ak.character_positions().is_none()
                && ak.character_widths().is_none()
                && ak.text_direction().is_none(),
            "RBA-01：等长口径不一致时不得写 character_positions/widths/text_direction"
        );
    }

    #[test]
    fn end_aligned_text_with_a_trailing_line_break_omits_character_metrics() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let run = text_run_fixture(
            &mut app,
            "abc\n",
            TextStyle {
                direction: TextDirection::Ltr,
                alignment: TextAlign::End,
                ..TextStyle::new(px(20.0))
            },
        );
        let _ = app.drain().expect("frame");
        app.assert_a11y_contract();
        let snapshot = app.a11y().expect("a11y delivery");
        let record = snapshot.node_for(run).expect("text run record");
        assert_eq!(
            record.character_lengths,
            vec![1_u8, 1, 1, 1],
            "文本接口本身仍可用（含换行符的 grapheme）"
        );
        assert!(
            record.character_positions.is_empty() && record.character_widths.is_empty(),
            "尾随硬换行 ⇒ 两行 ⇒ 必须省略位置/宽度（End 对齐下宽度守卫拦不住）：{:?} / {:?}",
            record.character_positions,
            record.character_widths
        );
    }

    #[test]
    fn auto_direction_pure_rtl_text_run_omits_character_metrics() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let run = text_run_fixture(&mut app, "אבג", TextStyle::new(px(20.0)));
        let _ = app.drain().expect("frame");
        app.assert_a11y_contract();
        let snapshot = app.a11y().expect("a11y delivery");
        let record = snapshot.node_for(run).expect("text run record");
        assert_eq!(
            record.character_lengths,
            vec![2_u8, 2, 2],
            "希伯来字母每个 2 字节，文本接口本身可用"
        );
        assert!(
            record.character_positions.is_empty() && record.character_widths.is_empty(),
            "Auto + 纯 RTL ⇒ 解析后不是 LTR ⇒ 必须省略位置/宽度：{:?} / {:?}",
            record.character_positions,
            record.character_widths
        );
    }

    fn rows_fixture(app: &mut TestApp, count: usize) -> Vec<NodeId> {
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        (0..count)
            .map(|index| {
                NodeBuilder::boxed()
                    .layout_override(LogicalRect::new(
                        px(0.0),
                        px(index as f32),
                        px(10.0),
                        px(1.0),
                    ))
                    .build(tree, root)
                    .expect("row")
            })
            .collect()
    }

    #[test]
    fn a_large_first_tree_is_not_throttled() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let rows = rows_fixture(&mut app, 5000);
        let _ = app.drain_pump().expect("首帧");
        assert!(
            app.a11y_update().expect("首笔").tree.is_some(),
            "首笔必须全树"
        );
        app.assert_a11y_contract();

        let mut batches = 1usize;
        while app.app().a11y_pending(window) {
            let _ = app.drain_pump().expect("后续批次（不推进时钟）");
            let update = app.a11y_update().expect("有待发批次时每轮都必须交付");
            assert!(update.tree.is_none(), "只有首笔携带 TreeInfo");
            app.assert_a11y_contract();
            batches += 1;
            assert!(batches < 16, "分批必须收敛（实际 {batches} 笔）");
        }
        assert!(
            batches >= 3,
            "5000 节点必须分多批（实际 {batches} 笔；若只有 1 笔说明预算/语义被改动）"
        );

        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_a11y(rows[0]);
        assert!(
            !app.app().a11y_pending(window),
            "单点增量仍在 A11Y_THROTTLE 窗口内（省电目标不变）"
        );
        app.advance(std::time::Duration::from_millis(100));
        assert!(app.app().a11y_pending(window), "节流窗口到期后增量恢复交付");
        let _ = app.drain_pump().expect("到期补发");
        app.assert_a11y_contract();
    }

    #[test]
    fn a_dpi_change_redelivers_all_batches_without_stall() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let rows = rows_fixture(&mut app, 5000);
        let _ = app.drain_pump().expect("首帧");
        while app.app().a11y_pending(window) {
            let _ = app.drain_pump().expect("首树剩余批次");
            app.assert_a11y_contract();
        }

        let scale = DevicePixelRatio::new(2.0).expect("scale");
        app.app_mut()
            .window_mut(window)
            .expect("window")
            .set_device_pixel_ratio(scale);
        let _ = app.drain_pump().expect("DPI 首笔");
        assert!(
            app.a11y_update().expect("DPI 首笔").tree.is_some(),
            "DPI 变化后的第一笔必须是全树"
        );
        app.assert_a11y_contract();
        let first = app.a11y().expect("DPI 首笔 snapshot").clone();
        let record = first.node_for(rows[0]).expect("row 0 在首笔里");
        let logical = app.probe().bounds(rows[0]).expect("logical bounds");
        assert_eq!(
            record.bounds,
            scale.to_covering_device_rect(logical),
            "DPI 变化后首笔的 bounds 必须已乘新 scale（不是旧值）"
        );

        let mut batches = 1usize;
        while app.app().a11y_pending(window) {
            let _ = app.drain_pump().expect("DPI 剩余批次（不推进时钟）");
            app.assert_a11y_contract();
            batches += 1;
            assert!(batches < 20, "DPI 全树分批必须收敛（实际 {batches} 笔）");
        }
        assert!(batches >= 3, "5000 节点的 DPI 全树必须分多批重发");
    }

    #[test]
    fn a11y_backlog_signals_a_pump_wake_after_every_non_final_batch() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let rows = rows_fixture(&mut app, 5000);
        let _ = app.drain_pump().expect("首帧");
        app.assert_a11y_contract();
        assert!(
            app.app().a11y_backlog_pending(),
            "首笔之后仍有分批 ⇒ 平台必须收到 wake 信号"
        );
        let mut batches = 1usize;
        let mut wakes = 1usize;
        while app.app().a11y_pending(window) {
            let _ = app.drain_pump().expect("后续批次（不推进时钟）");
            app.assert_a11y_contract();
            batches += 1;
            if app.app().a11y_backlog_pending() {
                wakes += 1;
            }
            assert!(batches < 16, "分批必须收敛（实际 {batches} 笔）");
        }
        assert!(batches >= 3, "5000 节点必须分多批");
        assert_eq!(
            wakes,
            batches - 1,
            "除最后一笔外每笔交付后都必须请求 wake（实际 {wakes} 次 / {batches} 批）"
        );
        assert!(
            !app.app().a11y_backlog_pending(),
            "收敛后不得残留 wake 信号（否则泵会空转）"
        );
        app.app_mut()
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_a11y(rows[0]);
        assert!(
            !app.app().a11y_backlog_pending(),
            "单点数据增量不得让泵连续唤醒"
        );
    }

    #[test]
    fn the_backlog_wake_helper_wakes_only_after_a_real_delivery() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let _rows = rows_fixture(&mut app, 5000);
        let dispatcher = app.app().dispatcher();

        let mut batches = 0usize;
        let mut wakes = 0usize;
        loop {
            let delivered_round = app.app().a11y_pending(window);
            if delivered_round {
                let _ = app.drain_pump().expect("泵轮（本批交付）");
                app.assert_a11y_contract();
                batches += 1;
                assert!(batches < 16, "分批必须收敛（实际 {batches} 批）");
            }
            let _ = dispatcher.take_frame_wake();
            app.app_mut().wake_for_a11y_backlog(dispatcher.as_ref());
            let woke = dispatcher.take_frame_wake();
            if !delivered_round {
                assert!(!woke, "本轮没有交付 ⇒ 绝不能唤醒泵（RE-05：I3 空闲零唤醒）");
                break;
            }
            if app.app().a11y_backlog_pending() {
                assert!(
                    woke,
                    "交付后仍有未走完批次 ⇒ 必须唤醒下一轮（第 {batches} 批）"
                );
                wakes += 1;
            } else {
                assert!(!woke, "最后一笔交付后不得再唤醒泵（已收敛）");
            }
        }
        assert!(batches >= 3, "5000 节点必须分多批");
        assert_eq!(
            wakes,
            batches - 1,
            "除最后一笔外每笔交付后必须唤醒一次（实际 {wakes} 次 / {batches} 批）"
        );
    }

    #[test]
    fn a11y_backlog_never_wakes_the_pump_without_a_consumer() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let _rows = rows_fixture(&mut app, 5000);
        let dispatcher = app.app().dispatcher();
        let mut wakes = 0usize;
        for round in 0..10 {
            let _ = app.drain_pump().expect("泵轮（无消费者）");
            let _ = dispatcher.take_frame_wake();
            app.app_mut().wake_for_a11y_backlog(dispatcher.as_ref());
            if dispatcher.take_frame_wake() {
                wakes += 1;
            }
            assert!(
                !app.app().a11y_backlog_pending(),
                "无消费者时不存在「已开始的交付批次」（round {round}）"
            );
            assert!(
                app.app().a11y_pending(window),
                "待发全量重建仍应报告工作（被叫醒后立刻交付；但不参与自唤醒）"
            );
        }
        assert_eq!(
            wakes, 0,
            "无 AT/无捕获时 a11y 绝不能唤醒泵（RE-05：否则单核 100% 忙等）"
        );
    }
}

#[cfg(test)]
mod dispatcher_tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use super::TestApp;
    use crate::{
        math::{LogicalRect, px},
        node::builder::NodeBuilder,
        paint::{
            color::Color,
            style::{PaintSpec, StylePatch},
        },
    };

    #[test]
    fn a_task_posted_from_another_thread_runs_on_the_next_drain() {
        let mut app = TestApp::new();
        let handle = app.handle();
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        std::thread::spawn(move || {
            handle.send(move |_app| flag.store(true, Ordering::SeqCst));
        })
        .join()
        .expect("posting thread");

        assert!(
            !ran.load(Ordering::SeqCst),
            "回投任务不能在下一次 drain 之前执行"
        );
        let _ = app.drain().expect("frame");
        assert!(ran.load(Ordering::SeqCst), "drain 必须执行本轮回投的任务");
        assert_eq!(app.tasks_run(), 1, "任务只允许执行一次");
        let _ = app.drain().expect("next frame");
        assert_eq!(app.tasks_run(), 1, "同一条任务不得重复执行");
    }

    #[test]
    fn a_task_runs_after_the_frame_and_its_write_lands_on_the_next_frame() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let color = app.app_mut().signal(Color::from_rgba8([255, 0, 0, 255]));
        {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            let root = tree.root();
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(20.0), px(20.0)))
                .paint(PaintSpec {
                    background: Color::from_rgba8([0, 0, 0, 255]),
                    ..PaintSpec::default()
                })
                .bind_style(move |cx| StylePatch {
                    background: Some(*cx.read(&color)),
                    ..StylePatch::default()
                })
                .build(tree, root)
                .expect("bound box");
        }
        let first = app.drain().expect("首帧").clone();
        assert!(first.rendered, "首帧必须提交");
        let settled = app.frame_record().frame_index;

        let handle = app.handle();
        handle.send(move |app| {
            app.write(&color, Color::from_rgba8([0, 255, 0, 255]));
        });
        let same_round = app.drain().expect("任务轮").clone();
        assert_eq!(
            app.frame_record().frame_index,
            settled,
            "任务在帧之后执行：本轮的 FrameIndex 不得因这次写入推进"
        );
        assert!(
            !same_round.rendered,
            "任务写下的状态只能下一轮出帧（本轮帧已经跑完）"
        );
        assert_eq!(app.tasks_run(), 1);

        let next = app.drain().expect("下一轮").clone();
        assert!(
            next.rendered,
            "任务写下的状态必须在下一轮出帧（丢了任务泵 / 没有唤醒都会停在这里）"
        );
        assert_eq!(
            app.frame_record().frame_index.get(),
            settled.get() + 1,
            "任务写入只允许产生一帧"
        );
    }

    #[test]
    fn tasks_run_even_when_the_frame_is_idle() {
        let mut app = TestApp::new();
        let _ = app.drain().expect("首帧");
        let handle = app.handle();
        let ran = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ran);
        handle.send(move |_app| {
            counter.fetch_add(1, Ordering::SeqCst);
        });

        let record = app.drain().expect("空闲帧").clone();
        assert!(!record.rendered, "任务本身不产生像素 ⇒ 帧门禁必须 Idle");
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "空闲帧也必须执行回投任务（生产泵的 ⑤ 与帧门禁无关）"
        );
    }

    #[test]
    fn tasks_run_in_fifo_order_and_only_once() {
        let mut app = TestApp::new();
        let handle = app.handle();
        let order = Arc::new(Mutex::new(Vec::new()));
        for index in 0..3usize {
            let log = Arc::clone(&order);
            handle.send(move |_app| log.lock().expect("task log").push(index));
        }
        let _ = app.drain().expect("frame");
        assert_eq!(
            *order.lock().expect("task log"),
            vec![0, 1, 2],
            "回投任务必须按 push 顺序执行"
        );
        assert_eq!(app.tasks_run(), 3);
        let _ = app.drain().expect("next frame");
        assert_eq!(app.tasks_run(), 3, "队列取空后不得重复执行");
    }
}
