use std::{cell::Cell, f32::consts::TAU, rc::Rc};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::App,
    error::EngineError,
    interaction::{AnimatedValue, AnimationSpec, Easing},
    math::{Point, Rect},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{SignalScope, Transient},
    },
    paint::{
        canvas::Canvas,
        color::Color,
        primitive::{Border, Quad, QuadPaint},
        style::StylePatch,
        style_builder::StyleBuilder,
    },
    window::WindowId,
};

use crate::theme::{components::ComponentTokens, handle::ThemeHandle, role::Role};

const ROTATION_TICKS: u32 = 6;

const DOTS: usize = 8;

const ROTATIONS_PER_ANIMATION: f32 = 10_000.0;

const MIN_DOT: f32 = 1.5;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SpinnerColor {
    Fixed(Color),
    Role(Role),
}

#[derive(Clone)]
pub struct Spinner {
    inner: Rc<SpinnerInner>,
    size: f32,
    label: Option<SmolStr>,
    spinning: bool,
}

struct SpinnerInner {
    key: SmolStr,
    duration: std::time::Duration,
    scope: SignalScope,
    theme: ThemeHandle,
    color: std::cell::Cell<SpinnerColor>,
    angle: Transient<f32>,
    target: Cell<f32>,
    last_paint: Cell<Option<(Rect, f32, Color)>>,
    geometry: std::cell::RefCell<Geometry>,
}

#[derive(Default)]
struct Geometry {
    window: Option<WindowId>,
    node: Option<NodeId>,
}

impl Spinner {
    pub fn new(app: &mut App, theme: ThemeHandle, key: impl Into<SmolStr>) -> Self {
        let scope = app.signal_scope();
        let tick = theme.tokens().animation_tick;
        Self {
            inner: Rc::new(SpinnerInner {
                key: key.into(),
                duration: tick * ROTATION_TICKS,
                scope,
                theme,
                color: std::cell::Cell::new(SpinnerColor::Fixed(Color::WHITE)),
                angle: app.transient_scoped(scope, 0.0f32),
                target: Cell::new(0.0),
                last_paint: Cell::new(None),
                geometry: std::cell::RefCell::new(Geometry::default()),
            }),
            size: ComponentTokens::standard().medium.icon_size,
            label: None,
            spinning: true,
        }
    }

    #[must_use]
    pub fn with_size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    #[must_use]
    pub fn with_color(self, color: Color) -> Self {
        self.inner.color.set(SpinnerColor::Fixed(color));
        self
    }

    #[must_use]
    pub fn with_role(self, role: Role) -> Self {
        self.inner.color.set(SpinnerColor::Role(role));
        self
    }

