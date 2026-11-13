//! 无 GPU 的测试引擎（I7 / 设计 §6.6）：`TestApp` / `TestWindow`。
//!
//! `TestApp::new()` 的 renderer 恒为 `None`：`Scene` 照常生成、记录照常抓取、present 是 no-op。
//! `drain()` 是**唯一**帧推进原语：一次最多一帧，不做「等到没事干」的隐式阻塞（设计 §6.2）。

use std::time::Duration;

use super::{
    probe::Probe,
    record::{FrameRecord, RenderedFrame},
    snapshot::{SnapshotCase, SnapshotError, SnapshotImage},
};
use crate::{
    app::App,
    error::EngineError,
    event::{InputEvent, InputOutcome},
    window::{FrameOutcome, WindowId, WindowOptions},
};

/// 无 GPU 的测试引擎：持有 runtime + `App` + 一个窗口。
pub struct TestApp {
    // `App::new` 需要 `Handle`；current_thread runtime 足够（P06 不 spawn）。
    // 字段只为保活（`App` 只持 `Handle`），不提供访问器。
    _runtime: tokio::runtime::Runtime,
    app: App,
    window: WindowId,
    /// 最近一次**提交**的绘制快照（空闲帧不会产生 `frame`，oracle 的增量侧要回退到这里）。
    last_rendered: Option<RenderedFrame>,
    /// 是否在每帧之后交付一笔 a11y 快照（`enable_a11y_capture` 打开）。
    a11y_capture: bool,
}

impl Default for TestApp {
    fn default() -> Self {
        Self::new()
    }
}

impl TestApp {
    /// 默认窗口（800×600、`Scale::ONE`）的 headless 引擎。
    #[must_use]
    pub fn new() -> Self {
        Self::with_options(WindowOptions::default())
    }

