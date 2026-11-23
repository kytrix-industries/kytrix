//! The acceptance self-check's script: the steps a real window must pass.
//!
//! The script is driven one step per frame by [`Script::tick`]. It is deliberately
//! **host-agnostic**: the tick takes `&mut App` and the settings hub, nothing else, so the
//! same steps run in the real window (the binary's frame route calls `tick`) and in the
//! headless rehearsal (`acceptance::tests` calls `tick` between the shell's route and the
//! frame). There is one implementation of the steps, not one for the window and one for
//! the test.
//!
//! # What a step may assume
//!
//! Coordinates come from [`Window::size`](ui_core::window::Window::size) and the shell's
//! own metrics ([`metrics`]): the pane's centre is the centre of the area below the
//! self-drawn title bar and right of the sidebar, and a shell control is clicked at the
//! centre of the node carrying its debug id (found by walking the tree, never by hard-coded
//! pixel positions). A layout change therefore moves the script with the shell instead of
//! breaking it; a step that cannot find its target fails with the step's name and what was
//! looked for.
//!
//! # What makes a step pass
//!
//! - **A frame commits**: the window's committed `frame_index` advances (the engine only
//!   advances it on a commit; an idle turn does not).
//! - **The input arrives**: [`App::inject_input`](ui_core::app::App::inject_input) returns
//!   the dispatch's receipt — a hit for the pointer steps, the dispatched action for the
//!   click.
//! - **The change is drawn**: a language write changes the text payloads the tree draws
//!   and a theme write changes the quad fills the scene carries, both after the write's
//!   frames; the sidebar click changes the stored mode and commits frames.
//! - **Nothing panics**: a panic anywhere in the pump kills the process, so a run that
//!   reaches its report has exercised the window without one.
//!
//! A failure seals the report with the failed step's name and closes the window: the
//! process exits non-zero with the log naming the step.

use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};

use app_config::{
    settings::{Appearance, Language},
    ui::SidebarMode,
};
use base_ui::theme::appearance::Appearance as ThemeAppearance;
use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    app::App,
    event::{InputEvent, Modifiers, PointerButton},
    math::{Point, Rect},
    node::id::NodeId,
    paint::{
        color::Color,
        primitive::{Primitive, QuadPaint},
    },
    window::{FrameIndex, WindowId},
};

use super::report::Report;
use crate::{
    composition::Composition,
    shell::{metrics, title_bar::TITLE_BAR_SIDEBAR_TOGGLE},
};

/// How many committed frames the script waits for before its first assertion.
///
/// A fresh window needs a frame to measure the shell and another to apply the geometry that
/// measurement produced (the headless host's own two ticks); the third is the margin that
/// makes the painted-primitive assertion about a settled window rather than a half-built
/// one.
const STARTUP_FRAMES: u32 = 3;

/// How many frames a state change (sidebar mode / language / appearance) is given to reach
/// the window: one turn applies the write in the shell's route, the next draws it.
const SETTLE_FRAMES: u32 = 2;

/// The painted-primitive floor. A shell draws hundreds; an empty or failed frame draws none.
const MINIMUM_PRIMITIVES: usize = 10;

/// A run that has not finished after this many pump turns is stuck. The script ticks once per
/// pump turn (not once per commit: a turn with a deferred frame still runs the routes), so
/// this is a much larger number than the run's frame count.
const TICK_BUDGET: u32 = 20_000;

/// A run that has not finished after this long is stuck (frames are arriving far too
/// slowly).
const TIME_BUDGET: Duration = Duration::from_secs(90);

/// The label the sidebar's settings entry draws in each language: the one sentence the
/// language step can name without depending on the window's contents.
const SETTINGS_LABEL_EN: &str = "Settings";
const SETTINGS_LABEL_ZH_CN: &str = "设置";

