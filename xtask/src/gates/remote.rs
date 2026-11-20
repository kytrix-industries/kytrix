//! The remote half of `cargo xtask gates`: the macOS/Linux runner.
//!
//! It reuses the protocol `.review/p1-remote.ps1` established by hand: the local
//! HEAD is force-pushed to the host's bare mirror `~/kytrix-p1.git` under the branch
//! `mig-<name>`, a dedicated clean source directory is reset to that branch, the
//! shared `CARGO_TARGET_DIR=~/kytrix-review-target` is reused, and a `mkdir` lock
//! serialises the runs on the host (macOS has no `flock(1)`; `mkdir` is the one
//! primitive both hosts have). The lock is the directory the P1/P2 scripts take,
//! the branch carries a `mig-` prefix, and the source directory uses a
//! `kytrix-mig-src-` prefix, so a migration gate run never compiles into the shared
//! target directory while a reviewer's run does and never pushes over a reviewer's
//! validation branch or touches their checkouts.

use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use super::{
    GateError, GateResult, GatesConfig, Heads, Output, PlatformPlan, ProcessOutput, RemoteNames,
    StreamKind, describe_exit, format_elapsed, stream_command,
};

/// The same lock `.review/p1-remote.ps1` takes, which is what keeps both flows
/// from compiling into the shared target directory at the same time.
const LOCK_DIRECTORY: &str = "/tmp/kytrix-p1-remote.lock";

/// The prefix of the per-run scratch checkout on every host.
const SOURCE_PREFIX: &str = "kytrix-mig-src-";

/// `ssh` exits 255 when the connection itself fails (DNS, TCP, authentication).
/// Any other status came from the remote script or from a process on the host.
const SSH_TRANSPORT_FAILURE: i32 = 255;

/// Runs one remote platform's plan and reports what each command did.
pub(super) fn run(
    root: &Path,
    plan: &PlatformPlan,
    config: &GatesConfig,
    names: &RemoteNames,
    output: &Output,
) -> Result<Vec<GateResult>, GateError> {
    let platform = plan.platform;
    let host = platform
        .ssh_host()
        .ok_or(GateError::NotRemote(platform.name()))?;
    let git_remote = platform
        .git_remote()
        .ok_or(GateError::NotRemote(platform.name()))?;

    output.line(
        StreamKind::Stdout,
        &format!(
            "---- [{}] ssh {host}: branch `{}`, scratch `~/{SOURCE_PREFIX}{}`",
            platform.name(),
            names.branch,
            names.directory
        ),
    );
    push(root, git_remote, names, output)?;

    let script = build_script(plan, config.lock_timeout_secs, names);
    output.line(
        StreamKind::Stdout,
        &format!(
            "---- [{}] script: {} lines, {} commands",
            platform.name(),
            script.lines().count(),
            plan.commands.len()
        ),
    );

    let script_path = write_script(&script)?;
    let script_file = File::open(&script_path);
    let arguments = ssh_arguments(host);
    output.clear_heads();
    let session = match script_file {
        Ok(file) => stream_command("ssh", &arguments, root, Stdio::from(file), output),
        Err(source) => Err(GateError::ScriptOpen {
            path: script_path.clone(),
            source,
        }),
    };
    // The script is a throwaway; a failed delete must not mask the gate verdict.
    let _ = fs::remove_file(&script_path);
    let session = session?;

    output.line(
        StreamKind::Stdout,
        &format!(
            "---- [{}] ssh {} after {}",
            platform.name(),
            describe_exit(session.exit_code),
            format_elapsed(session.elapsed)
        ),
    );

    let heads = output.take_heads();
    let markers = output.take_markers();
    interpret(plan, session, &heads, &markers, host)
}