    /// 指定建窗参数的 headless 引擎。
    ///
    /// # Panics
    /// 当前线程的 tokio runtime 建不起来时 panic（测试环境的基础设施故障，不是引擎行为）。
    #[must_use]
    pub fn with_options(options: WindowOptions) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(options);
        Self {
            _runtime: runtime,
            app,
            window,
            last_rendered: None,
            a11y_capture: false,
        }
    }

    /// 当前窗口句柄。
    #[must_use]
    pub fn window_id(&self) -> WindowId {
        self.window
    }

    /// 只读 `App`（写信号 / 建树需要 `app_mut`）。
    #[must_use]
    pub fn app(&self) -> &App {
        &self.app
    }

    /// 可变 `App`（`NodeBuilder::build` / `App::write` / 字体栈）。
    #[must_use]
    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    /// 推进**一帧**（空闲帧也返回记录：`rendered == false`）。
    ///
    /// # Errors
    /// `EngineError::InvalidMeasure` / `TextShape` / `AtlasExhausted`：本帧已中止（未提交），
    /// damage/dirty 保留，下一次 `drain` 重试。
    pub fn drain(&mut self) -> Result<&FrameRecord, EngineError> {
        let outcome = self
            .app
            .frame(self.window)
            .expect("TestApp's window must exist");
        outcome?;
        // P11：a11y 交付在**帧之后**（与平台泵的 `update_a11y` 同序）。纯 `A11Y` 变化不提交帧，
        // 所以这一步与 `FrameOutcome` 无关（`FrameRecord.a11y` 在空闲帧也存在）。
        if self.a11y_capture
            && let Some(snapshot) = self.app.a11y_take(self.window)
        {
            self.app.attach_a11y(self.window, snapshot);
        }
        if let Some(frame) = self.frame_record().frame.clone() {
            self.last_rendered = Some(frame);
        }
        Ok(self.frame_record())
    }

    /// 最近一次 `drain` 的调度记录。
    #[must_use]
    pub fn frame_record(&self) -> &FrameRecord {
        self.app
            .window(self.window)
            .expect("TestApp's window must exist")
            .record()
            .expect("drain() must have produced a frame record")
    }

    /// 最近一次提交的绘制快照（空闲帧 → `None`）。
    #[must_use]
    pub fn rendered(&self) -> Option<&RenderedFrame> {
        self.frame_record().frame.as_ref()
    }

    /// 探针（节点树 + 场景只读）。
    #[must_use]
    pub fn probe(&self) -> Probe<'_> {
        let window = self
            .app
            .window(self.window)
            .expect("TestApp's window must exist");
        Probe::new(window.tree())
    }

    /// 打开 a11y 快照捕获：之后每次 `drain` 都把本帧的交付写进 `FrameRecord.a11y`。
    ///
    /// 首帧恒为全树（`A11yBridge.full_pending`），捕获开关只允许一个 drainer（生产里是平台
    /// adapter），见 `design/phase-11-a11y.md` S-112。
    pub fn enable_a11y_capture(&mut self) {
        self.a11y_capture = true;
        self.app.set_a11y_capture(self.window, true);
    }

    /// 最近一帧的 a11y 交付（`None` = 本帧没有 a11y 工作，或捕获未打开）。
    #[must_use]
    pub fn a11y(&self) -> Option<&crate::a11y::A11yTreeSnapshot> {
        self.frame_record().a11y.as_ref()
    }

    /// 注入一个输入事件：走**与平台泵相同**的 `App::dispatch_input`（命中 / 焦点 / 拖拽 / 动作 / 滚动）。
    pub fn inject(&mut self, event: InputEvent) -> InputOutcome {
        self.app.dispatch_input(self.window, event)
    }

    /// 推进测试时钟（双击窗口 / 动画 / 拖拽时间都读它；不改任何状态）。
    pub fn advance(&mut self, by: Duration) {
        self.app.advance_clock(by);
    }

    /// 强制整棵树的 lane 失效（oracle 的「全量」路径）。
    pub fn invalidate_all_lanes(&mut self) {
        if let Some(window) = self.app.window_mut(self.window) {
            window.force_full_repaint();
        }
    }

    /// 增量帧 vs 全量帧的差异（`None` = 一致；任何差异即 stale drawing）。
    pub fn oracle_diff(&mut self) -> Option<String> {
        self.drain().expect("incremental frame");
        // 增量侧可能整帧空闲（lane 全部复用）——那时用最近一次提交的快照。
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

    /// 差异即 panic（CI 必跑；设计 §3.8 的 full-refresh oracle）。
    ///
    /// # Panics
    /// 两条路径的 `(primitive, order)` 序列或解析后的 quad 不同。
    pub fn assert_oracle(&mut self) {
        if let Some(diff) = self.oracle_diff() {
            panic!("stale drawing: {diff}");
        }
    }

    /// 像素快照：Windows 上用 WARP 真渲染当前 `Scene`；macOS 用离屏 Metal；Linux 恒 `Gpu`
    /// （`new_offscreen` 需要真实 `wl_surface`，见 `design/phase-12-macos-linux.md` F-137）。
    ///
    /// 两张图集页都会上传：字形页恒上传，图片页只在 `App::resources` 非空时上传（P13 §8.3）。
    ///
    /// # Errors
    /// [`SnapshotError::NoRenderer`]（本构建没有渲染器）或 [`SnapshotError::Gpu`]
    /// （设备 / 管线 / 渲染 / 读回失败）。
    pub fn snapshot(&self, case: SnapshotCase) -> Result<SnapshotImage, SnapshotError> {
        debug_assert!(case.scale.factor() > 0.0, "snapshot scale must be positive");
        let window = self.app.window(self.window).ok_or(SnapshotError::Gpu {
            detail: smol_str::SmolStr::new_static("snapshot: window was closed"),
        })?;
        let surface = crate::math::Rect::new(0.0, 0.0, case.size.width, case.size.height)
            .to_physical_covering(case.scale);
        let size = (
            surface.width().max(1) as u32,
            surface.height().max(1) as u32,
        );
        // 快照的渲染器是临时建的：图集页尺寸必须与 App 的分配器一致，否则图集页传不进去。
        let ((page_width, page_height), glyph_pixels) = self.app.glyph_page_pixels();
        let mut renderer = Self::offscreen_renderer(size, page_width).map_err(gpu_error)?;
        let mut rows = crate::render::DirtyRows::default();
        rows.mark(0, u32::MAX);
        renderer.queue_atlas_upload(crate::paint::atlas::AtlasKind::Glyph, &glyph_pixels, rows);
        // 图片页：只有插过资源才上传（否则每个快照都白传 16 MiB）。
        if !self.app.resources().is_empty() {
            let ((image_width, image_height), image_pixels) = self.app.resource_page_pixels();
            debug_assert_eq!(
                (image_width, image_height),
                (page_width, page_height),
                "字形页与图片页必须同尺寸（渲染器只有一个 page 参数）"
            );
            let mut rows = crate::render::DirtyRows::default();
            rows.mark(0, u32::MAX);
            renderer.queue_atlas_upload(crate::paint::atlas::AtlasKind::Image, image_pixels, rows);
        }
        let image = renderer
            .render_snapshot(window.instance_buffer())
            .map_err(gpu_error)?;
        Ok(SnapshotImage {
            width: image.width,
            height: image.height,
            rgba8: image.rgba8,
        })
    }

    /// 离屏渲染器（Windows：WARP；macOS：真实 Metal；Linux：构造期返回 `UnsupportedGpu`）。
    #[cfg(windows)]
    fn offscreen_renderer(
        size: (u32, u32),
        page: u32,
    ) -> Result<crate::render::backend::Renderer, EngineError> {
        crate::render::backend::Renderer::new_warp_offscreen_with_page(size, page)
    }

    #[cfg(not(windows))]
    fn offscreen_renderer(
        size: (u32, u32),
        page: u32,
    ) -> Result<crate::render::backend::Renderer, EngineError> {
        crate::render::backend::Renderer::new_offscreen_with_page(size, page)
    }

    /// 单窗口借用视图（多窗口驱动的入口）。
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

/// 多窗口驱动：借用 `App` + 一个 `WindowId`。
pub struct TestWindow<'a> {
    app: &'a mut App,
    id: WindowId,
}

