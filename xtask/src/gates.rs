//! `cargo xtask gates`: the typed three-platform gate runner.
//!
//! The flow is the one `.review/p1-gates.ps1` (local Windows) and
//! `.review/p1-remote.ps1` (macOS/Linux over ssh) established by hand: the same
//! commands, the same remote scratch-clone / shared-target protocol, and the
//! same host lock, so a migration gate run can never race a reviewer's run on
//! the same host. What this adds is that the commands are a plan of typed
//! records: every command is echoed before it runs, every command reports its
//! exit code and elapsed time, and any failure makes the process exit non-zero
//! with the failed commands named at the end.

mod remote;

use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, MutexGuard},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use remote::RemoteMarker;
use thiserror::Error;

/// The crate whose gates carry the extra feature sets and test suites.
const UI_CORE: &str = "ui_core";

/// The product crate whose layer gates include a release build (plan §2.4).
const KYTRIX: &str = "kytrix";

/// The gate runner's own crate: like `ui_core` it does not link `gpui-pre`.
const XTASK: &str = "xtask";

/// The build-script switch the local Windows gates set while the migration is in
/// flight: `ui_core`'s process manifest and `gpui-pre`'s manifest both carry
/// `RT_MANIFEST` ID 1, so an exe that links both dies with
/// `CVT1100: duplicate resource. type:MANIFEST, name:1`. A scope that stays
/// inside the migration engine (`ui_core`, `xtask`) keeps the manifest link
/// under test; every other crate still pulls `gpui-pre` in.
///
/// Transitional: removed together with `gpui-kit` in P5
/// (`.docs/gui-migration/spec-deltas/build-transition.md`).
const TRANSITION_MANIFEST_ENV: &str = "KYTRIX_UI_CORE_MANIFEST";

/// `rustfmt` and `clippy` live in the nightly toolchain of this workspace.
const NIGHTLY: &str = "+nightly";

/// `ui_core`'s test run with the features the P1/P2 baselines used.
const UI_CORE_TEST_FEATURES: &str = "hud,test-support";

/// The `snapshot` and `gui_smoke` suites run with the P1/P2 scripts' feature set
/// (`test-support` alone), so their golden/GUI evidence stays comparable with the
/// historical `.review/p1-*` logs.
const UI_CORE_TEST_SUPPORT: &str = "test-support";

/// How long a remote run waits for the shared host lock by default.
const DEFAULT_LOCK_TIMEOUT_SECS: u64 = 1800;

/// Output lines kept per stream for failure diagnostics; the log keeps it all.
const HEAD_LINES: usize = 40;

/// Trailing stderr lines kept for failures that explain themselves at the end
/// (a linker error, a write that hit ENOSPC, a process killed mid-build).
const TAIL_LINES: usize = 20;

/// Which host a gate command runs on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Platform {
    Win,
    Mac,
    Linux,
}

impl Platform {
    fn from_arg(value: &str) -> Option<Self> {
        match value {
            "win" => Some(Self::Win),
            "mac" => Some(Self::Mac),
            "linux" => Some(Self::Linux),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Win => "win",
            Self::Mac => "mac",
            Self::Linux => "linux",
        }
    }

    /// The `~/.ssh/config` alias; the local Windows host has none.
    fn ssh_host(self) -> Option<&'static str> {
        match self {
            Self::Win => None,
            Self::Mac => Some("my_macos"),
            Self::Linux => Some("my_ubuntu"),
        }
    }

    /// The git remote that mirrors this repository as a bare repo on the host.
    fn git_remote(self) -> Option<&'static str> {
        match self {
            Self::Win => None,
            Self::Mac => Some("mac"),
            Self::Linux => Some("linux"),
        }
    }
}

/// Which gate set a run executes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    Fast,
    Quick,
    Layer,
    /// Release builds of the scope (plan §2.4, `.review/p1-gates.ps1`'s `release`).
    Release,
    /// The `ui_core` perf bench, built but not run (`.review/p1-remote.ps1`'s `no-run-bench`).
    Bench,
    /// The `ui_core` acceptance harness in its headless CPU mode
    /// (`.review/p1-remote.ps1`'s `acceptance-cpu`).
    Accept,
    /// Every step the selected hosts can run: `quick` plus `layer`.
    All,
}

impl Step {
    fn from_arg(value: &str) -> Option<Self> {
        match value {
            "fast" => Some(Self::Fast),
            "quick" => Some(Self::Quick),
            "layer" => Some(Self::Layer),
            "release" => Some(Self::Release),
            "bench" => Some(Self::Bench),
            "accept" => Some(Self::Accept),
            "all" => Some(Self::All),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Quick => "quick",
            Self::Layer => "layer",
            Self::Release => "release",
            Self::Bench => "bench",
            Self::Accept => "accept",
            Self::All => "all",
        }
    }
}

/// The packages a run covers.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Scope {
    /// Every workspace member (`--workspace`, or `--all` for `cargo fmt`).
    Workspace,
    Crates(Vec<String>),
}

impl Scope {
    fn from_arg(value: &str) -> Result<Self, GateError> {
        if value == "all" {
            return Ok(Self::Workspace);
        }

        let mut crates: Vec<String> = Vec::new();
        for name in value.split(',') {
            let name = name.trim();
            if name.is_empty() {
                return Err(GateError::EmptyScope);
            }
            let valid = name.chars().all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || character == '-'
                    || character == '_'
            });
            if !valid {
                return Err(GateError::InvalidCrateName(name.to_owned()));
            }
            if !crates.iter().any(|existing| existing == name) {
                crates.push(name.to_owned());
            }
        }

        Ok(Self::Crates(crates))
    }

    fn render(&self) -> String {
        match self {
            Self::Workspace => "all".to_owned(),
            Self::Crates(crates) => crates.join(","),
        }
    }

    fn includes(&self, name: &str) -> bool {
        match self {
            Self::Workspace => true,
            Self::Crates(crates) => crates.iter().any(|crate_name| crate_name == name),
        }
    }

    /// The packages whose gates run with default features: everything in scope
    /// except `ui_core`, whose gates run with the P1/P2 feature sets instead.
    /// `None` when the scope is `ui_core` alone.
    fn default_feature_packages(&self) -> Option<PackageSelection> {
        match self {
            Self::Workspace => Some(PackageSelection::WorkspaceExceptUiCore),
            Self::Crates(crates) => {
                let names = crates
                    .iter()
                    .filter(|name| name.as_str() != UI_CORE)
                    .cloned()
                    .collect::<Vec<_>>();
                if names.is_empty() {
                    None
                } else {
                    Some(PackageSelection::Crates(names))
                }
            }
        }
    }
}

/// How a cargo command names the packages it covers.
#[derive(Clone, Debug, Eq, PartialEq)]
enum PackageSelection {
    WorkspaceExceptUiCore,
    Crates(Vec<String>),
}

impl PackageSelection {
    fn push_args(&self, args: &mut Vec<String>) {
        match self {
            // `ui_core` has its own command with the feature sets, so the
            // default-feature pass excludes it instead of testing it twice.
            Self::WorkspaceExceptUiCore => {
                args.push("--workspace".to_owned());
                args.push("--exclude".to_owned());
                args.push(UI_CORE.to_owned());
            }
            Self::Crates(crates) => push_package_args(crates, args),
        }
    }
}

/// One `cargo` invocation of a platform's gate plan.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CargoCommand {
    /// Short name used by the console and the remote markers.
    label: String,
    /// Arguments after `cargo`; `+nightly` comes first for fmt and clippy.
    args: Vec<String>,
}

impl CargoCommand {
    fn render(&self) -> String {
        format!("cargo {}", self.args.join(" "))
    }
}

/// The gate commands of one platform, in execution order.
struct PlatformPlan {
    platform: Platform,
    step: Step,
    commands: Vec<CargoCommand>,
}

/// The branch and scratch directory a remote run uses on every host.
struct RemoteNames {
    branch: String,
    directory: String,
}

/// What `--platform`, `--scope`, `--step` and friends resolved to.
#[derive(Debug)]
pub struct GatesConfig {
    platforms: Vec<Platform>,
    scope: Scope,
    step: Step,
    log: Option<PathBuf>,
    remote_dir: Option<String>,
    lock_timeout_secs: u64,
}

