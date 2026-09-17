//! `cargo xtask codex-schema`: pinned Codex protocol snapshot maintenance.
//!
//! - `check` verifies every recorded snapshot under `schemas/codex/<version>/`
//!   against its provenance `manifest.json` and validates the fixtures under
//!   `fixtures/codex/<version>/`; it runs fully offline and is part of
//!   `cargo xtask check`.
//! - `export` regenerates one snapshot with an explicitly provided Codex
//!   executable. It is a manual maintenance step, never part of CI.
//!
//! xtask deliberately depends on no production crate: the `cargo xtask check`
//! pipeline is bootstrapped through this package and must keep building while
//! production code is refactored. The pinned baseline therefore stays in
//! `bridge_codex::protocol::CODEX_SCHEMA_BASELINE`; its agreement with the
//! snapshot directory name and manifest is asserted by bridge-codex's own
//! protocol tests, which `cargo xtask check` runs in its test step. This
//! command verifies every snapshot directory independently of that constant.
use crate::cargo_cli;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Repository-relative root of the versioned schema snapshots.
pub const SCHEMA_ROOT: &str = "schemas/codex";

/// Repository-relative root of the recorded protocol fixtures.
pub const FIXTURE_ROOT: &str = "fixtures/codex";

/// Source command recorded in every generated `manifest.json`.
pub const SOURCE_COMMAND: &str =
    "codex app-server generate-json-schema --experimental --out <directory>";

/// Schema files a complete snapshot must contain.
pub const REQUIRED_SCHEMA_FILES: [&str; 9] = [
    "InitializeParams.json",
    "TurnStartParams.json",
    "ThreadListParams.json",
    "CommandExecutionRequestApprovalParams.json",
    "CommandExecutionRequestApprovalResponse.json",
    "FileChangeRequestApprovalParams.json",
    "FileChangeRequestApprovalResponse.json",
    "ToolRequestUserInputParams.json",
    "ToolRequestUserInputResponse.json",
];

/// Options of the `export` subcommand.
pub struct ExportOptions {
    /// Explicit Codex executable; `PATH` is never consulted.
    pub codex: PathBuf,
    /// Override the version discovered from `<codex> --version`.
    pub version: Option<String>,
    /// Replace an existing snapshot of the same version.
    pub force: bool,
}

fn sanitize(text: &str) -> String {
    const LIMIT: usize = 2000;
    let trimmed = text.trim();
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_owned();
    }
    let mut head: String = trimmed.chars().take(LIMIT).collect();
    head.push_str("…[truncated]");
    head
}

fn is_semver(token: &str) -> bool {
    let mut components = 0;
    for part in token.split('.') {
        components += 1;
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
    }
    components == 3
}

/// Extract the first `x.y.z` version token from `<codex> --version` output.
pub fn parse_cli_version(output: &str) -> Result<String, String> {
    for token in output.split_whitespace() {
        if is_semver(token) {
            return Ok(token.to_owned());
        }
    }
    Err(format!(
        "no x.y.z version found in the executable's --version output: {}",
        sanitize(output)
    ))
}

