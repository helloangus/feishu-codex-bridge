//! Release packaging.
//!
//! Builds `bridge` for the host target only, derives the executable path from
//! cargo's own JSON build messages (never from a guessed `target/release`
//! fallback), assembles the exact five-file package in a staging directory,
//! verifies checksums and documentation links, and only then publishes the
//! final directory.
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

/// Files distributed in a release package, in `SHA256SUMS` order.
pub const CHECKSUMMED_FILES: [&str; 4] = [
    "bridge",
    "bridge.example.toml",
    "DEPLOYMENT.md",
    "BUILD-INFO.txt",
];

/// Exact content of a published package directory.
pub const PACKAGE_FILES: [&str; 5] = [
    "bridge",
    "bridge.example.toml",
    "DEPLOYMENT.md",
    "BUILD-INFO.txt",
    "SHA256SUMS",
];

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{counter}", std::process::id())
}

/// Create a uniquely named directory below `parent` (same filesystem as the
/// final package directory so publication can be a rename).
pub fn create_unique_dir(parent: &Path, prefix: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    for _ in 0..128 {
        let candidate = parent.join(format!(".{prefix}-{}", unique_suffix()));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("cannot create {}: {error}", candidate.display())),
        }
    }
    Err(format!(
        "cannot create a unique directory below {}",
        parent.display()
    ))
}

/// Staging directory next to `final_dir` so publishing stays a rename.
pub fn create_staging_dir(final_dir: &Path) -> Result<PathBuf, String> {
    let parent = final_dir.parent().ok_or_else(|| {
        format!(
            "output directory '{}' has no parent directory",
            final_dir.display()
        )
    })?;
    let label = final_dir
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "package".to_string());
    create_unique_dir(parent, &format!("{label}-staging"))
}

/// Extract the freshly built `bridge` executable from cargo JSON output.
///
/// Only a `compiler-artifact` message for package `bridge-cli`, target
/// `bridge` (kind `bin`) carrying an `executable` path qualifies. If no such
/// message exists the build did not produce the binary in this invocation and
/// guessing a stale `target/release/bridge` path is forbidden.
pub fn parse_build_messages(messages: &str) -> Result<PathBuf, String> {
    let mut executable: Option<String> = None;
    for line in messages.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if value.get("reason").and_then(|reason| reason.as_str()) != Some("compiler-artifact") {
            continue;
        }
        let target = match value.get("target") {
            Some(target) => target,
            None => continue,
        };
        let name_matches = target.get("name").and_then(|name| name.as_str()) == Some("bridge");
        let kind_matches = target
            .get("kind")
            .and_then(|kinds| kinds.as_array())
            .is_some_and(|kinds| {
                kinds
                    .iter()
                    .filter_map(|kind| kind.as_str())
                    .any(|kind| kind == "bin")
            });
        let package_matches = value
            .get("package_id")
            .and_then(|id| id.as_str())
            .is_some_and(is_bridge_cli_package_id);
        if !(name_matches && kind_matches && package_matches) {
            continue;
        }
        if let Some(path) = value.get("executable").and_then(|path| path.as_str()) {
            executable = Some(path.to_string());
        }
    }
    executable.map(PathBuf::from).ok_or_else(|| {
        "cargo build messages do not contain the 'bridge' executable for bridge-cli; \
         refusing to guess a stale target/release path"
            .to_string()
    })
}

/// Match `bridge-cli` package ids across cargo id formats:
/// - legacy: `bridge-cli 0.1.0 (path+file:///repo)`
/// - new spec with explicit name: `path+file:///repo#bridge-cli@0.1.0`
/// - new spec with omitted name (name equals the last URL path segment,
///   as emitted for workspace path dependencies since cargo 1.77):
///   `path+file:///repo/crates/bridge-cli#0.1.0`
fn is_bridge_cli_package_id(package_id: &str) -> bool {
    if package_id.starts_with("bridge-cli ") {
        return true;
    }
    let Some(hash) = package_id.find('#') else {
        return false;
    };
    let fragment = &package_id[hash + 1..];
    if fragment.starts_with("bridge-cli@") {
        return true;
    }
    let url = &package_id[..hash];
    url.rsplit('/').next() == Some("bridge-cli") && is_version_fragment(fragment)
}

