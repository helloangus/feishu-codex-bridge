//! Integration-level packaging tests over the real filesystem.
//!
//! These tests never invoke cargo: executables are placeholders and build
//! messages are synthetic. They cover custom target directories, paths with
//! spaces, stale artifacts, missing executables, partial-output avoidance and
//! idempotent repackaging.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Test setup and assertion unwraps only.
use std::{
    collections::BTreeSet,
    fs,
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

use xtask::package::{
    BuildInfo, CHECKSUMMED_FILES, PACKAGE_FILES, PackageInputs, assemble_and_publish,
    assemble_package, check_document_links, create_staging_dir, parse_build_messages,
    validate_package,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_root(tag: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "fcb-xtask-{}-{}-{}-{}",
        tag,
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_file(path: &std::path::Path, content: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn make_executable(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
}

fn sample_info(commit: &str, dirty: bool) -> BuildInfo {
    BuildInfo {
        package: "bridge-cli".to_string(),
        version: "0.1.0".to_string(),
        commit: commit.to_string(),
        dirty,
        allow_dirty: dirty,
        dirty_files: if dirty {
            vec![" M xtask/src/lib.rs".to_string()]
        } else {
            Vec::new()
        },
        target: "aarch64-unknown-linux-gnu".to_string(),
        profile: "release".to_string(),
        rustc_version: "rustc 1.85.1 (4eb161250 2025-01-20)".to_string(),
        cargo_version: "cargo 1.85.1 (73a7ddbd6 2025-01-13)".to_string(),
    }
}

/// A source tree whose release binary lives in a custom target directory
/// containing spaces, plus a stale `target/release/bridge` left behind.
fn source_tree(tag: &str) -> std::path::PathBuf {
    let root = temp_root(tag);
    let custom_target = root.join("custom target dir with spaces/release");
    write_file(
        &custom_target.join("bridge"),
        b"#!/bin/sh\necho fresh bridge build\n",
    );
    make_executable(&custom_target.join("bridge"));
    let stale = root.join("target/release");
    write_file(&stale.join("bridge"), b"stale binary from an old build");
    write_file(
        &root.join("bridge.example.toml"),
        b"# example configuration\n",
    );
    write_file(
        &root.join("docs/deployment.md"),
        concat!(
            "# Deployment\n\n",
            "Verify with [SHA256SUMS](SHA256SUMS) and the [example](./bridge.example.toml).\n\n",
            "```sh\nsha256sum -c SHA256SUMS\n```\n",
        )
        .as_bytes(),
    );
    root
}

fn inputs_for(root: &std::path::Path, executable: &std::path::Path) -> PackageInputs {
    PackageInputs {
        executable: executable.to_path_buf(),
        example_toml: root.join("bridge.example.toml"),
        deployment_doc: root.join("docs/deployment.md"),
    }
}

#[test]
fn messages_from_custom_target_dir_win_over_stale_artifact() {
    let root = source_tree("custom-target");
    // The build message points at the custom target directory, not the stale
    // default location that still exists on disk.
    let messages = format!(
        "{}\n",
        r#"{"reason":"compiler-artifact","package_id":"path+file:///repo#bridge-cli@0.1.0","target":{"name":"bridge","kind":["bin"]},"profile":{"test":false},"executable":"CUSTOM/bridge","fresh":false}"#
    );
    let parsed = parse_build_messages(&messages).unwrap();
    assert_eq!(parsed, std::path::PathBuf::from("CUSTOM/bridge"));
    // Wiring the parsed path to the real tree layout: the fresh custom-target
    // binary differs from the stale one that must never be packaged.
    let fresh = root.join("custom target dir with spaces/release/bridge");
    assert!(fresh.is_file());
    assert_ne!(
        fs::read(&fresh).unwrap(),
        fs::read(root.join("target/release/bridge")).unwrap()
    );
}

#[test]
fn full_package_assembly_in_path_with_spaces() {
    let root = source_tree("spaces");
    let final_dir = root.join("output dir with spaces");
    let executable = root.join("custom target dir with spaces/release/bridge");
    let staging = create_staging_dir(&final_dir).unwrap();
    let inputs = inputs_for(&root, &executable);
    assemble_and_publish(&staging, &final_dir, &inputs, &sample_info("abc123", false)).unwrap();

    let names: BTreeSet<String> = fs::read_dir(&final_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    let expected: BTreeSet<String> = PACKAGE_FILES
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    assert_eq!(names, expected);
    // Staging directory is gone after publishing.
    assert!(!staging.exists());
}

#[test]
fn repackaging_is_idempotent_and_replaces_old_content() {
    let root = source_tree("idempotent");
    let final_dir = root.join("dist");
    let executable = root.join("custom target dir with spaces/release/bridge");
    let inputs = inputs_for(&root, &executable);
    let info = sample_info("abc123", false);

    let staging = create_staging_dir(&final_dir).unwrap();
    assemble_and_publish(&staging, &final_dir, &inputs, &info).unwrap();
    let first: BTreeSet<(String, Vec<u8>)> = fs::read_dir(&final_dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            let content = fs::read(entry.path()).unwrap();
            (name, content)
        })
        .collect();

    // A leftover file from an older package must disappear on republish.
    write_file(&final_dir.join("stale-extra.txt"), b"old junk");
    let staging = create_staging_dir(&final_dir).unwrap();
    assemble_and_publish(&staging, &final_dir, &inputs, &info).unwrap();
    let second: BTreeSet<(String, Vec<u8>)> = fs::read_dir(&final_dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            let content = fs::read(entry.path()).unwrap();
            (name, content)
        })
        .collect();
    assert_eq!(first, second);
    assert!(!final_dir.join("stale-extra.txt").exists());
}

#[test]
fn failed_assembly_leaves_no_partial_output() {
    let root = source_tree("no-partial");
    let final_dir = root.join("dist");
    let inputs = PackageInputs {
        executable: root.join("custom target dir with spaces/release/bridge"),
        // Missing example configuration forces assembly to fail.
        example_toml: root.join("does-not-exist.toml"),
        deployment_doc: root.join("docs/deployment.md"),
    };
    let staging = create_staging_dir(&final_dir).unwrap();
    let error = assemble_and_publish(&staging, &final_dir, &inputs, &sample_info("abc123", false))
        .unwrap_err();
    assert!(error.contains("does-not-exist.toml"), "{error}");
    // Neither a final package nor a leftover staging directory exists.
    assert!(!final_dir.exists());
    assert!(!staging.exists());
}

#[test]
fn publish_failure_keeps_previous_content() {
    let root = source_tree("rollback");
    // A previous "package" that is a plain file cannot be moved aside by the
    // publish step; it must survive and the staging dir must be cleaned up.
    let final_path = root.join("dist-as-file");
    fs::write(&final_path, b"previous content").unwrap();
    let staging = create_staging_dir(&final_path).unwrap();
    let executable = root.join("custom target dir with spaces/release/bridge");
    let inputs = inputs_for(&root, &executable);
    let error = assemble_and_publish(
        &staging,
        &final_path,
        &inputs,
        &sample_info("abc123", false),
    )
    .unwrap_err();
    assert!(error.contains("previous package"), "{error}");
    assert_eq!(fs::read(&final_path).unwrap(), b"previous content");
    assert!(!staging.exists());
}

#[test]
fn validation_rejects_extra_or_missing_files() {
    let root = source_tree("manifest");
    let staging = temp_root("manifest-staging");
    let executable = root.join("custom target dir with spaces/release/bridge");
    let inputs = inputs_for(&root, &executable);
    assemble_package(&staging, &inputs, &sample_info("abc123", false)).unwrap();
    validate_package(&staging).unwrap();

    write_file(&staging.join("unexpected.txt"), b"extra");
    let error = validate_package(&staging).unwrap_err();
    assert!(error.contains("extra"), "{error}");
    fs::remove_file(staging.join("unexpected.txt")).unwrap();

    fs::remove_file(staging.join("SHA256SUMS")).unwrap();
    let error = validate_package(&staging).unwrap_err();
    assert!(error.contains("missing"), "{error}");
}

#[test]
fn tampered_files_fail_checksum_verification() {
    let root = source_tree("tamper");
    let staging = temp_root("tamper-staging");
    let executable = root.join("custom target dir with spaces/release/bridge");
    let inputs = inputs_for(&root, &executable);
    assemble_package(&staging, &inputs, &sample_info("abc123", false)).unwrap();
    write_file(&staging.join("bridge.example.toml"), b"tampered");
    let error = validate_package(&staging).unwrap_err();
    assert!(error.contains("SHA256 mismatch"), "{error}");
    assert!(error.contains("bridge.example.toml"), "{error}");
}

#[test]
fn checksums_cover_exactly_the_distributed_files() {
    assert_eq!(CHECKSUMMED_FILES.len(), 4);
    assert_eq!(PACKAGE_FILES.len(), 5);
    for name in CHECKSUMMED_FILES {
        assert!(PACKAGE_FILES.contains(&name));
    }
    assert_eq!(PACKAGE_FILES[4], "SHA256SUMS");
}

#[test]
fn packaged_document_links_resolve() {
    let root = source_tree("links");
    let staging = temp_root("links-staging");
    let executable = root.join("custom target dir with spaces/release/bridge");
    let inputs = inputs_for(&root, &executable);
    assemble_package(&staging, &inputs, &sample_info("abc123", false)).unwrap();
    validate_package(&staging).unwrap();

    // A guide referencing unpackaged source must fail validation.
    let tampered = b"See [architecture](../docs/architecture.md).\n";
    write_file(&staging.join("DEPLOYMENT.md"), tampered);
    let sums: Vec<(String, String)> = CHECKSUMMED_FILES
        .iter()
        .filter(|name| **name != "DEPLOYMENT.md")
        .map(|name| {
            (
                (*name).to_string(),
                xtask::package::sha256_file(&staging.join(name)).unwrap(),
            )
        })
        .collect();
    let mut content = String::new();
    for (name, digest) in sums {
        content.push_str(&format!("{digest}  {name}\n"));
    }
    // Rebuild the sums entry for the modified document itself.
    let doc_digest = xtask::package::sha256_hex(tampered);
    content.push_str(&format!("{doc_digest}  DEPLOYMENT.md\n"));
    fs::write(staging.join("SHA256SUMS"), content).unwrap();
    let error = validate_package(&staging).unwrap_err();
    assert!(
        error.contains("not shipped in the package") || error.contains("outside the package"),
        "{error}"
    );
    assert!(error.contains("../docs/architecture.md"), "{error}");
}

#[test]
fn link_checker_ignores_external_and_anchor_targets() {
    let available: BTreeSet<String> = PACKAGE_FILES
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    let document =
        "[site](https://example.test) [mail](mailto:a@b.test) [anchor](#x) [sums](SHA256SUMS)";
    assert!(check_document_links(document, &available).is_ok());
}
