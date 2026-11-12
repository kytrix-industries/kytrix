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

    /// 像素快照：Windows 上用 WARP 真渲染当前 `Scene`，其余平台恒 `NoRenderer`。
    ///
    /// # Errors
    /// [`SnapshotError::NoRenderer`]（本构建没有渲染器）或 [`SnapshotError::Gpu`]
    /// （设备 / 管线 / 渲染 / 读回失败）。
    pub fn snapshot(&self, case: SnapshotCase) -> Result<SnapshotImage, SnapshotError> {
        debug_assert!(case.scale.factor() > 0.0, "snapshot scale must be positive");
        #[cfg(windows)]
        {
            let window = self.app.window(self.window).ok_or(SnapshotError::Gpu {
                detail: smol_str::SmolStr::new_static("snapshot: window was closed"),
            })?;
            let surface = crate::math::Rect::new(0.0, 0.0, case.size.width, case.size.height)
                .to_physical_covering(case.scale);
            let size = (
                surface.width().max(1) as u32,
                surface.height().max(1) as u32,
            );
            // 快照的渲染器是临时建的：图集页尺寸必须与 App 的分配器一致，否则字形页传不进去。
            let ((page_width, _page_height), pixels) = self.app.glyph_page_pixels();
            let mut renderer =
                crate::render::backend::Renderer::new_warp_offscreen_with_page(size, page_width)
                    .map_err(gpu_error)?;
            let mut rows = crate::render::DirtyRows::default();
            rows.mark(0, u32::MAX);
            renderer.queue_atlas_upload(crate::paint::atlas::AtlasKind::Glyph, &pixels, rows);
            let image = renderer
                .render_snapshot(window.instance_buffer())
                .map_err(gpu_error)?;
            Ok(SnapshotImage {
                width: image.width,
                height: image.height,
                rgba8: image.rgba8,
            })
        }
        #[cfg(not(windows))]
        {
            let _ = case;
            Err(SnapshotError::NoRenderer)
        }
    }

    /// 单窗口借用视图（多窗口驱动的入口）。
    pub fn window(&mut self) -> TestWindow<'_> {
        TestWindow {
            app: &mut self.app,
            id: self.window,
        }
    }
}

#[cfg(windows)]
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

    #[test]
    fn snapshot_without_gpu_is_a_typed_result() {
        let app = TestApp::new();
        let result = app.snapshot(SnapshotCase {
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
        #[cfg(not(windows))]
        assert_eq!(result, Err(SnapshotError::NoRenderer));
    }

    /// **文本像素级断言**（V-64 的回归面）：黑底白字，离屏读回后必须真有亮像素，
    /// 且覆盖率既不是 0 也不是整屏。只看 `GlyphRun` 实例/UV 字段抓不到上传或采样的错位。
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