impl TestWindow<'_> {
    /// 该窗口推进一帧。
    ///
    /// # Errors
    /// 同 [`TestApp::drain`]。
    pub fn drain(&mut self) -> Result<FrameOutcome, EngineError> {
        match self.app.frame(self.id) {
            Some(result) => result,
            None => Ok(FrameOutcome::Idle),
        }
    }

    /// 最近一次帧记录。
    #[must_use]
    pub fn frame_record(&self) -> &FrameRecord {
        self.app
            .window(self.id)
            .expect("window must exist")
            .record()
            .expect("drain() must have produced a frame record")
    }

    /// 探针。
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
        math::{Point, Rect, Scale},
        node::builder::NodeBuilder,
        paint::{
            color::{Color, ColorResolver, Interpolation, LinearPremulRgba},
            style::{PaintSpec, StylePatch},
        },
        test_support::{
            probe::Selector,
            record::{LaneOutcome, PrimitiveKind},
            snapshot::SnapshotCase,
        },
        text::TextStyle,
    };

    const RECT: Rect = Rect::new(4.0, 6.0, 40.0, 16.0);

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
        let _label = NodeBuilder::text("price", TextStyle::new(12.0))
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
        let hit = probe.hit(Point::new(10.0, 10.0)).expect("命中");
        assert_eq!(hit.node, node);
        assert!(probe.hit(Point::new(-5.0, -5.0)).is_none());
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
        let expected: LinearPremulRgba =
            ColorResolver::new(Interpolation::default()).resolve(Color::WHITE);
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
    fn second_drain_reuses_lanes() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let color = app.app_mut().signal(Color::WHITE);
        {
            let _ = NodeBuilder::boxed()
                .layout_override(RECT)
                .bind_style(move |cx| StylePatch {
                    background: Some(cx.read(&color)),
                    ..StylePatch::default()
                })
                .id("a")
                .build(app.app_mut().window_mut(window).unwrap().tree_mut(), root)
                .expect("挂载");
        }
        let _ = mount_painted(&mut app, "b");
        let _ = app.drain().expect("首帧");

        app.app_mut().write(&color, Color::BLACK);
        let record = app.drain().expect("第二帧").clone();
        assert!(record.rendered);
        assert!(
            record
                .paint_log
                .lanes
                .iter()
                .any(|entry| entry.outcome == LaneOutcome::Reused),
            "未变化的节点必须复用 lane：{:?}",
            record.paint_log
        );
        assert!(
            record.dirty_nodes.len() < record.paint_log.lanes.len(),
            "只有被改动的节点计入 dirty"
        );
    }

    #[test]
    fn oracle_matches_full_refresh() {
        let mut app = TestApp::new();
        let _ = mount_painted(&mut app, "panel");
        assert!(app.oracle_diff().is_none());
    }

    /// 快照在各平台的**类型化**结果（S-124：mac 真读回、Linux 真读回、Windows WARP 真渲染）。
    #[test]
    fn snapshot_result_is_typed_per_platform() {
        let app = TestApp::new();
        let result = app.snapshot(SnapshotCase {
            scene: crate::test_support::snapshot::SnapshotScene::Shapes,
            theme: crate::test_support::snapshot::SnapshotTheme::Light,
            size: crate::math::Size::new(64.0, 64.0),
            scale: Scale::new(1.0).expect("scale"),
        });
        #[cfg(windows)]
        {
            // Windows：WARP 真渲染（空场景 ⇒ 全透明的 64×64）。
            let image = result.expect("WARP snapshot");
            assert_eq!((image.width, image.height), (64, 64));
            assert_eq!(image.rgba8.len(), 64 * 64 * 4);
            assert!(image.rgba8.iter().all(|byte| *byte == 0));
        }
        #[cfg(target_os = "macos")]
        {
            // macOS：真实 Metal 离屏渲染（F-137：`new_offscreen` 可用，`Readback` 真实读回）。
            let image = result.expect("Metal offscreen snapshot");
            assert_eq!((image.width, image.height), (64, 64));
            assert_eq!(image.rgba8.len(), 64 * 64 * 4);
        }
        #[cfg(target_os = "linux")]
        {
            // Linux：Vulkan 离屏渲染器（无 surface；F-150）。宿主没有 Vulkan 设备时是
            // `SnapshotError::Gpu`（构造期失败），有设备时必须真读回一张 64×64。
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

    /// **文本像素级断言**（V-64 的回归面）：黑底白字，离屏读回后必须真有亮像素，
    /// 且覆盖率既不是 0 也不是整屏。只看 `GlyphRun` 实例/UV 字段抓不到上传或采样的错位。
    ///
    /// 仅 Windows（WARP）：这是字形页上传/采样的回归面，宿主无关的断言在 `frame_record` 里。
    #[cfg(windows)]
    #[test]
    fn rendered_text_produces_visible_glyph_pixels() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let mut app = TestApp::with_options(crate::window::WindowOptions {
            title: "text pixels".into(),
            size: crate::math::Size::new(200.0, 60.0),
            scale: Scale::ONE,
        });
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let _backplate = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 200.0, 60.0))
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
                size: 24.0,
                color: Color::WHITE,
                ..TextStyle::default()
            },
        )
        .layout_override(Rect::new(10.0, 8.0, 180.0, 40.0))
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
                size: crate::math::Size::new(200.0, 60.0),
                scale: Scale::ONE,
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
            .set_size(crate::math::Size::new(400.0, 300.0));
        let record = app.drain().expect("resize 帧").clone();
        assert!(record.damage_full, "resize 必须退化为全帧");
        let expected =
            Rect::new(0.0, 0.0, 400.0, 300.0).to_physical_covering(Scale::new(1.0).expect("scale"));
        assert_eq!(record.damage, vec![expected]);
    }

    #[test]
    fn text_glyphs_use_border_and_padding_offset() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let mut inner = taffy::Style::<smol_str::SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(80.0),
            height: taffy::style::Dimension::length(40.0),
        };
        let pad = taffy::style::LengthPercentage::length(4.0);
        inner.padding = taffy::geometry::Rect {
            left: pad,
            right: pad,
            top: pad,
            bottom: pad,
        };
        let style = crate::paint::style::Style {
            inner,
            clip: crate::paint::style::Clip::None,
        };
        let node = NodeBuilder::text("MMM", TextStyle::new(12.0))
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
            Point::new(4.0, 4.0),
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
            glyph_bounds.origin.x >= 3.0,
            "字形绝对坐标必须加上 padding 偏移（实际 {:?}）",
            glyph_bounds
        );
    }
}