/// What the script is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Wait for the first frames.
    Startup,
    /// The window committed pixels and text.
    Painted,
    /// A pointer move reaches the shell's page area.
    PointerMove,
    /// A language write changes the text the tree draws.
    Language,
    /// An appearance write changes the palette the scene draws.
    Theme,
    /// A scroll is delivered to the page area.
    Scroll,
    /// A click on the title bar's sidebar toggle changes the stored mode and redraws.
    PointerClick,
    /// Everything passed.
    Done,
}

/// What one tick of a step asks for.
enum StepOutcome {
    /// The step held; the next tick runs the next one.
    Pass,
    /// The step needs `n` more committed frames before its second half runs.
    Wait(u32),
    /// The script is done.
    Done,
    /// The step failed: `(step name, what was observed)`.
    Fail(&'static str, SmolStr),
}

/// The acceptance self-check's script.
pub(crate) struct Script {
    /// The window under test.
    window: WindowId,
    /// When the run started (the wall-clock budget and the report's duration).
    started: Instant,
    /// The step being run.
    phase: Phase,
    /// Whether the tick entered with a completed wait: the phase handler runs its
    /// "after the wait" half.
    resumed: bool,
    /// The committed frame a wait is waiting for (`None` = not waiting).
    wait_until: Option<u32>,
    /// Ticks run so far (the frame budget).
    ticks: u32,
    /// The window's committed frame at the last tick.
    last_index: u32,
    /// The step log.
    report: Report,
    /// The step that stopped the run.
    failure: Option<SmolStr>,
    /// Whether the run is over (pass or fail).
    finished: bool,
    /// How long the run took.
    elapsed: Duration,
    /// The frame the current phase's second half compares against.
    index_before: u32,
    /// The text payloads before the language write.
    texts_before: Vec<SmolStr>,
    /// The quad fills before the appearance write.
    colors_before: Vec<Color>,
    /// The sidebar mode before the toggle click.
    mode_before: SidebarMode,
    /// The language before the language write.
    language_before: Language,
    /// The committed frame a repaint was requested for (Some(index) = a frame is in\n    /// flight; requesting again would wake the pump for nothing and spin the route).
    requested_at: Option<u32>,
    /// Whether the palette before the appearance write was the dark one.
    ///
    /// The stored preference can be `System`, so the *written* value is not what decides
    /// whether the palette changes: the resolved appearance is. The step picks the other
    /// palette from this, not from the stored preference.
    was_dark: bool,
}

impl Script {
    /// A script for `window`, ready to run.
    pub(crate) fn new(window: WindowId) -> Self {
        Self {
            window,
            started: Instant::now(),
            phase: Phase::Startup,
            resumed: false,
            wait_until: None,
            ticks: 0,
            last_index: 0,
            requested_at: None,
            report: Report::new(window.to_bits()),
            failure: None,
            finished: false,
            elapsed: Duration::ZERO,
            index_before: 0,
            texts_before: Vec::new(),
            colors_before: Vec::new(),
            mode_before: SidebarMode::default(),
            language_before: Language::default(),
            was_dark: false,
        }
    }