/// A version-only fragment such as `0.1.0` or `0.1.0+build`.
fn is_version_fragment(fragment: &str) -> bool {
    match fragment.chars().next() {
        Some(first) if first.is_ascii_digit() => {}
        _ => return false,
    }
    fragment
        .chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '+' || c == '-')
}

pub struct RustcInfo {
    pub version_line: String,
    pub release: String,
    pub host: String,
}

/// Parse `rustc -vV` output: first line is the `--version` string, followed by
/// `release:` and `host:` fields.
pub fn parse_rustc_version(output: &str) -> Result<RustcInfo, String> {
    let mut lines = output.lines();
    let version_line = lines.next().unwrap_or("").trim().to_string();
    if version_line.is_empty() {
        return Err("rustc -vV output is empty".to_string());
    }
    let mut release: Option<String> = None;
    let mut host: Option<String> = None;
    for line in lines {
        if let Some(rest) = line.strip_prefix("release:") {
            release = Some(rest.trim().to_string());
        }
        if let Some(rest) = line.strip_prefix("host:") {
            host = Some(rest.trim().to_string());
        }
    }
    match (release, host) {
        (Some(release), Some(host)) => Ok(RustcInfo {
            version_line,
            release,
            host,
        }),
        _ => Err("rustc -vV output is missing release or host".to_string()),
    }
}

/// Packaging is host-only: any configured non-host build target is rejected.
/// `configured` holds `(source description, target)` pairs that are set.
pub fn check_host_target(host: &str, configured: &[(String, String)]) -> Result<(), String> {
    for (source, value) in configured {
        if value != host {
            return Err(format!(
                "non-host build target '{value}' from {source} cannot be packaged; \
                 packaging only supports the host target '{host}'"
            ));
        }
    }
    Ok(())
}

