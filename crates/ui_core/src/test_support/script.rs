//! 帧脚本执行器（L2.5）：按脚本推进 N 帧，逐帧收集记录并断言。
//!
//! L2 的断言原本散落在每个 `#[test]` 里手写 `drain()` + 取 `frame_record()`：多帧场景
//! （首帧提交 → 局部 damage → 空闲帧 → a11y 节流补发）每加一帧就多一层手写样板，而「第 k 帧」
//! 这个维度只能靠变量名表达。帧脚本把「动作 → 帧 → 记录」显式化：
//!
//! ```text
//! let mut script = FrameScript::new(app);
//! script.push(ScriptFrame::new("首帧"));
//! script.push(ScriptFrame::new("只改一个节点").write(color, Color::BLACK));
//! script.push(ScriptFrame::new("空闲帧"));
//! script.run().expect("帧脚本");
//! let report = script.report();
//! report.assert_rendered(0);
//! report.assert_damage_rects_at_most(1, 1);
//! report.assert_idle(2);
//! ```
//!
//! 每帧的交付摘要（[`A11yDelivery`]）记录「帧前是否有 a11y 脏位 / 本帧是否交付 / 是否全树 /
//! 节点数」，所以「被节流」是可断言的：`pending_before && !delivered && pending_after`。
//! 节流只存在于 [`A11yCapture::Pump`] 模式（走 [`TestApp::drain_pump`] 的平台泵闸门）；
//! [`A11yCapture::Off`] 下 a11y 不被观察，摘要恒为默认值。

use std::time::Duration;

use smol_str::SmolStr;

use crate::{
    app::App,
    error::EngineError,
    event::InputEvent,
    math::{Scale, Size},
    node::signal::Signal,
    test_support::{app::TestApp, record::FrameRecord},
    window::WindowId,
};

/// 一帧里要执行的动作（按加入顺序执行，然后推进一帧）。
enum FrameAction {
    /// 写信号 / 改树（`ScriptFrame::write` 与 `ScriptFrame::mutate` 的载体）。
    Mutate(Box<dyn FnOnce(&mut App)>),
    /// 投递一个输入事件（与平台泵相同的 `App::dispatch_input`）。
    Input(InputEvent),
    /// 推进可控时钟（双击窗口 / 动画 / a11y 节流都读它）。
    Advance(Duration),
    /// 改窗口逻辑尺寸。
    Resize(Size),
    /// 改 DPI 比例。
    Scale(Scale),
    /// 强制整棵树的 lane 失效（全量重绘路径）。
    InvalidateLanes,
}

/// 脚本里的一帧：一串动作 + 一个标签。
pub struct ScriptFrame {
    label: SmolStr,
    actions: Vec<FrameAction>,
}

impl ScriptFrame {
    /// 一帧。`label` 只进报告与断言失败信息，不参与执行。
    #[must_use]
    pub fn new(label: impl Into<SmolStr>) -> Self {
        Self {
            label: label.into(),
            actions: Vec::new(),
        }
    }

    /// 写一个信号（`App::write` 的糖；值没变时引擎自己 no-op）。
    #[must_use]
    pub fn write<T: Clone + PartialEq + 'static>(mut self, signal: Signal<T>, value: T) -> Self {
        self.actions.push(FrameAction::Mutate(Box::new(move |app| {
            app.write(&signal, value);
        })));
        self
    }

    /// 直接改 `App`（建/删节点、改焦点、换字体栈……）。
    #[must_use]
    pub fn mutate(mut self, mutate: impl FnOnce(&mut App) + 'static) -> Self {
        self.actions.push(FrameAction::Mutate(Box::new(mutate)));
        self
    }

    /// 投递一个输入事件。
    #[must_use]
    pub fn input(mut self, event: InputEvent) -> Self {
        self.actions.push(FrameAction::Input(event));
        self
    }

    /// 推进可控时钟。
    #[must_use]
    pub fn advance(mut self, by: Duration) -> Self {
        self.actions.push(FrameAction::Advance(by));
        self
    }

    /// 改窗口逻辑尺寸（引擎会把本帧退化为全帧 damage）。
    #[must_use]
    pub fn resize(mut self, size: Size) -> Self {
        self.actions.push(FrameAction::Resize(size));
        self
    }

    /// 改 DPI 比例（布局与图集都会换代）。
    #[must_use]
    pub fn scale(mut self, scale: Scale) -> Self {
        self.actions.push(FrameAction::Scale(scale));
        self
    }

    /// 强制全量重绘（lane 全部失效）。
    #[must_use]
    pub fn invalidate_lanes(mut self) -> Self {
        self.actions.push(FrameAction::InvalidateLanes);
        self
    }
}