    /// Runs one step (or one waiting frame). Returns whether the run is over.
    ///
    /// The host calls this once per frame **after** the shell's route, then submits the
    /// frame: that is the production order, so the state a step observes is the state the
    /// frame before it produced. A step that needs more frames returns with a repaint
    /// requested and the same phase runs again once the frames have committed.
    pub(crate) fn tick(&mut self, app: &mut App, composition: &Composition) -> bool {
        if self.finished {
            return true;
        }
        self.ticks += 1;
        let index = app
            .frame_index(self.window)
            .map_or(0, |index: FrameIndex| index.get());
        self.last_index = index;
        if self.requested_at.is_some_and(|at| index > at) {
            self.requested_at = None;
        }
        if self.ticks == 1 || self.ticks.is_multiple_of(120) {
            log::info!(
                "kytrix acceptance: tick {}, phase {:?}, committed frame {index}",
                self.ticks,
                self.phase
            );
        }

        if app.window(self.window).is_none() {
            self.abandon(SmolStr::new_static(
                "the window went away before the script finished",
            ));
            return true;
        }
        if self.ticks > TICK_BUDGET {
            self.stop(
                "budget.frames",
                format_smolstr!("{TICK_BUDGET} pump turns without finishing"),
            );
            return true;
        }
        if self.started.elapsed() > TIME_BUDGET {
            self.stop(
                "budget.time",
                format_smolstr!("{} ms without finishing", TIME_BUDGET.as_millis()),
            );
            return true;
        }

        if let Some(target) = self.wait_until {
            if index < target {
                self.request_frame(app);
                return false;
            }
            self.wait_until = None;
            self.resumed = true;
        }

        let phase = self.phase;
        match self.step(app, composition, index) {
            StepOutcome::Pass => {
                log::info!("kytrix acceptance: step {phase:?} passed");
                self.request_frame(app);
                false
            }
            StepOutcome::Wait(frames) => {
                let target = index + frames;
                log::info!(
                    "kytrix acceptance: step {phase:?} waits for frame {target} (now {index})"
                );
                self.wait_until = Some(target);
                self.request_frame(app);
                false
            }
            StepOutcome::Done => {
                self.elapsed = self.started.elapsed();
                self.report.seal(index, self.elapsed);
                self.finished = true;
                let _ = app.close_window(self.window);
                true
            }
            StepOutcome::Fail(name, detail) => {
                self.stop(name, detail);
                let _ = app.close_window(self.window);
                true
            }
        }
    }

    /// Whether the run is over.
    pub(crate) fn finished(&self) -> bool {
        self.finished
    }

    /// Whether the run is over and failed.
    pub(crate) fn failed(&self) -> bool {
        self.failure.is_some()
    }

    /// The failure reason (`step: what was observed`), for the headless rehearsal.
    #[cfg(test)]
    pub(crate) fn failure(&self) -> Option<&SmolStr> {
        self.failure.as_ref()
    }

    /// The step log as text (what the process log and `--acceptance-report` carry).
    pub(crate) fn report_text(&self) -> String {
        self.report.render()
    }

    /// Writes the step log to `path`.
    ///
    /// # Errors
    /// The file could not be created or written.
    pub(crate) fn write_report(&self, path: &Path) -> io::Result<()> {
        self.report.write(path)
    }

    /// Ends the run as a failure from **outside** the steps: the pump reported an error, or
    /// the window closed before the script finished.
    pub(crate) fn abandon(&mut self, reason: SmolStr) {
        if self.finished {
            return;
        }
        self.stop("host", reason);
    }

    /// The step log, for the headless rehearsal's assertions.
    #[cfg(test)]
    pub(crate) fn report(&self) -> &Report {
        &self.report
    }

    /// Records one tick's step.
    fn step(&mut self, app: &mut App, composition: &Composition, index: u32) -> StepOutcome {
        match self.phase {
            Phase::Startup => self.step_startup(index),
            Phase::Painted => self.step_painted(app, index),
            Phase::PointerMove => self.step_pointer_move(app, composition),
            Phase::PointerClick => self.step_pointer_click(app, composition, index),
            Phase::Language => self.step_language(app, composition, index),
            Phase::Theme => self.step_theme(app, composition, index),
            Phase::Scroll => self.step_scroll(app, composition),
            Phase::Done => StepOutcome::Done,
        }
    }

    /// Waits for the first frames, then records the committed cursor.
    fn step_startup(&mut self, index: u32) -> StepOutcome {
        if !std::mem::take(&mut self.resumed) {
            return StepOutcome::Wait(STARTUP_FRAMES);
        }
        if index == 0 {
            return StepOutcome::Fail(
                "startup.frames",
                SmolStr::new_static("the window never committed a frame"),
            );
        }
        self.report.pass(
            "startup.frames",
            format_smolstr!("committed frame {index} after {STARTUP_FRAMES} waits"),
        );
        self.advance(Phase::Painted);
        StepOutcome::Pass
    }

