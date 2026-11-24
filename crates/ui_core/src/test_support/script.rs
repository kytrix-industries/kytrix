use std::time::Duration;

use smol_str::SmolStr;

use crate::{
    app::App,
    error::EngineError,
    event::InputEvent,
    math::{DevicePixelRatio, LogicalSize},
    node::signal::Signal,
    test_support::{app::TestApp, record::FrameRecord},
    window::WindowId,
};

enum FrameAction {
    Mutate(Box<dyn FnOnce(&mut App)>),
    Input(InputEvent),
    Advance(Duration),
    Resize(LogicalSize),
    Scale(DevicePixelRatio),
    InvalidateLanes,
}

pub struct ScriptFrame {
    label: SmolStr,
    actions: Vec<FrameAction>,
}

impl ScriptFrame {
    #[must_use]
    pub fn new(label: impl Into<SmolStr>) -> Self {
        Self {
            label: label.into(),
            actions: Vec::new(),
        }
    }

    #[must_use]
    pub fn write<T: Clone + PartialEq + 'static>(mut self, signal: Signal<T>, value: T) -> Self {
        self.actions.push(FrameAction::Mutate(Box::new(move |app| {
            app.write(&signal, value);
        })));
        self
    }

    #[must_use]
    pub fn mutate(mut self, mutate: impl FnOnce(&mut App) + 'static) -> Self {
        self.actions.push(FrameAction::Mutate(Box::new(mutate)));
        self
    }

    #[must_use]
    pub fn input(mut self, event: InputEvent) -> Self {
        self.actions.push(FrameAction::Input(event));
        self
    }

    #[must_use]
    pub fn advance(mut self, by: Duration) -> Self {
        self.actions.push(FrameAction::Advance(by));
        self
    }

    #[must_use]
    pub fn resize(mut self, size: LogicalSize) -> Self {
        self.actions.push(FrameAction::Resize(size));
        self
    }

    #[must_use]
    pub fn scale(mut self, scale: DevicePixelRatio) -> Self {
        self.actions.push(FrameAction::Scale(scale));
        self
    }

    #[must_use]
    pub fn invalidate_lanes(mut self) -> Self {
        self.actions.push(FrameAction::InvalidateLanes);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum A11yCapture {
    Off,
    Pump,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct A11yDelivery {
    pub pending_before: bool,
    pub pending_after: bool,
    pub delivered: bool,
    pub full: bool,
    pub nodes: usize,
}

impl A11yDelivery {
    #[must_use]
    pub fn throttled(&self) -> bool {
        self.pending_before && !self.delivered && self.pending_after
    }
}

pub struct FrameSample {
    pub label: SmolStr,
    pub record: FrameRecord,
    pub a11y: A11yDelivery,
    pub a11y_update: Option<accesskit::TreeUpdate>,
    pub a11y_contract: Option<String>,
}

pub struct ScriptReport {
    samples: Vec<FrameSample>,
}

impl ScriptReport {
    #[must_use]
    pub fn samples(&self) -> &[FrameSample] {
        &self.samples
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    #[must_use]
    pub fn frame(&self, index: usize) -> &FrameSample {
        self.samples.get(index).unwrap_or_else(|| {
            panic!(
                "frame {index} does not exist: the script ran {} frame(s)",
                self.samples.len()
            )
        })
    }

    pub fn assert_rendered(&self, index: usize) {
        let sample = self.frame(index);
        assert!(
            sample.record.rendered,
            "frame {index} ({}) must commit pixels, but it is an idle frame",
            sample.label
        );
    }

    pub fn assert_idle(&self, index: usize) {
        let sample = self.frame(index);
        assert!(
            !sample.record.rendered,
            "frame {index} ({}) must be idle, but it committed frame {}",
            sample.label,
            sample.record.frame_index.get()
        );
        assert!(
            sample.record.damage.is_empty(),
            "idle frame {index} ({}) must not carry damage: {:?}",
            sample.label,
            sample.record.damage
        );
    }

    pub fn assert_damage_rects_at_most(&self, index: usize, max: usize) {
        let sample = self.frame(index);
        let rects = sample.record.damage.len();
        assert!(
            rects <= max,
            "frame {index} ({}) has {rects} damage rect(s), expected <= {max}: {:?}",
            sample.label,
            sample.record.damage
        );
    }

    pub fn assert_a11y_delivered(&self, index: usize) {
        let sample = self.frame(index);
        assert!(
            sample.a11y.delivered,
            "frame {index} ({}) must deliver an a11y snapshot, but none was produced \
             (pending_before={}, pending_after={}); is the script in A11yCapture::Pump mode?",
            sample.label, sample.a11y.pending_before, sample.a11y.pending_after
        );
    }

    pub fn assert_a11y_full(&self, index: usize) {
        let sample = self.frame(index);
        assert!(
            sample.a11y.delivered && sample.a11y.full,
            "frame {index} ({}) must deliver a full a11y tree (delivered={}, full={}, nodes={})",
            sample.label,
            sample.a11y.delivered,
            sample.a11y.full,
            sample.a11y.nodes
        );
    }

    pub fn assert_a11y_throttled(&self, index: usize) {
        let sample = self.frame(index);
        assert!(
            sample.a11y.throttled(),
            "frame {index} ({}) must have its a11y delivery throttled, but \
             pending_before={}, delivered={}, pending_after={}",
            sample.label,
            sample.a11y.pending_before,
            sample.a11y.delivered,
            sample.a11y.pending_after
        );
    }

    #[must_use]
    pub fn a11y_update(&self, index: usize) -> Option<&accesskit::TreeUpdate> {
        self.frame(index).a11y_update.as_ref()
    }

    pub fn assert_a11y_contract_ok(&self, index: usize) {
        let sample = self.frame(index);
        assert!(
            sample.a11y_contract.is_none(),
            "frame {index} ({}) delivered an invalid TreeUpdate: {}",
            sample.label,
            sample.a11y_contract.as_deref().unwrap_or("")
        );
    }

    pub fn assert_a11y_incremental_at_most(&self, index: usize, max: usize) {
        let sample = self.frame(index);
        let update = sample
            .a11y_update
            .as_ref()
            .unwrap_or_else(|| panic!("frame {index} ({}) delivered no TreeUpdate", sample.label));
        crate::test_support::a11y::assert_incremental_within(update, max);
    }

    #[must_use]
    pub fn a11y(&self, index: usize) -> A11yDelivery {
        self.frame(index).a11y
    }

    pub fn assert_a11y_contract_all(&self) {
        for index in 0..self.samples.len() {
            self.assert_a11y_contract_ok(index);
        }
    }

    #[must_use]
    pub fn a11y_snapshot(&self, index: usize) -> String {
        let sample = self.frame(index);
        let update = sample
            .a11y_update
            .as_ref()
            .unwrap_or_else(|| panic!("frame {index} ({}) delivered no TreeUpdate", sample.label));
        crate::test_support::a11y::tree_update_snapshot(update)
    }
}

pub struct FrameScript {
    app: TestApp,
    capture: A11yCapture,
    oracle: bool,
    queued: Vec<ScriptFrame>,
    report: ScriptReport,
}

impl FrameScript {
    #[must_use]
    pub fn new(app: TestApp) -> Self {
        Self {
            app,
            capture: A11yCapture::Off,
            oracle: false,
            queued: Vec::new(),
            report: ScriptReport {
                samples: Vec::new(),
            },
        }
    }

    #[must_use]
    pub fn with_a11y_capture(mut self, capture: A11yCapture) -> Self {
        if capture == A11yCapture::Pump {
            self.app.enable_a11y_capture();
        }
        self.capture = capture;
        self
    }

    #[must_use]
    pub fn with_a11y_oracle(mut self) -> Self {
        self.oracle = true;
        self
    }

    #[must_use]
    pub fn app(&self) -> &TestApp {
        &self.app
    }

    #[must_use]
    pub fn app_mut(&mut self) -> &mut TestApp {
        &mut self.app
    }

    pub fn push(&mut self, frame: ScriptFrame) -> &mut Self {
        self.queued.push(frame);
        self
    }

    #[must_use]
    pub fn report(&self) -> &ScriptReport {
        &self.report
    }

    pub fn run(&mut self) -> Result<(), EngineError> {
        for frame in std::mem::take(&mut self.queued) {
            let sample = self.execute(frame)?;
            self.report.samples.push(sample);
        }
        if self.capture == A11yCapture::Pump {
            self.report.assert_a11y_contract_all();
            self.app.assert_a11y_contract();
        }
        if self.oracle {
            self.app.assert_a11y_oracle();
        }
        Ok(())
    }

    fn execute(&mut self, frame: ScriptFrame) -> Result<FrameSample, EngineError> {
        let ScriptFrame { label, actions } = frame;
        for action in actions {
            match action {
                FrameAction::Mutate(mutate) => mutate(self.app.app_mut()),
                FrameAction::Input(event) => {
                    self.app.inject(event);
                }
                FrameAction::Advance(by) => self.app.advance(by),
                FrameAction::Resize(size) => self.set_window_size(size),
                FrameAction::Scale(scale) => self.app.set_dpr(scale),
                FrameAction::InvalidateLanes => self.app.invalidate_all_lanes(),
            }
        }
        let window = self.app.window_id();
        let pending_before = window_pending(&self.app, window);
        match self.capture {
            A11yCapture::Off => {
                self.app.drain()?;
            }
            A11yCapture::Pump => {
                self.app.drain_pump()?;
            }
        }
        let record = self.app.frame_record().clone();
        let a11y = match self.capture {
            A11yCapture::Off => A11yDelivery::default(),
            A11yCapture::Pump => {
                let pending_after = window_pending(&self.app, window);
                let (delivered, full, nodes) =
                    record.a11y.as_ref().map_or((false, false, 0), |snapshot| {
                        (true, snapshot.full, snapshot.nodes.len())
                    });
                A11yDelivery {
                    pending_before,
                    pending_after,
                    delivered,
                    full,
                    nodes,
                }
            }
        };
        Ok(FrameSample {
            label,
            record,
            a11y,
            a11y_update: self.app.a11y_update().cloned(),
            a11y_contract: self.app.a11y_contract_error().map(String::from),
        })
    }

    fn set_window_size(&mut self, size: LogicalSize) {
        let window = self.app.window_id();
        if let Some(target) = self.app.app_mut().window_mut(window) {
            target.set_size(size);
        }
    }
}

fn window_pending(app: &TestApp, window: WindowId) -> bool {
    app.app()
        .window(window)
        .is_some_and(|target| target.a11y_pending())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{A11yCapture, FrameScript, ScriptFrame};
    use crate::{
        a11y::{A11yOverride, A11yRole},
        event::{Action, InputEvent, KeyCode, Modifiers, PointerButton},
        math::{DevicePixelRatio, LogicalPoint, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, signal::Signal},
        paint::{
            color::Color,
            style::{PaintSpec, StylePatch},
        },
        test_support::{app::TestApp, probe::Selector},
    };

    fn painted_app() -> (TestApp, Signal<Color>) {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let background = app.app_mut().signal(Color::WHITE);
        let source = background;
        let _node = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(10.0), px(10.0), px(40.0), px(16.0)))
            .paint(PaintSpec {
                background: Color::WHITE,
                ..PaintSpec::default()
            })
            .on_click(Action::custom("press"))
            .bind_style(move |cx| StylePatch {
                background: Some(*cx.read(&source)),
                ..StylePatch::default()
            })
            .id("label")
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                root,
            )
            .expect("挂载");
        (app, background)
    }

    #[test]
    fn script_asserts_commit_damage_and_idle_by_frame_index() {
        let (app, background) = painted_app();
        let mut script = FrameScript::new(app);
        script.push(ScriptFrame::new("首帧"));
        script.push(ScriptFrame::new("只改一个节点").write(background, Color::BLACK));
        script.push(ScriptFrame::new("空闲帧"));
        script.run().expect("帧脚本");
        let report = script.report();

        assert_eq!(report.len(), 3, "脚本必须恰好执行 3 帧");
        report.assert_rendered(0);
        report.assert_rendered(1);
        report.assert_damage_rects_at_most(1, 1);
        report.assert_idle(2);

        let first = report.frame(0).record.frame_index.get();
        assert_eq!(
            report.frame(1).record.frame_index.get(),
            first + 1,
            "第二帧必须推进提交游标"
        );
        assert_eq!(
            report.frame(2).record.frame_index.get(),
            first + 1,
            "空闲帧不得推进提交游标"
        );
        assert!(
            script
                .app()
                .probe()
                .by_selector(Selector::Id("label"))
                .is_some(),
            "脚本必须保留引擎的探针能力"
        );
    }

    #[test]
    fn script_actions_cover_input_resize_and_scale() {
        let (app, _background) = painted_app();
        let mut script = FrameScript::new(app);
        script.push(ScriptFrame::new("首帧"));
        script.push(ScriptFrame::new("点击").input(InputEvent::pointer_down(
            LogicalPoint::new(px(4.0), px(4.0)),
            PointerButton::Primary,
            Modifiers::empty(),
        )));
        script.push(ScriptFrame::new("改尺寸").resize(LogicalSize::new(px(400.0), px(300.0))));
        script.push(ScriptFrame::new("改 DPI").scale(DevicePixelRatio::new(2.0).expect("scale")));
        script.push(ScriptFrame::new("全量重绘").invalidate_lanes());
        script.run().expect("帧脚本");
        let report = script.report();

        report.assert_rendered(0);
        assert!(
            report.frame(2).record.damage_full,
            "resize 必须退化为全帧：{:?}",
            report.frame(2).record.damage
        );
        let expected = DevicePixelRatio::ONE.to_covering_device_rect(LogicalRect::new(
            px(0.0),
            px(0.0),
            px(400.0),
            px(300.0),
        ));
        assert_eq!(report.frame(2).record.damage, vec![expected]);
        report.assert_rendered(3);
        report.assert_rendered(4);
    }

    #[test]
    fn focus_is_delivered_immediately_but_data_updates_stay_throttled() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let button = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(10.0), px(10.0), px(40.0), px(16.0)))
            .paint(PaintSpec::default())
            .on_click(Action::custom("press"))
            .a11y(A11yOverride {
                label: Some(smol_str::SmolStr::new_static("提交")),
                ..A11yOverride::default()
            })
            .id("submit")
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                root,
            )
            .expect("挂载");

        let mut script = FrameScript::new(app)
            .with_a11y_capture(A11yCapture::Pump)
            .with_a11y_oracle();
        script.push(ScriptFrame::new("首帧"));
        script.push(
            ScriptFrame::new("只移焦点（必须立即交付）").mutate(move |app| {
                app.set_focus(window, Some(button));
            }),
        );
        script.push(ScriptFrame::new("改数据（必须被节流）").mutate(move |app| {
            let tree = app.window_mut(window).expect("窗口").tree_mut();
            tree.set_a11y_patch(
                button,
                crate::a11y::A11yPatch {
                    label: Some(smol_str::SmolStr::new_static("提交（改）")),
                    ..crate::a11y::A11yPatch::default()
                },
            );
            tree.mark_a11y(button);
        }));
        script.push(ScriptFrame::new("时钟推进 100ms").advance(Duration::from_millis(100)));
        script.run().expect("帧脚本");
        let report = script.report();

        report.assert_a11y_full(0);
        report.assert_a11y_delivered(1);
        assert!(!report.a11y(1).full, "焦点笔必须是增量");
        assert!(!report.frame(1).record.rendered, "纯焦点变化不得提交像素");
        let focus_update = report.a11y_update(1).expect("焦点笔的 TreeUpdate");
        assert!(
            focus_update
                .nodes
                .iter()
                .any(|(id, _)| *id == focus_update.focus),
            "焦点节点必须与 focus 同笔送达（A3：否则 consumer 指向不存在的节点）"
        );
        report.assert_a11y_throttled(2);
        report.assert_a11y_delivered(3);
        assert!(
            report.a11y_snapshot(3).contains("提交（改）"),
            "补发必须携带最终值（推迟而非丢弃）：{}",
            report.a11y_snapshot(3)
        );
        report.assert_a11y_contract_all();
    }

    #[test]
    #[should_panic(expected = "inconsistent a11y tree")]
    fn a11y_oracle_is_red_when_a_node_forgets_mark_a11y() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let node = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(10.0), px(10.0), px(40.0), px(16.0)))
            .a11y(A11yOverride {
                label: Some(smol_str::SmolStr::new_static("旧标签")),
                ..A11yOverride::default()
            })
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                root,
            )
            .expect("挂载");
        let mut script = FrameScript::new(app)
            .with_a11y_capture(A11yCapture::Pump)
            .with_a11y_oracle();
        script.push(ScriptFrame::new("首帧（全树）"));
        script.push(
            ScriptFrame::new("改语义但故意漏 mark_a11y").mutate(move |app| {
                let tree = app.window_mut(window).expect("窗口").tree_mut();
                tree.set_a11y_patch(
                    node,
                    crate::a11y::A11yPatch {
                        label: Some(smol_str::SmolStr::new_static("新标签")),
                        ..crate::a11y::A11yPatch::default()
                    },
                );
            }),
        );
        script.run().expect("帧执行本身不报错，oracle 才该失败");
    }

    #[test]
    fn script_key_input_reaches_the_focus_chain() {
        let (app, _background) = painted_app();
        let window = app.window_id();
        let mut script = FrameScript::new(app);
        script.push(ScriptFrame::new("首帧"));
        script.push(
            ScriptFrame::new("Tab").input(InputEvent::key_down(KeyCode::Tab, Modifiers::empty())),
        );
        script.run().expect("帧脚本");
        let report = script.report();
        report.assert_rendered(0);
        assert!(
            script.app().app().focused(window).is_some(),
            "Tab 必须把焦点落到可聚焦节点上"
        );
    }

    #[test]
    fn multi_frame_a11y_delivery_is_budgeted_and_valid() {
        use crate::test_support::a11y::ShadowTree;

        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        for _ in 0..3000 {
            NodeBuilder::boxed()
                .build(
                    app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                    root,
                )
                .expect("node");
        }
        let nodes = app.app().window(window).expect("窗口").tree().len();

        let mut script = FrameScript::new(app).with_a11y_capture(A11yCapture::Pump);
        for index in 0..4 {
            script.push(
                ScriptFrame::new(format!("分批 {index}")).advance(Duration::from_millis(100)),
            );
        }
        script.run().expect("帧脚本");
        let report = script.report();

        report.assert_a11y_full(0);
        let mut shadow = ShadowTree::new();
        let mut delivered = 0usize;
        for index in 0..report.len() {
            let Some(update) = report.a11y_update(index) else {
                continue;
            };
            assert!(
                update.nodes.len() <= crate::a11y::adapter::A11Y_BATCH_BUDGET,
                "frame {index} 单笔超预算：{}",
                update.nodes.len()
            );
            shadow
                .apply(update)
                .unwrap_or_else(|error| panic!("frame {index} 违反 AccessKit 契约：{error}"));
            delivered += 1;
        }
        assert!(delivered >= 2, "3000 节点必须分批（实际 {delivered} 笔）");
        assert_eq!(shadow.len(), nodes, "分批不得丢节点");
        assert_eq!(
            script.app().a11y_shadow_summaries().len(),
            nodes,
            "TestApp 的影子树与脚本里独立应用的影子树必须一致"
        );
        report.assert_a11y_contract_all();
    }

    #[test]
    fn a11y_update_snapshot_is_human_reviewable() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let table = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Table),
                label: Some(smol_str::SmolStr::new_static("行情")),
                ..A11yOverride::default()
            })
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                root,
            )
            .expect("table");
        let header = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::Row),
                ..A11yOverride::default()
            })
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                table,
            )
            .expect("row");
        NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::ColumnHeader),
                label: Some(smol_str::SmolStr::new_static("价格")),
                sort: Some(crate::a11y::A11ySort::Descending),
                ..A11yOverride::default()
            })
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                header,
            )
            .expect("header");
        let input = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(A11yRole::TextInput),
                label: Some(smol_str::SmolStr::new_static("数量")),
                ..A11yOverride::default()
            })
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                root,
            )
            .expect("input");
        NodeBuilder::text("价🌍ab", crate::text::TextStyle::new(px(12.0)))
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                input,
            )
            .expect("text");
        let _ = app.drain().expect("首帧");
        app.assert_a11y_contract();

        let snapshot = crate::test_support::a11y::tree_update_snapshot(
            app.a11y_update().expect("首帧必须有 TreeUpdate"),
        );
        for expected in [
            "role=Table",
            "role=ColumnHeader",
            "sort=Descending",
            "role=TextRun",
            "character_lengths=[3, 4, 1, 1]",
            "rows=1",
        ] {
            assert!(
                snapshot.contains(expected),
                "快照必须包含 {expected}：\n{snapshot}"
            );
        }
        assert!(snapshot.contains("focus: #"), "{snapshot}");
    }
}