/// a11y 交付模式（脚本级显式开关）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum A11yCapture {
    /// 不捕获（默认）：脚本只测像素与调度，a11y 摘要恒为默认值。
    Off,
    /// 平台泵等价：脏位在 `A11Y_THROTTLE`（100 ms）内不交付。
    Pump,
}

/// 一帧的 a11y 交付摘要。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct A11yDelivery {
    /// 本帧开始时窗口是否有 a11y 脏位（**不含**节流判定）。
    pub pending_before: bool,
    /// 本帧结束后是否仍有脏位（被节流挡下时保持 `true`）。
    pub pending_after: bool,
    /// 本帧是否产出了一笔交付。
    pub delivered: bool,
    /// 交付是否全树（`false` = 增量）。
    pub full: bool,
    /// 交付携带的节点数（未交付 = 0）。
    pub nodes: usize,
}

impl A11yDelivery {
    /// 本帧的交付是否被 `A11Y_THROTTLE` 挡下：帧前有脏位、没交付、脏位保留到帧后。
    #[must_use]
    pub fn throttled(&self) -> bool {
        self.pending_before && !self.delivered && self.pending_after
    }
}

/// 一帧的执行结果。
pub struct FrameSample {
    /// 脚本里的帧标签。
    pub label: SmolStr,
    /// 该帧的调度记录（含 `stats` / `damage` / `damage_full` / `paint_log` / `a11y`）。
    pub record: FrameRecord,
    /// a11y 交付摘要。
    pub a11y: A11yDelivery,
}

/// 一次（或多次 `run`）的逐帧结果。
pub struct ScriptReport {
    samples: Vec<FrameSample>,
}

impl ScriptReport {
    /// 全部帧样本（脚本顺序）。
    #[must_use]
    pub fn samples(&self) -> &[FrameSample] {
        &self.samples
    }

    /// 帧数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// 是否一帧都没有。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// 第 `index` 帧（0 基）。
    ///
    /// # Panics
    /// `index` 越界（脚本写错）。
    #[must_use]
    pub fn frame(&self, index: usize) -> &FrameSample {
        self.samples.get(index).unwrap_or_else(|| {
            panic!(
                "frame {index} does not exist: the script ran {} frame(s)",
                self.samples.len()
            )
        })
    }

    /// 第 `index` 帧必须提交像素。
    ///
    /// # Panics
    /// 该帧是空闲帧。
    pub fn assert_rendered(&self, index: usize) {
        let sample = self.frame(index);
        assert!(
            sample.record.rendered,
            "frame {index} ({}) must commit pixels, but it is an idle frame",
            sample.label
        );
    }

    /// 第 `index` 帧必须是**空闲零帧**：不提交像素、damage 为空。
    ///
    /// # Panics
    /// 该帧提交了像素，或带了 damage。
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

    /// 第 `index` 帧的 damage 矩形数必须 `<= max`（`0` = 不允许任何 damage）。
    ///
    /// # Panics
    /// 矩形数超过 `max`。
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

