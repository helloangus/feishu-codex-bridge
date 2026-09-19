//! Repository hygiene gate: no Python source, bytecode, manifests or
//! interpreter calls in tracked files.
//!
//! All tracked files are scanned, including YAML workflows: Dependabot no
//! longer carries pip updates, so nothing needs to be excluded from the scan.
//! The only exclusion is this module's own source file, which must name the
//! banned patterns to ban them.
use std::{path::Path, process::Stdio};

/// Path of this module, exempted from the content scan (self-reference).
pub const SELF_PATH: &str = "xtask/src/hygiene.rs";

/// File name suffixes that mark Python source, bytecode or manifests.
pub const BANNED_SUFFIXES: &[&str] = &[".py", ".pyi", ".pyc"];

/// Interpreter or installer invocations that must not appear in tracked files.
pub const BANNED_PATTERNS: &[&str] = &["python3", "python -m", "pip install"];

/// Check a single tracked file (path with forward slashes) for violations.
pub fn check_entry(name: &str, content: &[u8]) -> Result<(), String> {
    if BANNED_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
        return Err(format!("tracked Python source or bytecode file '{name}'"));
    }
    let base = name.rsplit('/').next().unwrap_or(name);
    if base.starts_with("requirements") && base.ends_with(".txt") {
        return Err(format!("tracked Python dependency manifest '{name}'"));
    }
    let text = String::from_utf8_lossy(content);
    for (index, line) in text.lines().enumerate() {
        for pattern in BANNED_PATTERNS {
            if line.contains(pattern) {
                return Err(format!(
                    "tracked file '{name}' line {} contains banned pattern '{pattern}'",
                    index + 1
                ));
            }
        }
    }
    Ok(())
}

/// Enumerate tracked files via `git ls-files -z` and scan each one.
pub fn run(repo: &Path) -> Result<(), String> {
    let output = std::process::Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("cannot spawn git ls-files: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let listed = String::from_utf8_lossy(&output.stdout).to_string();
    let mut scanned = 0usize;
    for entry in listed.split('\0') {
        if entry.is_empty() {
            continue;
        }
        if entry == SELF_PATH {
            // This file names the banned patterns themselves.
            continue;
        }
        let content = std::fs::read(repo.join(entry))
            .map_err(|error| format!("cannot read tracked file '{entry}': {error}"))?;
        check_entry(entry, &content)?;
        scanned += 1;
    }
    println!("repository hygiene passed ({scanned} tracked files scanned, Python-free)");
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Assertion unwraps only.
    use super::{BANNED_PATTERNS, check_entry};

    #[test]
    fn python_source_is_rejected_by_suffix() {
        assert!(check_entry("tools/script.py", b"").is_err());
        assert!(check_entry("cache.pyc", b"").is_err());
        assert!(check_entry("stubs.pyi", b"").is_err());
        assert!(check_entry("requirements-dev.txt", b"").is_err());
        assert!(check_entry("requirements.txt", b"").is_err());
    }

    #[test]
    fn interpreter_calls_are_rejected_even_in_yaml() {
        let content = b"run: pip install wheel\n";
        let error = check_entry(".github/workflows/ci.yml", content).unwrap_err();
        assert!(error.contains("line 1"), "{error}");
        assert!(error.contains("pip install"), "{error}");
        let content = b"image: build.step\nrun: python3 -V\n";
        assert!(check_entry(".gitlab.yml", content).is_err());
        let content = b"run: python -m venv env\n";
        assert!(check_entry("docs/note.md", content).is_err());
    }

    #[test]
    fn clean_files_pass() {
        assert!(check_entry("crates/bridge-app/src/lib.rs", b"tokio::runtime();").is_ok());
        assert!(check_entry(".github/workflows/ci.yml", b"run: cargo xtask check\n").is_ok());
        assert!(check_entry("package.sh", b"#!/usr/bin/env bash\n").is_ok());
    }

    #[test]
    fn patterns_are_stable() {
        assert_eq!(BANNED_PATTERNS.len(), 3);
    }
}