/// Parses the arguments of `cargo xtask gates`.
pub fn parse_args(mut args: impl Iterator<Item = String>) -> Result<GatesConfig, GateError> {
    let mut platforms: Option<Vec<Platform>> = None;
    let mut scope: Option<Scope> = None;
    let mut step: Option<Step> = None;
    let mut log: Option<PathBuf> = None;
    let mut remote_dir: Option<String> = None;
    let mut lock_timeout_secs = DEFAULT_LOCK_TIMEOUT_SECS;
    let mut seen = [false; 6];

    while let Some(argument) = args.next() {
        let (name, inline) = match argument.split_once('=') {
            Some((name, value)) => (name, Some(value.to_owned())),
            None => (argument.as_str(), None),
        };
        let option = match name {
            "--platform" => Some(0),
            "--scope" => Some(1),
            "--step" => Some(2),
            "--log" => Some(3),
            "--remote-dir" => Some(4),
            "--lock-timeout-secs" => Some(5),
            _ => None,
        };
        // Last-wins would silently drop half of a mistyped invocation, so a
        // repeated option is a usage error instead.
        if let Some(index) = option
            && std::mem::replace(&mut seen[index], true)
        {
            return Err(GateError::DuplicateOption(name.to_owned()));
        }
        match name {
            "--platform" => {
                let value = inline
                    .or_else(|| args.next())
                    .ok_or(GateError::MissingValue("--platform"))?;
                platforms = Some(match value.as_str() {
                    "all" => vec![Platform::Win, Platform::Mac, Platform::Linux],
                    other => vec![
                        Platform::from_arg(other)
                            .ok_or_else(|| GateError::InvalidPlatform(other.to_owned()))?,
                    ],
                });
            }
            "--scope" => {
                let value = inline
                    .or_else(|| args.next())
                    .ok_or(GateError::MissingValue("--scope"))?;
                scope = Some(Scope::from_arg(&value)?);
            }
            "--step" => {
                let value = inline
                    .or_else(|| args.next())
                    .ok_or(GateError::MissingValue("--step"))?;
                step = Some(
                    Step::from_arg(&value).ok_or_else(|| GateError::InvalidStep(value.clone()))?,
                );
            }
            "--log" => {
                let value = inline
                    .or_else(|| args.next())
                    .ok_or(GateError::MissingValue("--log"))?;
                log = Some(PathBuf::from(value));
            }
            "--remote-dir" => {
                let value = inline
                    .or_else(|| args.next())
                    .ok_or(GateError::MissingValue("--remote-dir"))?;
                remote_dir = Some(validate_remote_directory(&value)?);
            }
            "--lock-timeout-secs" => {
                let value = inline
                    .or_else(|| args.next())
                    .ok_or(GateError::MissingValue("--lock-timeout-secs"))?;
                lock_timeout_secs = value
                    .parse::<u64>()
                    .ok()
                    .filter(|seconds| *seconds > 0)
                    .ok_or_else(|| GateError::InvalidLockTimeout(value.clone()))?;
            }
            other => return Err(GateError::UnknownOption(other.to_owned())),
        }
    }

    Ok(GatesConfig {
        platforms: platforms.ok_or(GateError::MissingOption("--platform"))?,
        scope: scope.ok_or(GateError::MissingOption("--scope"))?,
        step: step.ok_or(GateError::MissingOption("--step"))?,
        log,
        remote_dir,
        lock_timeout_secs,
    })
}