/// P10 的端到端用例（`TestApp::inject` 走与平台泵相同的分发路径）。
#[cfg(test)]
mod p10_tests {
    use super::TestApp;
    use crate::{
        event::{Action, InputEvent, Key, KeyState, Modifiers, PointerButton},
        math::{Point, Rect},
        node::builder::NodeBuilder,
        paint::style::{Clip, PaintSpec, Style, StylePatch},
    };

    fn app_with_button() -> (TestApp, crate::node::id::NodeId) {
        let mut app = TestApp::new();
        let window = app.window_id();
        let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        let node = NodeBuilder::boxed()
            .layout_override(Rect::new(10.0, 10.0, 60.0, 40.0))
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
        app.inject(InputEvent::PointerDown {
            position: Point::new(20.0, 20.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
        });
        let outcome = app.inject(InputEvent::PointerUp {
            position: Point::new(20.0, 20.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
        });

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
        let mut style = Style::default();
        style.inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(100.0),
            height: taffy::style::Dimension::length(100.0),
        };
        style.clip = Clip::Bounds;
        let container = NodeBuilder::boxed()
            .style(style)
            .build(tree, root)
            .expect("mount");
        for index in 0..4 {
            let _ = NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, index as f32 * 40.0, 100.0, 40.0))
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
            .set_scroll_content(&handle, crate::math::Size::new(100.0, 160.0))
            .expect("content");
        app.app_mut()
            .scroll_to(&handle, Point::new(0.0, 40.0))
            .expect("scroll");
        let record = app.drain().expect("scroll frame");

        // 视口 100×100（物理像素）+ AA 外扩 1 px（`DamageAccumulator::push` 的固定外扩）。
        let allowed = crate::math::PhysicalRect::new(-1, -1, 101, 101);
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
            app.inject(InputEvent::PointerDown {
                position: Point::new(20.0, 20.0),
                button: PointerButton::Primary,
                modifiers: Modifiers::empty(),
            });
            app.inject(InputEvent::PointerUp {
                position: Point::new(20.0, 20.0),
                button: PointerButton::Primary,
                modifiers: Modifiers::empty(),
            })
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

        assert_eq!(app.app().read_transient(&transient), 1.0);
        assert!(!app.app().animations_running(window));
        // 再推进一帧：没有脏位 ⇒ Idle（不提交、FrameIndex 不变）。
        let before = app.app().window(window).expect("window").frame_index();
        app.drain().expect("idle frame");
        let after = app.app().window(window).expect("window").frame_index();
        assert_eq!(before, after, "动画结束后必须回到空闲零帧");
    }