    #[must_use]
    pub fn with_label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }

    #[must_use]
    pub fn spinning(mut self, spinning: bool) -> Self {
        self.spinning = spinning;
        self
    }

    pub fn build(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let style = StyleBuilder::new().size(self.size, self.size).build();
        let builder = NodeBuilder::canvas(Canvas::new(Rect::default()))
            .id(format_smolstr!("{}:spinner", self.inner.key))
            .style(style)
            .tab_stop(false)
            .a11y(match &self.label {
                Some(label) => A11yOverride {
                    role: Some(A11yRole::ProgressIndicator),
                    label: Some(label.clone()),
                    ..A11yOverride::default()
                },
                None => A11yOverride {
                    hidden: true,
                    ..A11yOverride::default()
                },
            });
        let builder = match self.inner.color.get() {
            SpinnerColor::Role(_) => {
                let theme = self.inner.theme.clone();
                builder.bind_style(move |cx| {
                    let _ = theme.appearance(cx);
                    StylePatch::default()
                })
            }
            SpinnerColor::Fixed(_) => builder,
        };
        let node = {
            let tree = app
                .window_mut(window)
                .ok_or(EngineError::UnknownWindow { window })?
                .tree_mut();
            builder.build(tree, parent)?
        };
        {
            let mut geometry = self.inner.geometry.borrow_mut();
            geometry.window = Some(window);
            geometry.node = Some(node);
        }
        if self.spinning {
            self.register_rotation(app, window, node)?;
        }
        Ok(node)
    }

    #[must_use]
    pub fn node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().node
    }

    #[must_use]
    pub fn angle(&self, app: &App) -> f32 {
        *app.read_transient(&self.inner.angle)
    }

    #[must_use]
    pub fn color(&self, app: &App) -> Color {
        match self.inner.color.get() {
            SpinnerColor::Fixed(color) => color,
            SpinnerColor::Role(role) => {
                let appearance = self.inner.theme.current(app);
                self.inner.theme.theme_for(appearance).role(role)
            }
        }
    }

    pub fn set_color(&self, app: &mut App, color: Color) -> bool {
        let next = SpinnerColor::Fixed(color);
        if self.inner.color.get() == next {
            return false;
        }
        self.inner.color.set(next);
        self.inner.last_paint.set(None);
        let geometry = self.inner.geometry.borrow();
        if let (Some(window), Some(node)) = (geometry.window, geometry.node) {
            let _ = app.request_node_repaint(window, node);
        }
        true
    }

    #[must_use]
    pub fn is_spinning(&self) -> bool {
        self.spinning
    }

    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let (window, node) = {
            let geometry = self.inner.geometry.borrow();
            (geometry.window, geometry.node)
        };
        let (Some(window), Some(node)) = (window, node) else {
            return Ok(false);
        };
        let Some(bounds) = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree()
            .get(node)
            .ok()
            .map(|target| target.layout().bounds)
        else {
            return Ok(false);
        };
        if self.spinning {
            let angle = *app.read_transient(&self.inner.angle);
            let target = self.inner.target.get();
            if angle >= target - TAU {
                self.register_rotation(app, window, node)?;
            }
        }
        let angle = app.read_transient(&self.inner.angle).rem_euclid(TAU);
        let color = self.color(app);
        if self.inner.last_paint.get() == Some((bounds, angle, color)) {
            return Ok(false);
        }
        let canvas = spinner_canvas(bounds, angle, color);
        let changed = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .set_canvas(node, canvas)?;
        if changed {
            self.inner.last_paint.set(Some((bounds, angle, color)));
        }
        Ok(changed)
    }

    pub fn release(&self, app: &mut App) -> usize {
        app.release_scope(self.inner.scope)
    }

    fn register_rotation(
        &self,
        app: &mut App,
        window: WindowId,
        node: NodeId,
    ) -> Result<(), EngineError> {
        let start = *app.read_transient(&self.inner.angle);
        let target = start + TAU * ROTATIONS_PER_ANIMATION;
        self.inner.target.set(target);
        app.animate(
            window,
            AnimatedValue::Scalar {
                target: self.inner.angle,
                to: target,
            },
            &[node],
            AnimationSpec {
                duration: self.inner.duration * (ROTATIONS_PER_ANIMATION as u32),
                easing: Easing::Linear,
            },
        )?;
        Ok(())
    }
}

