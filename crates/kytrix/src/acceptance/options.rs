use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Options {
    report: Option<PathBuf>,
}

impl Options {
    pub(crate) fn take_report(mut self) -> Option<PathBuf> {
        self.report.take()
    }

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

    #[test]
    fn a_plain_start_should_not_request_the_acceptance_run() {
        assert_eq!(Options::parse(args(&[])), None);
        assert_eq!(Options::parse(args(&["--something-else"])), None);
    }

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

    #[test]
    fn the_report_path_should_be_read_from_the_flag() {
        let options = Options::parse(args(&["--acceptance-report=dir/out.txt", "--acceptance"]))
            .expect("the flag requests the run");
        assert_eq!(options.take_report(), Some(PathBuf::from("dir/out.txt")));
    }
}