    /// The window drew: it has a size, a non-trivial scene and text.
    fn step_painted(&mut self, app: &App, index: u32) -> StepOutcome {
        let Some(window) = app.window(self.window) else {
            return StepOutcome::Fail("painted", SmolStr::new_static("the window is gone"));
        };
        let size = window.size();
        let primitives = app.painted_primitives(self.window);
        let texts = app.texts(self.window);
        if size.width <= 0.0 || size.height <= 0.0 {
            return StepOutcome::Fail(
                "painted",
                format_smolstr!("zero-sized window ({}x{})", size.width, size.height),
            );
        }
        if primitives < MINIMUM_PRIMITIVES {
            return StepOutcome::Fail(
                "painted",
                format_smolstr!("only {primitives} painted primitive(s)"),
            );
        }
        if texts.is_empty() {
            return StepOutcome::Fail("painted", SmolStr::new_static("the window draws no text"));
        }
        self.report.pass(
            "painted",
            format_smolstr!(
                "frame {index}, size {}x{}, {primitives} primitive(s), {} text(s)",
                size.width,
                size.height,
                texts.len()
            ),
        );
        self.advance(Phase::PointerMove);
        StepOutcome::Pass
    }

    /// A pointer move reaches the page area: the receipt's hit is what the engine hovered,
    /// and the pointer position it recorded is the one the event carried.
    fn step_pointer_move(&mut self, app: &mut App, composition: &Composition) -> StepOutcome {
        let Some(centre) = pane_centre(app, self.window, composition) else {
            return StepOutcome::Fail("pointer.move", SmolStr::new_static("no window to aim at"));
        };
        let outcome = app.inject_input(
            self.window,
            InputEvent::pointer_move(centre, Modifiers::empty()),
        );
        let Some(hit) = outcome.hit else {
            return StepOutcome::Fail(
                "pointer.move",
                format_smolstr!("no hit at ({}, {})", centre.x, centre.y),
            );
        };
        if app.hovered(self.window) != Some(hit.node) {
            return StepOutcome::Fail(
                "pointer.move",
                SmolStr::new_static("the engine's hover does not follow the moved pointer"),
            );
        }
        if app.pointer_position(self.window) != Some(centre) {
            return StepOutcome::Fail(
                "pointer.move",
                SmolStr::new_static("the engine did not record the pointer position"),
            );
        }
        self.report.pass(
            "pointer.move",
            format_smolstr!(
                "hit node {} at ({}, {})",
                hit.node.to_bits(),
                centre.x,
                centre.y
            ),
        );
        self.advance(Phase::Language);
        StepOutcome::Pass
    }

    /// A click on the title bar's sidebar toggle is dispatched as an action and the shell
    /// applies it: the stored mode moves one step and the window commits the new layout.
    fn step_pointer_click(
        &mut self,
        app: &mut App,
        composition: &Composition,
        index: u32,
    ) -> StepOutcome {
        if !std::mem::take(&mut self.resumed) {
            let Some((target, bounds)) =
                click_target_for_debug_id(app, self.window, TITLE_BAR_SIDEBAR_TOGGLE)
            else {
                return StepOutcome::Fail(
                    "pointer.click",
                    format_smolstr!(
                        "{TITLE_BAR_SIDEBAR_TOGGLE} draws no interactive box with an area"
                    ),
                );
            };
            let centre = centre_of(bounds);
            let _ = app.inject_input(
                self.window,
                InputEvent::pointer_down(centre, PointerButton::Primary, Modifiers::empty()),
            );
            let up = app.inject_input(
                self.window,
                InputEvent::pointer_up(centre, PointerButton::Primary, Modifiers::empty()),
            );
            if up.action.is_none() {
                return StepOutcome::Fail(
                    "pointer.click",
                    format_smolstr!("no action at ({}, {})", centre.x, centre.y),
                );
            }
            if up.target != Some(target) {
                return StepOutcome::Fail(
                    "pointer.click",
                    SmolStr::new_static("the action was dispatched to another target"),
                );
            }
            self.mode_before = composition.hub.read(|config| config.ui.sidebar);
            self.index_before = index;
            return StepOutcome::Wait(SETTLE_FRAMES);
        }

        let mode = composition.hub.read(|config| config.ui.sidebar);
        let expected = metrics::next_mode(self.mode_before);
        if mode != expected {
            return StepOutcome::Fail(
                "pointer.click.settle",
                format_smolstr!(
                    "sidebar mode {:?} -> {:?}, expected {expected:?}",
                    self.mode_before,
                    mode
                ),
            );
        }
        if index <= self.index_before {
            return StepOutcome::Fail(
                "pointer.click.settle",
                SmolStr::new_static("the shell did not commit the new layout"),
            );
        }
        self.report.pass(
            "pointer.click",
            format_smolstr!(
                "sidebar {:?} -> {:?} in {} committed frame(s)",
                self.mode_before,
                mode,
                index - self.index_before
            ),
        );
        self.advance(Phase::Done);
        StepOutcome::Pass
    }