/// Minimal TOML scan for `[build] target = "..."` in a cargo config file.
pub fn parse_config_build_target(text: &str) -> Option<String> {
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        if section != "build" {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "target" {
                let value = value.trim();
                let value = value
                    .strip_prefix('"')
                    .and_then(|value| value.strip_suffix('"'))
                    .or_else(|| {
                        value
                            .strip_prefix('\'')
                            .and_then(|value| value.strip_suffix('\''))
                    })
                    .unwrap_or(value)
                    .trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// Read `[build] target` from the repository `.cargo/config.toml`/`config`.
pub fn read_config_build_target(repo: &Path) -> Result<Option<(String, String)>, String> {
    for name in ["config.toml", "config"] {
        let path = repo.join(".cargo").join(name);
        if path.is_file() {
            let text = fs::read_to_string(&path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            if let Some(target) = parse_config_build_target(&text) {
                return Ok(Some((format!(".cargo/{name} build.target"), target)));
            }
        }
    }
    Ok(None)
}

/// Split `git status --porcelain` output into non-empty entries.
pub fn parse_status_porcelain(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format_digest(&hasher.finalize())
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file =
        fs::File::open(path).map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format_digest(&hasher.finalize()))
}

fn format_digest(digest: &[u8]) -> String {
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

pub struct BuildInfo {
    pub package: String,
    pub version: String,
    pub commit: String,
    pub dirty: bool,
    pub allow_dirty: bool,
    pub dirty_files: Vec<String>,
    pub target: String,
    pub profile: String,
    pub rustc_version: String,
    pub cargo_version: String,
}

/// Deterministic rendering: packaging the same commit twice yields identical
/// bytes, which keeps repeated packaging idempotent.
pub fn render_build_info(info: &BuildInfo) -> String {
    let mut out = String::new();
    out.push_str("build-info-format: 1\n");
    out.push_str(&format!("package: {}\n", info.package));
    out.push_str(&format!("version: {}\n", info.version));
    out.push_str(&format!("commit: {}\n", info.commit));
    out.push_str(&format!("dirty: {}\n", info.dirty));
    if info.dirty {
        out.push_str("dirty-files:\n");
        for entry in &info.dirty_files {
            out.push_str(&format!("  - {entry}\n"));
        }
    }
    out.push_str(&format!("target: {}\n", info.target));
    out.push_str(&format!("profile: {}\n", info.profile));
    out.push_str(&format!("rustc: {}\n", info.rustc_version));
    out.push_str(&format!("cargo: {}\n", info.cargo_version));
    out.push_str(&format!("allow-dirty: {}\n", info.allow_dirty));
    out
}

pub struct PackageInputs {
    pub executable: PathBuf,
    pub example_toml: PathBuf,
    pub deployment_doc: PathBuf,
}

/// Assemble the exact package content into `staging` (not yet published).
pub fn assemble_package(
    staging: &Path,
    inputs: &PackageInputs,
    info: &BuildInfo,
) -> Result<(), String> {
    fs::create_dir_all(staging)
        .map_err(|error| format!("cannot create {}: {error}", staging.display()))?;
    copy_file(&inputs.executable, &staging.join("bridge"))?;
    set_executable_bit(&staging.join("bridge"))?;
    copy_file(&inputs.example_toml, &staging.join("bridge.example.toml"))?;
    copy_file(&inputs.deployment_doc, &staging.join("DEPLOYMENT.md"))?;
    fs::write(staging.join("BUILD-INFO.txt"), render_build_info(info))
        .map_err(|error| format!("cannot write BUILD-INFO.txt: {error}"))?;
    write_checksums(staging)
}

fn copy_file(from: &Path, to: &Path) -> Result<(), String> {
    fs::copy(from, to).map_err(|error| {
        format!(
            "cannot copy {} -> {}: {error}",
            from.display(),
            to.display()
        )
    })?;
    Ok(())
}

fn set_executable_bit(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)
            .map_err(|error| format!("cannot stat {}: {error}", path.display()))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)
            .map_err(|error| format!("cannot set executable bit on {}: {error}", path.display()))?;
    }
    Ok(())
}

fn write_checksums(staging: &Path) -> Result<(), String> {
    let mut content = String::new();
    for name in CHECKSUMMED_FILES {
        let digest = sha256_file(&staging.join(name))?;
        content.push_str(&format!("{digest}  {name}\n"));
    }
    fs::write(staging.join("SHA256SUMS"), content)
        .map_err(|error| format!("cannot write SHA256SUMS: {error}"))
}

/// Validate a fully assembled package directory:
/// exact five-file manifest, checksum self-verification and documentation
/// links that resolve inside the package.
pub fn validate_package(dir: &Path) -> Result<(), String> {
    let mut present: BTreeSet<String> = BTreeSet::new();
    let entries =
        fs::read_dir(dir).map_err(|error| format!("cannot list {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot list {}: {error}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect '{name}': {error}"))?;
        if !file_type.is_file() {
            return Err(format!(
                "package directory must contain only regular files; found '{name}'"
            ));
        }
        present.insert(name);
    }
    let expected: BTreeSet<String> = PACKAGE_FILES
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    if present != expected {
        let missing: Vec<&String> = expected.difference(&present).collect();
        let extra: Vec<&String> = present.difference(&expected).collect();
        return Err(format!(
            "package manifest mismatch in {}: missing={missing:?} extra={extra:?}",
            dir.display()
        ));
    }
    verify_checksums(dir)?;
    let document = fs::read_to_string(dir.join("DEPLOYMENT.md"))
        .map_err(|error| format!("cannot read packaged DEPLOYMENT.md: {error}"))?;
    check_document_links(&document, &expected)
}