fn spinner_canvas(bounds: Rect, angle: f32, color: Color) -> Canvas {
    let mut canvas = Canvas::new(bounds);
    let center = Point::new(
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + bounds.size.height / 2.0,
    );
    let dot = (bounds.size.width.min(bounds.size.height) / 6.0).max(MIN_DOT);
    let radius = (bounds.size.width.min(bounds.size.height) / 2.0 - dot / 2.0).max(0.0);
    for index in 0..DOTS {
        let phase = angle + index as f32 * TAU / DOTS as f32;
        let alpha = 0.15 + 0.85 * (index as f32 / (DOTS - 1) as f32);
        let position = Point::new(
            center.x + phase.cos() * radius,
            center.y + phase.sin() * radius,
        );
        canvas.push_quad(Quad {
            bounds: Rect::new(position.x - dot / 2.0, position.y - dot / 2.0, dot, dot),
            corner_radius: [dot / 2.0; 4],
            paint: QuadPaint::Solid {
                fill: color.with_opacity(alpha),
                border: None::<Border>,
            },
        });
    }
    canvas
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ui_core::{
        a11y::A11yRole,
        math::Rect,
        node::{id::NodeId, tree::NodeKind},
        test_support::app::TestApp,
    };

    use super::Spinner;
    use crate::theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role};

    struct Harness {
        app: TestApp,
        spinner: Spinner,
        node: NodeId,
    }

    fn harness(spinning: bool) -> Harness {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let spinner = Spinner::new(app.app_mut(), theme, "spin")
            .with_size(24.0)
            .with_color(ui_core::paint::color::Color::WHITE)
            .spinning(spinning);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = spinner.build(app.app_mut(), window, root).expect("build");
        let _ = app.drain().expect("frame");
        let _ = spinner.sync(app.app_mut()).expect("sync");
        let _ = app.drain().expect("frame");
        Harness { app, spinner, node }
    }

    fn canvas_quads(harness: &Harness) -> Vec<Rect> {
        let window = harness.app.window_id();
        let tree = harness.app.app().window(window).expect("window").tree();
        match tree.get(harness.node).expect("node").kind() {
            NodeKind::Canvas(canvas) => canvas
                .primitives()
                .iter()
                .filter_map(|primitive| match primitive {
                    ui_core::paint::primitive::Primitive::Quad(quad) => Some(quad.bounds),
                    _ => None,
                })
                .collect(),
            other => panic!("spinner 必须是 Canvas 节点，实际 {other:?}"),
        }
    }

    fn canvas_dot_fills(harness: &Harness) -> Vec<ui_core::paint::color::Color> {
        let window = harness.app.window_id();
        let tree = harness.app.app().window(window).expect("window").tree();
        match tree.get(harness.node).expect("node").kind() {
            NodeKind::Canvas(canvas) => canvas
                .primitives()
                .iter()
                .filter_map(|primitive| match primitive {
                    ui_core::paint::primitive::Primitive::Quad(quad) => match &quad.paint {
                        ui_core::paint::primitive::QuadPaint::Solid { fill, .. } => Some(*fill),
                        _ => None,
                    },
                    _ => None,
                })
                .collect(),
            other => panic!("spinner 必须是 Canvas 节点，实际 {other:?}"),
        }
    }

    #[test]
    fn a_spinner_draws_a_ring_of_dots_inside_its_layout_box() {
        let harness = harness(true);
        let bounds = harness.app.probe().bounds(harness.node).expect("bounds");
        assert!((bounds.size.width - 24.0).abs() < 0.5);
        assert!((bounds.size.height - 24.0).abs() < 0.5);
        let quads = canvas_quads(&harness);
        assert_eq!(quads.len(), 8, "8 个点");
        for quad in quads {
            assert!(
                quad.origin.x >= bounds.origin.x - 1.0
                    && quad.origin.x <= bounds.origin.x + bounds.size.width,
                "点必须落在节点的画布范围内"
            );
        }
    }

    #[test]
    fn the_spinner_rotates_with_the_engine_animation_clock() {
        let mut harness = harness(true);
        let window = harness.app.window_id();
        assert!(
            harness.app.app().animations_running(window),
            "旋转必须注册动画"
        );
        let before = canvas_quads(&harness);
        let angle_before = harness.spinner.angle(harness.app.app());
        harness.app.advance(Duration::from_millis(300));
        let _ = harness.app.drain().expect("frame");
        let redrawn = harness.spinner.sync(harness.app.app_mut()).expect("sync");
        assert!(redrawn, "角度变化后必须重画画布");
        let after = canvas_quads(&harness);
        assert!(
            harness.spinner.angle(harness.app.app()) > angle_before,
            "角度必须单调增加"
        );
        assert_ne!(before[0], after[0], "点的位置随角度改变");
    }

    #[test]
    fn a_role_coloured_spinner_follows_the_appearance_and_set_color_updates_it() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let spinner = Spinner::new(app.app_mut(), theme.clone(), "themed")
            .with_size(24.0)
            .with_role(Role::BrandBase)
            .spinning(false);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = spinner.build(app.app_mut(), window, root).expect("build");
        let _ = app.drain().expect("frame");
        let _ = spinner.sync(app.app_mut()).expect("sync");
        let _ = app.drain().expect("frame");
        let mut harness = Harness { app, spinner, node };

        const FIRST_DOT_ALPHA: f32 = 0.15;
        let light = Theme::of(Appearance::Light).role(Role::BrandBase);
        assert_eq!(
            canvas_dot_fills(&harness)[0],
            light.with_opacity(FIRST_DOT_ALPHA),
            "浅色外观下取 brand.base"
        );

        assert!(theme.set_appearance(harness.app.app_mut(), Appearance::Dark));
        let _ = harness.app.drain().expect("frame");
        assert!(
            harness.spinner.sync(harness.app.app_mut()).expect("sync"),
            "外观切换后角色色必须重画画布"
        );
        let dark = Theme::of(Appearance::Dark).role(Role::BrandBase);
        assert_ne!(light, dark, "深浅两套调色板的 brand.base 不同");
        assert_eq!(
            canvas_dot_fills(&harness)[0],
            dark.with_opacity(FIRST_DOT_ALPHA),
            "深色外观下取深色 brand.base"
        );

        let white = ui_core::paint::color::Color::WHITE;
        assert!(
            harness.spinner.set_color(harness.app.app_mut(), white),
            "运行时改固定色返回 true"
        );
        let _ = harness.app.drain().expect("frame");
        assert!(harness.spinner.sync(harness.app.app_mut()).expect("sync"));
        assert_eq!(
            canvas_dot_fills(&harness)[0],
            white.with_opacity(FIRST_DOT_ALPHA),
            "set_color 之后画固定色（角色跟随关闭）"
        );
        assert!(
            !harness.spinner.set_color(harness.app.app_mut(), white),
            "等值 set_color 是 no-op（不标脏、不出帧）"
        );
    }

    #[test]
    fn the_spinner_turns_once_in_nine_hundred_milliseconds() {
        use std::f32::consts::TAU;

        let mut harness = harness(true);
        let before = harness.spinner.angle(harness.app.app());
        harness.app.advance(Duration::from_millis(900));
        let _ = harness.app.drain().expect("frame");
        let _ = harness.spinner.sync(harness.app.app_mut()).expect("sync");
        let turns = (harness.spinner.angle(harness.app.app()) - before) / TAU;
        assert!(
            (turns - 1.0).abs() < 0.05,
            "900 ms 应转约 1 圈（实际 {turns:.3} 圈）"
        );
    }

    #[test]
    fn a_still_spinner_does_not_request_frames() {
        let mut harness = harness(false);
        let window = harness.app.window_id();
        assert!(!harness.app.app().animations_running(window));
        assert!(
            !harness
                .spinner
                .sync(harness.app.app_mut())
                .expect("still sync"),
            "静止 spinner 的稳态 sync 必须返回 false"
        );
        let dirty = harness
            .app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(harness.node)
            .expect("node")
            .dirty();
        assert!(
            !dirty.contains(ui_core::node::dirty::DirtyFlags::PAINT),
            "稳态 spinner 不得标 PAINT（实际 {dirty:?}）"
        );
        let record = harness.app.drain().expect("idle frame").clone();
        assert!(!record.rendered, "静止指示器不得出帧");
    }

    #[test]
    fn the_spinner_declares_an_indeterminate_progress_indicator() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let labelled = Spinner::new(app.app_mut(), theme.clone(), "labelled")
            .with_label("Loading")
            .spinning(false);
        let decorative = Spinner::new(app.app_mut(), theme, "decorative").spinning(false);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let labelled_node = labelled
            .build(app.app_mut(), window, root)
            .expect("labelled");
        let decorative_node = decorative
            .build(app.app_mut(), window, root)
            .expect("decorative");
        let _ = app.drain().expect("frame");
        let snapshot = app.a11y().expect("a11y");
        let record = snapshot.node_for(labelled_node).expect("a11y node");
        assert_eq!(record.role, A11yRole::ProgressIndicator);
        assert_eq!(record.label_text(), Some("Loading"));
        assert!(!record.hidden);
        let decorative_record = snapshot.node_for(decorative_node).expect("a11y node");
        assert!(decorative_record.hidden, "无 label 的旋转器是装饰图形");
    }

    #[test]
    fn releasing_the_spinner_reclaims_its_angle_slot() {
        let mut harness = harness(true);
        assert_eq!(harness.spinner.release(harness.app.app_mut()), 1);
        assert_eq!(harness.spinner.release(harness.app.app_mut()), 0);
    }
}
