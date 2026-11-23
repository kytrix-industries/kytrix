use std::{cell::Cell, rc::Rc};

use smol_str::{SmolStr, format_smolstr};
use taffy::style::Dimension;
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
    },
    paint::style::{Style, StylePatch},
    window::WindowId,
};

use crate::{
    style_builder::{PaintBuilder, StyleBuilder},
    theme::handle::ThemeHandle,
};

const TRACK_HEIGHT: f32 = 8.0;

#[derive(Clone)]
pub struct Progress {
    inner: Rc<ProgressInner>,
    label: Option<SmolStr>,
    initial: f32,
}

struct ProgressInner {
    key: SmolStr,
    theme: ThemeHandle,
    scope: SignalScope,
    value: Signal<f32>,
    applied: Cell<f32>,
    geometry: std::cell::RefCell<Geometry>,
}

#[derive(Default)]
struct Geometry {
    track: Option<NodeId>,
    indicator: Option<NodeId>,
}

impl Progress {
    pub fn new(app: &mut App, theme: ThemeHandle, key: impl Into<SmolStr>) -> Self {
        let scope = app.signal_scope();
        Self {
            inner: Rc::new(ProgressInner {
                key: key.into(),
                theme,
                scope,
                value: app.signal_scoped(scope, 0.0f32),
                applied: Cell::new(0.0),
                geometry: std::cell::RefCell::new(Geometry::default()),
            }),
            label: None,
            initial: 0.0,
        }
    }

    #[must_use]
    pub fn value(mut self, value: f32) -> Self {
        self.initial = clamp_percent(value);
        self
    }

    #[must_use]
    pub fn label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn build(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let initial = clamp_percent(self.initial);
        app.write(&self.inner.value, initial);
        let theme_track = self.inner.theme.clone();
        let value = self.inner.value;
        let label = self.label.clone();
        let track = NodeBuilder::boxed()
            .id(format_smolstr!("{}:track", self.inner.key))
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .width_full()
                    .height(TRACK_HEIGHT)
                    .build(),
            )
            .paint(PaintBuilder::new().rounded(2.0).build())
            .bind_style(move |cx| StylePatch {
                background: Some(theme_track.components(cx).progress.track),
                ..StylePatch::default()
            })
            .bind_a11y(move |cx| A11yPatch {
                label: label.clone(),
                numeric_value: Some(f64::from(*cx.read(&value))),
                min_numeric_value: Some(0.0),
                max_numeric_value: Some(100.0),
                ..A11yPatch::default()
            })
            .a11y(A11yOverride {
                role: Some(A11yRole::ProgressIndicator),
                label: self.label.clone(),
                ..A11yOverride::default()
            })
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                parent,
            )?;
        let theme_indicator = self.inner.theme.clone();
        let indicator = NodeBuilder::boxed()
            .id(format_smolstr!("{}:indicator", self.inner.key))
            .style(indicator_style(initial))
            .paint(PaintBuilder::new().rounded(2.0).build())
            .bind_style(move |cx| StylePatch {
                background: Some(theme_indicator.components(cx).progress.progress),
                ..StylePatch::default()
            })
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                track,
            )?;
        {
            let mut geometry = self.inner.geometry.borrow_mut();
            geometry.track = Some(track);
            geometry.indicator = Some(indicator);
        }
        self.inner.applied.set(initial);
        Ok(track)
    }

    #[must_use]
    pub fn track_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().track
    }

    #[must_use]
    pub fn indicator_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().indicator
    }

    #[must_use]
    pub fn current_value(&self, app: &App) -> f32 {
        app.read_with(&self.inner.value, |value| *value)
    }

    pub fn set_value(&self, app: &mut App, value: f32) -> bool {
        app.write(&self.inner.value, clamp_percent(value))
    }

    pub fn sync(&self, app: &mut App, window: WindowId) -> Result<bool, EngineError> {
        let Some(indicator) = self.inner.geometry.borrow().indicator else {
            return Ok(false);
        };
        let value = self.current_value(app);
        if (self.inner.applied.get() - value).abs() <= f32::EPSILON {
            return Ok(false);
        }
        self.inner.applied.set(value);
        app.window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut()
            .set_style(indicator, indicator_style(value))?;
        Ok(true)
    }

    pub fn release(&self, app: &mut App) -> usize {
        app.release_scope(self.inner.scope)
    }
}

fn indicator_style(value: f32) -> Style {
    let mut style = StyleBuilder::new().height_full().min_width_0().build();
    style.inner.size.width = Dimension::percent(clamp_percent(value) / 100.0);
    style
}