/// Run the Codex executable directly (no shell, no `PATH` resolution).
fn run_codex(codex: &Path, args: &[&str], step: &str) -> Result<String, String> {
    let output = Command::new(codex)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| format!("step '{step}': cannot run {}: {error}", codex.display()))?;
    if !output.status.success() {
        return Err(format!(
            "step '{step}': {} {args:?} failed with {}; stderr: {}",
            codex.display(),
            output.status,
            sanitize(&String::from_utf8_lossy(&output.stderr))
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn list_json_files(dir: &Path) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for entry in
        fs::read_dir(dir).map_err(|error| format!("cannot list {}: {error}", dir.display()))?
    {
        let entry = entry.map_err(|error| format!("cannot list {}: {error}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".json") {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Sorted version-directory names below `root` (the root itself must exist).
fn list_version_dirs(root: &Path) -> Result<Vec<String>, String> {
    if !root.is_dir() {
        return Err(format!("snapshot root {} is missing", root.display()));
    }
    let mut versions = Vec::new();
    for entry in
        fs::read_dir(root).map_err(|error| format!("cannot list {}: {error}", root.display()))?
    {
        let entry = entry.map_err(|error| format!("cannot list {}: {error}", root.display()))?;
        if !entry
            .file_type()
            .map_err(|error| format!("cannot inspect {}: {error}", entry.path().display()))?
            .is_dir()
        {
            continue;
        }
        versions.push(entry.file_name().to_string_lossy().into_owned());
    }
    versions.sort();
    Ok(versions)
}

/// Build the provenance manifest for freshly generated schema files.
pub fn build_manifest(cli_version: &str, generated: &Path) -> Result<String, String> {
    let mut files = BTreeMap::new();
    for name in list_json_files(generated)? {
        let digest = crate::package::sha256_file(&generated.join(&name))?;
        files.insert(name, digest);
    }
    if files.is_empty() {
        return Err("schema generation produced no .json files".to_string());
    }
    let manifest = json!({"cli_version": cli_version, "command": SOURCE_COMMAND, "files": files});
    let mut text = serde_json::to_string_pretty(&manifest)
        .map_err(|error| format!("cannot serialize manifest: {error}"))?;
    text.push('\n');
    Ok(text)
}

/// Verify that a generated snapshot contains every required schema file.
pub fn check_required_files(generated: &Path) -> Result<(), String> {
    let mut missing = Vec::new();
    let mut invalid = Vec::new();
    for name in REQUIRED_SCHEMA_FILES {
        let path = generated.join(name);
        match fs::read(&path) {
            Ok(bytes) => {
                if serde_json::from_slice::<Value>(&bytes).is_err() {
                    invalid.push(name);
                }
            }
            Err(_) => missing.push(name),
        }
    }
    if missing.is_empty() && invalid.is_empty() {
        return Ok(());
    }
    let mut message = String::from("required schema files are incomplete:");
    if !missing.is_empty() {
        message.push_str(&format!(" missing {missing:?}"));
    }
    if !invalid.is_empty() {
        message.push_str(&format!(" not valid JSON {invalid:?}"));
    }
    Err(message)
}

fn load_schema(schema_dir: &Path, name: &str) -> Result<Value, String> {
    let bytes = fs::read(schema_dir.join(name))
        .map_err(|error| format!("cannot read schema '{name}': {error}"))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("schema '{name}' is invalid: {error}"))
}

fn validate_instance(failures: &mut Vec<String>, label: &str, schema: &Value, instance: &Value) {
    let validator = match jsonschema::validator_for(schema) {
        Ok(validator) => validator,
        Err(error) => {
            failures.push(format!("{label}: schema itself is invalid: {error}"));
            return;
        }
    };
    if let Err(error) = validator.validate(instance) {
        failures.push(format!("{label}: {error}"));
    }
}

/// Validate every recorded fixture against its pinned schema; returns one
/// description per failure so callers can report the full list.
pub fn fixture_failures(fixtures_dir: &Path, schema_dir: &Path) -> Result<Vec<String>, String> {
    let mut failures = Vec::new();
    let names = list_json_files(fixtures_dir)?;
    if names.is_empty() {
        return Err(format!("no fixtures found in {}", fixtures_dir.display()));
    }
    for name in names {
        let path = fixtures_dir.join(&name);
        let value: Value = match fs::read(&path)
            .map_err(|error| format!("cannot read fixture '{name}': {error}"))
            .and_then(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|error| format!("fixture '{name}' is invalid: {error}"))
            }) {
            Ok(value) => value,
            Err(message) => {
                failures.push(message);
                continue;
            }
        };
        if name.starts_with("turn-start-") {
            match load_schema(schema_dir, "TurnStartParams.json") {
                Ok(schema) => {
                    validate_instance(&mut failures, &format!("fixture '{name}'"), &schema, &value)
                }
                Err(message) => failures.push(message),
            }
        } else if name == "server-requests.json" {
            let Some(cases) = value.as_array() else {
                failures.push(format!("fixture '{name}' must contain an array of cases"));
                continue;
            };
            for (index, case) in cases.iter().enumerate() {
                let Some(base) = case["schema"].as_str() else {
                    failures.push(format!("fixture '{name}' case {index} has no schema name"));
                    continue;
                };
                for (suffix, field) in [("Params", "params"), ("Response", "reply")] {
                    let schema_name = format!("{base}{suffix}.json");
                    let label = format!("fixture '{name}' case {index} field '{field}'");
                    match load_schema(schema_dir, &schema_name) {
                        Ok(schema) => {
                            validate_instance(&mut failures, &label, &schema, &case[field])
                        }
                        Err(message) => failures.push(format!("{label}: {message}")),
                    }
                }
            }
        } else {
            failures.push(format!("unrecognized fixture '{name}'"));
        }
    }
    Ok(failures)
}

fn validate_fixtures(fixtures_dir: &Path, schema_dir: &Path) -> Result<(), String> {
    let failures = fixture_failures(fixtures_dir, schema_dir)?;
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

/// Verify one snapshot directory: manifest provenance, digests, required
/// files and no unlisted schema files.
pub fn verify_snapshot_dir(schema_dir: &Path, version: &str) -> Result<(), String> {
    let manifest_path = schema_dir.join("manifest.json");
    let manifest: Value = fs::read(&manifest_path)
        .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))
        .and_then(|bytes| {
            serde_json::from_slice(&bytes)
                .map_err(|error| format!("{} is invalid: {error}", manifest_path.display()))
        })?;
    if manifest["cli_version"] != json!(version) {
        return Err(format!(
            "snapshot '{version}': manifest cli_version {} does not match the directory \
             name; re-export the snapshot or fix the manifest",
            manifest["cli_version"]
        ));
    }
    if manifest["command"] != json!(SOURCE_COMMAND) {
        return Err(format!(
            "snapshot '{version}': manifest command {} does not match the recorded \
             source command '{SOURCE_COMMAND}'",
            manifest["command"]
        ));
    }
    let files = manifest["files"]
        .as_object()
        .ok_or_else(|| format!("snapshot '{version}': manifest.json has no 'files' object"))?;
    if files.is_empty() {
        return Err(format!(
            "snapshot '{version}': manifest.json lists no files"
        ));
    }
    for (name, digest) in files {
        let Some(digest) = digest.as_str() else {
            return Err(format!(
                "snapshot '{version}': manifest digest for '{name}' is not a string"
            ));
        };
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(format!(
                "snapshot '{version}': manifest digest for '{name}' is not lowercase SHA-256 hex"
            ));
        }
        let actual = crate::package::sha256_file(&schema_dir.join(name))
            .map_err(|error| format!("snapshot '{version}': manifest file '{name}': {error}"))?;
        if actual != digest {
            return Err(format!(
                "snapshot '{version}': manifest digest mismatch for '{name}': \
                 recorded {digest}, actual {actual}"
            ));
        }
    }
    for name in list_json_files(schema_dir)? {
        if name != "manifest.json" && !files.contains_key(&name) {
            return Err(format!(
                "snapshot '{version}': schema file '{name}' is not listed in manifest.json"
            ));
        }
    }
    for name in REQUIRED_SCHEMA_FILES {
        if !files.contains_key(name) {
            return Err(format!(
                "snapshot '{version}': manifest.json is missing required file '{name}'"
            ));
        }
    }
    // Digest agreement alone cannot distinguish a consistently rewritten
    // pair; every required file must still be a valid JSON Schema document.
    for name in REQUIRED_SCHEMA_FILES {
        let schema = load_schema(schema_dir, name)
            .map_err(|error| format!("snapshot '{version}': {error}"))?;
        if let Err(error) = jsonschema::validator_for(&schema) {
            return Err(format!(
                "snapshot '{version}': schema '{name}' is not a valid JSON Schema: {error}"
            ));
        }
    }
    Ok(())
}

/// Offline verification of every snapshot and fixture directory under the
/// given roots (parametrized for hermetic tests). Every schema snapshot must
/// ship a matching recorded fixture set, so a half-finished upgrade cannot
/// pass unnoticed.
pub fn check_roots(schemas_root: &Path, fixtures_root: &Path) -> Result<(), String> {
    let schema_versions = list_version_dirs(schemas_root)?;
    let fixture_versions = list_version_dirs(fixtures_root)?;
    if schema_versions.is_empty() {
        return Err(format!(
            "no schema snapshots under {}",
            schemas_root.display()
        ));
    }
    if fixture_versions.is_empty() {
        return Err(format!(
            "no fixture directories under {}",
            fixtures_root.display()
        ));
    }
    for version in &schema_versions {
        verify_snapshot_dir(&schemas_root.join(version), version)?;
        if !fixture_versions.contains(version) {
            return Err(format!(
                "schema snapshot '{version}' has no fixtures under {}; record fixtures \
                 for it before it can pass check",
                fixtures_root.display()
            ));
        }
    }
    for version in &fixture_versions {
        if !schema_versions.contains(version) {
            return Err(format!(
                "fixtures for '{version}' under {} have no matching schema snapshot under {}",
                fixtures_root.display(),
                schemas_root.display()
            ));
        }
        validate_fixtures(&fixtures_root.join(version), &schemas_root.join(version))?;
    }
    Ok(())
}

/// `codex-schema check` entry point used by the `cargo xtask check` pipeline.
pub fn run_check(repo: &Path) -> Result<(), String> {
    check_roots(&repo.join(SCHEMA_ROOT), &repo.join(FIXTURE_ROOT))?;
    println!("codex protocol snapshots match their manifests and fixtures");
    Ok(())
}

/// Assemble the staged snapshot next to its target and swap it in. Other
/// version directories are never touched; replacing the same version requires
/// `--force`.
fn publish_snapshot(
    schemas_root: &Path,
    version: &str,
    generated: &Path,
    manifest: &str,
    force: bool,
) -> Result<(), String> {
    let target = schemas_root.join(version);
    if target.exists() && !force {
        return Err(format!(
            "{} already exists; pass --force to replace it",
            target.display()
        ));
    }
    fs::create_dir_all(schemas_root)
        .map_err(|error| format!("cannot create {}: {error}", schemas_root.display()))?;
    let staging =
        crate::package::create_unique_dir(schemas_root, &format!("codex-{version}-staging"))?;
    let outcome = (|| -> Result<(), String> {
        for name in list_json_files(generated)? {
            fs::copy(generated.join(&name), staging.join(&name))
                .map_err(|error| format!("cannot copy '{name}' into staging: {error}"))?;
        }
        fs::write(staging.join("manifest.json"), manifest)
            .map_err(|error| format!("cannot write staged manifest: {error}"))?;
        check_required_files(&staging)
    })();
    if let Err(error) = outcome {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    if target.exists() {
        let trash =
            crate::package::create_unique_dir(schemas_root, &format!("codex-{version}-old"))?;
        if let Err(error) = fs::rename(&target, &trash) {
            let _ = fs::remove_dir_all(&staging);
            let _ = fs::remove_dir_all(&trash);
            return Err(format!("cannot move the previous snapshot aside: {error}"));
        }
        if let Err(error) = fs::rename(&staging, &target) {
            let restore = fs::rename(&trash, &target);
            let _ = fs::remove_dir_all(&staging);
            return match restore {
                Ok(()) => Err(format!(
                    "cannot publish snapshot, previous restored: {error}"
                )),
                Err(rollback) => Err(format!(
                    "cannot publish snapshot ({error}) and restoring the previous snapshot \
                     also failed ({rollback})"
                )),
            };
        }
        let _ = fs::remove_dir_all(&trash);
    } else if let Err(error) = fs::rename(&staging, &target) {
        let _ = fs::remove_dir_all(&staging);
        return Err(format!("cannot publish snapshot: {error}"));
    }
    Ok(())
}

/// Regenerate one snapshot with an explicit Codex executable. The exported
/// snapshot only becomes active when a maintainer records its fixtures and
/// updates `CODEX_SCHEMA_BASELINE`; existing versions are never touched.
pub fn export_snapshot(
    schemas_root: &Path,
    fixtures_root: &Path,
    options: &ExportOptions,
) -> Result<(), String> {
    if !options.codex.is_file() {
        return Err(format!(
            "step 'version': '{}' is not an executable file; pass --codex <path> explicitly",
            options.codex.display()
        ));
    }
    let version_output = run_codex(&options.codex, &["--version"], "version")?;
    let discovered =
        parse_cli_version(&version_output).map_err(|error| format!("step 'version': {error}"))?;
    let version = match &options.version {
        Some(requested) => {
            if requested != &discovered {
                return Err(format!(
                    "step 'version': --version '{requested}' does not match the executable \
                     reporting '{discovered}'"
                ));
            }
            requested.clone()
        }
        None => discovered,
    };
    let temp = tempfile::tempdir()
        .map_err(|error| format!("step 'generate': cannot create temporary directory: {error}"))?;
    let out_dir = temp.path().join("schemas");
    fs::create_dir_all(&out_dir)
        .map_err(|error| format!("step 'generate': cannot create output directory: {error}"))?;
    // Every token stays a separate argument; no shell interpolation.
    let out_argument = out_dir.to_string_lossy().into_owned();
    let generate_args = [
        "app-server",
        "generate-json-schema",
        "--experimental",
        "--out",
        out_argument.as_str(),
    ];
    run_codex(&options.codex, &generate_args, "generate")?;
    let generated = list_json_files(&out_dir)?;
    if generated.is_empty() {
        return Err("step 'generate': the executable produced no .json schema files".to_string());
    }
    let manifest =
        build_manifest(&version, &out_dir).map_err(|error| format!("step 'manifest': {error}"))?;
    check_required_files(&out_dir).map_err(|error| format!("step 'required-files': {error}"))?;
    // Compatibility pre-check: the candidate schemas must still accept every
    // recorded fixture set the repository carries.
    let mut incompatibilities = Vec::new();
    for fixture_version in list_version_dirs(fixtures_root)
        .map_err(|error| format!("step 'compatibility': {error}"))?
    {
        let failures = fixture_failures(&fixtures_root.join(&fixture_version), &out_dir)
            .map_err(|error| format!("step 'compatibility': {error}"))?;
        if !failures.is_empty() {
            incompatibilities.push(format!(
                "fixtures '{fixture_version}':\n  {}",
                failures.join("\n  ")
            ));
        }
    }
    if !incompatibilities.is_empty() && !options.force {
        return Err(format!(
            "step 'compatibility': the new schema rejects recorded fixtures; re-record them \
             and update CODEX_SCHEMA_BASELINE, or pass --force to keep going:\n{}",
            incompatibilities.join("\n")
        ));
    }
    publish_snapshot(schemas_root, &version, &out_dir, &manifest, options.force)
        .map_err(|error| format!("step 'publish': {error}"))?;
    // The temporary directory (and any half-finished staging state) is gone.
    drop(temp);
    println!(
        "published schema snapshot: {}",
        schemas_root.join(&version).display()
    );
    println!(
        "manual follow-up required: record fixtures under {} for the new schema, run the \
         bridge-codex protocol contract tests, and only then update CODEX_SCHEMA_BASELINE \
         in crates/bridge-codex/src/protocol.rs",
        fixtures_root.join(&version).display()
    );
    if !incompatibilities.is_empty() {
        println!(
            "manual follow-up required: recorded fixtures no longer match the new schema \
             ({} fixture set(s) affected):",
            incompatibilities.len()
        );
        for entry in &incompatibilities {
            println!("  - {entry}");
        }
    }
    Ok(())
}

fn parse_export_options(args: &[String]) -> Result<ExportOptions, String> {
    let mut codex: Option<PathBuf> = None;
    let mut version = None;
    let mut force = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--force" => force = true,
            "--codex" => {
                let value = iter.next().ok_or("--codex requires a path argument")?;
                codex = Some(PathBuf::from(value));
            }
            "--version" => {
                let value = iter.next().ok_or("--version requires a value argument")?;
                version = Some(value.clone());
            }
            other => {
                return Err(format!(
                    "unknown export option '{other}'; see 'cargo xtask codex-schema export --help'"
                ));
            }
        }
    }
    Ok(ExportOptions {
        codex: codex.ok_or("export requires --codex <executable path>; PATH is never consulted")?,
        version,
        force,
    })
}