    #[test]
    fn tab_focus_is_visible_through_the_probe() {
        let (_app_unused, _node_unused) = app_with_button();
        let (mut app, node) = app_with_button();
        app.inject(InputEvent::Key {
            key: Key::Tab,
            state: KeyState::Down,
            modifiers: Modifiers::empty(),
        });
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
        let mut outer_style = Style::default();
        outer_style.inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(200.0),
            height: taffy::style::Dimension::length(200.0),
        };
        outer_style.clip = Clip::Bounds;
        let outer = NodeBuilder::boxed()
            .style(outer_style)
            .build(tree, root)
            .expect("outer");
        let mut inner_style = Style::default();
        inner_style.inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(200.0),
            height: taffy::style::Dimension::length(200.0),
        };
        inner_style.inner.margin = taffy::geometry::Rect {
            left: taffy::style::LengthPercentageAuto::length(0.0),
            right: taffy::style::LengthPercentageAuto::length(0.0),
            top: taffy::style::LengthPercentageAuto::length(100.0),
            bottom: taffy::style::LengthPercentageAuto::length(0.0),
        };
        inner_style.clip = Clip::Bounds;
        let inner = NodeBuilder::boxed()
            .style(inner_style)
            .build(tree, outer)
            .expect("inner");
        for index in 0..6 {
            let _ = NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, index as f32 * 40.0, 200.0, 40.0))
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
            .set_scroll_content(&outer_handle, crate::math::Size::new(200.0, 400.0))
            .expect("outer content");
        let inner_handle = app
            .app_mut()
            .make_scroll(window, inner, crate::interaction::ScrollConfig::default())
            .expect("inner scroll");
        app.app_mut()
            .set_scroll_content(&inner_handle, crate::math::Size::new(200.0, 240.0))
            .expect("inner content");
        // 外层先滚 30（内层屏幕 y = 100 − 30 = 70），把这一帧的 damage 消费掉。
        app.app_mut()
            .scroll_to(&outer_handle, Point::new(0.0, 30.0))
            .expect("outer scroll");
        let _ = app.drain().expect("outer frame");

        app.app_mut()
            .scroll_to(&inner_handle, Point::new(0.0, 10.0))
            .expect("inner scroll");
        let record = app.drain().expect("inner frame");

        // 内层视口在屏幕上 = (0, 70, 200, 70+200) + 1 px AA 外扩。
        let allowed = crate::math::PhysicalRect::new(-1, 69, 201, 271);
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
                .any(|rect| (69..=71).contains(&rect.y0)),
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
            .layout_override(Rect::new(10.0, 10.0, 40.0, 40.0))
            .paint(PaintSpec {
                background: crate::paint::color::Color::from_rgba8([9, 9, 9, 255]),
                ..PaintSpec::default()
            })
            .on_click(Action::custom("a"))
            .focus_ring(crate::paint::style::FocusRing::new(
                2.0,
                2.0,
                crate::paint::color::Color::from_rgba8([0, 120, 255, 255]),
            ))
            .build(tree, root)
            .expect("ringed");
        let other = NodeBuilder::boxed()
            .layout_override(Rect::new(100.0, 10.0, 40.0, 40.0))
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

        // 环外扩 = bounds.outset(offset 2 + width 2) ⇒ (6, 6, 48, 48) → 物理像素覆盖 (6,6)-(54,54)。
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

