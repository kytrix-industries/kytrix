mod bump;
mod gates;

use std::{io, path::PathBuf, process::ExitCode};

use bump::{BumpKind, BumpOutcome, find_repository_root, run};
use thiserror::Error;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_usage();
        return ExitCode::SUCCESS;
    }

    match run_cli(args) {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_cli(args: Vec<String>) -> Result<ExitCode, CliError> {
    let mut args = args.into_iter();
    let command = args.next().ok_or(CliError::MissingCommand)?;
    match command.as_str() {
        "bump-version" => {
            run_bump(args)?;
            Ok(ExitCode::SUCCESS)
        }
        "gates" => run_gates(args),
        command => Err(CliError::UnknownCommand(command.to_owned())),
    }
}

fn run_gates(args: impl Iterator<Item = String>) -> Result<ExitCode, CliError> {
    let config = gates::parse_args(args)?;
    let root = repository_root()?;

    if gates::run(&root, &config)? {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::FAILURE)
    }
}

fn run_bump(mut args: impl Iterator<Item = String>) -> Result<(), CliError> {
    let kind = args.next().ok_or(CliError::MissingKind)?;
    let kind = BumpKind::from_arg(&kind).ok_or(CliError::InvalidKind(kind))?;
    if let Some(extra) = args.next() {
        return Err(CliError::UnexpectedArgument(extra));
    }

    let root = repository_root()?;
    let outcome = run(&root, kind)?;
    print_outcome(&outcome);

    Ok(())
}

fn repository_root() -> Result<PathBuf, CliError> {
    let current_dir = std::env::current_dir().map_err(CliError::CurrentDirectory)?;
    Ok(find_repository_root(&current_dir)?)
}

fn print_outcome(outcome: &BumpOutcome) {
    println!("Bumped kytrix {} -> {}", outcome.previous, outcome.next);
    println!("  Cargo.toml: [workspace.package] version");
    println!("  Packager.toml: version");
    if outcome.notes_created {
        println!(
            "  release_notes/v{}/: created (en.md, zh-CN.md)",
            outcome.next
        );
    } else {
        println!(
            "  release_notes/v{}/: already exists (left unchanged)",
            outcome.next
        );
    }
    println!(
        "Next: review release_notes/v{0}/en.md and release_notes/v{0}/zh-CN.md and tag v{0}.",
        outcome.next
    );
}