/// `cargo xtask codex-schema` command entry.
pub fn run_command(args: &[String]) -> Result<(), String> {
    let repo = cargo_cli::git_repo_root()?;
    match args.first().map(String::as_str) {
        None => {
            print_usage();
            Err("missing codex-schema subcommand".to_string())
        }
        Some("help" | "--help" | "-h") => {
            print_usage();
            Ok(())
        }
        Some("check") => match args[1..].first().map(String::as_str) {
            None => run_check(&repo),
            Some("--help" | "-h") => {
                println!("usage: cargo xtask codex-schema check");
                Ok(())
            }
            Some(other) => Err(format!(
                "unknown check option '{other}'; see 'cargo xtask codex-schema check --help'"
            )),
        },
        Some("export") => {
            if args[1..].iter().any(|arg| arg == "--help" || arg == "-h") {
                println!("{HELP_EXPORT}");
                return Ok(());
            }
            let options = parse_export_options(&args[1..])?;
            export_snapshot(&repo.join(SCHEMA_ROOT), &repo.join(FIXTURE_ROOT), &options)
        }
        Some(other) => Err(format!(
            "unknown codex-schema subcommand '{other}'; see 'cargo xtask codex-schema help'"
        )),
    }
}

fn print_usage() {
    println!(
        "usage: cargo xtask codex-schema <subcommand>\n\
         \n\
         subcommands:\n\
         \x20 check           verify every recorded snapshot manifest and its fixtures\n\
         \x20                 (offline; part of 'cargo xtask check')\n\
         \x20 export          regenerate one snapshot with an explicit Codex executable;\n\
         \x20                 options: --codex <path> (required), --version <x.y.z>,\n\
         \x20                 --force; explicit maintenance only, never run in CI\n\
         \x20 help            print this message"
    );
}

const HELP_EXPORT: &str =
    "usage: cargo xtask codex-schema export --codex <path> [--version x.y.z] [--force]";
