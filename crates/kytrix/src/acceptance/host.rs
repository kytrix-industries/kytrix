use std::{cell::RefCell, path::PathBuf, rc::Rc, time::Duration};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{app::App, error::EngineError, window::WindowId};

use super::{options::Options, script::Script};
use crate::composition::Composition;

const TICK_INTERVAL: Duration = Duration::from_millis(4);

pub(crate) struct Run {
    script: Rc<RefCell<Script>>,
    report: Option<PathBuf>,
}

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
        log::error!("kytrix acceptance: cannot schedule the next tick: {error}");
    }
}

impl Run {
    pub(crate) fn record_engine_error(&self, error: &EngineError) {
        self.script
            .borrow_mut()
            .abandon(format_smolstr!("the event loop failed: {error}"));
    }

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
