//! Dependency boundary gate over `cargo metadata --no-deps` output.
//!
//! Every workspace package must be classified explicitly: production crate or
//! development tool. Normal, build and dev dependencies are checked against
//! per-package allowlists; renamed dependencies are keyed by their real
//! package name (`dep["name"]`), not the local alias.

/// Development tools; production packages must not depend on these with
/// normal or build dependencies. Test tooling may be used through
/// dev-dependencies (see the per-kind rule in `check_dependency`).
pub const DEV_TOOLS: &[&str] = &["xtask", "test-support"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    Production,
    DevTool,
}

type Allowlist = &'static [&'static str];

/// `(package, class, normal allowlist, build allowlist, dev allowlist)`.
///
/// The lists mirror the current manifests; extending them requires a
/// deliberate architecture review, not a drive-by dependency bump.
const RULES: &[(&str, Class, Allowlist, Allowlist, Allowlist)] = &[
    ("bridge-core", Class::Production, &["thiserror"], &[], &[]),
    (
        "bridge-app",
        Class::Production,
        &["bridge-core", "thiserror", "tokio", "tokio-util"],
        &[],
        &[],
    ),
    (
        "bridge-local",
        Class::Production,
        &[
            "bridge-app",
            "bridge-core",
            "fs2",
            "rustix",
            "serde",
            "serde_json",
            "sha2",
            "similar",
            "tempfile",
            "thiserror",
            "tokio",
            "walkdir",
        ],
        &[],
        &[],
    ),
    (
        "bridge-feishu",
        Class::Production,
        &[
            "base64",
            "bridge-app",
            "bridge-core",
            "futures-util",
            "ipnet",
            "percent-encoding",
            "prost",
            "rand",
            "reqwest",
            "serde",
            "serde_json",
            "thiserror",
            "tokio",
            "tokio-rustls",
            "tokio-tungstenite",
            "tokio-util",
            "webpki-roots",
        ],
        &[],
        &["http", "rcgen", "tempfile", "tokio"],
    ),
    (
        "bridge-codex",
        Class::Production,
        &[
            "bridge-app",
            "bridge-core",
            "futures-util",
            "rustix",
            "serde",
            "serde_json",
            "thiserror",
            "tokio",
            "tokio-util",
        ],
        &[],
        &["jsonschema", "sha2", "tempfile"],
    ),
    (
        "bridge-cli",
        Class::Production,
        &[
            "bridge-app",
            "bridge-codex",
            "bridge-core",
            "bridge-feishu",
            "bridge-local",
            "clap",
            "fs2",
            "rustix",
            "serde",
            "serde_json",
            "sha2",
            "tempfile",
            "tokio",
            "tokio-util",
            "toml",
        ],
        &[],
        &["tokio"],
    ),
    ("xtask", Class::DevTool, &["serde_json", "sha2"], &[], &[]),
    (
        "test-support",
        Class::DevTool,
        &[
            "bridge-app",
            "bridge-codex",
            "bridge-core",
            "bridge-local",
            "rustix",
            "serde_json",
            "sha2",
            "tempfile",
            "tokio",
            "tokio-util",
        ],
        &[],
        &[],
    ),
];

fn find_rule(
    name: &str,
) -> Option<&'static (&'static str, Class, Allowlist, Allowlist, Allowlist)> {
    RULES.iter().find(|(package, ..)| *package == name)
}

/// Validate one `cargo metadata` document (as produced with `--no-deps`).
pub fn check_metadata(metadata: &serde_json::Value) -> Result<(), String> {
    let packages = metadata
        .get("packages")
        .and_then(|value| value.as_array())
        .ok_or("cargo metadata is missing the packages array")?;
    for package in packages {
        let name = package
            .get("name")
            .and_then(|value| value.as_str())
            .ok_or("cargo metadata package is missing a name")?;
        let rule =
            find_rule(name).ok_or_else(|| {
                format!(
                    "unclassified workspace package '{name}': register it in the xtask boundary rules as production or development tool"
                )
            })?;
        let dependencies = package
            .get("dependencies")
            .and_then(|value| value.as_array())
            .ok_or_else(|| format!("cargo metadata for '{name}' is missing dependencies"))?;
        for dep in dependencies {
            check_dependency(name, rule, dep)?;
        }
    }
    Ok(())
}