    /// 第 `index` 帧必须产出一笔 a11y 交付。
    ///
    /// # Panics
    /// 本帧没有 a11y 交付（没有工作，或被节流）。
    pub fn assert_a11y_delivered(&self, index: usize) {
        let sample = self.frame(index);
        assert!(
            sample.a11y.delivered,
            "frame {index} ({}) must deliver an a11y snapshot, but none was produced \
             (pending_before={}, pending_after={}); is the script in A11yCapture::Pump mode?",
            sample.label, sample.a11y.pending_before, sample.a11y.pending_after
        );
    }

    /// 第 `index` 帧的 a11y 交付必须是**全树**（`full == true`）。
    ///
    /// # Panics
    /// 本帧没有交付，或交付是增量。
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

    /// 第 `index` 帧的 a11y 交付必须被 `A11Y_THROTTLE` 节流。
    ///
    /// # Panics
    /// 本帧交付了、本帧没有 a11y 脏位，或脏位被消费掉了（不是节流）。
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
}

/// 帧脚本执行器：持有 `TestApp`，按顺序执行排队的帧并累积报告。
pub struct FrameScript {
    app: TestApp,
    capture: A11yCapture,
    queued: Vec<ScriptFrame>,
    report: ScriptReport,
}

impl FrameScript {
    /// 包一个已经挂好场景的 `TestApp`（默认不捕获 a11y）。
    #[must_use]
    pub fn new(app: TestApp) -> Self {
        Self {
            app,
            capture: A11yCapture::Off,
            queued: Vec::new(),
            report: ScriptReport {
                samples: Vec::new(),
            },
        }
    }

    /// 打开 a11y 捕获（[`A11yCapture::Pump`] 需要捕获开关，见 `TestApp::enable_a11y_capture`）。
    #[must_use]
    pub fn with_a11y_capture(mut self, capture: A11yCapture) -> Self {
        if capture == A11yCapture::Pump {
            self.app.enable_a11y_capture();
        }
        self.capture = capture;
        self
    }

    /// 只读引擎（探针 / 记录 / 窗口参数）。
    #[must_use]
    pub fn app(&self) -> &TestApp {
        &self.app
    }

    /// 可变引擎（脚本外的一次性设置：挂场景、改焦点、换字体栈）。
    #[must_use]
    pub fn app_mut(&mut self) -> &mut TestApp {
        &mut self.app
    }

    /// 排队一帧。
    pub fn push(&mut self, frame: ScriptFrame) -> &mut Self {
        self.queued.push(frame);
        self
    }

    /// 已执行的结果（`run` 之前为空）。
    #[must_use]
    pub fn report(&self) -> &ScriptReport {
        &self.report
    }

    /// 执行全部排队帧（队列清空、报告累积；之后再 `push` + `run` 可续跑）。
    ///
    /// 结果用 [`Self::report`] 取：`run` 不返回报告的借用，避免「执行完就锁住 `app`」的坑。
    ///
    /// # Errors
    /// 任一帧失败（`EngineError`）：失败帧之前的结果留在报告里，之后的帧不再执行。
    pub fn run(&mut self) -> Result<(), EngineError> {
        for frame in std::mem::take(&mut self.queued) {
            let sample = self.execute(frame)?;
            self.report.samples.push(sample);
        }
        Ok(())
    }

    /// 执行一帧：动作 → 推进一帧 → 抓记录与 a11y 摘要。
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
                FrameAction::Scale(scale) => self.set_window_scale(scale),
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
            // 不观察就不报告：`Off` 下脏位永远不会被消费，拿它推「被节流」是假阳性。
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
        })
    }

    /// 改窗口尺寸（`Window::set_size`；引擎会把本帧退化为全帧 damage）。
    fn set_window_size(&mut self, size: Size) {
        let window = self.app.window_id();
        if let Some(target) = self.app.app_mut().window_mut(window) {
            target.set_size(size);
        }
    }

    /// 改 DPI 比例（`Window::set_scale`）。
    fn set_window_scale(&mut self, scale: Scale) {
        let window = self.app.window_id();
        if let Some(target) = self.app.app_mut().window_mut(window) {
            target.set_scale(scale);
        }
    }
}

