//! 旋转加载指示器（旧 `crate::spinner` 的引擎化重建，P2-4a）。
//!
//! 旧实现是一个 `svg()` 圆环字形 + `with_animation(..).repeat()` 的旋转变换。新引擎没有「节点变换」
//! 通道（E7 只有图片填充与调制，`GeometryDraw` 才是唯一带 `Transform` 的绘制），所以本模块用
//! **自绘画布**（`NodeKind::Canvas`）画一圈渐隐的点，每个 `sync` 帧按当前角度重画：
//!
//! - 角度是 `Transient<f32>`，由 `App::animate(AnimatedValue::Scalar, ..)` 驱动（§3.7：
//!   高频状态不进依赖图）；一圈 900 ms（`animation_tick × 6`），一次注册覆盖约 2.5 小时 /
//!   10 000 圈，剩余不足一圈时 `sync` 自动续注册；
//! - `sync` 读角度 → `Window::set_canvas` 重画（`Canvas` 图元用**窗口绝对坐标**，几何从
//!   `Probe::bounds` 取节点当前布局盒）；画布内容（布局盒 + 角度）与上次相同时短路返回 `false`，
//!   静止 / 稳态不标 `PAINT`、不请求帧（I3）；
//! - 颜色是**显式参数**：引擎没有文本颜色继承链（评估 §7.2 第 10 条），调用方必须给
//!   `with_color`（旧实现取「周围文字色」，新实现由调用方传入同一角色色）。
//!
//! # a11y
//!
//! 有 `with_label` 时声明 `ProgressIndicator` + label（不确定性进度条的可访问名，屏幕阅读器用它
//! 播报「正在加载」）；没有 label 时与图标一样显式 `hidden`（装饰性图形，不产生 `MissingAltText`
//! 诊断）。
//!
//! # 生命周期
//!
//! 角度瞬态值归组件自有 `SignalScope`（[`Spinner::release`] 回收）；动画槽由引擎在释放时同批取消。

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
    },
    window::WindowId,
};

use crate::{
    style_builder::StyleBuilder,
    theme::{components::ComponentTokens, handle::ThemeHandle},
};

/// 一圈的时长 = 主题动画节拍 × 6（旧 `ANIMATION_TICK * 6` = 150 ms × 6 = 900 ms）。
const ROTATION_TICKS: u32 = 6;

/// 点数量（旧 Phosphor `circle-notch` 的缺口用一圈点近似）。
const DOTS: usize = 8;

/// 一次注册覆盖的圈数（≈ 2.5 小时；剩余不足一圈时 `sync` 续注册）。
const ROTATIONS_PER_ANIMATION: f32 = 10_000.0;

/// 点的最小直径（逻辑像素）。
const MIN_DOT: f32 = 1.5;

/// 旋转加载指示器。
#[derive(Clone)]
pub struct Spinner {
    inner: Rc<SpinnerInner>,
    size: f32,
    color: Color,
    label: Option<SmolStr>,
    spinning: bool,
}

struct SpinnerInner {
    key: SmolStr,
    duration: std::time::Duration,
    scope: SignalScope,
    /// 累计角度（弧度；画布取 `angle.rem_euclid(TAU)`）。
    angle: Transient<f32>,
    /// 当前动画的目标角度（续注册判据）。
    target: Cell<f32>,
    /// 上一次真正写进画布的内容 `(节点当前布局盒, 角度)`：稳态（静止 / 角度未变 / 盒未变）
    /// 短路返回 `false`，不重复标 `PAINT`（I3）。
    last_paint: Cell<Option<(Rect, f32)>>,
    geometry: std::cell::RefCell<Geometry>,
}

#[derive(Default)]
struct Geometry {
    window: Option<WindowId>,
    node: Option<NodeId>,
}