    /// A language write reaches the drawn text: the tree's text payloads change and the
    /// catalogue's own label for the new language is among them.
    fn step_language(
        &mut self,
        app: &mut App,
        composition: &Composition,
        index: u32,
    ) -> StepOutcome {
        if !std::mem::take(&mut self.resumed) {
            self.language_before = composition.hub.read(|config| config.language);
            self.texts_before = app.texts(self.window);
            self.index_before = index;
            let target = match self.language_before {
                Language::En => Language::ZhCn,
                Language::ZhCn => Language::En,
            };
            if let Err(error) = composition.hub.edit(app, |config| config.language = target) {
                return StepOutcome::Fail(
                    "language",
                    format_smolstr!("the settings write failed: {error}"),
                );
            }
            return StepOutcome::Wait(SETTLE_FRAMES);
        }

        let language = composition.hub.read(|config| config.language);
        let expected = match language {
            Language::ZhCn => SETTINGS_LABEL_ZH_CN,
            Language::En => SETTINGS_LABEL_EN,
        };
        let texts = app.texts(self.window);
        if texts == self.texts_before {
            return StepOutcome::Fail(
                "language",
                format_smolstr!(
                    "{:?} -> {language:?} changed no drawn text",
                    self.language_before
                ),
            );
        }
        if !texts.iter().any(|text| text.as_str() == expected) {
            return StepOutcome::Fail(
                "language",
                format_smolstr!("the drawn text does not contain `{expected}`"),
            );
        }
        if index <= self.index_before {
            return StepOutcome::Fail(
                "language",
                SmolStr::new_static("the language write committed no frame"),
            );
        }
        self.report.pass(
            "language",
            format_smolstr!(
                "{:?} -> {language:?}: text nodes {} -> {}, `{expected}` drawn",
                self.language_before,
                self.texts_before.len(),
                texts.len()
            ),
        );
        self.advance(Phase::Theme);
        StepOutcome::Pass
    }