/// 窗口是否有 a11y 脏位（**不含** `A11Y_THROTTLE` 判定；`App::a11y_pending` 带节流）。
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
        a11y::A11yOverride,
        event::{Action, InputEvent, Key, KeyState, Modifiers, PointerButton},
        math::{Point, Rect, Scale, Size},
        node::{builder::NodeBuilder, signal::Signal},
        paint::{
            color::Color,
            style::{PaintSpec, StylePatch},
        },
        test_support::{app::TestApp, probe::Selector},
    };

    /// 一个绑了背景色信号的 button（`write` / `input` 动作的落点；`on_click` 让它可被 Tab 聚焦）。
    fn painted_app() -> (TestApp, Signal<Color>) {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let background = app.app_mut().signal(Color::WHITE);
        let source = background;
        let _node = NodeBuilder::boxed()
            .layout_override(Rect::new(10.0, 10.0, 40.0, 16.0))
            .paint(PaintSpec {
                background: Color::WHITE,
                ..PaintSpec::default()
            })
            .on_click(Action::custom("press"))
            .bind_style(move |cx| StylePatch {
                background: Some(cx.read(&source)),
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
        script.push(ScriptFrame::new("点击").input(InputEvent::PointerDown {
            position: Point::new(4.0, 4.0),
            button: PointerButton::Primary,
            modifiers: Modifiers::empty(),
        }));
        script.push(ScriptFrame::new("改尺寸").resize(Size::new(400.0, 300.0)));
        script.push(ScriptFrame::new("改 DPI").scale(Scale::new(2.0).expect("scale")));
        script.push(ScriptFrame::new("全量重绘").invalidate_lanes());
        script.run().expect("帧脚本");
        let report = script.report();

        report.assert_rendered(0);
        assert!(
            report.frame(2).record.damage_full,
            "resize 必须退化为全帧：{:?}",
            report.frame(2).record.damage
        );
        let expected = Rect::new(0.0, 0.0, 400.0, 300.0).to_physical_covering(Scale::ONE);
        assert_eq!(report.frame(2).record.damage, vec![expected]);
        report.assert_rendered(3);
        report.assert_rendered(4);
    }

    #[test]
    fn script_reports_throttled_a11y_delivery() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let button = NodeBuilder::boxed()
            .layout_override(Rect::new(10.0, 10.0, 40.0, 16.0))
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

        let mut script = FrameScript::new(app).with_a11y_capture(A11yCapture::Pump);
        script.push(ScriptFrame::new("首帧"));
        script.push(ScriptFrame::new("只移焦点").mutate(move |app| {
            app.set_focus(window, Some(button));
        }));
        script.push(ScriptFrame::new("节流未到期"));
        script.push(ScriptFrame::new("时钟推进 100ms").advance(Duration::from_millis(100)));
        script.run().expect("帧脚本");
        let report = script.report();

        report.assert_a11y_full(0);
        report.assert_a11y_throttled(1);
        report.assert_a11y_throttled(2);
        report.assert_a11y_delivered(3);
        assert!(
            !report.frame(3).a11y.full,
            "补发必须是增量（焦点变化），实际 full={}",
            report.frame(3).a11y.full
        );
        assert!(!report.frame(1).record.rendered, "纯焦点变化不得提交像素");
    }

    #[test]
    fn script_key_input_reaches_the_focus_chain() {
        let (app, _background) = painted_app();
        let window = app.window_id();
        let mut script = FrameScript::new(app);
        script.push(ScriptFrame::new("首帧"));
        script.push(ScriptFrame::new("Tab").input(InputEvent::Key {
            key: Key::Tab,
            state: KeyState::Down,
            modifiers: Modifiers::empty(),
        }));
        script.run().expect("帧脚本");
        let report = script.report();
        report.assert_rendered(0);
        assert!(
            script.app().app().focused(window).is_some(),
            "Tab 必须把焦点落到可聚焦节点上"
        );
    }
}
