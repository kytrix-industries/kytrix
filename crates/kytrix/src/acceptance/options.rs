//! The acceptance self-check's command line.
//!
//! The binary takes two arguments, both optional:
//!
//! - `--acceptance`: run the P6 acceptance self-check in a real window instead of starting
//!   the application for a user;
//! - `--acceptance-report=<path>`: also write the step log to that file.
//!
//! Everything else is ignored, which is the ordinary start's behaviour for an argument-less
//! launch and keeps this parser from growing a second source of truth about the shell's
//! command line.

use std::path::PathBuf;

/// The acceptance self-check's command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Options {
    /// Where to write the step log; `None` = the log goes to `log::info!` only.
    report: Option<PathBuf>,
}

impl Options {
    /// Where to write the step log, taken once by the host (None = log only).
    pub(crate) fn take_report(mut self) -> Option<PathBuf> {
        self.report.take()
    }

    /// The options of an acceptance run, or `None` when this process is a normal start.
    pub(crate) fn parse(args: impl IntoIterator<Item = String>) -> Option<Self> {
        let mut options = Self::default();
        let mut requested = false;
        for arg in args {
            if arg == "--acceptance" {
                requested = true;
            } else if let Some(path) = arg.strip_prefix("--acceptance-report=") {
                options.report = Some(PathBuf::from(path));
            }
        }
        requested.then_some(options)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::Options;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(ToString::to_string).collect()
    }

    /// A start with no argument is a start for a user: no acceptance run.
    #[test]
    fn a_plain_start_should_not_request_the_acceptance_run() {
        assert_eq!(Options::parse(args(&[])), None);
        assert_eq!(Options::parse(args(&["--something-else"])), None);
    }

    /// `--acceptance` is the switch; a report path on its own does not run one.
    #[test]
    fn only_the_flag_should_start_the_run() {
        assert_eq!(
            Options::parse(args(&["--acceptance"])),
            Some(Options::default())
        );
        assert_eq!(
            Options::parse(args(&["--acceptance-report=out.txt"])),
            None,
            "a report path without the flag is a normal start"
        );
    }

    /// The report path is read from its `=` form; the flag and the path may come in any
    /// order, and the host takes the path once.
    #[test]
    fn the_report_path_should_be_read_from_the_flag() {
        let options = Options::parse(args(&["--acceptance-report=dir/out.txt", "--acceptance"]))
            .expect("the flag requests the run");
        assert_eq!(options.take_report(), Some(PathBuf::from("dir/out.txt")));
    }
}
