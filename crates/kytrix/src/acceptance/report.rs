use std::{fs, io, path::Path, time::Duration};

use smol_str::{SmolStr, format_smolstr};

#[derive(Clone, Debug)]
struct Step {
    name: &'static str,
    passed: bool,
    detail: SmolStr,
}

#[derive(Clone, Debug)]
pub(crate) struct Report {
    window: u32,
    steps: Vec<Step>,
    failure: Option<SmolStr>,
    frames: u32,
    elapsed: Duration,
}

impl Report {
    pub(crate) fn new(window: u32) -> Self {
        Self {
            window,
            steps: Vec::new(),
            failure: None,
            frames: 0,
            elapsed: Duration::ZERO,
        }
    }

    pub(crate) fn pass(&mut self, name: &'static str, detail: SmolStr) {
        self.steps.push(Step {
            name,
            passed: true,
            detail,
        });
    }

    pub(crate) fn fail(&mut self, name: &'static str, detail: SmolStr) {
        self.failure = Some(format_smolstr!("{name}: {detail}"));
        self.steps.push(Step {
            name,
            passed: false,
            detail,
        });
    }

    #[cfg(test)]
    pub(crate) fn failed(&self) -> bool {
        self.failure.is_some()
    }

    pub(crate) fn seal(&mut self, frames: u32, elapsed: Duration) {
        self.frames = frames;
        self.elapsed = elapsed;
    }

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

    pub(crate) fn write(&self, path: &Path) -> io::Result<()> {
        fs::write(path, self.render())
    }

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
