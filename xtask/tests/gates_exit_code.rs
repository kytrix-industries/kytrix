//! Process-level lock on the gate exit-code contract.
//!
//! `gates::all_passed` has a unit test, but the contract that matters is the one
//! the shell sees: a run in which any command fails must exit non-zero. This runs
//! the real binary against a scope that cannot resolve (so its single `cargo`
//! command fails) and asserts the process status, the summary line and the
//! `--log` overwrite semantics in one go.

use std::{fs, process::Command};

#[test]
fn a_failed_gate_command_makes_the_process_exit_non_zero() {
    let directory = std::env::temp_dir().join(format!("kytrix-gates-exit-{}", std::process::id()));
    fs::create_dir_all(&directory).expect("create the temporary log directory");
    let log = directory.join("gates.log");
    fs::write(&log, "stale run\n").expect("seed the previous run's log");

    let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .args([
            "gates",
            "--platform",
            "win",
            "--scope",
            "no_such_crate",
            "--step",
            "release",
            "--log",
        ])
        .arg(&log)
        .output()
        .expect("run the xtask binary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(
        output.status.code(),
        Some(0),
        "a failed gate command must fail the process\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("RESULT: FAIL"),
        "the summary must name the failure:\n{stdout}"
    );
    assert!(
        stdout.contains("no_such_crate"),
        "the failed command must be listed:\n{stdout}"
    );

    let written = fs::read_to_string(&log).expect("read the run log");
    assert!(
        !written.contains("stale run"),
        "an explicit `--log` path must be truncated:\n{written}"
    );
    assert!(
        written.contains("== kytrix gates =="),
        "the log must hold this run:\n{written}"
    );

    let _ = fs::remove_dir_all(&directory);
}