/// Runs the gate plan and reports whether every command passed.
///
/// A failed command does not stop the remaining commands on its host: the whole
/// plan runs so that one invocation shows every failure, and the caller exits
/// non-zero once the summary has named them.
pub fn run(root: &Path, config: &GatesConfig) -> Result<bool, GateError> {
    let plans = build_plan(config)?;
    let log_path = config
        .log
        .clone()
        .unwrap_or_else(|| default_log_path(root, config));
    if let Some(parent) = log_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| GateError::LogDirectory {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    // The log file is this run's evidence, so an explicit `--log` path is
    // truncated: appending would mix a previous run's failures into this one's.
    let log = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
        .map_err(|source| GateError::LogOpen {
            path: log_path.clone(),
            source,
        })?;
    let output = Output::new(log);
    let started = Instant::now();

    output.line(StreamKind::Stdout, "== kytrix gates ==");
    output.line(
        StreamKind::Stdout,
        &format!("   platform: {}", platform_label(&config.platforms)),
    );
    output.line(
        StreamKind::Stdout,
        &format!("   step:     {}", config.step.name()),
    );
    output.line(
        StreamKind::Stdout,
        &format!("   scope:    {}", config.scope.render()),
    );
    output.line(
        StreamKind::Stdout,
        &format!("   checkout: {}", root.display()),
    );
    output.line(
        StreamKind::Stdout,
        &format!("   log:      {}", log_path.display()),
    );

    let remote_names = if plans.iter().any(|plan| plan.platform.ssh_host().is_some()) {
        Some(remote_names(config, root)?)
    } else {
        None
    };

    for plan in &plans {
        let host = match plan.platform.ssh_host() {
            Some(host) => format!(" (ssh {host})"),
            None => String::new(),
        };
        output.line(
            StreamKind::Stdout,
            &format!("== plan [{}]{host} ==", plan.platform.name()),
        );
        for (index, command) in plan.commands.iter().enumerate() {
            output.line(
                StreamKind::Stdout,
                &format!("  {}. {}", index + 1, command.render()),
            );
        }
    }

    let mut results: Vec<GateResult> = Vec::with_capacity(plans.len());
    for plan in &plans {
        let platform_results = match plan.platform {
            Platform::Win => run_local_plan(root, plan, &config.scope, &output)?,
            Platform::Mac | Platform::Linux => {
                let names = remote_names
                    .as_ref()
                    .ok_or(GateError::NotRemote(plan.platform.name()))?;
                remote::run(root, plan, config, names, &output)?
            }
        };
        for result in &platform_results {
            print_result(&output, result);
        }
        results.extend(platform_results);
    }

    print_summary(&output, &results, started.elapsed(), &log_path);

    if let Some(source) = output.take_log_error() {
        return Err(GateError::LogWrite { source });
    }

    Ok(all_passed(&results))
}

/// Whether every command of a run passed.
///
/// This is the single place the exit-code contract is decided: the process exits
/// non-zero as soon as any command reports anything other than exit 0, including
/// a command that produced no exit status at all. An empty result set is not a
/// pass either: a run that executed nothing has gated nothing.
fn all_passed(results: &[GateResult]) -> bool {
    !results.is_empty() && results.iter().all(|result| result.exit_code == Some(0))
}

/// Whether the local Windows run must skip `ui_core`'s manifest link.
///
/// The transition flag exists because an exe linking both `ui_core` and
/// `gpui-pre` gets two `RT_MANIFEST` ID 1 resources (`CVT1100`). Only `ui_core`
/// and this runner are free of `gpui-pre`; a workspace-style scope certainly is
/// not, because it includes the product crates that still do.
fn skips_ui_core_manifest(scope: &Scope) -> bool {
    match scope {
        Scope::Workspace => true,
        Scope::Crates(crates) => crates
            .iter()
            .any(|name| name.as_str() != UI_CORE && name.as_str() != XTASK),
    }
}

/// Executes the local Windows plan.
fn run_local_plan(
    root: &Path,
    plan: &PlatformPlan,
    scope: &Scope,
    output: &Output,
) -> Result<Vec<GateResult>, GateError> {
    let transition_env: &[(&str, &str)] = if skips_ui_core_manifest(scope) {
        output.line(
            StreamKind::Stdout,
            &format!(
                "   transition: {TRANSITION_MANIFEST_ENV}=0（过渡期跳过 ui_core 进程清单：与 gpui-pre manifest 冲突，P5 后移除）"
            ),
        );
        &[(TRANSITION_MANIFEST_ENV, "0")]
    } else {
        &[]
    };
    let mut results = Vec::with_capacity(plan.commands.len());
    for (index, command) in plan.commands.iter().enumerate() {
        output.line(
            StreamKind::Stdout,
            &format!(
                "---- [{}] {}/{} {}",
                plan.platform.name(),
                index + 1,
                plan.commands.len(),
                command.render()
            ),
        );
        let process = stream_command_with_env(
            "cargo",
            &command.args,
            root,
            Stdio::null(),
            output,
            transition_env,
        )?;
        results.push(GateResult {
            platform: plan.platform,
            label: command.label.clone(),
            command: command.render(),
            exit_code: process.exit_code,
            elapsed: process.elapsed,
        });
    }
    Ok(results)
}

/// One finished gate command.
#[derive(Debug)]
struct GateResult {
    platform: Platform,
    label: String,
    command: String,
    exit_code: Option<i32>,
    elapsed: Duration,
}

/// Whether a step has anything to run on `platform`.
fn step_runs_on(step: Step, platform: Platform) -> bool {
    match step {
        Step::Fast => platform == Platform::Win,
        Step::Quick => platform != Platform::Win,
        Step::Layer | Step::Release | Step::Bench | Step::Accept | Step::All => true,
    }
}

/// Builds the plan for the selected platforms.
///
/// `--platform` selects hosts and the step decides which of them it runs on, so
/// `--platform all --step quick` is the two remote hosts, `--platform all --step
/// all` is both remote checks plus the full set everywhere, and `--platform all
/// --step layer` is all three hosts. A selection that leaves the step nothing to
/// run is an error instead of a silent no-op.
fn build_plan(config: &GatesConfig) -> Result<Vec<PlatformPlan>, GateError> {
    let platforms = config
        .platforms
        .iter()
        .copied()
        .filter(|platform| step_runs_on(config.step, *platform))
        .collect::<Vec<_>>();
    match config.step {
        Step::Fast if platforms.is_empty() => return Err(GateError::FastNeedsLocalWindows),
        Step::Quick if platforms.is_empty() => return Err(GateError::QuickNeedsRemote),
        _ => {}
    }
    // `bench` and `accept` drive `ui_core`'s perf bench and acceptance harness;
    // there is nothing for them to run on another crate, so a scope without
    // `ui_core` is rejected instead of producing an empty plan.
    if matches!(config.step, Step::Bench | Step::Accept) && !config.scope.includes(UI_CORE) {
        return Err(GateError::StepNeedsUiCore(config.step.name()));
    }

    Ok(platforms
        .into_iter()
        .map(|platform| PlatformPlan {
            platform,
            step: config.step,
            commands: commands_for(config.step, platform, &config.scope),
        })
        .collect())
}

fn commands_for(step: Step, platform: Platform, scope: &Scope) -> Vec<CargoCommand> {
    let mut commands: Vec<CargoCommand> = Vec::with_capacity(9);

    // `quick` is the cheap remote compile check, so it goes first: a broken
    // compile fails the run before the full test set is spent on it.
    if matches!(step, Step::Quick | Step::All) && platform != Platform::Win {
        commands.extend(check_commands(scope));
    }
    match step {
        Step::Quick => {}
        Step::Fast => commands.extend(gate_commands(scope, false)),
        Step::Layer | Step::All => {
            // `layer` runs the ignored `gui_smoke` suite on every platform: it is
            // offscreen on all three (Windows WARP/D3D12, macOS Metal, Linux
            // Vulkan via lavapipe), and the P1/P2 baselines ran it on mac and
            // linux as well as on Windows.
            commands.extend(gate_commands(scope, true));
            // Plan §2.4: a layer run that involves the product crate also checks
            // that it still builds in release mode.
            if scope.includes(KYTRIX) {
                commands.push(CargoCommand {
                    label: format!("release({KYTRIX})"),
                    args: release_args(&PackageSelection::Crates(vec![KYTRIX.to_owned()]), false),
                });
            }
        }
        Step::Release => commands.extend(release_commands(scope)),
        Step::Bench => commands.extend(bench_commands()),
        Step::Accept => commands.extend(accept_commands()),
    }

    commands
}

fn check_commands(scope: &Scope) -> Vec<CargoCommand> {
    let mut commands = Vec::with_capacity(2);
    if let Some(packages) = scope.default_feature_packages() {
        commands.push(CargoCommand {
            label: "check".to_owned(),
            args: check_args(&packages, false),
        });
    }
    if scope.includes(UI_CORE) {
        // `--all-features` is not optional here: `ui_core`'s unit tests use
        // `test-support` APIs (`Window::instance_buffer`, the fixture seams), so
        // a default-feature `--all-targets` check cannot compile on any platform.
        // `.review/p1-remote.ps1`'s check step passed the same flag.
        let ui_core = PackageSelection::Crates(vec![UI_CORE.to_owned()]);
        commands.push(CargoCommand {
            label: "check(ui_core,all-features)".to_owned(),
            args: check_args(&ui_core, true),
        });
    }
    commands
}

/// The fmt/clippy/test/snapshot set that `fast` and `layer` share on every host.
fn gate_commands(scope: &Scope, gui_smoke: bool) -> Vec<CargoCommand> {
    let mut commands = vec![CargoCommand {
        label: "fmt".to_owned(),
        args: fmt_args(scope),
    }];

    if let Some(packages) = scope.default_feature_packages() {
        commands.push(CargoCommand {
            label: "clippy".to_owned(),
            args: clippy_args(&packages, false),
        });
        commands.push(CargoCommand {
            label: "test".to_owned(),
            args: test_args(&packages, None),
        });
    }

    if scope.includes(UI_CORE) {
        let ui_core = PackageSelection::Crates(vec![UI_CORE.to_owned()]);
        commands.push(CargoCommand {
            label: "clippy(ui_core,all-features)".to_owned(),
            args: clippy_args(&ui_core, true),
        });
        commands.push(CargoCommand {
            label: format!("test(ui_core,{UI_CORE_TEST_FEATURES})"),
            args: test_args(&ui_core, Some(UI_CORE_TEST_FEATURES)),
        });
        let mut snapshot = test_args(&ui_core, Some(UI_CORE_TEST_SUPPORT));
        snapshot.push("--test".to_owned());
        snapshot.push("snapshot".to_owned());
        commands.push(CargoCommand {
            label: "snapshot".to_owned(),
            args: snapshot,
        });

        if gui_smoke {
            let mut gui_smoke = test_args(&ui_core, Some(UI_CORE_TEST_SUPPORT));
            for argument in ["--test", "gui_smoke", "--", "--ignored", "--nocapture"] {
                gui_smoke.push(argument.to_owned());
            }
            commands.push(CargoCommand {
                label: "gui-smoke".to_owned(),
                args: gui_smoke,
            });
        }
    }

    commands
}

/// The `--step release` set: every package of the scope in release mode.
fn release_commands(scope: &Scope) -> Vec<CargoCommand> {
    let mut commands = Vec::with_capacity(2);
    if let Some(packages) = scope.default_feature_packages() {
        commands.push(CargoCommand {
            label: "release".to_owned(),
            args: release_args(&packages, false),
        });
    }
    if scope.includes(UI_CORE) {
        // `.review/p1-gates.ps1`'s `release` step builds `ui_core` with
        // `--all-features`, so the released feature surface is the full one.
        let ui_core = PackageSelection::Crates(vec![UI_CORE.to_owned()]);
        commands.push(CargoCommand {
            label: "release(ui_core,all-features)".to_owned(),
            args: release_args(&ui_core, true),
        });
    }
    commands
}

/// The `--step bench` set: the perf bench the P1/P2 scripts built but did not run.
fn bench_commands() -> Vec<CargoCommand> {
    vec![CargoCommand {
        label: "bench(ui_core,perf)".to_owned(),
        args: bench_args(),
    }]
}

/// The `--step accept` set: the headless CPU acceptance harness.
fn accept_commands() -> Vec<CargoCommand> {
    vec![CargoCommand {
        label: "accept(ui_core,cpu)".to_owned(),
        args: vec![
            "run".to_owned(),
            "--release".to_owned(),
            "-p".to_owned(),
            UI_CORE.to_owned(),
            "--example".to_owned(),
            "acceptance".to_owned(),
            "--".to_owned(),
            "cpu".to_owned(),
        ],
    }]
}

fn fmt_args(scope: &Scope) -> Vec<String> {
    let mut args = vec![NIGHTLY.to_owned(), "fmt".to_owned(), "--check".to_owned()];
    match scope {
        // `cargo fmt` spells the whole-workspace selection `--all`.
        Scope::Workspace => args.push("--all".to_owned()),
        Scope::Crates(crates) => push_package_args(crates, &mut args),
    }
    args
}

fn check_args(packages: &PackageSelection, all_features: bool) -> Vec<String> {
    let mut args = vec!["check".to_owned()];
    packages.push_args(&mut args);
    args.push("--all-targets".to_owned());
    if all_features {
        args.push("--all-features".to_owned());
    }
    args
}

fn clippy_args(packages: &PackageSelection, all_features: bool) -> Vec<String> {
    let mut args = vec![NIGHTLY.to_owned(), "clippy".to_owned()];
    packages.push_args(&mut args);
    args.push("--all-targets".to_owned());
    // `--all-features` is a superset of `hud,test-support`; it is what the P1/P2
    // baselines ran for `ui_core`, so `ui_core` keeps that coverage here.
    if all_features {
        args.push("--all-features".to_owned());
    }
    for argument in ["--", "-D", "warnings"] {
        args.push(argument.to_owned());
    }
    args
}

fn release_args(packages: &PackageSelection, all_features: bool) -> Vec<String> {
    let mut args = vec!["build".to_owned()];
    packages.push_args(&mut args);
    args.push("--release".to_owned());
    if all_features {
        args.push("--all-features".to_owned());
    }
    args
}

fn bench_args() -> Vec<String> {
    let mut args = vec!["bench".to_owned()];
    let ui_core = PackageSelection::Crates(vec![UI_CORE.to_owned()]);
    ui_core.push_args(&mut args);
    args.push("--features".to_owned());
    args.push(UI_CORE_TEST_SUPPORT.to_owned());
    for argument in ["--bench", "perf", "--no-run"] {
        args.push(argument.to_owned());
    }
    args
}

fn test_args(packages: &PackageSelection, features: Option<&str>) -> Vec<String> {
    let mut args = vec!["test".to_owned()];
    packages.push_args(&mut args);
    if let Some(features) = features {
        args.push("--features".to_owned());
        args.push(features.to_owned());
    }
    args
}

fn push_package_args(crates: &[String], args: &mut Vec<String>) {
    for name in crates {
        args.push("-p".to_owned());
        args.push(name.clone());
    }
}

fn platform_label(platforms: &[Platform]) -> &'static str {
    match platforms {
        [Platform::Win] => "win",
        [Platform::Mac] => "mac",
        [Platform::Linux] => "linux",
        _ => "all",
    }
}

