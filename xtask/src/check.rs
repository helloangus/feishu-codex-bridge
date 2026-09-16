//! `cargo xtask check`: single verification entry point.
//!
//! Covers every command listed in AGENTS.md plus packaging structure checks.
//! Dependency fetching is a separate explicit action (`fetch`); check itself
//! always passes `--locked` and optionally `--offline` to cargo.
use crate::{boundaries, cargo_cli, hygiene, package};
use std::path::Path;

pub struct CheckOptions {
    pub fetch: bool,
    pub offline: bool,
}

fn cargo_args(base: &[&str], offline: bool, tail: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = base.iter().map(|arg| (*arg).to_string()).collect();
    if offline {
        args.push("--offline".to_string());
    }
    for arg in tail {
        args.push((*arg).to_string());
    }
    args
}

fn step(name: &str, action: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    println!("==> xtask check: {name}");
    action().inspect_err(|_| {
        eprintln!("xtask check step '{name}' failed");
    })
}

/// Run `cargo fetch --locked` so later steps can execute without the network.
pub fn run_fetch(repo: &Path, offline: bool) -> Result<(), String> {
    let args = cargo_args(&["fetch", "--locked", "-j", "1"], offline, &[]);
    cargo_cli::run_streamed(&cargo_cli::cargo_program(), &args, &[], repo)
}

fn boundaries_step(repo: &Path) -> Result<(), String> {
    let metadata = cargo_cli::cargo_metadata_locked(repo)?;
    boundaries::check_metadata(&metadata)?;
    println!("crate dependency boundaries passed");
    Ok(())
}

fn shell_syntax_step(repo: &Path) -> Result<(), String> {
    cargo_cli::run_streamed(
        "bash",
        &[
            "-n".to_string(),
            "setup.sh".to_string(),
            "start.sh".to_string(),
            "package.sh".to_string(),
        ],
        &[],
        repo,
    )
}

fn packaging_structure_step(repo: &Path) -> Result<(), String> {
    // Structural check without a release build: assemble with a placeholder
    // binary through the same code path used for real packaging.
    let temp = std::env::temp_dir();
    let input_dir = package::create_unique_dir(&temp, "fcb-package-check-input")?;
    let staging = package::create_unique_dir(&temp, "fcb-package-check")?;
    let placeholder = input_dir.join("bridge");
    std::fs::write(&placeholder, b"packaging structure check placeholder\n")
        .map_err(|error| format!("cannot write placeholder binary: {error}"))?;
    let inputs = package::PackageInputs {
        executable: placeholder,
        example_toml: repo.join("bridge.example.toml"),
        deployment_doc: repo.join("docs/deployment.md"),
    };
    let info = package::BuildInfo {
        package: "bridge-cli".to_string(),
        version: "0.0.0".to_string(),
        commit: "packaging-structure-check".to_string(),
        dirty: false,
        allow_dirty: false,
        dirty_files: Vec::new(),
        target: "structure-check".to_string(),
        profile: "structure-check".to_string(),
        rustc_version: "structure-check".to_string(),
        cargo_version: "structure-check".to_string(),
    };
    let outcome = package::assemble_package(&staging, &inputs, &info)
        .and_then(|()| package::validate_package(&staging));
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_dir_all(&input_dir);
    outcome?;
    println!("package structure and document links passed");
    Ok(())
}

/// Run the full verification pipeline; the first failing step aborts.
pub fn run_check(repo: &Path, options: &CheckOptions) -> Result<(), String> {
    let offline = options.offline;
    if options.fetch {
        step("fetch", || run_fetch(repo, offline))?;
    }
    step("fmt", || {
        cargo_cli::run_streamed(
            &cargo_cli::cargo_program(),
            &cargo_args(&["fmt", "--all", "--", "--check"], false, &[]),
            &[],
            repo,
        )
    })?;
    step("clippy", || {
        cargo_cli::run_streamed(
            &cargo_cli::cargo_program(),
            &cargo_args(
                &[
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--locked",
                    "-j",
                    "1",
                ],
                offline,
                &["--", "-D", "warnings"],
            ),
            &[],
            repo,
        )
    })?;
    step("test", || {
        cargo_cli::run_streamed(
            &cargo_cli::cargo_program(),
            &cargo_args(
                &["test", "--workspace", "--locked", "-j", "1"],
                offline,
                &["--", "--test-threads=1"],
            ),
            &[],
            repo,
        )
    })?;
    step("build", || {
        cargo_cli::run_streamed(
            &cargo_cli::cargo_program(),
            &cargo_args(
                &[
                    "build",
                    "-p",
                    "bridge-cli",
                    "--bin",
                    "bridge",
                    "--locked",
                    "-j",
                    "1",
                ],
                offline,
                &[],
            ),
            &[],
            repo,
        )
    })?;
    step("doc", || {
        cargo_cli::run_streamed(
            &cargo_cli::cargo_program(),
            &cargo_args(
                &["doc", "--workspace", "--no-deps", "--locked", "-j", "1"],
                offline,
                &[],
            ),
            &[("RUSTDOCFLAGS", "-D warnings")],
            repo,
        )
    })?;
    step("boundaries", || boundaries_step(repo))?;
    step("hygiene", || hygiene::run(repo))?;
    step("shell-syntax", || shell_syntax_step(repo))?;
    step("git-diff-check", || {
        cargo_cli::git_captured(&["diff", "--check"]).map(|_| ())
    })?;
    step("packaging-structure", || packaging_structure_step(repo))?;
    println!("xtask check passed");
    Ok(())
}

/// `cargo xtask check` command entry.
pub fn run_command(args: &[String]) -> Result<(), String> {
    let mut options = CheckOptions {
        fetch: false,
        offline: false,
    };
    for arg in args {
        match arg.as_str() {
            "--fetch" => options.fetch = true,
            "--offline" => options.offline = true,
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            other => {
                return Err(format!(
                    "unknown check option '{other}'; see 'cargo xtask check --help'"
                ));
            }
        }
    }
    let repo = cargo_cli::git_repo_root()?;
    run_check(&repo, &options)
}

/// `cargo xtask fetch` command entry.
pub fn run_fetch_command(args: &[String]) -> Result<(), String> {
    for arg in args {
        match arg.as_str() {
            "--offline" => {}
            "--help" | "-h" => {
                println!("usage: cargo xtask fetch [--offline]");
                return Ok(());
            }
            other => {
                return Err(format!(
                    "unknown fetch option '{other}'; see 'cargo xtask fetch --help'"
                ));
            }
        }
    }
    let repo = cargo_cli::git_repo_root()?;
    run_fetch(&repo, false)
}

fn print_usage() {
    println!(
        "usage: cargo xtask check [--fetch] [--offline]\n\
         \n\
         Runs: fmt, clippy, workspace tests (serial), product debug build,\n\
         rustdoc (warnings as errors), dependency boundaries, repository\n\
         hygiene, shell syntax, git diff --check and packaging structure.\n\
         \n\
         Options:\n\
         \x20 --fetch     run 'cargo fetch --locked' before the checks\n\
         \x20 --offline   pass --offline to every cargo invocation"
    );
}
