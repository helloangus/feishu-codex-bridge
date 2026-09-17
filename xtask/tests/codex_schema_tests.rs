//! Offline tests for the codex-schema maintenance commands.
//!
//! `export` is exercised end-to-end against a stub executable that mimics the
//! `--version` and `app-server generate-json-schema` surface, so no real
//! Codex binary, network or login is ever needed. `check` is additionally run
//! against the repository's committed snapshot and fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used)] // Test setup and assertion unwraps only.
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};
use xtask::codex_schema::{
    ExportOptions, REQUIRED_SCHEMA_FILES, build_manifest, check_required_files, check_roots,
    export_snapshot, parse_cli_version, run_check, verify_snapshot_dir,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "fcb-codex-schema-{}-{}-{}-{}",
        tag,
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

/// Version directories recorded in the committed repository fixtures.
fn repo_fixture_versions() -> Vec<String> {
    let root = repo_root().join("fixtures/codex");
    let mut versions: Vec<String> = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    versions.sort();
    versions
}

/// A schema that accepts every instance.
const MINIMAL_SCHEMA: &str = r#"{"type":"object"}"#;

/// A schema that rejects every instance (required field can never appear in
/// the recorded fixtures).
const REJECTING_SCHEMA: &str =
    r#"{"type":"object","required":["fcb-codex-schema-test-impossible"]}"#;

fn write_required_schemas(dir: &Path, body: &str) {
    fs::create_dir_all(dir).unwrap();
    for name in REQUIRED_SCHEMA_FILES {
        fs::write(dir.join(name), body).unwrap();
    }
}

/// Write one internally consistent schema snapshot below `schemas_root`.
fn make_snapshot(schemas_root: &Path, version: &str, schema_body: &str) {
    let dir = schemas_root.join(version);
    write_required_schemas(&dir, schema_body);
    let manifest = build_manifest(version, &dir).unwrap();
    fs::write(dir.join("manifest.json"), manifest).unwrap();
}

/// Write one minimal fixture set below `fixtures_root`.
fn make_fixtures(fixtures_root: &Path, version: &str) {
    let dir = fixtures_root.join(version);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("turn-start-default.json"), MINIMAL_SCHEMA).unwrap();
    // An empty case list needs no schema files and always validates.
    fs::write(dir.join("server-requests.json"), "[]").unwrap();
}

/// Install a stub Codex executable mimicking the two used entry points.
fn install_stub_codex(dir: &Path, version: &str, schema_body: &str) -> PathBuf {
    let names = REQUIRED_SCHEMA_FILES
        .iter()
        .map(|name| name.trim_end_matches(".json"))
        .collect::<Vec<_>>()
        .join(" ");
    let script = r#"#!/bin/bash
if [ "$1" = "--version" ]; then
  echo "codex-cli @VERSION@ (stub)"
  exit 0
fi
if [ "$1" = "app-server" ]; then
  out=""
  prev=""
  for argument in "$@"; do
    if [ "$prev" = "--out" ]; then out="$argument"; fi
    prev="$argument"
  done
  if [ -z "$out" ]; then
    echo "missing --out" >&2
    exit 1
  fi
  mkdir -p "$out"
  for name in @NAMES@; do
    printf '%s' '@SCHEMA@' > "$out/$name.json"
  done
  exit 0
fi
echo "unexpected invocation: $*" >&2
exit 1
"#
    .replace("@VERSION@", version)
    .replace("@NAMES@", &names)
    .replace("@SCHEMA@", schema_body);
    let path = dir.join("codex-stub");
    fs::write(&path, script).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn export_options(codex: PathBuf, version: Option<String>, force: bool) -> ExportOptions {
    ExportOptions {
        codex,
        version,
        force,
    }
}

#[test]
fn parses_versions_from_realistic_output() {
    assert_eq!(
        parse_cli_version("codex-cli 0.153.4 (aabbccdd 2026-01-01)").unwrap(),
        "0.153.4"
    );
    assert_eq!(parse_cli_version("0.154.0\n").unwrap(), "0.154.0");
    assert!(parse_cli_version("no version information").is_err());
    assert!(parse_cli_version("1.2 is not three components").is_err());
    assert!(parse_cli_version("codex-cli ..  (empty)").is_err());
}

#[test]
fn manifest_lists_files_with_digests_and_source_command() {
    let dir = temp_root("manifest");
    fs::write(dir.join("TurnStartParams.json"), MINIMAL_SCHEMA).unwrap();
    fs::write(dir.join("aaa-first.json"), MINIMAL_SCHEMA).unwrap();
    fs::write(dir.join("ignored.txt"), "not json").unwrap();
    let manifest = build_manifest("9.9.9", &dir).unwrap();
    let value: serde_json::Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(value["cli_version"], "9.9.9");
    assert_eq!(
        value["command"],
        "codex app-server generate-json-schema --experimental --out <directory>"
    );
    let files = value["files"].as_object().unwrap();
    assert_eq!(files.len(), 2);
    // BTreeMap ordering keeps the manifest deterministic.
    let mut names = files.keys().cloned().collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, vec!["TurnStartParams.json", "aaa-first.json"]);
    let digest = xtask::package::sha256_file(&dir.join("TurnStartParams.json")).unwrap();
    assert_eq!(files["TurnStartParams.json"], digest.as_str());
    // Deterministic regeneration.
    assert_eq!(build_manifest("9.9.9", &dir).unwrap(), manifest);
}

