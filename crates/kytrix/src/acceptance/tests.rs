use super::script::Script;
use crate::test_support::Harness;

const REHEARSAL_FRAMES: usize = 400;

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