impl Spinner {
    /// 建指示器。`key` 是调试 id 前缀；一圈的时长取主题 `animation_tick × 6`（= 900 ms）。
    pub fn new(app: &mut App, theme: ThemeHandle, key: impl Into<SmolStr>) -> Self {
        let scope = app.signal_scope();
        let tick = theme.tokens().animation_tick;
        Self {
            inner: Rc::new(SpinnerInner {
                key: key.into(),
                duration: tick * ROTATION_TICKS,
                scope,
                angle: app.transient_scoped(scope, 0.0f32),
                target: Cell::new(0.0),
                last_paint: Cell::new(None),
                geometry: std::cell::RefCell::new(Geometry::default()),
            }),
            size: ComponentTokens::standard().medium.icon_size,
            color: Color::WHITE,
            label: None,
            spinning: true,
        }
    }

    /// 逻辑边长（正方形，默认 `medium.icon_size` = 12 px；调用方通常给 `size.icon_size()`）。
    #[must_use]
    pub fn with_size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    /// 点的颜色（引擎没有颜色继承，必须显式给）。
    #[must_use]
    pub fn with_color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    /// 可访问名（给了才以 `ProgressIndicator` 进 a11y；否则是装饰性图形）。
    #[must_use]
    pub fn with_label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// 是否旋转（`false` = 静止的加载图形，不出帧）。
    #[must_use]
    pub fn spinning(mut self, spinning: bool) -> Self {
        self.spinning = spinning;
        self
    }

    /// 挂载画布节点（首帧前画布为空，`sync` 在布局完成后重画）。
    ///
    /// # Errors
    /// `UnknownWindow` / `StaleNode`。
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

    /// 挂载节点句柄（测试 / 组合控件用）。
    #[must_use]
    pub fn node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().node
    }

    /// 当前旋转角度（弧度；画布用 `rem_euclid(TAU)`）。
    #[must_use]
    pub fn angle(&self, app: &App) -> f32 {
        *app.read_transient(&self.inner.angle)
    }

    /// 是否在旋转（读配置，不读状态）。
    #[must_use]
    pub fn is_spinning(&self) -> bool {
        self.spinning
    }

    /// 帧驱动：把当前角度画成点环；剩余不足一圈时续注册动画。
    ///
    /// 返回是否重画了画布。稳态（静止 / 角度未变 / 布局盒未变）返回 `false`，不调 `set_canvas`
    /// （I3：不请求帧、不标 `PAINT`）。
    ///
    /// # Errors
    /// `StaleNode` / `UnknownWindow`。
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
        if self.inner.last_paint.get() == Some((bounds, angle)) {
            return Ok(false);
        }
        let canvas = spinner_canvas(bounds, angle, self.color);
        let changed = app
            .window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .set_canvas(node, canvas)?;
        if changed {
            self.inner.last_paint.set(Some((bounds, angle)));
        }
        Ok(changed)
    }

    /// 释放信号作用域（动态创建 / 销毁的指示器调用）。
    pub fn release(&self, app: &mut App) -> usize {
        app.release_scope(self.inner.scope)
    }

    /// 注册「从当前角度转 10 000 圈」的长动画（续注册的同一入口）。
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

/// 把一圈渐隐的点画进画布（窗口绝对坐标）。
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
    use crate::theme::{appearance::Appearance, handle::ThemeHandle};

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

    /// 画布：一圈 8 个点，布局盒是 24×24。
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

    /// 旋转：动画推进后角度增加，点的位置改变（重画到新角度）。
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

    /// 速度：900 ms 恰好转一圈（旧 `ANIMATION_TICK * 6`；变异：周期再乘 150 → 本测试红）。
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

    /// 静止：不注册动画；稳态 `sync` 短路返回 false，也不标 `PAINT`（I3）。
    #[test]
    fn a_still_spinner_does_not_request_frames() {
        let mut harness = harness(false);
        let window = harness.app.window_id();
        assert!(!harness.app.app().animations_running(window));
        // harness 已画过一帧；静止节点之后的 `sync` 不得再写画布（否则每帧都标 PAINT）。
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

    /// a11y：有 label 时是 `ProgressIndicator` + 可访问名；没有时是显式 hidden 的装饰图形。
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

    /// 停用：信号槽回收、幂等。
    #[test]
    fn releasing_the_spinner_reclaims_its_angle_slot() {
        let mut harness = harness(true);
        assert_eq!(harness.spinner.release(harness.app.app_mut()), 1);
        assert_eq!(harness.spinner.release(harness.app.app_mut()), 0);
    }
}