fn validate_remote_directory(value: &str) -> Result<String, GateError> {
    let starts_well = value
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_lowercase() || character.is_ascii_digit());
    let valid = starts_well
        && value.len() <= 64
        && value.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || character == '.'
                || character == '-'
                || character == '_'
        });
    if valid {
        Ok(value.to_owned())
    } else {
        Err(GateError::InvalidRemoteDirectory(value.to_owned()))
    }
}

/// Resolves the scratch branch/directory name shared by the remote hosts.
///
/// The branch always carries the `mig-` prefix, so a run started from `main` or a
/// P1/P2 branch (`p1-a`, `main`) cannot force-push over that branch in the shared
/// bare mirror; the scratch directory keeps the `kytrix-mig-src-` prefix instead.
fn remote_names(config: &GatesConfig, root: &Path) -> Result<RemoteNames, GateError> {
    let directory = match &config.remote_dir {
        Some(name) => name.clone(),
        None => {
            let branch = current_branch(root)?;
            sanitize_branch(&branch)
                .ok_or_else(|| GateError::RemoteNameFromBranch(branch.clone()))?
        }
    };
    let branch = if directory.starts_with("mig-") {
        directory.clone()
    } else {
        format!("mig-{directory}")
    };
    Ok(RemoteNames { branch, directory })
}

fn current_branch(root: &Path) -> Result<String, GateError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .map_err(GateError::Git)?;
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || branch.is_empty() || branch == "HEAD" {
        return Err(GateError::DetachedHead);
    }
    Ok(branch)
}

/// Lowercases a branch name and replaces everything outside `a-z0-9._-` with a
/// single `-`, because the name becomes both a remote branch and a directory.
fn sanitize_branch(branch: &str) -> Option<String> {
    let mut name = String::with_capacity(branch.len());
    let mut after_dash = true;
    for character in branch.chars() {
        let character = character.to_ascii_lowercase();
        let allowed = character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || character == '.'
            || character == '_';
        if allowed {
            name.push(character);
            after_dash = false;
        } else if !after_dash {
            name.push('-');
            after_dash = true;
        }
    }
    let name = name.trim_end_matches('-');
    (!name.is_empty() && name.len() <= 64).then(|| name.to_owned())
}

/// The main checkout's `.review` directory, which is where gate evidence lives.
///
/// Agent worktrees sit at `<main>/.agents/worktrees/<name>` and are real checkouts
/// of this repository, but the evidence directory belongs to the repository as a
/// whole and is shared by every worktree, so the run has to look through them.
fn review_directory(root: &Path) -> PathBuf {
    let worktrees = root.parent();
    let agents = worktrees.and_then(Path::parent);
    if let (Some(worktrees), Some(agents)) = (worktrees, agents)
        && worktrees.file_name() == Some("worktrees".as_ref())
        && agents.file_name() == Some(".agents".as_ref())
        && let Some(main) = agents.parent()
    {
        return main.join(".review");
    }
    root.join(".review")
}

fn default_log_path(root: &Path, config: &GatesConfig) -> PathBuf {
    review_directory(root).join(format!(
        "mig-gates-{}-{}.log",
        unix_seconds(),
        platform_label(&config.platforms)
    ))
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

fn describe_exit(exit_code: Option<i32>) -> String {
    match exit_code {
        Some(code) => format!("exit {code}"),
        None => "no exit status".to_owned(),
    }
}

fn format_elapsed(elapsed: Duration) -> String {
    format!("{:.1}s", elapsed.as_secs_f64())
}

fn print_result(output: &Output, result: &GateResult) {
    output.line(
        StreamKind::Stdout,
        &format!(
            "---- [{}] {} {} {} ({})",
            result.platform.name(),
            result.label,
            if result.exit_code == Some(0) {
                "PASS"
            } else {
                "FAIL"
            },
            format_elapsed(result.elapsed),
            describe_exit(result.exit_code)
        ),
    );
}

fn print_summary(output: &Output, results: &[GateResult], elapsed: Duration, log_path: &Path) {
    let failed = results
        .iter()
        .filter(|result| result.exit_code != Some(0))
        .collect::<Vec<_>>();
    if failed.is_empty() {
        output.line(
            StreamKind::Stdout,
            &format!(
                "RESULT: PASS ({} commands, {})",
                results.len(),
                format_elapsed(elapsed)
            ),
        );
    } else {
        output.line(
            StreamKind::Stdout,
            &format!(
                "RESULT: FAIL ({} of {} commands)",
                failed.len(),
                results.len()
            ),
        );
        for result in failed {
            output.line(
                StreamKind::Stdout,
                &format!(
                    "  [{}] {}: {} ({})",
                    result.platform.name(),
                    result.label,
                    result.command,
                    describe_exit(result.exit_code)
                ),
            );
        }
    }
    output.line(StreamKind::Stdout, &format!("log: {}", log_path.display()));
}

/// One finished child process.
struct ProcessOutput {
    exit_code: Option<i32>,
    elapsed: Duration,
}

/// Runs `program`, streaming both output streams to the console and the log.
fn stream_command(
    program: &str,
    args: &[String],
    cwd: &Path,
    stdin: Stdio,
    output: &Output,
) -> Result<ProcessOutput, GateError> {
    stream_command_with_env(program, args, cwd, stdin, output, &[])
}

/// [`stream_command`] with extra environment variables for the child.
///
/// Only the local Windows plan needs this: the transition manifest switch must
/// reach `ui_core`'s build script, and it is set per run instead of in the gate
/// runner's own environment (the remote hosts have no manifest conflict).
fn stream_command_with_env(
    program: &str,
    args: &[String],
    cwd: &Path,
    stdin: Stdio,
    output: &Output,
    envs: &[(&str, &str)],
) -> Result<ProcessOutput, GateError> {
    let started = Instant::now();
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in envs {
        command.env(name, value);
    }
    let mut child = command.spawn().map_err(|source| GateError::Spawn {
        program: program.to_owned(),
        source,
    })?;

    let stdout = child.stdout.take().ok_or(GateError::MissingPipe {
        program: program.to_owned(),
        stream: "stdout",
    })?;
    let stderr = child.stderr.take().ok_or(GateError::MissingPipe {
        program: program.to_owned(),
        stream: "stderr",
    })?;

    thread::scope(|scope| {
        scope.spawn(|| pump(stdout, StreamKind::Stdout, output));
        scope.spawn(|| pump(stderr, StreamKind::Stderr, output));
    });

    let status = child.wait().map_err(|source| GateError::Wait {
        program: program.to_owned(),
        source,
    })?;

    Ok(ProcessOutput {
        exit_code: status.code(),
        elapsed: started.elapsed(),
    })
}

/// Copies one child stream into the run output line by line.
///
/// A read error only means the child closed the pipe while exiting, and the
/// child's exit status is the verdict, so the partial output is all that is lost.
fn pump(reader: impl Read, kind: StreamKind, output: &Output) {
    let mut reader = BufReader::new(reader);
    let mut bytes: Vec<u8> = Vec::with_capacity(1024);
    loop {
        bytes.clear();
        match reader.read_until(b'\n', &mut bytes) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&bytes);
        output.line(kind, line.trim_end_matches(['\r', '\n']));
    }
}

/// Which stream a line of child output came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamKind {
    Stdout,
    Stderr,
}

/// The leading lines of each stream plus the trailing stderr, kept so a failure
/// can be diagnosed from the summary without opening the log.
#[derive(Default)]
struct Heads {
    stdout: Vec<String>,
    stderr: Vec<String>,
    stderr_tail: VecDeque<String>,
    truncated: bool,
}

impl Heads {
    fn render(&self) -> String {
        let mut lines: Vec<String> = Vec::with_capacity(HEAD_LINES + TAIL_LINES + 4);
        if !self.stderr.is_empty() {
            lines.push("  stderr:".to_owned());
            lines.extend(self.stderr.iter().map(|line| format!("    {line}")));
        }
        if self.truncated && !self.stderr_tail.is_empty() {
            lines.push("  stderr (tail):".to_owned());
            lines.extend(self.stderr_tail.iter().map(|line| format!("    {line}")));
        }
        if !self.stdout.is_empty() {
            lines.push("  stdout:".to_owned());
            lines.extend(self.stdout.iter().map(|line| format!("    {line}")));
        }
        if lines.is_empty() {
            lines.push("  (no output)".to_owned());
        }
        if self.truncated {
            lines.push("  (truncated; the full output is in the log)".to_owned());
        }
        lines.join("\n")
    }
}

/// Console plus log-file sink for one gate run.
struct Output {
    state: Mutex<OutputState>,
}

struct OutputState {
    log: File,
    log_error: Option<io::Error>,
    heads: Heads,
    markers: Vec<RemoteMarker>,
}

impl Output {
    fn new(log: File) -> Self {
        Self {
            state: Mutex::new(OutputState {
                log,
                log_error: None,
                heads: Heads::default(),
                markers: Vec::new(),
            }),
        }
    }