/// P11 的端到端用例：`TestApp` 在每帧之后交付一笔 a11y 快照（与平台泵同序）。
#[cfg(test)]
mod p11_tests {
    use smol_str::SmolStr;

    use super::TestApp;
    use crate::{
        a11y::{
            A11yAction, A11yDiagnostic, A11yNodeId, A11yOverride, A11yPatch, A11yRole,
            A11yTreeSnapshot,
        },
        event::Action,
        math::{Point, Rect, Scale},
        node::{builder::NodeBuilder, dirty::DirtyFlags, id::NodeId},
        paint::{
            canvas::Canvas,
            color::Color,
            style::{Clip, PaintSpec, Style, StylePatch},
        },
        text::TextStyle,
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

    #[test]
    fn a11y_roles_labels_and_values() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let root = {
            let window = app.window_id();
            app.app().window(window).expect("window").tree().root()
        };
        let label = mount(&mut app, |tree, root| {
            NodeBuilder::text("价格", TextStyle::new(12.0))
                .layout_override(Rect::new(0.0, 0.0, 40.0, 20.0))
                .id("price")
                .build(tree, root)
                .expect("text")
        });
        let button = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 24.0, 60.0, 24.0))
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
            NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 52.0, 60.0, 30.0)))
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
                .layout_override(Rect::new(0.0, 0.0, 60.0, 24.0))
                .paint(white())
                .on_click(Action::custom("press"))
                .bind_a11y(move |cx| A11yPatch {
                    label: Some(cx.read(&label_signal)),
                    ..A11yPatch::default()
                })
                .id("press")
                .build(tree, root)
                .expect("button")
        });
        let _ = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 30.0, 60.0, 24.0))
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
                .layout_override(Rect::new(0.0, 0.0, 60.0, 24.0))
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
        assert_eq!(snap.focus(), A11yNodeId::from_engine(button));
        assert_eq!(snap.focus_node().engine, button, "焦点节点必须在快照里");
    }

    #[test]
    fn a11y_structure_matches_the_tree() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let panel = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .id("panel")
                .build(tree, root)
                .expect("panel")
        });
        let _ = app.drain().expect("first");

        let child = mount(&mut app, |tree, _root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 20.0, 20.0))
                .id("child")
                .build(tree, panel)
                .expect("child")
        });
        let _ = app.drain().expect("second");
        let expected: Vec<A11yNodeId> = app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(panel)
            .expect("panel")
            .children()
            .iter()
            .map(|child| A11yNodeId::from_engine(*child))
            .collect();
        let snap = snapshot(&app);
        let actual = snap.children_of(A11yNodeId::from_engine(panel));
        assert_eq!(actual, expected, "父的 children 必须与节点树完全一致");
        assert!(snap.node_for(child).is_some(), "新子节点必须出现");
    }

    #[test]
    fn a11y_bounds_equal_probe_times_scale() {
        let scale = Scale::new(1.25).expect("scale");
        let mut app = TestApp::with_options(WindowOptions {
            title: SmolStr::new_static("scale"),
            size: crate::math::Size::new(400.0, 300.0),
            scale,
        });
        app.enable_a11y_capture();
        let node = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(8.0, 12.0, 40.0, 24.0))
                .paint(white())
                .id("box")
                .build(tree, root)
                .expect("box")
        });
        let _ = app.drain().expect("frame");
        let logical = app.probe().bounds(node).expect("bounds");
        let expected = logical.to_physical(scale);
        let snap = snapshot(&app);
        assert_eq!(
            snap.node_for(node).expect("box").bounds,
            expected,
            "a11y bounds 必须是 逻辑 × scale 逐边 round"
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
                .layout_override(Rect::new(0.0, 0.0, 60.0, 24.0))
                .paint(white())
                .on_click(Action::custom("press"))
                .bind_a11y(move |cx| A11yPatch {
                    disabled: Some(cx.read(&signal)),
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
            NodeBuilder::canvas(Canvas::new(Rect::new(0.0, 0.0, 60.0, 30.0)))
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
                .layout_override(Rect::new(0.0, 0.0, 20.0, 20.0))
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
        // 直接写 patch 不经过 `refresh_styles`，这里显式标脏（生产路径由 `refresh_styles` 负责）。
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
            snap.visible_nodes().all(|node| node.engine != hidden),
            "hidden 节点不得出现在可见集合里"
        );
    }

    #[test]
    fn a11y_scale_change_resends_the_full_tree() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let node = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 20.0, 20.0))
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
            .set_scale(Scale::new(1.5).expect("scale"));
        let _ = app.drain().expect("second");
        let snap = snapshot(&app);
        assert!(snap.full, "DPI 变化必须整棵重发（bounds 全是物理像素）");
        assert_eq!(
            snap.node_for(node).expect("box").bounds,
            Rect::new(0.0, 0.0, 20.0, 20.0).to_physical(Scale::new(1.5).expect("scale"))
        );
    }

    #[test]
    fn a11y_virtual_list_exposes_only_mounted_rows() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let mut inner = taffy::Style::<SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(100.0),
            height: taffy::style::Dimension::length(80.0),
        };
        inner.position = taffy::style::Position::Relative;
        let container = mount(&mut app, move |tree, root| {
            NodeBuilder::boxed()
                .style(Style {
                    inner,
                    clip: Clip::Bounds,
                })
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
                    extent: 20.0,
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
        let _ = Point::ZERO;
    }
}