fn check_dependency(
    package: &str,
    rule: &(&'static str, Class, Allowlist, Allowlist, Allowlist),
    dep: &serde_json::Value,
) -> Result<(), String> {
    // Renamed dependencies are identified by their real package name.
    let real_name = dep
        .get("name")
        .and_then(|value| value.as_str())
        .ok_or_else(|| format!("dependency entry of '{package}' is missing its real name"))?;
    let kind = match dep.get("kind") {
        None => "normal",
        Some(value) if value.is_null() => "normal",
        Some(value) => match value.as_str() {
            Some("build") => "build",
            Some("dev") => "dev",
            _ => {
                return Err(format!(
                    "dependency '{real_name}' of '{package}' has an unknown kind"
                ));
            }
        },
    };
    if rule.1 == Class::Production && DEV_TOOLS.contains(&real_name) {
        return Err(format!(
            "production package '{package}' must not depend on development tool '{real_name}' ({kind} dependency)"
        ));
    }
    let allowed: Allowlist = match kind {
        "normal" => rule.2,
        "build" => rule.3,
        _ => rule.4,
    };
    if !allowed.contains(&real_name) {
        return Err(format!(
            "{package} {kind} dependency '{real_name}' is not in the allowlist; review architecture before changing it"
        ));
    }
    Ok(())
}

/// `cargo xtask check-boundaries` entry point.
pub fn run_command() -> Result<(), String> {
    let root = crate::cargo_cli::git_repo_root()?;
    let metadata = crate::cargo_cli::cargo_metadata_locked(&root)?;
    check_metadata(&metadata)?;
    println!("crate dependency boundaries passed");
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)] // Synthetic fixtures and assertion unwraps only.
    use super::{DEV_TOOLS, check_metadata};
    use serde_json::json;

    fn metadata_for(packages: serde_json::Value) -> serde_json::Value {
        json!({ "packages": packages })
    }

    fn dep(name: &str, kind: serde_json::Value) -> serde_json::Value {
        let mut entry = json!({ "name": name, "req": "*" });
        if !kind.is_null() {
            entry["kind"] = kind;
        }
        entry
    }

    fn package(name: &str, deps: Vec<serde_json::Value>) -> serde_json::Value {
        json!({ "name": name, "version": "0.1.0", "dependencies": deps })
    }

    #[test]
    fn classified_packages_pass() {
        let metadata = metadata_for(json!([
            package(
                "bridge-core",
                vec![dep("thiserror", serde_json::Value::Null)]
            ),
            package(
                "bridge-app",
                vec![dep("bridge-core", serde_json::Value::Null)]
            ),
            package(
                "bridge-codex",
                vec![
                    dep("tempfile", json!("dev")),
                    dep("jsonschema", json!("dev")),
                    dep("tokio", serde_json::Value::Null),
                ],
            ),
            package("xtask", vec![dep("serde_json", serde_json::Value::Null)]),
        ]));
        assert!(check_metadata(&metadata).is_ok());
    }

    #[test]
    fn production_must_not_depend_on_dev_tool() {
        let metadata = metadata_for(json!([package(
            "bridge-app",
            vec![dep("xtask", serde_json::Value::Null)]
        ),]));
        let error = check_metadata(&metadata).expect_err("must fail");
        assert!(
            error.contains("must not depend on development tool 'xtask'"),
            "{error}"
        );
        assert!(error.contains("normal dependency"), "{error}");
    }

    #[test]
    fn production_dev_dependency_on_dev_tool_is_forbidden() {
        let metadata = metadata_for(json!([package(
            "bridge-core",
            vec![dep("xtask", json!("dev"))]
        ),]));
        let error = check_metadata(&metadata).expect_err("must fail");
        assert!(
            error.contains("development tool 'xtask' (dev dependency)"),
            "{error}"
        );
    }

    #[test]
    fn unclassified_package_fails() {
        let metadata = metadata_for(json!([
            package("bridge-core", vec![]),
            package("mystery-tool", vec![]),
        ]));
        let error = check_metadata(&metadata).expect_err("must fail");
        assert!(
            error.contains("unclassified workspace package 'mystery-tool'"),
            "{error}"
        );
    }

    #[test]
    fn renamed_dependency_uses_real_name() {
        // Renamed allowed dependency passes: real name "thiserror".
        let ok = metadata_for(json!([package(
            "bridge-core",
            vec![json!({ "name": "thiserror", "rename": "error", "req": "*" })],
        )]));
        assert!(check_metadata(&ok).is_ok());
        // Renamed dev tool is still detected through its real name.
        let bad = metadata_for(json!([package(
            "bridge-core",
            vec![json!({ "name": "xtask", "rename": "innocent", "req": "*" })],
        )]));
        let error = check_metadata(&bad).expect_err("must fail");
        assert!(error.contains("'xtask'"), "{error}");
    }

    #[test]
    fn build_dependency_is_checked_separately() {
        let metadata = metadata_for(json!([package(
            "bridge-core",
            vec![dep("cc", json!("build"))],
        )]));
        let error = check_metadata(&metadata).expect_err("must fail");
        assert!(
            error.contains("build dependency 'cc' is not in the allowlist"),
            "{error}"
        );
    }

    #[test]
    fn dev_dependency_allowlist_is_enforced() {
        let metadata = metadata_for(json!([package(
            "bridge-codex",
            vec![dep("futures-util", json!("dev"))],
        )]));
        let error = check_metadata(&metadata).expect_err("must fail");
        assert!(
            error.contains("dev dependency 'futures-util' is not in the allowlist"),
            "{error}"
        );
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let metadata = metadata_for(json!([package(
            "bridge-core",
            vec![dep("thiserror", json!("weird"))],
        )]));
        let error = check_metadata(&metadata).expect_err("must fail");
        assert!(error.contains("unknown kind"), "{error}");
    }

    #[test]
    fn dev_tools_are_listed() {
        assert!(DEV_TOOLS.contains(&"xtask"));
        assert!(DEV_TOOLS.contains(&"test-support"));
    }
}