    /// The lock is never poisoned on purpose: a panicking writer must not turn
    /// the remaining gate output into a second panic.
    fn state(&self) -> MutexGuard<'_, OutputState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Writes one line to the console, the log file and the diagnostics head.
    fn line(&self, kind: StreamKind, text: &str) {
        // A closed console pipe (`| tee` dying, a crashed pager) must not fail the
        // gate run: the log file is the evidence, the console is convenience.
        match kind {
            StreamKind::Stdout => {
                let _ = writeln!(io::stdout().lock(), "{text}");
            }
            StreamKind::Stderr => {
                let _ = writeln!(io::stderr().lock(), "{text}");
            }
        }

        let mut state = self.state();
        if kind == StreamKind::Stdout
            && let Some(marker) = remote::parse_marker(text)
        {
            state.markers.push(marker);
        }
        if kind == StreamKind::Stderr {
            let tail = &mut state.heads.stderr_tail;
            if tail.len() == TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(text.to_owned());
        }
        let head = match kind {
            StreamKind::Stdout => &mut state.heads.stdout,
            StreamKind::Stderr => &mut state.heads.stderr,
        };
        if head.len() < HEAD_LINES {
            head.push(text.to_owned());
        } else {
            state.heads.truncated = true;
        }
        if let Err(error) = writeln!(state.log, "{text}")
            && state.log_error.is_none()
        {
            state.log_error = Some(error);
        }
    }

    fn clear_heads(&self) {
        self.state().heads = Heads::default();
    }

    fn take_heads(&self) -> Heads {
        std::mem::take(&mut self.state().heads)
    }

    fn take_markers(&self) -> Vec<RemoteMarker> {
        std::mem::take(&mut self.state().markers)
    }

    fn take_log_error(&self) -> Option<io::Error> {
        self.state().log_error.take()
    }
}

