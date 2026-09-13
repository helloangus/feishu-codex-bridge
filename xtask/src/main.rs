//! Check actual Cargo dependency names, including renamed dependencies.
use std::{
    error::Error,
    process::{Command, ExitCode},
};
fn run() -> Result<(), Box<dyn Error>> {
    let action = std::env::args().nth(1).unwrap_or_default();
    if action != "check-boundaries" {
        return Err("usage: cargo xtask check-boundaries".into());
    }
    let output = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .output()?;
    if !output.status.success() {
        return Err("cargo metadata failed".into());
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    for package in metadata["packages"].as_array().ok_or("missing packages")? {
        let name = package["name"].as_str().ok_or("missing package name")?;
        let allowed: &[&str] = match name {
            "bridge-core" => &["thiserror"],
            "bridge-app" => &["bridge-core", "thiserror", "tokio", "tokio-util"],
            "bridge-local" => &[
                "tokio",
                "bridge-core",
                "bridge-app",
                "serde",
                "serde_json",
                "thiserror",
                "tempfile",
                "fs2",
                "walkdir",
                "similar",
                "rustix",
                "sha2",
            ],
            "bridge-feishu" => &[
                "bridge-core",
                "bridge-app",
                "serde",
                "serde_json",
                "thiserror",
                "reqwest",
                "tokio-tungstenite",
                "prost",
                "base64",
                "tokio-rustls",
                "webpki-roots",
                "percent-encoding",
                "ipnet",
                "rand",
                "tokio",
                "tokio-util",
                "futures-util",
            ],
            "bridge-codex" => &[
                "rustix",
                "bridge-core",
                "bridge-app",
                "serde",
                "serde_json",
                "thiserror",
                "tokio",
                "tokio-util",
                "futures-util",
            ],
            _ => continue,
        };
        for dep in package["dependencies"]
            .as_array()
            .ok_or("missing dependencies")?
        {
            if dep["kind"] == "dev" {
                continue;
            }
            let dependency = dep["name"].as_str().ok_or("missing dependency name")?;
            if !allowed.contains(&dependency) {
                return Err(format!("{name} must not depend on {dependency}; review architecture before changing the allowlist").into());
            }
        }
    }
    println!("crate dependency boundaries passed");
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
