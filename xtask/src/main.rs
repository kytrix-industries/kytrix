mod bump;

use std::{io, process::ExitCode};

use bump::{BumpKind, BumpOutcome, find_repository_root, run};
use thiserror::Error;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_usage();
        return ExitCode::SUCCESS;
    }

    match run_cli(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_cli(args: Vec<String>) -> Result<(), CliError> {
    let mut args = args.into_iter();
    let command = args.next().ok_or(CliError::MissingCommand)?;
    if command != "bump-version" {
        return Err(CliError::UnknownCommand(command));
    }

    let kind = args.next().ok_or(CliError::MissingKind)?;
    let kind = BumpKind::from_arg(&kind).ok_or(CliError::InvalidKind(kind))?;
    if let Some(extra) = args.next() {
        return Err(CliError::UnexpectedArgument(extra));
    }

    let current_dir = std::env::current_dir().map_err(CliError::CurrentDirectory)?;
    let root = find_repository_root(&current_dir)?;
    let outcome = run(&root, kind)?;
    print_outcome(&outcome);

    Ok(())
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
        "Usage: cargo xtask bump-version <major|minor|patch>\n",
        "\n",
        "Bumps [workspace.package] version in Cargo.toml and the top-level version\n",
        "in Packager.toml, and creates release_notes/vX.Y.Z/en.md and zh-CN.md when\n",
        "the version directory is missing. A version directory that already exists\n",
        "must hold both files."
    ));
}

#[derive(Debug, Error)]
enum CliError {
    #[error("missing subcommand; expected `bump-version`")]
    MissingCommand,
    #[error("unknown subcommand `{0}`; expected `bump-version`")]
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
}
