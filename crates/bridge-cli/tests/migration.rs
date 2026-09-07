use std::{fs, process::Command};
#[test]
fn dry_run_import_export_preserve_python_files() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("python");
    fs::create_dir(&source)?;
    let content = r#"{"user:/tmp/project":"thread"}"#;
    fs::write(source.join(".feishu-codex-session"), content)?;
    let destination = temp.path().join("rust");
    let output = Command::new(env!("CARGO_BIN_EXE_bridge"))
        .args(["migrate", "import-python", "--source"])
        .arg(&source)
        .arg("--destination")
        .arg(&destination)
        .arg("--dry-run")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!destination.exists());
    let output = Command::new(env!("CARGO_BIN_EXE_bridge"))
        .args(["migrate", "import-python", "--source"])
        .arg(&source)
        .arg("--destination")
        .arg(&destination)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(source.join(".feishu-codex-session"))?,
        content
    );
    let exported = temp.path().join("exported");
    let output = Command::new(env!("CARGO_BIN_EXE_bridge"))
        .args(["migrate", "export-python", "--state-dir"])
        .arg(&destination)
        .arg("--output")
        .arg(&exported)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(
            exported.join(".feishu-codex-session")
        )?)?,
        serde_json::from_str::<serde_json::Value>(content)?
    );
    Ok(())
}
#[test]
fn failed_import_cannot_be_opened_as_valid_state() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    fs::write(temp.path().join("migration-in-progress"), "incomplete")?;
    assert!(bridge_local::state::JsonStore::open(temp.path()).is_err());
    Ok(())
}
