//! The real window's acceptance host: the script's clock, and the report.
//!
//! The acceptance build starts the application exactly like a normal start — the same
//! composition root, the same shell, the same platform pump — and then hands the script a
//! repeating one-shot timer (E10). Each time it fires, the script runs one step; a step that
//! needs frames asks for them through the engine's public repaint entry, and the pump draws
//! them in the turn the tick ran in.
//!
//! A timer rather than a frame route is what keeps the run independent of the frame budget: a
//! pump turn only runs when something wakes it, and a frame request's wake can be consumed by
//! the very turn that made it. The timer is the engine's own "wake me later" path, so every
//! tick is guaranteed to happen and the run cannot stall waiting for an unrelated message.
//!
//! Nothing here is compiled into a normal start: `--acceptance` is a feature of the
//! acceptance build, not a runtime mode of the shipped application.

use std::{cell::RefCell, path::PathBuf, rc::Rc, time::Duration};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{app::App, error::EngineError, window::WindowId};

use super::{options::Options, script::Script};
use crate::composition::Composition;

/// How long between two script ticks. The acceptance run is a few dozen ticks, so the exact
/// interval only decides how long a run takes; it is small enough that the wall-clock budget
/// is never the limit and large enough not to keep the machine busy.
const TICK_INTERVAL: Duration = Duration::from_millis(4);

/// A running acceptance self-check.
pub(crate) struct Run {
    /// The script the timer drives.
    script: Rc<RefCell<Script>>,
    /// Where the log is written when the caller named a path.
    report: Option<PathBuf>,
}

/// Starts the acceptance run's timer chain and returns its handle.
///
/// The first timer is scheduled here (before the pump starts); every tick schedules the next
/// one until the script is over. That chain is the run's only clock.
pub(crate) fn start(
    app: &mut App,
    composition: &Rc<Composition>,
    window: WindowId,
    options: Options,
) -> Run {
    let script = Rc::new(RefCell::new(Script::new(window)));
    let report = options.take_report();
    log::info!(
        "kytrix acceptance: driving window {} (report: {})",
        window.to_bits(),
        report
            .as_ref()
            .map_or("<log only>".to_owned(), |path| path.display().to_string())
    );
    schedule_tick(app, Rc::clone(&script), Rc::clone(composition), window);
    Run { script, report }
}

/// Schedules the next script tick.
fn schedule_tick(
    app: &mut App,
    script: Rc<RefCell<Script>>,
    composition: Rc<Composition>,
    window: WindowId,
) {
    let outcome = app.after(window, TICK_INTERVAL, move |app| {
        let finished = script.borrow_mut().tick(app, &composition);
        if !finished {
            schedule_tick(app, script, composition, window);
        }
    });
    if let Err(error) = outcome {
        // The window is gone: there is nothing left to drive, and the run's own window check
        // reports the failure on the next tick if one could still happen.
        log::error!("kytrix acceptance: cannot schedule the next tick: {error}");
    }
}

impl Run {
    /// Ends the run with the pump's failure (the event loop returned an error).
    pub(crate) fn record_engine_error(&self, error: &EngineError) {
        self.script
            .borrow_mut()
            .abandon(format_smolstr!("the event loop failed: {error}"));
    }

    /// Seals the log, writes it where the caller asked, and answers whether the run passed.
    ///
    /// A run that is still going here has lost its window before the script finished (the
    /// close path can win): that is a failure, and the log says so instead of claiming a
    /// pass the steps never reached.
    pub(crate) fn finish(self) -> bool {
        if !self.script.borrow().finished() {
            self.script.borrow_mut().abandon(SmolStr::new_static(
                "the window closed before the script finished",
            ));
        }

        let script = self.script.borrow();
        if script.failed() {
            log::error!("kytrix acceptance: FAILED\n{}", script.report_text());
        } else {
            log::info!("kytrix acceptance: PASSED\n{}", script.report_text());
        }
        let mut passed = !script.failed();
        if let Some(path) = &self.report {
            match script.write_report(path) {
                Ok(()) => log::info!("kytrix acceptance: report written to {}", path.display()),
                Err(error) => {
                    log::error!(
                        "kytrix acceptance: cannot write {}: {error}",
                        path.display()
                    );
                    passed = false;
                }
            }
        }
        passed
    }
}
