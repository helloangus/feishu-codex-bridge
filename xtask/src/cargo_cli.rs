//! Process helpers for cargo, rustc and git invocations.
//!
//! Every cargo invocation goes through here so that job limits stay explicit,
//! nested cargo runs see a stable environment, and child stderr is passed
//! through for diagnosable, unmodified output.
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Environment variables injected by `cargo run` (the `cargo xtask` alias).
///
/// Nested cargo builds fingerprint their environment; keeping these variables
/// makes every `cargo xtask`-driven build look different from a plain shell
/// build and triggers spurious full-workspace rebuilds. They are stripped
/// before any nested cargo invocation so packaging and checks reuse the same
/// artifacts regardless of how xtask was launched.
const RUN_INJECTED_ENV: &[&str] = &[
    "CARGO_MANIFEST_DIR",
    "CARGO_MANIFEST_PATH",
    "CARGO_PKG_AUTHORS",
    "CARGO_PKG_DESCRIPTION",
    "CARGO_PKG_HOMEPAGE",
    "CARGO_PKG_LICENSE",
    "CARGO_PKG_LICENSE_FILE",
    "CARGO_PKG_NAME",
    "CARGO_PKG_README",
    "CARGO_PKG_REPOSITORY",
    "CARGO_PKG_RUST_VERSION",
    "CARGO_PKG_VERSION",
    "CARGO_PKG_VERSION_MAJOR",
    "CARGO_PKG_VERSION_MINOR",
    "CARGO_PKG_VERSION_PATCH",
    "CARGO_PKG_VERSION_PRE",
    "LD_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "RUST_RECURSION_COUNT",
];

/// Cargo binary to drive; `CARGO` is exported by `cargo run`/`cargo xtask`.
pub fn cargo_program() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

fn prepare_command(program: &str) -> Command {
    let mut command = Command::new(program);
    for key in RUN_INJECTED_ENV {
        command.env_remove(key);
    }
    command
}

/// Repository root as reported by git; all repo-relative paths resolve here.
pub fn git_repo_root() -> Result<PathBuf, String> {
    let out = git_captured(&["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(out.trim());
    if root.as_os_str().is_empty() {
        return Err("git rev-parse --show-toplevel returned an empty path".to_string());
    }
    Ok(root)
}

/// Run git with both streams captured; stdout is returned as UTF-8 text.
pub fn git_captured(args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("cannot spawn git {args:?}: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {args:?} failed: {}", stderr.trim()));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("git {args:?} printed non-UTF-8 output: {error}"))
}

/// Run a program with inherited stdio so diagnostics stream to the user.
pub fn run_streamed(
    program: &str,
    args: &[String],
    envs: &[(&str, &str)],
    cwd: &Path,
) -> Result<(), String> {
    let mut command = prepare_command(program);
    command.args(args).current_dir(cwd);
    for (key, value) in envs {
        command.env(key, value);
    }
    let status = command
        .status()
        .map_err(|error| format!("cannot spawn {program} {args:?}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} {args:?} failed with {status}"))
    }
}

/// Run a program capturing stdout while stderr stays inherited (live output).
pub fn run_capturing_stdout(
    program: &str,
    args: &[String],
    envs: &[(&str, &str)],
    cwd: &Path,
) -> Result<Vec<u8>, String> {
    let mut command = prepare_command(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot spawn {program} {args:?}: {error}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{program} {args:?} did not provide a stdout pipe"))?;
    let mut buffer = Vec::new();
    stdout
        .read_to_end(&mut buffer)
        .map_err(|error| format!("cannot read {program} stdout: {error}"))?;
    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for {program} {args:?}: {error}"))?;
    if status.success() {
        Ok(buffer)
    } else {
        Err(format!("{program} {args:?} failed with {status}"))
    }
}

/// `cargo metadata --format-version 1 --no-deps --locked` as parsed JSON.
pub fn cargo_metadata_locked(cwd: &Path) -> Result<serde_json::Value, String> {
    let args = ["metadata", "--format-version", "1", "--no-deps", "--locked"];
    let mut command = prepare_command(&cargo_program());
    command
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command
        .output()
        .map_err(|error| format!("cannot spawn cargo metadata: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("cargo metadata failed: {}", stderr.trim()));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("cargo metadata printed invalid JSON: {error}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Assertion unwraps only.
    use super::RUN_INJECTED_ENV;

    #[test]
    fn run_injected_environment_is_covered() {
        // The variables observed in `cargo run` children on this toolchain
        // must stay on the strip list, or nested builds rebuild the world.
        for key in [
            "CARGO_MANIFEST_DIR",
            "CARGO_PKG_NAME",
            "LD_LIBRARY_PATH",
            "RUST_RECURSION_COUNT",
        ] {
            assert!(RUN_INJECTED_ENV.contains(&key), "{key} missing");
        }
    }
}