/// The `ssh` invocation for a host: never prompt, fail a dead TCP connect in 15 s
/// instead of hanging, and give up on a silently dropped link after two minutes.
pub(super) fn ssh_arguments(host: &str) -> Vec<String> {
    [
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=15",
        "-o",
        "ServerAliveInterval=30",
        "-o",
        "ServerAliveCountMax=4",
        host,
        "bash",
        "-s",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// Turns what a remote script wrote into one result per command, or names why the
/// run stopped short.
///
/// Everything is decided from the marker lines and the ssh exit status, so the
/// classification is unit-testable without a host. The remote script ends with
/// `KYTRIX_TOTAL_EXIT`, but a script that is killed (OOM, ENOSPC, a connection
/// lost mid-run) never gets there: that is a remote abort, not an authentication
/// failure, unless the ssh transport itself failed with [`SSH_TRANSPORT_FAILURE`].
pub(super) fn interpret(
    plan: &PlatformPlan,
    session: ProcessOutput,
    heads: &Heads,
    markers: &[RemoteMarker],
    host: &str,
) -> Result<Vec<GateResult>, GateError> {
    let completed = markers
        .iter()
        .any(|marker| matches!(marker, RemoteMarker::TotalExit(_)));
    if !completed {
        // The host lock is held by another run, which the script names explicitly.
        if let Some(timeout) = markers.iter().find_map(|marker| match marker {
            RemoteMarker::LockTimeout(seconds) => Some(*seconds),
            _ => None,
        }) {
            return Err(GateError::RemoteLockTimeout {
                host: host.to_owned(),
                timeout,
            });
        }
        // The scratch checkout could not be prepared; name the stage.
        if let Some(stage) = markers.iter().find_map(|marker| match marker {
            RemoteMarker::Abort(stage) => Some(stage.as_str()),
            _ => None,
        }) {
            return Err(GateError::RemoteAborted {
                host: host.to_owned(),
                detail: format!("stage `{stage}` failed"),
                head: heads.render(),
            });
        }
        if session.exit_code == Some(SSH_TRANSPORT_FAILURE) {
            return Err(GateError::SshFailed {
                host: host.to_owned(),
                exit: describe_exit(session.exit_code),
                head: heads.render(),
            });
        }
        // ssh connected, so the script died on the host: an OOM-killed cargo, a
        // full disk, a shell that lost its terminal. Blaming key authentication
        // here sends the reader in the wrong direction, so report the status that
        // actually came back; `Heads` keeps the stderr tail that explains it.
        return Err(GateError::RemoteAborted {
            host: host.to_owned(),
            detail: format!(
                "the gate script died before its summary ({})",
                describe_exit(session.exit_code)
            ),
            head: heads.render(),
        });
    }

    let mut results = Vec::with_capacity(plan.commands.len());
    for (index, command) in plan.commands.iter().enumerate() {
        // The first marker for an index wins: the script writes exactly one per
        // command, so a repeat can only come from output echoing a marker line.
        let reported = markers.iter().find_map(|marker| match marker {
            RemoteMarker::GateExit {
                index: reported_index,
                exit_code,
                elapsed_secs,
            } if *reported_index == index => {
                Some((Some(*exit_code), Duration::from_secs(*elapsed_secs)))
            }
            _ => None,
        });
        // A command that never reported a marker is not a pass: its exit code
        // stays absent, which `all_passed` treats as a failure.
        let (exit_code, elapsed) = reported.unwrap_or((None, Duration::ZERO));
        results.push(GateResult {
            platform: plan.platform,
            label: command.label.clone(),
            command: command.render(),
            exit_code,
            elapsed,
        });
    }

    Ok(results)
}

fn push(root: &Path, remote: &str, names: &RemoteNames, output: &Output) -> Result<(), GateError> {
    let refspec = format!("HEAD:refs/heads/{}", names.branch);
    let arguments = vec![
        "push".to_owned(),
        "--quiet".to_owned(),
        "--force".to_owned(),
        remote.to_owned(),
        refspec,
    ];
    let pushed = stream_command("git", &arguments, root, Stdio::null(), output)?;
    if pushed.exit_code == Some(0) {
        return Ok(());
    }

    Err(GateError::PushFailed {
        remote: remote.to_owned(),
        branch: names.branch.clone(),
        exit: describe_exit(pushed.exit_code),
    })
}

fn write_script(script: &str) -> Result<PathBuf, GateError> {
    let path = std::env::temp_dir().join(format!(
        "kytrix-gates-{}-{}.sh",
        std::process::id(),
        super::unix_seconds()
    ));
    fs::write(&path, script.as_bytes()).map_err(|source| GateError::ScriptWrite {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// Builds the bash script a host runs.
///
/// Every argument is single-quoted: crate names are validated against
/// `[a-z0-9_-]` and the branch/directory names against `[a-z0-9._-]` when they
/// are parsed, so no argument can carry a quote or a `$` into the script.
pub(super) fn build_script(
    plan: &PlatformPlan,
    lock_timeout_secs: u64,
    names: &RemoteNames,
) -> String {
    let platform = plan.platform.name();
    let step = plan.step.name();
    let mut lines: Vec<String> = Vec::with_capacity(plan.commands.len() + 48);

    lines.push("set -u".to_owned());
    lines.push(format!(
        "# kytrix gates: {platform} {step} (generated by `cargo xtask gates`)"
    ));
    lines.push(format!("echo '== gates: {platform} {step} ='"));
    lines.push(format!("LOCKDIR={LOCK_DIRECTORY}"));
    lines.push("tries=0".to_owned());
    lines.push("while ! mkdir \"$LOCKDIR\" 2>/dev/null; do".to_owned());
    lines.push("  tries=$((tries+1))".to_owned());
    lines.push(format!("  if [ \"$tries\" -gt {lock_timeout_secs} ]; then"));
    lines.push(format!(
        "    echo \"KYTRIX_LOCK_TIMEOUT={lock_timeout_secs}\""
    ));
    lines.push("    exit 9".to_owned());
    lines.push("  fi".to_owned());
    lines.push("  sleep 1".to_owned());
    lines.push("done".to_owned());
    lines.push("trap 'rmdir \"$LOCKDIR\" 2>/dev/null' EXIT".to_owned());
    lines.push(
        "export PATH=\"$HOME/.cargo/bin:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin\""
            .to_owned(),
    );
    lines.push("export CARGO_TERM_COLOR=never".to_owned());
    lines.push("export CARGO_TARGET_DIR=\"$HOME/kytrix-review-target\"".to_owned());
    lines.push(format!("BRANCH='{}'", names.branch));
    lines.push(format!("SRC=\"$HOME/{SOURCE_PREFIX}{}\"", names.directory));
    lines.push("if [ ! -d \"$SRC/.git\" ]; then".to_owned());
    lines.push("  git clone -q \"$HOME/kytrix-p1.git\" \"$SRC\" || { echo \"KYTRIX_ABORT=clone\"; exit 2; }".to_owned());
    lines.push("fi".to_owned());
    lines.push("cd \"$SRC\" || { echo \"KYTRIX_ABORT=cd\"; exit 3; }".to_owned());
    lines.push(
        "git fetch -q origin \"$BRANCH\" || { echo \"KYTRIX_ABORT=fetch\"; exit 4; }".to_owned(),
    );
    lines.push("git checkout -q -f -B \"$BRANCH\" \"origin/$BRANCH\" || { echo \"KYTRIX_ABORT=checkout\"; exit 5; }".to_owned());
    lines.push(
        "git reset -q --hard \"origin/$BRANCH\" || { echo \"KYTRIX_ABORT=reset\"; exit 6; }"
            .to_owned(),
    );
    lines.push("git clean -qfdx >/dev/null 2>&1 || true".to_owned());
    // Build-script fingerprints in the shared target directory are mtime based,
    // so a checkout that moves backwards can be judged fresh and run the previous
    // commit's build script. Touching build.rs forces it to be rebuilt from the
    // source that is actually being gated.
    lines.push("if [ -f crates/ui_core/build.rs ]; then".to_owned());
    lines.push("  touch crates/ui_core/build.rs".to_owned());
    lines.push("fi".to_owned());
    lines.push("echo \"== host: $(uname -srm) ==\"".to_owned());
    lines.push("echo \"== source: $SRC ==\"".to_owned());
    lines.push(
        "echo \"== head: $(git rev-parse --short HEAD) $(git log -1 --format=%s) ==\"".to_owned(),
    );
    lines.push("echo \"== target: $CARGO_TARGET_DIR ==\"".to_owned());
    lines.push("echo \"== lock: acquired ($LOCKDIR) ==\"".to_owned());
    lines.push("overall=0".to_owned());
    lines.push("gate() {".to_owned());
    lines.push("  gate_index=\"$1\"".to_owned());
    lines.push("  shift".to_owned());
    lines.push("  echo \"---- CMD [$gate_index]: cargo $*\"".to_owned());
    lines.push("  gate_started=$(date +%s)".to_owned());
    lines.push("  cargo \"$@\"".to_owned());
    lines.push("  gate_code=$?".to_owned());
    lines.push("  gate_ended=$(date +%s)".to_owned());
    lines.push("  gate_elapsed=$((gate_ended-gate_started))".to_owned());
    lines.push("  echo \"---- EXIT [$gate_index]=$gate_code elapsed=${gate_elapsed}s\"".to_owned());
    lines.push("  echo \"KYTRIX_GATE_EXIT=$gate_code $gate_index $gate_elapsed\"".to_owned());
    lines.push("  if [ \"$gate_code\" -ne 0 ]; then".to_owned());
    lines.push("    overall=1".to_owned());
    lines.push("  fi".to_owned());
    lines.push("}".to_owned());
    for (index, command) in plan.commands.iter().enumerate() {
        let mut words = vec![format!("gate {index}")];
        for argument in &command.args {
            words.push(format!("'{argument}'"));
        }
        lines.push(words.join(" "));
    }
    lines.push("echo \"KYTRIX_TOTAL_EXIT=$overall\"".to_owned());
    lines.push("exit \"$overall\"".to_owned());
    lines.push(String::new());

    lines.join("\n")
}

/// A machine-readable line the remote script writes, so the local run can
/// attribute exit codes without parsing `cargo` output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RemoteMarker {
    /// `KYTRIX_GATE_EXIT=<exit code> <command index> <elapsed seconds>`
    GateExit {
        index: usize,
        exit_code: i32,
        elapsed_secs: u64,
    },
    /// `KYTRIX_TOTAL_EXIT=<0|1>`: the script reached its summary.
    TotalExit(i32),
    /// `KYTRIX_ABORT=<stage>`: the scratch checkout could not be prepared.
    Abort(String),
    /// `KYTRIX_LOCK_TIMEOUT=<seconds>`: another run holds the shared host lock.
    LockTimeout(u64),
}

const MARKER_PREFIX: &str = "KYTRIX_";

pub(super) fn parse_marker(line: &str) -> Option<RemoteMarker> {
    let mut fields = line.split_whitespace();
    let (name, value) = fields
        .next()?
        .strip_prefix(MARKER_PREFIX)?
        .split_once('=')?;
    match name {
        "GATE_EXIT" => Some(RemoteMarker::GateExit {
            exit_code: value.parse().ok()?,
            index: fields.next()?.parse().ok()?,
            elapsed_secs: fields.next()?.parse().ok()?,
        }),
        "TOTAL_EXIT" => Some(RemoteMarker::TotalExit(value.parse().ok()?)),
        "ABORT" => Some(RemoteMarker::Abort(value.to_owned())),
        "LOCK_TIMEOUT" => Some(RemoteMarker::LockTimeout(value.parse().ok()?)),
        _ => None,
    }
}