fn clamp_percent(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 100.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use ui_core::{a11y::A11yRole, node::id::NodeId, test_support::app::TestApp};

    use super::Progress;
    use crate::{
        style_builder::StyleBuilder,
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    struct Harness {
        app: TestApp,
        progress: Progress,
        track: NodeId,
        indicator: NodeId,
    }

    fn harness(value: f32) -> Harness {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let progress = Progress::new(app.app_mut(), theme, "progress").value(value);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let parent = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            ui_core::node::builder::NodeBuilder::boxed()
                .style(StyleBuilder::new().width(200.0).height(20.0).build())
                .build(tree, root)
                .expect("parent")
        };
        let track = progress
            .build(app.app_mut(), window, parent)
            .expect("build");
        let indicator = progress.indicator_node().expect("indicator");
        let _ = app.drain().expect("frame");
        let _ = progress.sync(app.app_mut(), window).expect("sync");
        let _ = app.drain().expect("frame");
        Harness {
            app,
            progress,
            track,
            indicator,
        }
    }

    #[test]
    fn the_initial_value_sets_the_indicator_width_on_the_first_frame() {
        let harness = harness(40.0);
        let track = harness.app.probe().bounds(harness.track).expect("track");
        let indicator = harness
            .app
            .probe()
            .bounds(harness.indicator)
            .expect("indicator");
        assert_eq!(track.size.height, 8.0);
        assert!((track.size.width - 200.0).abs() < 0.5, "轨道撑满父盒");
        assert!(
            (indicator.size.width - 80.0).abs() < 0.5,
            "40% 的 200 px 是 80 px（实际 {}）",
            indicator.size.width
        );
    }

    #[test]
    fn set_value_rewrites_the_indicator_and_then_settles() {
        let mut harness = harness(40.0);
        let window = harness.app.window_id();
        assert!(harness.progress.set_value(harness.app.app_mut(), 70.0));
        assert!(
            harness
                .progress
                .sync(harness.app.app_mut(), window)
                .expect("sync")
        );
        let _ = harness.app.drain().expect("frame");
        let indicator = harness
            .app
            .probe()
            .bounds(harness.indicator)
            .expect("indicator");
        assert!(
            (indicator.size.width - 140.0).abs() < 0.5,
            "70% 的 200 px 是 140 px（实际 {}）",
            indicator.size.width
        );
        assert!(
            !harness
                .progress
                .sync(harness.app.app_mut(), window)
                .expect("sync"),
            "值未变时 sync 必须返回 false"
        );
        let record = harness.app.drain().expect("idle").clone();
        assert!(!record.rendered, "稳态不得出帧（I3）");
    }

    #[test]
    fn values_are_clamped_to_the_percentage_range() {
        let mut harness = harness(0.0);
        harness.progress.set_value(harness.app.app_mut(), 300.0);
        assert_eq!(harness.progress.current_value(harness.app.app()), 100.0);
        harness.progress.set_value(harness.app.app_mut(), -5.0);
        assert_eq!(harness.progress.current_value(harness.app.app()), 0.0);
        harness.progress.set_value(harness.app.app_mut(), f32::NAN);
        assert_eq!(harness.progress.current_value(harness.app.app()), 0.0);
    }

    #[test]
    fn the_progress_bar_declares_its_numeric_value_to_a11y() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let progress = Progress::new(app.app_mut(), theme, "a11y")
            .label("Upload")
            .value(40.0);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = progress.build(app.app_mut(), window, root).expect("build");
        let _ = app.drain().expect("frame");
        let snapshot = app.a11y().expect("a11y");
        let record = snapshot.node_for(node).expect("a11y node");
        assert_eq!(record.role, A11yRole::ProgressIndicator);
        assert_eq!(record.label_text(), Some("Upload"));
        assert_eq!(record.numeric_value, Some(40.0));
        assert_eq!(record.min_numeric_value, Some(0.0));
        assert_eq!(record.max_numeric_value, Some(100.0));
    }

    #[test]
    fn the_track_and_indicator_use_the_theme_progress_recipe() {
        let harness = harness(50.0);
        let window = harness.app.window_id();
        let theme = crate::theme::resolved::Theme::of(Appearance::Light);
        for (node, expected) in [
            (harness.track, theme.components().progress.track),
            (harness.indicator, theme.components().progress.progress),
        ] {
            let patch = harness
                .app
                .app()
                .window(window)
                .expect("window")
                .tree()
                .get(node)
                .expect("node")
                .style_patch()
                .background;
            assert_eq!(
                patch.map(|color| ui_core::paint::color::ColorResolver
                    .resolve(color)
                    .components),
                Some(
                    ui_core::paint::color::ColorResolver
                        .resolve(expected)
                        .components
                )
            );
        }
    }
}