#[derive(Debug, Error)]
pub enum GateError {
    #[error("missing required option `{0}`")]
    MissingOption(&'static str),
    #[error("option `{0}` needs a value")]
    MissingValue(&'static str),
    #[error("option `{0}` was given more than once; pass it once")]
    DuplicateOption(String),
    #[error("unknown option `{0}`")]
    UnknownOption(String),
    #[error("unknown platform `{0}`; expected win, mac, linux, or all")]
    InvalidPlatform(String),
    #[error("unknown step `{0}`; expected fast, quick, layer, release, bench, accept, or all")]
    InvalidStep(String),
    #[error("`--scope` names no crate; pass a comma separated crate list or `all`")]
    EmptyScope,
    #[error("invalid crate name `{0}`: crate names may only contain a-z, 0-9, `-`, and `_`")]
    InvalidCrateName(String),
    #[error(
        "invalid `--remote-dir` `{0}`: use at most 64 characters from a-z, 0-9, `.`, `-`, and `_`, starting with a letter or digit"
    )]
    InvalidRemoteDirectory(String),
    #[error("invalid `--lock-timeout-secs` `{0}`: expected a positive whole number of seconds")]
    InvalidLockTimeout(String),
    #[error(
        "step `fast` runs only on the local Windows host; use `--platform win`, or a remote-capable step (`quick`, `layer`, `all`, `release`, `bench`, `accept`) for the other hosts"
    )]
    FastNeedsLocalWindows,
    #[error(
        "step `quick` covers only the remote hosts; the local Windows host has no quick step, so use `fast`, `layer`, `all`, `release`, `bench`, or `accept`"
    )]
    QuickNeedsRemote,
    #[error("step `{0}` runs the `ui_core` harness; include `ui_core` in `--scope`")]
    StepNeedsUiCore(&'static str),
    #[error(transparent)]
    Git(io::Error),
    #[error(
        "the checkout is on a detached HEAD; pass `--remote-dir <name>` to name the remote scratch branch"
    )]
    DetachedHead,
    #[error(
        "branch `{0}` does not yield a usable remote directory name; pass `--remote-dir <name>`"
    )]
    RemoteNameFromBranch(String),
    #[error("could not create the log directory `{path}`: {source}")]
    LogDirectory { path: PathBuf, source: io::Error },
    #[error("could not open the log file `{path}`: {source}")]
    LogOpen { path: PathBuf, source: io::Error },
    #[error("could not write the gate log: {source}")]
    LogWrite { source: io::Error },
    #[error("could not start `{program}`: {source}")]
    Spawn { program: String, source: io::Error },
    #[error("`{program}` did not expose its {stream}")]
    MissingPipe {
        program: String,
        stream: &'static str,
    },
    #[error("could not wait for `{program}`: {source}")]
    Wait { program: String, source: io::Error },
    #[error("could not write the remote script `{path}`: {source}")]
    ScriptWrite { path: PathBuf, source: io::Error },
    #[error("could not read the remote script `{path}`: {source}")]
    ScriptOpen { path: PathBuf, source: io::Error },
    #[error("platform `{0}` runs on the local host and has no ssh gate runner")]
    NotRemote(&'static str),
    #[error("git push to `{remote}` failed for branch `{branch}` ({exit})")]
    PushFailed {
        remote: String,
        branch: String,
        exit: String,
    },
    #[error(
        "ssh to `{host}` failed at the transport layer ({exit}); check that `ssh {host}` reaches the host with key authentication. Output:\n{head}"
    )]
    SshFailed {
        host: String,
        exit: String,
        head: String,
    },
    #[error(
        "the gate script on `{host}` stopped before reporting its summary ({detail}). Output:\n{head}"
    )]
    RemoteAborted {
        host: String,
        detail: String,
        head: String,
    },
    #[error(
        "timed out after {timeout}s waiting for the remote gate lock on `{host}`; another gate run there is using the shared target directory"
    )]
    RemoteLockTimeout { host: String, timeout: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Result<GatesConfig, GateError> {
        parse_args(arguments.iter().map(|argument| (*argument).to_owned()))
    }

    fn rendered_plan(config: &GatesConfig) -> Vec<(Platform, Vec<String>)> {
        build_plan(config)
            .expect("build the gate plan")
            .iter()
            .map(|plan| {
                (
                    plan.platform,
                    plan.commands.iter().map(CargoCommand::render).collect(),
                )
            })
            .collect()
    }

    fn fast_scope(scope: &str) -> GatesConfig {
        parse(&["--platform", "win", "--scope", scope, "--step", "fast"])
            .expect("parse the fast run")
    }

    #[test]
    fn a_fast_run_covers_fmt_clippy_and_test() {
        let config = fast_scope("xtask");
        assert_eq!(config.step, Step::Fast);
        assert_eq!(config.lock_timeout_secs, DEFAULT_LOCK_TIMEOUT_SECS);
        assert_eq!(
            rendered_plan(&config),
            [(
                Platform::Win,
                vec![
                    "cargo +nightly fmt --check -p xtask".to_owned(),
                    "cargo +nightly clippy -p xtask --all-targets -- -D warnings".to_owned(),
                    "cargo test -p xtask".to_owned(),
                ]
            )]
        );
    }

    #[test]
    fn ui_core_adds_the_feature_sets_and_the_snapshot_suite() {
        let config = fast_scope("ui_core");
        assert_eq!(
            rendered_plan(&config),
            [(
                Platform::Win,
                vec![
                    "cargo +nightly fmt --check -p ui_core".to_owned(),
                    "cargo +nightly clippy -p ui_core --all-targets --all-features -- -D warnings"
                        .to_owned(),
                    "cargo test -p ui_core --features hud,test-support".to_owned(),
                    "cargo test -p ui_core --features test-support --test snapshot".to_owned(),
                ]
            )]
        );
    }

    #[test]
    fn a_scope_that_mixes_ui_core_with_other_crates_splits_the_commands() {
        let config = fast_scope("ui_core,xtask");
        assert_eq!(
            rendered_plan(&config),
            [(
                Platform::Win,
                vec![
                    "cargo +nightly fmt --check -p ui_core -p xtask".to_owned(),
                    "cargo +nightly clippy -p xtask --all-targets -- -D warnings".to_owned(),
                    "cargo test -p xtask".to_owned(),
                    "cargo +nightly clippy -p ui_core --all-targets --all-features -- -D warnings"
                        .to_owned(),
                    "cargo test -p ui_core --features hud,test-support".to_owned(),
                    "cargo test -p ui_core --features test-support --test snapshot".to_owned(),
                ]
            )]
        );
    }

    #[test]
    fn the_whole_workspace_uses_the_workspace_selection() {
        let config = fast_scope("all");
        assert_eq!(
            rendered_plan(&config),
            [(
                Platform::Win,
                vec![
                    "cargo +nightly fmt --check --all".to_owned(),
                    "cargo +nightly clippy --workspace --exclude ui_core --all-targets -- -D warnings".to_owned(),
                    "cargo test --workspace --exclude ui_core".to_owned(),
                    "cargo +nightly clippy -p ui_core --all-targets --all-features -- -D warnings".to_owned(),
                    "cargo test -p ui_core --features hud,test-support".to_owned(),
                    "cargo test -p ui_core --features test-support --test snapshot".to_owned(),
                ]
            )]
        );
    }

    #[test]
    fn only_a_scope_inside_the_migration_engine_keeps_the_manifest_link() {
        for scope in ["ui_core", "xtask", "ui_core,xtask"] {
            assert!(
                !skips_ui_core_manifest(&fast_scope(scope).scope),
                "`--scope {scope}` does not link `gpui-pre`: its Windows run keeps the ui_core manifest link under test"
            );
        }
        for scope in ["base_ui", "kytrix", "ui_core,base_ui", "all"] {
            assert!(
                skips_ui_core_manifest(&fast_scope(scope).scope),
                "`--scope {scope}` still links `gpui-pre`: its Windows build scripts must skip the ui_core manifest link"
            );
        }
    }

    #[test]
    fn a_layer_run_on_windows_appends_the_gui_smoke_run() {
        let config = parse(&["--platform", "win", "--scope", "ui_core", "--step", "layer"])
            .expect("parse the layer run");
        let plan = build_plan(&config).expect("build the gate plan");
        let commands = &plan.first().expect("one platform").commands;
        let last = commands.last().expect("a last command");
        assert_eq!(last.label, "gui-smoke");
        assert_eq!(
            last.render(),
            "cargo test -p ui_core --features test-support --test gui_smoke -- --ignored --nocapture"
        );
    }

    #[test]
    fn a_layer_run_runs_the_gui_smoke_on_every_platform() {
        let config = parse(&["--platform", "all", "--scope", "ui_core", "--step", "layer"])
            .expect("parse the layer run");
        let plans = build_plan(&config).expect("build the gate plan");
        assert_eq!(
            plans.iter().map(|plan| plan.platform).collect::<Vec<_>>(),
            [Platform::Win, Platform::Mac, Platform::Linux]
        );

        for plan in &plans {
            let labels = plan
                .commands
                .iter()
                .map(|command| command.label.as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                labels,
                [
                    "fmt",
                    "clippy(ui_core,all-features)",
                    "test(ui_core,hud,test-support)",
                    "snapshot",
                    "gui-smoke"
                ],
                "the layer set on {:?} must run the offscreen gui smoke suite",
                plan.platform
            );
            let gui_smoke = plan.commands.last().expect("a last command");
            assert_eq!(
                gui_smoke.render(),
                "cargo test -p ui_core --features test-support --test gui_smoke -- --ignored --nocapture"
            );
        }
    }

    #[test]
    fn a_kytrix_layer_run_adds_the_release_build() {
        let config = parse(&["--platform", "win", "--scope", "kytrix", "--step", "layer"])
            .expect("parse the layer run");
        let plans = rendered_plan(&config);
        assert_eq!(
            plans[0].1.last().map(String::as_str),
            Some("cargo build -p kytrix --release")
        );

        let ui_core = parse(&["--platform", "win", "--scope", "ui_core", "--step", "layer"])
            .expect("parse the layer run");
        assert!(
            !rendered_plan(&ui_core)[0]
                .1
                .iter()
                .any(|command| command.contains("--release")),
            "a ui_core layer run has no release build"
        );
    }

    #[test]
    fn the_release_step_builds_the_scope_in_release_mode() {
        let config = parse(&[
            "--platform",
            "win",
            "--scope",
            "ui_core,xtask",
            "--step",
            "release",
        ])
        .expect("parse the release run");
        assert_eq!(
            rendered_plan(&config),
            [(
                Platform::Win,
                vec![
                    "cargo build -p xtask --release".to_owned(),
                    "cargo build -p ui_core --release --all-features".to_owned(),
                ]
            )]
        );
    }

    #[test]
    fn the_bench_step_builds_the_ui_core_perf_bench() {
        let config = parse(&[
            "--platform",
            "linux",
            "--scope",
            "ui_core",
            "--step",
            "bench",
        ])
        .expect("parse the bench run");
        assert_eq!(
            rendered_plan(&config),
            [(
                Platform::Linux,
                vec![
                    "cargo bench -p ui_core --features test-support --bench perf --no-run"
                        .to_owned()
                ]
            )]
        );
    }

    #[test]
    fn the_accept_step_runs_the_acceptance_harness_on_every_platform() {
        let config = parse(&[
            "--platform",
            "all",
            "--scope",
            "ui_core",
            "--step",
            "accept",
        ])
        .expect("parse the accept run");
        let accept = "cargo run --release -p ui_core --example acceptance -- cpu".to_owned();
        assert_eq!(
            rendered_plan(&config),
            [
                (Platform::Win, vec![accept.clone()]),
                (Platform::Mac, vec![accept.clone()]),
                (Platform::Linux, vec![accept]),
            ]
        );
    }

    #[test]
    fn the_ui_core_harness_steps_need_ui_core_in_scope() {
        for step in ["bench", "accept"] {
            let config = parse(&["--platform", "win", "--scope", "xtask", "--step", step])
                .expect("parse the run");
            assert!(
                matches!(
                    build_plan(&config),
                    Err(GateError::StepNeedsUiCore(needed)) if needed == step
                ),
                "step `{step}` without ui_core must be rejected"
            );
        }
    }

    #[test]
    fn a_quick_run_checks_every_remote_host() {
        let config = parse(&["--platform", "all", "--scope", "ui_core", "--step", "quick"])
            .expect("parse the quick run");
        assert_eq!(
            rendered_plan(&config),
            [
                (
                    Platform::Mac,
                    vec!["cargo check -p ui_core --all-targets --all-features".to_owned()]
                ),
                (
                    Platform::Linux,
                    vec!["cargo check -p ui_core --all-targets --all-features".to_owned()]
                ),
            ]
        );
    }

    #[test]
    fn a_quick_run_without_ui_core_checks_the_default_features() {
        let config = parse(&["--platform", "linux", "--scope", "xtask", "--step", "quick"])
            .expect("parse the quick run");
        assert_eq!(
            rendered_plan(&config),
            [(
                Platform::Linux,
                vec!["cargo check -p xtask --all-targets".to_owned()]
            )]
        );
    }

    #[test]
    fn a_quick_run_over_the_workspace_splits_ui_core_out() {
        let config = parse(&["--platform", "mac", "--scope", "all", "--step", "quick"])
            .expect("parse the quick run");
        assert_eq!(
            rendered_plan(&config),
            [(
                Platform::Mac,
                vec![
                    "cargo check --workspace --exclude ui_core --all-targets".to_owned(),
                    "cargo check -p ui_core --all-targets --all-features".to_owned(),
                ]
            )]
        );
    }

    #[test]
    fn a_full_run_on_windows_is_the_layer_set() {
        let all = parse(&["--platform", "win", "--scope", "ui_core", "--step", "all"])
            .expect("parse the full run");
        let layer = parse(&["--platform", "win", "--scope", "ui_core", "--step", "layer"])
            .expect("parse the layer run");
        assert_eq!(rendered_plan(&all), rendered_plan(&layer));
    }

    #[test]
    fn a_full_run_checks_the_remotes_before_the_test_set() {
        let config = parse(&["--platform", "all", "--scope", "ui_core", "--step", "all"])
            .expect("parse the full run");
        let plans = rendered_plan(&config);
        assert_eq!(
            plans
                .iter()
                .map(|(platform, _)| *platform)
                .collect::<Vec<_>>(),
            [Platform::Win, Platform::Mac, Platform::Linux]
        );
        let remote = [
            "cargo check -p ui_core --all-targets --all-features".to_owned(),
            "cargo +nightly fmt --check -p ui_core".to_owned(),
            "cargo +nightly clippy -p ui_core --all-targets --all-features -- -D warnings"
                .to_owned(),
            "cargo test -p ui_core --features hud,test-support".to_owned(),
            "cargo test -p ui_core --features test-support --test snapshot".to_owned(),
            "cargo test -p ui_core --features test-support --test gui_smoke -- --ignored --nocapture"
                .to_owned(),
        ];
        assert_eq!(plans[1].1, remote);
        assert_eq!(plans[2].1, remote);
        assert_eq!(
            plans[0].1.last().map(String::as_str),
            Some(
                "cargo test -p ui_core --features test-support --test gui_smoke -- --ignored --nocapture"
            )
        );
    }

    #[test]
    fn options_accept_the_inline_form() {
        let config = parse(&[
            "--platform=mac",
            "--scope=ui_core,xtask",
            "--step=quick",
            "--log=target/gates.log",
            "--remote-dir=mig-p0-2",
            "--lock-timeout-secs=60",
        ])
        .expect("parse the inline options");
        assert_eq!(config.platforms, [Platform::Mac]);
        assert_eq!(
            config.scope,
            Scope::Crates(vec!["ui_core".to_owned(), "xtask".to_owned()])
        );
        assert_eq!(config.step, Step::Quick);
        assert_eq!(config.log.as_deref(), Some(Path::new("target/gates.log")));
        assert_eq!(config.remote_dir.as_deref(), Some("mig-p0-2"));
        assert_eq!(config.lock_timeout_secs, 60);
    }

    #[test]
    fn a_fast_run_selected_for_every_host_only_runs_on_windows() {
        let config = parse(&["--platform", "all", "--scope", "xtask", "--step", "fast"])
            .expect("parse the run");
        let plans = build_plan(&config).expect("build the gate plan");
        assert_eq!(
            plans.iter().map(|plan| plan.platform).collect::<Vec<_>>(),
            [Platform::Win]
        );
    }

    #[test]
    fn a_remote_only_selection_leaves_fast_nothing_to_run() {
        let config = parse(&["--platform", "mac", "--scope", "xtask", "--step", "fast"])
            .expect("parse the run");
        assert!(matches!(
            build_plan(&config),
            Err(GateError::FastNeedsLocalWindows)
        ));
    }

    #[test]
    fn a_windows_only_selection_leaves_quick_nothing_to_run() {
        let config = parse(&["--platform", "win", "--scope", "xtask", "--step", "quick"])
            .expect("parse the run");
        assert!(matches!(
            build_plan(&config),
            Err(GateError::QuickNeedsRemote)
        ));
    }

    #[test]
    fn missing_and_unknown_options_are_named() {
        assert!(matches!(
            parse(&["--scope", "xtask", "--step", "fast"]),
            Err(GateError::MissingOption("--platform"))
        ));
        assert!(matches!(
            parse(&["--platform", "win", "--scope", "xtask"]),
            Err(GateError::MissingOption("--step"))
        ));
        assert!(matches!(
            parse(&["--platform", "win", "--scope", "xtask", "--step"]),
            Err(GateError::MissingValue("--step"))
        ));
        assert!(matches!(
            parse(&[
                "--platform",
                "win",
                "--scope",
                "xtask",
                "--step",
                "fast",
                "--nope"
            ]),
            Err(GateError::UnknownOption(_))
        ));
    }

    #[test]
    fn repeated_options_are_rejected_instead_of_overwriting() {
        assert!(matches!(
            parse(&[
                "--platform",
                "win",
                "--platform",
                "mac",
                "--scope",
                "xtask",
                "--step",
                "fast"
            ]),
            Err(GateError::DuplicateOption(name)) if name == "--platform"
        ));
        assert!(matches!(
            parse(&[
                "--platform=win",
                "--scope=xtask",
                "--step=fast",
                "--log=a",
                "--log=b"
            ]),
            Err(GateError::DuplicateOption(name)) if name == "--log"
        ));
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(matches!(
            parse(&[
                "--platform",
                "solaris",
                "--scope",
                "xtask",
                "--step",
                "fast"
            ]),
            Err(GateError::InvalidPlatform(_))
        ));
        assert!(matches!(
            parse(&["--platform", "win", "--scope", "xtask", "--step", "slow"]),
            Err(GateError::InvalidStep(_))
        ));
        assert!(matches!(Scope::from_arg(""), Err(GateError::EmptyScope)));
        assert!(matches!(
            Scope::from_arg("ui_core,"),
            Err(GateError::EmptyScope)
        ));
        assert!(matches!(
            Scope::from_arg("ui core"),
            Err(GateError::InvalidCrateName(_))
        ));
        assert!(matches!(
            Scope::from_arg("Ui_Core"),
            Err(GateError::InvalidCrateName(_))
        ));
        assert!(matches!(
            parse(&[
                "--platform",
                "win",
                "--scope",
                "xtask",
                "--step",
                "fast",
                "--lock-timeout-secs",
                "0"
            ]),
            Err(GateError::InvalidLockTimeout(_))
        ));
        assert!(matches!(
            parse(&[
                "--platform",
                "win",
                "--scope",
                "xtask",
                "--step",
                "fast",
                "--lock-timeout-secs",
                "soon"
            ]),
            Err(GateError::InvalidLockTimeout(_))
        ));
        for invalid in [".", "..", ".hidden", "-leading", "_leading", "a b"] {
            assert!(
                matches!(
                    parse(&[
                        "--platform",
                        "win",
                        "--scope",
                        "xtask",
                        "--step",
                        "fast",
                        "--remote-dir",
                        invalid,
                    ]),
                    Err(GateError::InvalidRemoteDirectory(_))
                ),
                "`--remote-dir {invalid}` must be rejected"
            );
        }
    }

    #[test]
    fn a_scope_list_is_deduplicated() {
        assert_eq!(
            Scope::from_arg("xtask,xtask,ui_core").expect("parse the scope"),
            Scope::Crates(vec!["xtask".to_owned(), "ui_core".to_owned()])
        );
    }

    #[test]
    fn a_branch_name_becomes_a_remote_directory_name() {
        assert_eq!(sanitize_branch("mig/p0-2"), Some("mig-p0-2".to_owned()));
        assert_eq!(
            sanitize_branch("gui-engine-migration"),
            Some("gui-engine-migration".to_owned())
        );
        assert_eq!(sanitize_branch("P0/A--B"), Some("p0-a-b".to_owned()));
        assert_eq!(sanitize_branch("///"), None);
        assert_eq!(sanitize_branch(""), None);
    }

    #[test]
    fn the_remote_branch_always_carries_the_mig_prefix() {
        let config = parse(&[
            "--platform",
            "mac",
            "--scope",
            "xtask",
            "--step",
            "quick",
            "--remote-dir",
            "p1-a",
        ])
        .expect("parse the remote run");
        let names = remote_names(&config, Path::new("E:/kytrix")).expect("resolve the names");
        assert_eq!(names.branch, "mig-p1-a");
        assert_eq!(names.directory, "p1-a");

        // A name that already starts with `mig-` is not prefixed twice, so a
        // migration branch keeps its own remote branch instead of a `mig-mig-*`.
        let config = parse(&[
            "--platform",
            "mac",
            "--scope",
            "xtask",
            "--step",
            "quick",
            "--remote-dir",
            "mig-p0-2",
        ])
        .expect("parse the remote run");
        let names = remote_names(&config, Path::new("E:/kytrix")).expect("resolve the names");
        assert_eq!(names.branch, "mig-p0-2");
        assert_eq!(names.directory, "mig-p0-2");
    }

    #[test]
    fn agent_worktrees_share_the_main_checkout_review_directory() {
        let main = Path::new("E:/kytrix");
        assert_eq!(
            review_directory(&main.join(".agents/worktrees/p0-2")),
            main.join(".review")
        );
        assert_eq!(review_directory(main), main.join(".review"));
        assert_eq!(
            review_directory(Path::new("E:/other/worktrees/p0-2")),
            Path::new("E:/other/worktrees/p0-2").join(".review")
        );
    }

    #[test]
    fn the_default_log_name_carries_the_timestamp_and_platform() {
        let config = fast_scope("xtask");
        let path = default_log_path(Path::new("E:/kytrix"), &config);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("a log name");
        assert!(name.starts_with("mig-gates-"));
        assert!(name.ends_with("-win.log"), "unexpected log name {name}");
        assert_eq!(path.parent(), Some(Path::new("E:/kytrix/.review")));
    }

    #[test]
    fn a_remote_plan_names_the_scratch_branch_from_the_checkout_branch() {
        let config = parse(&["--platform", "mac", "--scope", "xtask", "--step", "quick"])
            .expect("parse the run");
        let names = RemoteNames {
            branch: "mig-p0-2".to_owned(),
            directory: "mig-p0-2".to_owned(),
        };
        let plan = build_plan(&config).expect("build the gate plan").remove(0);
        let script = remote::build_script(&plan, config.lock_timeout_secs, &names);
        assert!(
            script.contains("LOCKDIR=/tmp/kytrix-p1-remote.lock"),
            "{script}"
        );
        assert!(
            script.contains("if [ \"$tries\" -gt 1800 ]; then"),
            "{script}"
        );
        assert!(script.contains("BRANCH='mig-p0-2'"), "{script}");
        assert!(
            script.contains("SRC=\"$HOME/kytrix-mig-src-mig-p0-2\""),
            "{script}"
        );
        assert!(script.contains("KYTRIX_LOCK_TIMEOUT=1800"), "{script}");
        assert!(
            script.contains("git clone -q \"$HOME/kytrix-p1.git\" \"$SRC\""),
            "{script}"
        );
        assert!(script.contains("touch crates/ui_core/build.rs"), "{script}");
        assert!(
            script.contains("export CARGO_TARGET_DIR=\"$HOME/kytrix-review-target\""),
            "{script}"
        );
        assert!(
            script.contains("gate 0 'check' '-p' 'xtask' '--all-targets'"),
            "{script}"
        );
        assert!(
            script.contains("echo \"KYTRIX_TOTAL_EXIT=$overall\""),
            "{script}"
        );
        // The lock must be released on every exit path, successful or not: a
        // leaked lock makes the next run on that host wait for the full timeout.
        assert!(
            script.contains("trap 'rmdir \"$LOCKDIR\" 2>/dev/null' EXIT"),
            "{script}"
        );
    }

    #[test]
    fn the_ssh_invocation_sets_a_connect_timeout() {
        let arguments = remote::ssh_arguments("my_macos");
        assert!(
            arguments.contains(&"ConnectTimeout=15".to_owned()),
            "{arguments:?}"
        );
        assert!(
            arguments.contains(&"BatchMode=yes".to_owned()),
            "{arguments:?}"
        );
        assert_eq!(
            &arguments[arguments.len() - 3..],
            ["my_macos", "bash", "-s"]
        );
    }

    #[test]
    fn a_remote_script_quotes_every_cargo_argument() {
        let config = parse(&[
            "--platform",
            "linux",
            "--scope",
            "ui_core",
            "--step",
            "layer",
        ])
        .expect("parse the run");
        let names = RemoteNames {
            branch: "mig-p0-2".to_owned(),
            directory: "mig-p0-2".to_owned(),
        };
        let plan = build_plan(&config).expect("build the gate plan").remove(0);
        let script = remote::build_script(&plan, 5, &names);
        assert!(
            script.contains("gate 1 '+nightly' 'clippy' '-p' 'ui_core' '--all-targets' '--all-features' '--' '-D' 'warnings'"),
            "{script}"
        );
        assert!(script.contains("if [ \"$tries\" -gt 5 ]; then"), "{script}");
    }

    #[test]
    fn remote_markers_round_trip() {
        assert_eq!(
            remote::parse_marker("KYTRIX_GATE_EXIT=0 2 41"),
            Some(RemoteMarker::GateExit {
                index: 2,
                exit_code: 0,
                elapsed_secs: 41,
            })
        );
        assert_eq!(
            remote::parse_marker("KYTRIX_TOTAL_EXIT=1"),
            Some(RemoteMarker::TotalExit(1))
        );
        assert_eq!(
            remote::parse_marker("KYTRIX_ABORT=checkout"),
            Some(RemoteMarker::Abort("checkout".to_owned()))
        );
        assert_eq!(
            remote::parse_marker("KYTRIX_LOCK_TIMEOUT=1800"),
            Some(RemoteMarker::LockTimeout(1800))
        );
        assert_eq!(remote::parse_marker("KYTRIX_GATE_EXIT=oops 2 3"), None);
        assert_eq!(remote::parse_marker("KYTRIX_GATE_EXIT=0"), None);
        assert_eq!(remote::parse_marker("running 12 tests"), None);
    }

    #[test]
    fn heads_render_the_leading_output_and_the_truncation_note() {
        let mut heads = Heads {
            stdout: vec!["compiling".to_owned(), "running tests".to_owned()],
            stderr: vec!["warning: unused".to_owned()],
            stderr_tail: VecDeque::new(),
            truncated: false,
        };
        let rendered = heads.render();
        assert!(
            rendered.contains("stderr:\n    warning: unused"),
            "{rendered}"
        );
        assert!(rendered.contains("stdout:\n    compiling"), "{rendered}");
        assert!(!rendered.contains("truncated"), "{rendered}");

        heads.truncated = true;
        heads.stderr_tail = VecDeque::from(["error: out of disk".to_owned()]);
        let rendered = heads.render();
        assert!(rendered.contains("truncated"), "{rendered}");
        assert!(
            rendered.contains("stderr (tail):\n    error: out of disk"),
            "{rendered}"
        );
    }

    fn gate_result(exit_code: Option<i32>) -> GateResult {
        GateResult {
            platform: Platform::Win,
            label: "test".to_owned(),
            command: "cargo test".to_owned(),
            exit_code,
            elapsed: Duration::ZERO,
        }
    }

    #[test]
    fn a_run_passes_only_when_every_command_passed() {
        assert!(all_passed(&[gate_result(Some(0)), gate_result(Some(0))]));
        assert!(!all_passed(&[gate_result(Some(0)), gate_result(Some(101))]));
        // A command that never produced an exit status did not pass.
        assert!(!all_passed(&[gate_result(None)]));
        // A run that executed nothing has not gated anything.
        assert!(!all_passed(&[]));
    }

    fn remote_plan(platform: Platform, scope: &str, step: &str) -> PlatformPlan {
        let config = parse(&[
            "--platform",
            platform.name(),
            "--scope",
            scope,
            "--step",
            step,
        ])
        .expect("parse the remote run");
        build_plan(&config)
            .expect("build the remote plan")
            .remove(0)
    }

    fn session(exit_code: Option<i32>) -> ProcessOutput {
        ProcessOutput {
            exit_code,
            elapsed: Duration::ZERO,
        }
    }

    #[test]
    fn a_remote_summary_maps_every_marker_to_its_command() {
        let plan = remote_plan(Platform::Mac, "ui_core", "layer");
        let markers = [
            RemoteMarker::GateExit {
                index: 0,
                exit_code: 0,
                elapsed_secs: 1,
            },
            RemoteMarker::GateExit {
                index: 2,
                exit_code: 101,
                elapsed_secs: 3,
            },
            RemoteMarker::TotalExit(1),
        ];
        let results = remote::interpret(
            &plan,
            session(Some(1)),
            &Heads::default(),
            &markers,
            "my_macos",
        )
        .expect("a completed run");
        assert_eq!(results.len(), plan.commands.len());
        assert_eq!(results[0].exit_code, Some(0));
        assert_eq!(results[0].elapsed, Duration::from_secs(1));
        // A command without its own marker never counts as a pass.
        assert_eq!(results[1].exit_code, None);
        assert_eq!(results[2].exit_code, Some(101));
        assert_eq!(results[4].exit_code, None);
        assert!(!all_passed(&results));
    }

    #[test]
    fn the_first_marker_for_a_command_wins() {
        let plan = remote_plan(Platform::Mac, "ui_core", "layer");
        let markers = [
            RemoteMarker::GateExit {
                index: 1,
                exit_code: 0,
                elapsed_secs: 4,
            },
            RemoteMarker::GateExit {
                index: 1,
                exit_code: 101,
                elapsed_secs: 9,
            },
            RemoteMarker::TotalExit(0),
        ];
        let results = remote::interpret(
            &plan,
            session(Some(0)),
            &Heads::default(),
            &markers,
            "my_macos",
        )
        .expect("a completed run");
        assert_eq!(results[1].exit_code, Some(0));
        assert_eq!(results[1].elapsed, Duration::from_secs(4));
    }

    #[test]
    fn a_remote_lock_timeout_names_the_host() {
        let plan = remote_plan(Platform::Linux, "xtask", "layer");
        let error = remote::interpret(
            &plan,
            session(Some(9)),
            &Heads::default(),
            &[RemoteMarker::LockTimeout(1800)],
            "my_ubuntu",
        )
        .expect_err("a lock timeout is an error");
        assert!(
            matches!(&error, GateError::RemoteLockTimeout { host, timeout } if host == "my_ubuntu" && *timeout == 1800),
            "{error}"
        );
    }

    #[test]
    fn a_remote_checkout_abort_names_the_stage() {
        let plan = remote_plan(Platform::Mac, "xtask", "quick");
        let error = remote::interpret(
            &plan,
            session(Some(5)),
            &Heads::default(),
            &[RemoteMarker::Abort("checkout".to_owned())],
            "my_macos",
        )
        .expect_err("an aborted checkout is an error");
        let message = error.to_string();
        assert!(message.contains("my_macos"), "{message}");
        assert!(message.contains("checkout"), "{message}");
    }

    #[test]
    fn an_ssh_transport_failure_is_reported_as_ssh() {
        let plan = remote_plan(Platform::Mac, "xtask", "quick");
        let error = remote::interpret(
            &plan,
            session(Some(255)),
            &Heads::default(),
            &[],
            "my_macos",
        )
        .expect_err("a transport failure is an error");
        assert!(
            matches!(&error, GateError::SshFailed { host, .. } if host == "my_macos"),
            "{error}"
        );
    }

    #[test]
    fn a_script_killed_before_its_summary_is_a_remote_abort() {
        let plan = remote_plan(Platform::Linux, "xtask", "layer");
        let heads = Heads {
            stdout: Vec::new(),
            stderr: (0..HEAD_LINES)
                .map(|line| format!("compiling line {line}"))
                .collect(),
            stderr_tail: VecDeque::from(["error: Disk quota exceeded (os error 122)".to_owned()]),
            truncated: true,
        };
        let error = remote::interpret(&plan, session(Some(137)), &heads, &[], "my_ubuntu")
            .expect_err("a killed script is an error");
        assert!(matches!(&error, GateError::RemoteAborted { .. }), "{error}");
        let message = error.to_string();
        assert!(message.contains("my_ubuntu"), "{message}");
        assert!(message.contains("exit 137"), "{message}");
        assert!(
            message.contains("Disk quota exceeded"),
            "the stderr tail must be kept: {message}"
        );
    }

    #[test]
    fn an_interrupted_script_is_not_reported_as_an_ssh_failure() {
        // ssh itself was killed locally (no exit status): the classification must
        // not point the reader at key authentication.
        let plan = remote_plan(Platform::Mac, "xtask", "quick");
        let error = remote::interpret(&plan, session(None), &Heads::default(), &[], "my_macos")
            .expect_err("an interrupted run is an error");
        assert!(matches!(&error, GateError::RemoteAborted { .. }), "{error}");
    }
}
