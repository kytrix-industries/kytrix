//! The acceptance script's headless rehearsal.
//!
//! The script is the same value in both hosts: the real window's frame route calls
//! [`Script::tick`] once per frame, and this module calls it once per frame between the
//! shell's own route and the frame — the production order. What heads-up runs here is
//! therefore what the acceptance build runs, which is how `cargo test` covers the steps
//! without a window.
//!
//! The rehearsal also proves the **failure** path: a window that goes away before the
//! script finishes is reported as a failed run, not as a pass nobody reached.

use super::script::Script;
use crate::test_support::Harness;

/// How many frames the rehearsal may take before it calls the script stuck (the script's
/// own budget is smaller; this is the test's backstop).
const REHEARSAL_FRAMES: usize = 400;

/// Runs the script to its end over the headless shell and returns it.
fn rehearse() -> Script {
    let mut harness = Harness::new();
    let mut script = Script::new(harness.window);
    for _ in 0..REHEARSAL_FRAMES {
        harness
            .composition
            .frame_router
            .tick(harness.app.app_mut())
            .expect("the shell's route runs");
        let finished = script.tick(harness.app.app_mut(), harness.composition.as_ref());
        if finished {
            return script;
        }
        let _ = harness.app.drain();
    }
    panic!(
        "the acceptance script did not finish in {REHEARSAL_FRAMES} frames:\n{}",
        script.report_text()
    );
}

/// Every step passes on the real shell over the headless engine, and the log names them in
/// the order they ran.
#[test]
fn the_acceptance_script_should_pass_on_the_shell() {
    let script = rehearse();

    assert!(script.finished(), "the run reached its end");
    assert!(
        !script.failed(),
        "every step should hold:\n{}",
        script.report_text()
    );
    assert_eq!(
        script.report().step_names(),
        [
            "startup.frames",
            "painted",
            "pointer.move",
            "language",
            "theme",
            "scroll",
            "pointer.click"
        ],
        "the script runs every step, in one order:\n{}",
        script.report_text()
    );
    let text = script.report_text();
    assert!(text.contains("RESULT: PASS (7 steps)"), "{text}");
    assert!(
        text.contains("committed frame"),
        "the startup step records the cursor"
    );
}

/// The steps leave the application on the state they ended in: the language and the
/// appearance writes are committed settings, not drafts.
#[test]
fn the_acceptance_steps_should_commit_what_they_wrote() {
    let mut harness = Harness::new();
    let mut script = Script::new(harness.window);
    let language = harness.composition.hub.read(|config| config.language);
    for _ in 0..REHEARSAL_FRAMES {
        harness
            .composition
            .frame_router
            .tick(harness.app.app_mut())
            .expect("the shell's route runs");
        let finished = script.tick(harness.app.app_mut(), harness.composition.as_ref());
        if finished {
            break;
        }
        let _ = harness.app.drain();
    }

    assert!(script.finished(), "the run reached its end");
    assert!(!script.failed(), "{}", script.report_text());
    assert_ne!(
        harness.composition.hub.read(|config| config.language),
        language,
        "the language step wrote the other language through the commit seam"
    );
}

/// A window that is closed before the script runs is a failed run whose log names the
/// host, not a pass that reported nothing.
#[test]
fn a_window_that_closed_before_the_script_should_fail_the_run() {
    let mut harness = Harness::new();
    let window = harness.window;
    harness.shutdown();

    let mut script = Script::new(window);
    assert!(
        script.tick(harness.app.app_mut(), harness.composition.as_ref()),
        "the tick detects the closed window and ends the run"
    );
    assert!(script.finished());
    assert!(script.failed());
    assert!(
        script
            .failure()
            .is_some_and(|reason| reason.starts_with("host: ")),
        "the log says the host lost the window: {:?}",
        script.failure()
    );
    assert!(script.report_text().contains("RESULT: FAIL"));
}

/// `--acceptance-report=<path>` is the evidence a CI run keeps: a passing run writes the
/// whole log there.
#[test]
fn the_report_should_be_written_where_the_run_points_it() {
    let script = rehearse();
    let path = std::env::temp_dir().join(format!(
        "kytrix-acceptance-report-{}.txt",
        std::process::id()
    ));

    script.write_report(&path).expect("the report is written");
    let text = std::fs::read_to_string(&path).expect("the report is readable");
    let _ = std::fs::remove_file(&path);

    assert!(text.starts_with("kytrix acceptance (P6-1)"), "{text}");
    assert!(text.contains("RESULT: PASS (7 steps)"), "{text}");
    assert!(text.contains("pointer.click:"), "{text}");
}