#[test]
fn manifest_requires_at_least_one_file() {
    let dir = temp_root("manifest-empty");
    assert!(build_manifest("9.9.9", &dir).is_err());
}

#[test]
fn required_files_check_reports_missing_and_invalid() {
    let dir = temp_root("required");
    write_required_schemas(&dir, MINIMAL_SCHEMA);
    check_required_files(&dir).unwrap();
    fs::remove_file(dir.join("ThreadListParams.json")).unwrap();
    fs::write(dir.join("TurnStartParams.json"), "not json").unwrap();
    let error = check_required_files(&dir).unwrap_err();
    assert!(error.contains("ThreadListParams.json"), "{error}");
    assert!(error.contains("TurnStartParams.json"), "{error}");
}

#[test]
fn check_passes_on_the_committed_repository() {
    run_check(&repo_root()).unwrap();
}

#[test]
fn manifest_version_must_match_the_directory_name() {
    let root = temp_root("manifest-version");
    let schemas_root = root.join("schemas/codex");
    make_snapshot(&schemas_root, "1.0.0", MINIMAL_SCHEMA);
    verify_snapshot_dir(&schemas_root.join("1.0.0"), "1.0.0").unwrap();
    let error = verify_snapshot_dir(&schemas_root.join("1.0.0"), "9.9.9").unwrap_err();
    assert!(
        error.contains("does not match the directory name"),
        "{error}"
    );
}

#[test]
fn check_requires_fixture_parity_for_every_snapshot() {
    let root = temp_root("parity");
    let schemas_root = root.join("schemas/codex");
    let fixtures_root = root.join("fixtures/codex");
    make_snapshot(&schemas_root, "1.0.0", MINIMAL_SCHEMA);
    // A missing or empty fixture root is rejected.
    let error = check_roots(&schemas_root, &fixtures_root).unwrap_err();
    assert!(
        error.contains("missing") || error.contains("no fixture"),
        "{error}"
    );
    make_fixtures(&fixtures_root, "1.0.0");
    check_roots(&schemas_root, &fixtures_root).unwrap();
    // An additional snapshot without fixtures cannot pass unnoticed.
    make_snapshot(&schemas_root, "2.0.0", MINIMAL_SCHEMA);
    let error = check_roots(&schemas_root, &fixtures_root).unwrap_err();
    assert!(error.contains("has no fixtures"), "{error}");
    // Fixtures without a matching snapshot are rejected as well.
    fs::remove_dir_all(schemas_root.join("2.0.0")).unwrap();
    make_fixtures(&fixtures_root, "9.9.9");
    let error = check_roots(&schemas_root, &fixtures_root).unwrap_err();
    assert!(error.contains("no matching schema snapshot"), "{error}");
    // Two empty roots are rejected explicitly.
    let empty_schemas = root.join("empty/schemas/codex");
    let empty_fixtures = root.join("empty/fixtures/codex");
    fs::create_dir_all(&empty_schemas).unwrap();
    fs::create_dir_all(&empty_fixtures).unwrap();
    let error = check_roots(&empty_schemas, &empty_fixtures).unwrap_err();
    assert!(error.contains("no schema snapshots"), "{error}");
}

#[test]
fn check_detects_tampered_digests_and_unknown_files() {
    let repo = repo_root();
    let baseline = repo_fixture_versions()
        .into_iter()
        .next()
        .expect("repository must record one fixture version");
    let root = temp_root("tamper");
    let schemas_root = root.join("schemas/codex");
    let fixtures_root = root.join("fixtures/codex");
    for entry in ["schemas", "fixtures"] {
        let target = root.join(entry).join("codex").join(&baseline);
        fs::create_dir_all(&target).unwrap();
        for name in fs::read_dir(repo.join(entry).join("codex").join(&baseline))
            .unwrap()
            .flatten()
        {
            fs::copy(name.path(), target.join(name.file_name())).unwrap();
        }
    }
    check_roots(&schemas_root, &fixtures_root).unwrap();
    // Tampering with one schema file must fail the digest check.
    fs::write(
        schemas_root.join(&baseline).join("InitializeParams.json"),
        "{}",
    )
    .unwrap();
    let error = check_roots(&schemas_root, &fixtures_root).unwrap_err();
    assert!(error.contains("digest mismatch"), "{error}");
    assert!(error.contains("InitializeParams.json"), "{error}");
    // Restoring the file and adding an unlisted schema file must fail too.
    fs::copy(
        repo.join("schemas/codex")
            .join(&baseline)
            .join("InitializeParams.json"),
        schemas_root.join(&baseline).join("InitializeParams.json"),
    )
    .unwrap();
    fs::write(schemas_root.join(&baseline).join("Extra.json"), "{}").unwrap();
    let error = check_roots(&schemas_root, &fixtures_root).unwrap_err();
    assert!(error.contains("not listed in manifest"), "{error}");
    fs::remove_file(schemas_root.join(&baseline).join("Extra.json")).unwrap();
    // Removing the only fixture set keeps failing check: the empty fixture
    // root is rejected before the per-snapshot parity rule can apply.
    fs::remove_dir_all(fixtures_root.join(&baseline)).unwrap();
    let error = check_roots(&schemas_root, &fixtures_root).unwrap_err();
    assert!(error.contains("no fixture directories"), "{error}");
}