    /// An appearance write reaches the palette: the resolved appearance flips and the quad
    /// fills the scene carries change.
    fn step_theme(&mut self, app: &mut App, composition: &Composition, index: u32) -> StepOutcome {
        if !std::mem::take(&mut self.resumed) {
            self.was_dark = composition.theme.current(app) == ThemeAppearance::Dark;
            self.colors_before = painted_colors(app, self.window);
            self.index_before = index;
            let target = if self.was_dark {
                Appearance::Light
            } else {
                Appearance::Dark
            };
            if let Err(error) = composition
                .hub
                .edit(app, |config| config.appearance = target)
            {
                return StepOutcome::Fail(
                    "theme",
                    format_smolstr!("the settings write failed: {error}"),
                );
            }
            return StepOutcome::Wait(SETTLE_FRAMES);
        }

        let appearance = composition.hub.read(|config| config.appearance);
        let is_dark = composition.theme.current(app) == ThemeAppearance::Dark;
        let colors = painted_colors(app, self.window);
        if is_dark == self.was_dark {
            return StepOutcome::Fail(
                "theme",
                format_smolstr!(
                    "the resolved palette stayed {} (stored preference {appearance:?})",
                    if is_dark { "dark" } else { "light" }
                ),
            );
        }
        if self.colors_before.is_empty() {
            return StepOutcome::Fail(
                "theme",
                SmolStr::new_static("the scene carries no quad fill to compare"),
            );
        }
        if colors == self.colors_before {
            return StepOutcome::Fail(
                "theme",
                format_smolstr!(
                    "the palette flipped but repainted the same {} colour(s)",
                    colors.len()
                ),
            );
        }
        if index <= self.index_before {
            return StepOutcome::Fail(
                "theme",
                SmolStr::new_static("the appearance write committed no frame"),
            );
        }
        self.report.pass(
            "theme",
            format_smolstr!(
                "{} -> {}: {} quad fill(s) changed in {} committed frame(s) (stored preference {appearance:?})",
                if self.was_dark { "dark" } else { "light" },
                if is_dark { "dark" } else { "light" },
                self.colors_before.len(),
                index - self.index_before
            ),
        );
        self.advance(Phase::Scroll);
        StepOutcome::Pass
    }

    /// A scroll over the page area is delivered (receipt + recorded position); no scrolling
    /// content is required, the pane may have none.
    fn step_scroll(&mut self, app: &mut App, composition: &Composition) -> StepOutcome {
        let Some(centre) = pane_centre(app, self.window, composition) else {
            return StepOutcome::Fail("scroll", SmolStr::new_static("no window to aim at"));
        };
        let outcome = app.inject_input(
            self.window,
            InputEvent::scroll_lines(centre, Point::new(0.0, -1.0), Modifiers::empty()),
        );
        if outcome.hit.is_none() {
            return StepOutcome::Fail(
                "scroll",
                format_smolstr!("no hit at ({}, {})", centre.x, centre.y),
            );
        }
        if app.pointer_position(self.window) != Some(centre) {
            return StepOutcome::Fail(
                "scroll",
                SmolStr::new_static("the engine did not record the scroll's position"),
            );
        }
        self.report.pass(
            "scroll",
            format_smolstr!("delivered at ({}, {})", centre.x, centre.y),
        );
        self.advance(Phase::PointerClick);
        StepOutcome::Pass
    }

    /// Moves to the next phase.
    fn advance(&mut self, phase: Phase) {
        self.phase = phase;
        self.resumed = false;
    }

    /// Asks for the next frame: the script's own steps are what keeps the pump ticking.
    ///
    /// At most one request is outstanding per committed frame. The pump runs the routes once
    /// per turn (a turn with a deferred frame still runs them), so requesting on every turn
    /// would keep a wake message in flight and spin the route; while a request is pending the
    /// engine's own budget deferral schedules the wake that brings the frame.
    fn request_frame(&mut self, app: &mut App) {
        if self.requested_at == Some(self.last_index) {
            return;
        }
        self.requested_at = Some(self.last_index);
        let _ = app.request_repaint(self.window);
    }