fn verify_checksums(dir: &Path) -> Result<(), String> {
    let content = fs::read_to_string(dir.join("SHA256SUMS"))
        .map_err(|error| format!("cannot read SHA256SUMS: {error}"))?;
    let mut listed: Vec<&str> = Vec::new();
    for line in content.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let (digest, name) = line
            .split_once("  ")
            .ok_or_else(|| format!("SHA256SUMS line is not '<hex>  <name>': '{line}'"))?;
        let name = name.trim();
        if !CHECKSUMMED_FILES.contains(&name) {
            return Err(format!("SHA256SUMS lists unexpected file '{name}'"));
        }
        if listed.contains(&name) {
            return Err(format!("SHA256SUMS lists '{name}' twice"));
        }
        listed.push(name);
        let actual = sha256_file(&dir.join(name))?;
        if actual != digest.trim() {
            return Err(format!(
                "SHA256 mismatch for '{name}': expected {}, actual {actual}",
                digest.trim()
            ));
        }
    }
    for name in CHECKSUMMED_FILES {
        if !listed.contains(&name) {
            return Err(format!("SHA256SUMS is missing the entry for '{name}'"));
        }
    }
    Ok(())
}

/// Verify that every relative markdown link in the packaged deployment guide
/// resolves to a file shipped in the package.
pub fn check_document_links(document: &str, available: &BTreeSet<String>) -> Result<(), String> {
    let mut in_fence = false;
    for (index, raw) in document.lines().enumerate() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let line_number = index + 1;
        let bytes = raw.as_bytes();
        let mut cursor = 0usize;
        while cursor + 1 < bytes.len() {
            if bytes[cursor] == b']' && bytes[cursor + 1] == b'(' {
                let rest = &raw[cursor + 2..];
                let end = rest.find(')').ok_or_else(|| {
                    format!("DEPLOYMENT.md line {line_number} has an unterminated link")
                })?;
                let target = &rest[..end];
                check_link_target(target, available, line_number)?;
                cursor += end + 3;
            } else {
                cursor += 1;
            }
        }
    }
    Ok(())
}

fn check_link_target(
    target: &str,
    available: &BTreeSet<String>,
    line_number: usize,
) -> Result<(), String> {
    let target = target.trim();
    if target.is_empty()
        || target.starts_with('#')
        || target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
    {
        return Ok(());
    }
    let path = target.split('#').next().unwrap_or("");
    if path.is_empty() {
        return Ok(());
    }
    let candidate = path.trim_start_matches("./");
    if candidate.starts_with('/') {
        return Err(format!(
            "DEPLOYMENT.md line {line_number} links to absolute path '{target}' which is not shipped in the package"
        ));
    }
    if candidate.split('/').any(|segment| segment == "..") {
        return Err(format!(
            "DEPLOYMENT.md line {line_number} links outside the package: '{target}'"
        ));
    }
    if !available.contains(candidate) {
        return Err(format!(
            "DEPLOYMENT.md line {line_number} links to '{target}' which is not shipped in the package"
        ));
    }
    Ok(())
}

/// Move `staging` to `final_dir`, replacing any previous package. A failure
/// while replacing restores the previous package.
pub fn publish(staging: &Path, final_dir: &Path) -> Result<(), String> {
    let parent = final_dir.parent().ok_or_else(|| {
        format!(
            "output directory '{}' has no parent directory",
            final_dir.display()
        )
    })?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    if final_dir.exists() {
        let trash = create_unique_dir(parent, "package-old")?;
        if let Err(error) = fs::rename(final_dir, &trash) {
            let _ = fs::remove_dir_all(&trash);
            return Err(format!("cannot move the previous package aside: {error}"));
        }
        match fs::rename(staging, final_dir) {
            Ok(()) => {
                let _ = fs::remove_dir_all(&trash);
                Ok(())
            }
            Err(error) => {
                if let Err(rollback) = fs::rename(&trash, final_dir) {
                    Err(format!(
                        "publish failed ({error}) and restoring the previous package also failed ({rollback})"
                    ))
                } else {
                    Err(format!(
                        "publish failed, previous package restored: {error}"
                    ))
                }
            }
        }
    } else {
        fs::rename(staging, final_dir).map_err(|error| format!("cannot publish package: {error}"))
    }
}

/// Assemble into `staging`, validate, and only then publish to `final_dir`.
/// On any failure the staging directory is removed and `final_dir` keeps its
/// previous content, so no half-finished package is ever published.
pub fn assemble_and_publish(
    staging: &Path,
    final_dir: &Path,
    inputs: &PackageInputs,
    info: &BuildInfo,
) -> Result<(), String> {
    let outcome = assemble_package(staging, inputs, info).and_then(|()| validate_package(staging));
    if let Err(error) = outcome {
        let _ = fs::remove_dir_all(staging);
        return Err(error);
    }
    if let Err(error) = publish(staging, final_dir) {
        let _ = fs::remove_dir_all(staging);
        return Err(error);
    }
    Ok(())
}