#[test]
fn export_publishes_a_complete_snapshot_from_the_stub() {
    let repo = repo_root();
    let root = temp_root("export");
    let codex = install_stub_codex(&root, "0.154.0", MINIMAL_SCHEMA);
    let schemas_root = root.join("schemas/codex");
    export_snapshot(
        &schemas_root,
        &repo.join("fixtures/codex"),
        &export_options(codex, None, false),
    )
    .unwrap();
    let published = schemas_root.join("0.154.0");
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(published.join("manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["cli_version"], "0.154.0");
    for name in REQUIRED_SCHEMA_FILES {
        assert!(published.join(name).is_file(), "{name} missing");
        let digest = xtask::package::sha256_file(&published.join(name)).unwrap();
        assert_eq!(manifest["files"][name], digest.as_str());
    }
    verify_snapshot_dir(&published, "0.154.0").unwrap();
    // No staging leftovers next to the published snapshot.
    for entry in fs::read_dir(&schemas_root).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        assert!(!name.contains("staging"), "leftover staging dir: {name}");
    }
    // The freshly exported snapshot is not yet pinned: it lacks fixtures, so
    // check must keep failing until a maintainer records them.
    let error = check_roots(&schemas_root, &repo.join("fixtures/codex")).unwrap_err();
    assert!(error.contains("has no fixtures"), "{error}");
}

#[test]
fn export_rejects_version_conflicts_and_existing_snapshots() {
    let repo = repo_root();
    let root = temp_root("export-conflict");
    let codex = install_stub_codex(&root, "0.154.0", MINIMAL_SCHEMA);
    let schemas_root = root.join("schemas/codex");
    let fixtures_root = repo.join("fixtures/codex");
    let error = export_snapshot(
        &schemas_root,
        &fixtures_root,
        &export_options(codex.clone(), Some("9.9.9".into()), false),
    )
    .unwrap_err();
    assert!(error.contains("does not match"), "{error}");
    export_snapshot(
        &schemas_root,
        &fixtures_root,
        &export_options(codex.clone(), Some("0.154.0".into()), false),
    )
    .unwrap();
    // Same version again requires --force.
    let error = export_snapshot(
        &schemas_root,
        &fixtures_root,
        &export_options(codex.clone(), None, false),
    )
    .unwrap_err();
    assert!(error.contains("already exists"), "{error}");
    assert!(error.contains("--force"), "{error}");
    // --force replaces it; an unrelated older version is never touched.
    let older = schemas_root.join("0.100.0");
    fs::create_dir_all(&older).unwrap();
    fs::write(older.join("sentinel"), "keep me").unwrap();
    export_snapshot(
        &schemas_root,
        &fixtures_root,
        &export_options(codex, None, true),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(older.join("sentinel")).unwrap(),
        "keep me"
    );
    assert!(schemas_root.join("0.154.0").join("manifest.json").is_file());
}

#[test]
fn export_aborts_on_incompatible_fixtures_unless_forced() {
    let repo = repo_root();
    let root = temp_root("export-incompatible");
    let codex = install_stub_codex(&root, "0.155.0", REJECTING_SCHEMA);
    let schemas_root = root.join("schemas/codex");
    let fixtures_root = repo.join("fixtures/codex");
    let recorded = repo_fixture_versions();
    assert!(!recorded.is_empty(), "repository must record fixtures");
    let error = export_snapshot(
        &schemas_root,
        &fixtures_root,
        &export_options(codex.clone(), None, false),
    )
    .unwrap_err();
    assert!(error.contains("step 'compatibility'"), "{error}");
    assert!(!schemas_root.join("0.155.0").exists(), "nothing published");
    export_snapshot(
        &schemas_root,
        &fixtures_root,
        &export_options(codex, None, true),
    )
    .unwrap();
    assert!(schemas_root.join("0.155.0").join("manifest.json").is_file());
}
