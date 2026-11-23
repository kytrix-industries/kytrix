//! The P6 acceptance self-check: a real window driven by a script.
//!
//! The three-platform acceptance cannot rely on a human clicking a window on each host, and
//! a build that links the local translation engine (libomp on macOS) and the audio backend
//! (libasound2-dev on Linux) does not even exist on the acceptance hosts. So the acceptance
//! build is the same application with two features off, and the acceptance run is a script
//! that drives the real window and answers with an exit code:
//!
//! ```text
//! cargo build -p kytrix --no-default-features --features acceptance
//! kytrix --acceptance --acceptance-report=acceptance.txt
//! ```
//!
//! The script injects pointer input through the engine's production-visible `automation`
//! feature, reads what the window painted (frame cursor, primitive count, text payloads,
//! scene colours), writes the settings to move language and appearance, and asserts that
//! each change reached the window. Every step is named in the report, so a failure says
//! which step of a real window broke instead of only that something did.
//!
//! - [`options`]: the command line (`--acceptance`, `--acceptance-report=<path>`);
//! - [`script`]: the steps, host-agnostic (the same steps run in the real pump and in the
//!   headless rehearsal);
//! - [`report`]: the step log;
//! - [`host`]: the real window's frame route and the exit decision.
//!
//! The headless rehearsal in `acceptance::tests` runs the script against the shell over the
//! engine's test host, which is what keeps the steps covered by `cargo test` without a
//! window.

pub(crate) mod options;
pub(crate) mod report;
pub(crate) mod script;

#[cfg(feature = "acceptance")]
pub(crate) mod host;

#[cfg(test)]
mod tests;
