//! The acceptance self-check's step log.
//!
//! One line per step: its name (the thing that was checked), `PASS` / `FAIL`, and what the
//! step actually observed. A failure stops the run at the step that failed, so the log names
//! the step instead of only saying that the window was wrong somewhere.
//!
//! The log is plain text on purpose: the acceptance run must be readable from a CI terminal
//! and from a file without a serializer, and the only numbers it carries (frames, counts,
//! milliseconds) are ones a human can compare between hosts.

use std::{fs, io, path::Path, time::Duration};

use smol_str::{SmolStr, format_smolstr};

/// One step's result.
#[derive(Clone, Debug)]
struct Step {
    /// What was checked, e.g. `pointer.click`.
    name: &'static str,
    /// Whether it held.
    passed: bool,
    /// What it observed (counts, ids, sizes).
    detail: SmolStr,
}

/// The whole run's log.
#[derive(Clone, Debug)]
pub(crate) struct Report {
    /// The window the script drove (its raw id; the log only prints it).
    window: u32,
    /// The step results, in the order they ran.
    steps: Vec<Step>,
    /// The step that stopped the run, if any.
    failure: Option<SmolStr>,
    /// The window's committed frame at the end of the run.
    frames: u32,
    /// How long the run took.
    elapsed: Duration,
}

impl Report {
    /// An empty log for the window numbered `window` (`WindowId::to_bits`).
    pub(crate) fn new(window: u32) -> Self {
        Self {
            window,
            steps: Vec::new(),
            failure: None,
            frames: 0,
            elapsed: Duration::ZERO,
        }
    }

    /// Records a step that held.
    pub(crate) fn pass(&mut self, name: &'static str, detail: SmolStr) {
        self.steps.push(Step {
            name,
            passed: true,
            detail,
        });
    }

    /// Records the step that stopped the run.
    pub(crate) fn fail(&mut self, name: &'static str, detail: SmolStr) {
        self.failure = Some(format_smolstr!("{name}: {detail}"));
        self.steps.push(Step {
            name,
            passed: false,
            detail,
        });
    }

    /// Whether the run failed.
    #[cfg(test)]
    pub(crate) fn failed(&self) -> bool {
        self.failure.is_some()
    }

    /// Seals the log with the frame the window ended on and the time the run took.
    pub(crate) fn seal(&mut self, frames: u32, elapsed: Duration) {
        self.frames = frames;
        self.elapsed = elapsed;
    }

    /// The log as the text written to the report file and the process log.
    pub(crate) fn render(&self) -> String {
        let mut text = String::with_capacity(256 + self.steps.len() * 96);
        text.push_str("kytrix acceptance (P6-1)\n");
        text.push_str(&format!(
            "window {}: {} committed frame(s), {:.2} s\n",
            self.window,
            self.frames,
            self.elapsed.as_secs_f64()
        ));
        for step in &self.steps {
            let verdict = if step.passed { "PASS" } else { "FAIL" };
            text.push_str(&format!("  {verdict} {}: {}\n", step.name, step.detail));
        }
        match &self.failure {
            Some(reason) => text.push_str(&format!("RESULT: FAIL ({reason})\n")),
            None => text.push_str(&format!("RESULT: PASS ({} steps)\n", self.steps.len())),
        }
        text
    }

    /// Writes the log to `path` (the file is created or replaced).
    pub(crate) fn write(&self, path: &Path) -> io::Result<()> {
        fs::write(path, self.render())
    }

    /// The step names, for tests that assert the script ran each of them.
    #[cfg(test)]
    pub(crate) fn step_names(&self) -> Vec<&'static str> {
        self.steps.iter().map(|step| step.name).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use smol_str::SmolStr;

    use super::Report;

    /// A passing run renders every step with its detail and a PASS verdict.
    #[test]
    fn a_passing_run_should_render_every_step() {
        let mut report = Report::new(1);
        report.pass("startup.frames", SmolStr::new_static("committed 3 frames"));
        report.pass("painted", SmolStr::new_static("primitives=431"));
        report.seal(7, Duration::from_millis(1234));

        let text = report.render();
        assert!(!report.failed());
        assert!(text.contains("PASS startup.frames: committed 3 frames"));
        assert!(text.contains("PASS painted: primitives=431"));
        assert!(text.contains("RESULT: PASS (2 steps)"));
        assert!(text.contains("1.23 s"));
        assert!(!text.contains("FAIL"));
    }

    /// A failing run names the failing step and does not claim a pass.
    #[test]
    fn a_failing_run_should_name_the_step() {
        let mut report = Report::new(1);
        report.pass("startup.frames", SmolStr::new_static("ok"));
        report.fail("pointer.click", SmolStr::new_static("no action"));
        report.seal(0, Duration::ZERO);

        let text = report.render();
        assert!(report.failed());
        assert!(text.contains("FAIL pointer.click: no action"));
        assert!(text.contains("RESULT: FAIL (pointer.click: no action)"));
        assert_eq!(report.step_names(), ["startup.frames", "pointer.click"]);
    }
}