pub struct RepoFacts {
    pub root: PathBuf,
    pub commit: String,
    pub dirty: bool,
    pub dirty_files: Vec<String>,
    pub rustc: RustcInfo,
    pub cargo_version: String,
    pub package_version: String,
}

/// Collect git, toolchain and package-version facts for the repository.
pub fn collect_repo_facts() -> Result<RepoFacts, String> {
    let root = crate::cargo_cli::git_repo_root()?;
    let commit = crate::cargo_cli::git_captured(&["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let status =
        crate::cargo_cli::git_captured(&["status", "--porcelain", "--untracked-files=all"])?;
    let dirty_files = parse_status_porcelain(&status);
    let dirty = !dirty_files.is_empty();
    let version_output =
        crate::cargo_cli::run_capturing_stdout("rustc", &["-vV".to_string()], &[], &root)?;
    let rustc = parse_rustc_version(&String::from_utf8_lossy(&version_output))?;
    let cargo_output = crate::cargo_cli::run_capturing_stdout(
        &crate::cargo_cli::cargo_program(),
        &["--version".to_string()],
        &[],
        &root,
    )?;
    let cargo_version = String::from_utf8_lossy(&cargo_output).trim().to_string();
    let metadata = crate::cargo_cli::cargo_metadata_locked(&root)?;
    let package_version = package_version_from_metadata(&metadata, "bridge-cli")?;
    Ok(RepoFacts {
        root,
        commit,
        dirty,
        dirty_files,
        rustc,
        cargo_version,
        package_version,
    })
}

fn package_version_from_metadata(
    metadata: &serde_json::Value,
    name: &str,
) -> Result<String, String> {
    let packages = metadata
        .get("packages")
        .and_then(|value| value.as_array())
        .ok_or("cargo metadata is missing the packages array")?;
    for package in packages {
        if package.get("name").and_then(|value| value.as_str()) == Some(name) {
            return package
                .get("version")
                .and_then(|value| value.as_str())
                .map(str::to_string)
                .ok_or_else(|| format!("package '{name}' is missing its version"));
        }
    }
    Err(format!("cargo metadata does not contain package '{name}'"))
}

/// Refuse to package a dirty tree (including untracked source files) unless
/// the caller passes `--allow-dirty`.
pub fn enforce_clean_tree(facts: &RepoFacts, allow_dirty: bool) -> Result<(), String> {
    if !facts.dirty {
        return Ok(());
    }
    if allow_dirty {
        return Ok(());
    }
    Err(format!(
        "working tree is dirty ({} entries listed below); commit them or pass --allow-dirty\n{}",
        facts.dirty_files.len(),
        facts.dirty_files.join("\n")
    ))
}

/// Build the host release binary and publish a verified package.
pub fn run_command(args: &[String]) -> Result<(), String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_usage();
        return Ok(());
    }
    let (allow_dirty, output_dir) = parse_args(args)?;
    let facts = collect_repo_facts()?;
    let mut configured: Vec<(String, String)> = Vec::new();
    if let Some(value) = std::env::var("CARGO_BUILD_TARGET")
        .ok()
        .filter(|value| !value.is_empty())
    {
        configured.push(("CARGO_BUILD_TARGET".to_string(), value));
    }
    if let Some(entry) = read_config_build_target(&facts.root)? {
        configured.push(entry);
    }
    check_host_target(&facts.rustc.host, &configured)?;
    enforce_clean_tree(&facts, allow_dirty)?;
    let output = match output_dir {
        Some(dir) => {
            let path = PathBuf::from(dir);
            if path.is_absolute() {
                path
            } else {
                facts.root.join(path)
            }
        }
        None => facts.root.join("dist"),
    };
    let executable = build_release_binary(&facts)?;
    let info = BuildInfo {
        package: "bridge-cli".to_string(),
        version: facts.package_version.clone(),
        commit: facts.commit.clone(),
        dirty: facts.dirty,
        allow_dirty,
        dirty_files: facts.dirty_files.clone(),
        target: facts.rustc.host.clone(),
        profile: "release".to_string(),
        rustc_version: facts.rustc.version_line.clone(),
        cargo_version: facts.cargo_version.clone(),
    };
    let inputs = PackageInputs {
        executable,
        example_toml: facts.root.join("bridge.example.toml"),
        deployment_doc: facts.root.join("docs/deployment.md"),
    };
    let staging = create_staging_dir(&output)?;
    assemble_and_publish(&staging, &output, &inputs, &info)?;
    println!("published package: {}", output.display());
    println!("files: {}", PACKAGE_FILES.join(", "));
    println!(
        "host-only build ({}, release); no cross-compilation was performed",
        facts.rustc.host
    );
    Ok(())
}