    /// Ends the run as a failure with a step's name.
    fn stop(&mut self, name: &'static str, detail: SmolStr) {
        self.report.fail(name, detail.clone());
        self.failure = Some(format_smolstr!("{name}: {detail}"));
        self.elapsed = self.started.elapsed();
        self.report.seal(self.last_index, self.elapsed);
        self.finished = true;
    }
}

/// The centre of the shell's page area: below the self-drawn title bar and right of the
/// sidebar, both from the window's own size and the shell's metrics.
fn pane_centre(app: &App, window: WindowId, composition: &Composition) -> Option<Point> {
    let size = app.window(window)?.size();
    let sidebar = metrics::width(composition.hub.read(|config| config.ui.sidebar));
    Some(Point::new(
        (sidebar + size.width) / 2.0,
        (metrics::TITLE_BAR_HEIGHT + size.height) / 2.0,
    ))
}

/// The click target of the shell control carrying `debug_id`, and its box.
///
/// The debug id may match more than one node: the title bar rebuilds its toggle when the
/// language changes and leaves the previous wrapper behind without its children, so the
/// wrapper that carries an interactive box is the live one. Of the boxes that exist, the
/// largest is the control's own square (decorations inside it are smaller).
fn click_target_for_debug_id(
    app: &App,
    window: WindowId,
    debug_id: &str,
) -> Option<(NodeId, Rect)> {
    let tree = app.window(window)?.tree();
    let mut wrappers = Vec::new();
    let mut stack = vec![tree.root()];
    while let Some(node) = stack.pop() {
        if tree.debug_id(node) == Some(debug_id) {
            wrappers.push(node);
        }
        if let Ok(entry) = tree.get(node) {
            stack.extend(entry.children().iter().rev().copied());
        }
    }
    let area = |bounds: &Rect| bounds.size.width * bounds.size.height;
    wrappers
        .into_iter()
        .filter_map(|wrapper| click_target(app, window, wrapper))
        .max_by(|(_, left), (_, right)| area(left).total_cmp(&area(right)))
}

/// The largest node at or below `wrapper` that takes pointer input and has a real box.
///
/// A shell control is a wrapper (for its debug id) around the kit component that owns the
/// action; the component builds several nodes, and only its square is both interactive and
/// laid out. Clicking the largest one is the component's own square rather than a decoration
/// inside it (`kytrix::test_support` clicks the same node). The wrapper itself is the
/// fallback: a control that carries its own interaction has no interactive child.
fn click_target(app: &App, window: WindowId, wrapper: NodeId) -> Option<(NodeId, Rect)> {
    let tree = app.window(window)?.tree();
    let mut best: Option<(NodeId, Rect)> = None;
    let mut stack = vec![wrapper];
    while let Some(node) = stack.pop() {
        let Ok(entry) = tree.get(node) else {
            continue;
        };
        let bounds = entry.layout().bounds;
        if entry.style_patch().interactive == Some(true)
            && bounds.size.width > 0.0
            && bounds.size.height > 0.0
        {
            let area = bounds.size.width * bounds.size.height;
            if best.is_none_or(|(_, current)| area > current.size.width * current.size.height) {
                best = Some((node, bounds));
            }
        }
        stack.extend(entry.children().iter().rev().copied());
    }
    best.or_else(|| {
        node_bounds(app, window, wrapper).and_then(|bounds| {
            (bounds.size.width > 0.0 && bounds.size.height > 0.0).then_some((wrapper, bounds))
        })
    })
}

/// The node's layout rectangle (window coordinates).
fn node_bounds(app: &App, window: WindowId, node: NodeId) -> Option<Rect> {
    app.window(window)?
        .tree()
        .get(node)
        .ok()
        .map(|entry| entry.layout().bounds)
}

/// The centre of `bounds`.
fn centre_of(bounds: Rect) -> Point {
    Point::new(
        bounds.origin.x + bounds.size.width / 2.0,
        bounds.origin.y + bounds.size.height / 2.0,
    )
}

/// Every quad fill the window's scene carries, in paint order.
///
/// This is the appearance step's observation: the engine resolves the theme into `Color`s
/// before they reach the scene, so a palette that did not repaint leaves the same list.
fn painted_colors(app: &App, window: WindowId) -> Vec<Color> {
    let Some(window) = app.window(window) else {
        return Vec::new();
    };
    window
        .scene()
        .iter()
        .filter_map(|primitive| match primitive {
            Primitive::Quad(quad) => Some(match &quad.paint {
                QuadPaint::Solid { fill, .. } => *fill,
                QuadPaint::Gradient(gradient) => gradient.from,
            }),
            _ => None,
        })
        .collect()
}