fn print_usage() {
    println!(concat!(
        "Usage:\n",
        "  cargo xtask bump-version <major|minor|patch>\n",
        "  cargo xtask gates --platform <win|mac|linux|all> --scope <crate,...|all>\n",
        "                    --step <fast|quick|layer|release|bench|accept|all>\n",
        "                    [--log <path>] [--remote-dir <name>] [--lock-timeout-secs <n>]\n",
        "\n",
        "bump-version bumps [workspace.package] version in Cargo.toml and the top-level\n",
        "version in Packager.toml, and creates release_notes/vX.Y.Z/en.md and zh-CN.md\n",
        "when the version directory is missing. A version directory that already exists\n",
        "must hold both files.\n",
        "\n",
        "gates runs the three-platform gate set for the crates named by --scope. For every\n",
        "crate in scope it runs the same commands the P1/P2 gate scripts ran by hand:\n",
        "\n",
        "  fast     local Windows only: `cargo +nightly fmt --check`, `cargo +nightly\n",
        "           clippy --all-targets -- -D warnings`, `cargo test`. When `ui_core` is\n",
        "           in scope it also runs the feature-enabled suite with\n",
        "           `--features hud,test-support`, clippy with `--all-features` (the\n",
        "           P1/P2 baseline coverage) and the pixel `--test snapshot` suite.\n",
        "  quick    remote only (mac, linux): `cargo check --all-targets` over ssh, on a\n",
        "           clean scratch checkout sharing the host's compiled-target cache.\n",
        "           `ui_core` is checked with `--all-features` because its unit tests use\n",
        "           `test-support` APIs, which a default-feature `--all-targets` build\n",
        "           cannot compile on any platform.\n",
        "  layer    every selected platform: the `fast` command set, plus the ignored\n",
        "           `gui_smoke` suite when `ui_core` is in scope. `gui_smoke` is offscreen\n",
        "           on all three hosts (D3D12/WARP on Windows, Metal on macOS, Vulkan via\n",
        "           lavapipe on Linux) and the P1/P2 logs show it passing on mac and\n",
        "           linux, so layer runs it on every platform, not just Windows. The\n",
        "           snapshot and gui_smoke suites run with `--features test-support`, the\n",
        "           P1/P2 script feature set; the `test` step keeps `hud,test-support`.\n",
        "           A layer run with `kytrix` in scope also runs\n",
        "           `cargo build -p kytrix --release` (plan §2.4).\n",
        "  release  every selected platform: `cargo build --release` for the scope\n",
        "           (`ui_core` with `--all-features`, matching .review/p1-gates.ps1).\n",
        "  bench    every selected platform: build the `ui_core` perf bench without\n",
        "           running it (`cargo bench -p ui_core --features test-support --bench\n",
        "           perf --no-run`, matching .review/p1-remote.ps1's no-run-bench).\n",
        "  accept   every selected platform: run the `ui_core` acceptance harness in its\n",
        "           headless CPU mode (`cargo run --release -p ui_core --example\n",
        "           acceptance -- cpu`, matching .review/p1-remote.ps1's acceptance-cpu).\n",
        "           `bench` and `accept` need `ui_core` in --scope; the manual three-\n",
        "           platform windowed acceptance of plan §2.4 M6/M7 stays a human step.\n",
        "  all      `quick` plus `layer`: on the remote hosts the cheap `check` first\n",
        "           and then the `layer` set; on Windows the `layer` set. `release`,\n",
        "           `bench` and `accept` are separate steps, not part of `all`.\n",
        "\n",
        "`--platform` selects the hosts and each step runs only where it has work:\n",
        "`fast` on win, `quick` on mac and linux, `layer`, `release`, `bench`, `accept`\n",
        "and `all` on all three. `--platform all` therefore runs `quick` on both remote\n",
        "hosts and `layer` everywhere; a selection that leaves the step nothing to run is\n",
        "an error rather than a silent no-op.\n",
        "\n",
        "`--scope` takes a comma separated crate list (names are validated against\n",
        "[a-z0-9_-]) or `all`, which selects every workspace member (`--workspace`, and\n",
        "`--all` for `cargo fmt`). Repeating an option is a usage error instead of a\n",
        "silent last-wins overwrite.\n",
        "\n",
        "A remote run force-pushes HEAD to the host's bare mirror `~/kytrix-p1.git` under\n",
        "the branch `mig-<name>` (a leading `mig-` in <name> is not doubled), resets\n",
        "`~/kytrix-mig-src-<name>` to that branch, touches `crates/ui_core/build.rs` so no\n",
        "build script can be reused across commits, and holds the same mkdir lock\n",
        "(`/tmp/kytrix-p1-remote.lock`) as `.review/p1-remote.ps1` while it runs, because\n",
        "the target directory is shared. `--remote-dir` names the scratch branch and\n",
        "directory (at most 64 characters from `a-z0-9._-`, starting with a letter or\n",
        "digit); by default the name is the current branch with everything outside\n",
        "`a-z0-9._-` replaced by `-`. ssh runs with `BatchMode=yes`, `ConnectTimeout=15`\n",
        "and a two-minute server-alive window.\n",
        "\n",
        "Every command is echoed with its exit code and elapsed time. `--log` writes the\n",
        "run log to <path>, overwriting an existing file so one run's evidence is never\n",
        "mixed with another's (default `.review/mig-gates-<unix-seconds>-<platform>.log`\n",
        "of the main checkout). `--lock-timeout-secs` bounds the wait for the remote lock\n",
        "(default 1800). A failed command does not stop the remaining commands of its\n",
        "host; when any command fails the process exits non-zero and the failed commands\n",
        "are listed last."
    ));
}

#[derive(Debug, Error)]
enum CliError {
    #[error("missing subcommand; expected `bump-version` or `gates`")]
    MissingCommand,
    #[error("unknown subcommand `{0}`; expected `bump-version` or `gates`")]
    UnknownCommand(String),
    #[error("missing bump kind; expected major, minor, or patch")]
    MissingKind,
    #[error("unknown bump kind `{0}`; expected major, minor, or patch")]
    InvalidKind(String),
    #[error("unexpected argument `{0}`")]
    UnexpectedArgument(String),
    #[error("could not determine the current directory: {0}")]
    CurrentDirectory(#[source] io::Error),
    #[error(transparent)]
    Bump(#[from] bump::BumpError),
    #[error(transparent)]
    Gates(#[from] gates::GateError),
}