/// Parse package CLI options; `--release` is accepted but is already the
/// only profile. Returns (allow_dirty, output directory).
fn parse_args(args: &[String]) -> Result<(bool, Option<String>), String> {
    let mut allow_dirty = false;
    let mut output_dir: Option<String> = None;
    for arg in args {
        match arg.as_str() {
            "--allow-dirty" => allow_dirty = true,
            "--release" => {}
            other if other.starts_with('-') => {
                return Err(format!(
                    "unknown package option '{other}'; see 'cargo xtask package --help'"
                ));
            }
            other => {
                if output_dir.is_some() {
                    return Err("package accepts at most one output directory".to_string());
                }
                output_dir = Some(other.to_string());
            }
        }
    }
    Ok((allow_dirty, output_dir))
}

/// Build the release binary and return its path from the build messages.
fn build_release_binary(facts: &RepoFacts) -> Result<std::path::PathBuf, String> {
    println!(
        "==> xtask package: cargo build -p bridge-cli --bin bridge --release --locked -j 1 (host {})",
        facts.rustc.host
    );
    let build_args: Vec<String> = [
        "build",
        "-p",
        "bridge-cli",
        "--bin",
        "bridge",
        "--release",
        "--locked",
        "-j",
        "1",
        "--message-format=json",
    ]
    .iter()
    .map(|arg| (*arg).to_string())
    .collect();
    let messages = crate::cargo_cli::run_capturing_stdout(
        &crate::cargo_cli::cargo_program(),
        &build_args,
        &[],
        &facts.root,
    )?;
    let messages = String::from_utf8(messages)
        .map_err(|error| format!("cargo build printed non-UTF-8 JSON messages: {error}"))?;
    let executable = parse_build_messages(&messages)?;
    if !executable.is_file() {
        return Err(format!(
            "cargo reported executable '{}' but it is not a file on disk",
            executable.display()
        ));
    }
    Ok(executable)
}