#[cfg(test)]
mod p11_fix_tests {
    use smol_str::SmolStr;

    use super::TestApp;
    use crate::{
        a11y::A11yPatch,
        event::Action,
        interaction::ScrollConfig,
        layout::LayoutOverride,
        math::{Rect, Scale, Size},
        node::{builder::NodeBuilder, dirty::DirtyFlags, id::NodeId},
        paint::{
            color::Color,
            style::{Clip, PaintSpec, Style},
        },
        window::FrameOutcome,
    };

    fn white() -> PaintSpec {
        PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        }
    }

    fn percent_style() -> Style {
        let mut inner = taffy::Style::<SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::percent(1.0),
            height: taffy::style::Dimension::percent(1.0),
        };
        Style {
            inner,
            clip: Clip::None,
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

    /// 审查 #1：窗口 resize 后子孙的布局盒变了，增量笔必须把它们重发。
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
            .set_size(Size::new(400.0, 300.0));
        let _ = app.drain().expect("second");

        let after = app.probe().bounds(child).expect("bounds");
        assert_ne!(before, after, "resize 必须真的改变子节点的布局盒");
        let snap = app.a11y().expect("resize 后必须有 a11y 交付");
        assert_eq!(
            snap.node_for(child)
                .expect("布局盒变了的子节点必须重发")
                .bounds,
            after.to_physical(app.app().window(window).expect("window").scale()),
            "a11y bounds 必须等于 逻辑 × scale"
        );
    }

    /// 审查 #2：`set_layout_override` / `set_style` 等走 `invalidate_layout` 的路径必须标 `A11Y`。
    #[test]
    fn a11y_bounds_follow_layout_override() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let node = mount(&mut app, |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 20.0, 20.0))
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
                Some(LayoutOverride::new(Rect::new(0.0, 0.0, 44.0, 24.0))),
            )
            .expect("override");
        let _ = app.drain().expect("second");

        let logical = app.probe().bounds(node).expect("bounds");
        assert_eq!(logical.size, Size::new(44.0, 24.0), "覆盖必须生效");
        let snap = app.a11y().expect("覆盖后必须有 a11y 交付");
        assert_eq!(
            snap.node_for(node).expect("必须重发").bounds,
            logical.to_physical(Scale::ONE)
        );
    }

    /// 审查 #3：普通滚动（非 a11y action）也必须让容器的 a11y 子树失效。
    #[test]
    fn a11y_bounds_follow_scroll() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let mut inner = taffy::Style::<SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(200.0),
            height: taffy::style::Dimension::length(100.0),
        };
        inner.position = taffy::style::Position::Relative;
        let container = mount(&mut app, move |tree, root| {
            NodeBuilder::boxed()
                .style(Style {
                    inner,
                    clip: Clip::Bounds,
                })
                .id("viewport")
                .build(tree, root)
                .expect("container")
        });
        let row = mount(&mut app, |tree, _root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 100.0, 40.0))
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
            .set_scroll_content(&handle, Size::new(200.0, 400.0))
            .expect("content");
        let _ = app.drain().expect("first");
        let before = app
            .a11y()
            .expect("first a11y")
            .node_for(row)
            .expect("row")
            .bounds;

        app.app_mut()
            .scroll_by(&handle, crate::math::Point::new(0.0, 60.0))
            .expect("scroll");
        let _ = app.drain().expect("second");

        let logical = app.probe().bounds(row).expect("bounds");
        let snap = app.a11y().expect("滚动后必须有 a11y 交付");
        let after = snap.node_for(row).expect("行必须重发").bounds;
        assert_ne!(before, after, "滚动必须改变行的 a11y 位置");
        assert_eq!(
            after,
            crate::interaction::scroll::to_screen(
                app.app().window(window).expect("window").tree(),
                row,
                logical
            )
            .to_physical(Scale::ONE)
        );
    }

    /// 审查 #6：写 `bind_a11y` 的信号**不得**产生任何像素（Idle、无 damage、无 PAINT）。
    #[test]
    fn a11y_bind_a11y_write_is_idle_without_damage() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let disabled = app.app_mut().signal(false);
        let signal = disabled;
        let button = mount(&mut app, move |tree, root| {
            NodeBuilder::boxed()
                .layout_override(Rect::new(0.0, 0.0, 60.0, 24.0))
                .paint(white())
                .on_click(Action::custom("press"))
                .bind_a11y(move |cx| A11yPatch {
                    disabled: Some(cx.read(&signal)),
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
}
