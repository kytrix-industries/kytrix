//! Bounded execution of desktop discovery commands.

use std::{
    io::Read,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const DISCOVERY_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const DISCOVERY_COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(super) fn bounded_command_output(command: &mut Command) -> Option<Output> {
    bounded_command_output_with_timeout(command, DISCOVERY_COMMAND_TIMEOUT)
}

pub(super) fn bounded_command_output_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> Option<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                child.stdout.take()?.read_to_end(&mut stdout).ok()?;
                return Some(Output {
                    status,
                    stdout,
                    stderr: Vec::new(),
                });
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(DISCOVERY_COMMAND_POLL_INTERVAL);
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}