fn print_usage() {
    println!(
        "usage: cargo xtask package [--release] [--allow-dirty] [OUTPUT_DIR]\n\
         \n\
         Builds the host release binary and publishes a verified package\n\
         (bridge, bridge.example.toml, DEPLOYMENT.md, BUILD-INFO.txt, SHA256SUMS).\n\
         \n\
         Options:\n\
         \x20 --release        explicit no-op: release is the only supported profile\n\
         \x20 --allow-dirty    package a dirty working tree (recorded in BUILD-INFO.txt)\n\
         \x20 OUTPUT_DIR       final package directory (default: dist/ in the repository)"
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Synthetic fixtures and assertion unwraps only.
    use super::*;

    const ARTIFACT_TEMPLATE: &str = concat!(
        r#"{"reason":"compiler-artifact","package_id":"path+file:///tmp/sp ace/repo/crates/bridge-cli#0.1.0","#,
        r#""target":{"name":"bridge","kind":["bin"],"crate_types":["bin"],"edition":"2024"},"#,
        r#""profile":{"test":false},"executable":"TARGET/bridge","fresh":false}"#
    );

    #[test]
    fn parses_real_cargo_185_message() {
        // Captured verbatim from `cargo build --message-format=json` on this
        // toolchain (fresh re-emit after a completed build).
        let line = r#"{"reason":"compiler-artifact","package_id":"path+file:///home/orangepi/dev/fcb-d/crates/bridge-cli#0.1.0","target":{"name":"bridge","kind":["bin"],"crate_types":["bin"],"edition":"2024"},"config_hashes":{},"profile":{"gradle":false,"opt_level":"3","debug_assertions":true},"features":[],"filenames":["/home/orangepi/dev/fcb-d/target/release/bridge"],"executable":"/home/orangepi/dev/fcb-d/target/release/bridge","fresh":true}"#;
        let path = parse_build_messages(line).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/home/orangepi/dev/fcb-d/target/release/bridge")
        );
    }

    #[test]
    fn parses_executable_from_synthetic_messages() {
        let messages = format!(
            "{}\n{}\n",
            r#"{"reason":"build-finished","success":true}"#,
            ARTIFACT_TEMPLATE.replace("TARGET", "/tmp/custom target dir/release")
        );
        let path = parse_build_messages(&messages).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/custom target dir/release/bridge"));
    }

    #[test]
    fn takes_the_last_matching_artifact() {
        let messages = format!(
            "{}\n{}\n",
            ARTIFACT_TEMPLATE.replace("TARGET", "/tmp/old"),
            ARTIFACT_TEMPLATE.replace("TARGET", "/tmp/new")
        );
        let path = parse_build_messages(&messages).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/new/bridge"));
    }

    #[test]
    fn artifact_without_executable_is_not_a_match() {
        let without: serde_json::Value = serde_json::from_str(
            &ARTIFACT_TEMPLATE.replace(r#","executable":"TARGET/bridge""#, ""),
        )
        .unwrap();
        let messages = format!("{}\n", serde_json::to_string(&without).unwrap());
        assert!(parse_build_messages(&messages).is_err());
    }

    #[test]
    fn other_binaries_and_packages_are_ignored() {
        let other_target = ARTIFACT_TEMPLATE
            .replace(r#""name":"bridge""#, r#""name":"bridge-cli""#)
            .replace("TARGET", "/tmp/x");
        let other_package = ARTIFACT_TEMPLATE
            .replace(
                "repo/crates/bridge-cli#0.1.0",
                "repo/crates/other-crate#0.1.0",
            )
            .replace("TARGET", "/tmp/x");
        let messages = format!("{other_target}\n{other_package}\n");
        assert!(parse_build_messages(&messages).is_err());
    }

    #[test]
    fn missing_executable_errors_without_fallback() {
        let error = parse_build_messages("").err().unwrap();
        assert!(error.contains("refusing to guess"), "{error}");
        assert!(!error.contains("target/release/bridge\""), "{error}");
    }

    #[test]
    fn legacy_and_new_package_id_formats_match() {
        assert!(is_bridge_cli_package_id(
            "bridge-cli 0.1.0 (path+file:///repo)"
        ));
        assert!(is_bridge_cli_package_id(
            "path+file:///repo#bridge-cli@0.1.0"
        ));
        // Real cargo 1.85 output for a workspace path dependency: the name is
        // omitted because it equals the last URL path segment.
        assert!(is_bridge_cli_package_id(
            "path+file:///home/u/repo/crates/bridge-cli#0.1.0"
        ));
        assert!(!is_bridge_cli_package_id(
            "path+file:///repo#bridge-app@0.1.0"
        ));
        assert!(!is_bridge_cli_package_id(
            "path+file:///home/u/repo/crates/bridge-app#0.1.0"
        ));
        assert!(!is_bridge_cli_package_id("registry+https://example#0.1.0"));
    }

    #[test]
    fn rustc_version_parsing() {
        let info = parse_rustc_version(
            "rustc 1.85.1 (4eb161250 2025-01-20)\nbinary: rustc\ncommit-hash: 4eb161250\nrelease: 1.85.1\nhost: aarch64-unknown-linux-gnu\n",
        )
        .unwrap();
        assert_eq!(info.host, "aarch64-unknown-linux-gnu");
        assert_eq!(info.release, "1.85.1");
        assert!(info.version_line.starts_with("rustc 1.85.1"));
        assert!(parse_rustc_version("nothing useful").is_err());
    }

    #[test]
    fn host_target_gate() {
        let host = "aarch64-unknown-linux-gnu";
        assert!(check_host_target(host, &[]).is_ok());
        let matching = vec![(String::from("CARGO_BUILD_TARGET"), host.to_string())];
        assert!(check_host_target(host, &matching).is_ok());
        let mismatch = vec![(
            String::from("CARGO_BUILD_TARGET"),
            String::from("x86_64-unknown-linux-gnu"),
        )];
        let error = check_host_target(host, &mismatch).err().unwrap();
        assert!(error.contains("non-host build target"), "{error}");
    }

    #[test]
    fn config_target_parser() {
        let text = "[alias]\nxtask = \"run\"\n\n[build]\n# target = \"commented\"\ntarget = \"x86_64-unknown-linux-gnu\"\n";
        assert_eq!(
            parse_config_build_target(text),
            Some(String::from("x86_64-unknown-linux-gnu"))
        );
        assert_eq!(parse_config_build_target("[alias]\nx = \"1\"\n"), None);
        assert_eq!(
            parse_config_build_target("[build]\ntarget = 'a-pc'\n"),
            Some(String::from("a-pc"))
        );
    }

    #[test]
    fn porcelain_parsing() {
        let entries = parse_status_porcelain(" M xtask/src/lib.rs\n?? new/file.rs\n\n");
        assert_eq!(entries.len(), 2);
        assert!(entries[0].contains("lib.rs"));
        assert!(parse_status_porcelain("").is_empty());
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn build_info_is_deterministic_and_lists_dirty_files() {
        let base = BuildInfo {
            package: "bridge-cli".to_string(),
            version: "0.1.0".to_string(),
            commit: "abc123".to_string(),
            dirty: false,
            allow_dirty: false,
            dirty_files: vec![],
            target: "a-pc".to_string(),
            profile: "release".to_string(),
            rustc_version: "rustc 1.85.1".to_string(),
            cargo_version: "cargo 1.85.1".to_string(),
        };
        let clean = render_build_info(&base);
        assert_eq!(clean, render_build_info(&base));
        assert!(clean.contains("dirty: false"));
        assert!(clean.contains("commit: abc123"));
        assert!(clean.contains("version: 0.1.0"));
        assert!(clean.contains("target: a-pc"));
        let dirty = BuildInfo {
            dirty: true,
            allow_dirty: true,
            dirty_files: vec!["?? notes.txt".to_string()],
            ..base
        };
        let rendered = render_build_info(&dirty);
        assert!(rendered.contains("dirty: true"));
        assert!(rendered.contains("  - ?? notes.txt"));
        assert!(rendered.contains("allow-dirty: true"));
    }

    #[test]
    fn document_links_must_resolve_inside_the_package() {
        let available: BTreeSet<String> = PACKAGE_FILES
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        let ok = "See [sums](SHA256SUMS) and [cfg](./bridge.example.toml) or [web](https://example.test).\n";
        assert!(check_document_links(ok, &available).is_ok());
        let missing = "See [ops](docs/operations.md).\n";
        let error = check_document_links(missing, &available).err().unwrap();
        assert!(error.contains("line 1"), "{error}");
        assert!(error.contains("docs/operations.md"), "{error}");
        let outside = "See [x](../src/main.rs).\n";
        assert!(check_document_links(outside, &available).is_err());
        let absolute = "See [x](/etc/bridge.md).\n";
        assert!(check_document_links(absolute, &available).is_err());
        let anchor_only = "Jump [down](#section).\n";
        assert!(check_document_links(anchor_only, &available).is_ok());
    }

    #[test]
    fn links_inside_code_fences_are_ignored() {
        let available: BTreeSet<String> = PACKAGE_FILES
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        let document = "Intro.\n\n```sh\nprintf '](' \narr[x](not-a-link.md)\n```\n\nDone.\n";
        assert!(check_document_links(document, &available).is_ok());
    }
}
