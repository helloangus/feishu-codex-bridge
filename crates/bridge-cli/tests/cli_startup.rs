//! Startup-failure recording through the real CLI binary: bounded, sanitized
//! records before any diagnostics sink exists, plus the safe human message.
use std::{fs, process::Command};

type BoxError = Box<dyn std::error::Error>;

#[test]
fn invalid_config_startup_writes_bounded_record_and_safe_message() -> Result<(), BoxError> {
    let temp = tempfile::tempdir()?;
    let temp = temp.path();
    let config = temp.join("bridge.toml");
    let secret_marker = "totally_unique_invalid_payload_9f3a";
    fs::write(
        &config,
        format!("[workspace]\nroot = '/tmp'\n{secret_marker} = true\n"),
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_bridge"))
        .args(["run", "--config"])
        .arg(&config)
        .output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    // The bounded startup record names the stage only.
    assert!(
        stderr.contains("\"event\":\"startup_failed\",\"stage\":\"config\""),
        "stderr: {stderr}"
    );
    // The human message is the safe categorized display text.
    assert!(
        stderr.contains("错误：配置 TOML 格式无效"),
        "stderr: {stderr}"
    );
    // Raw parse internals and arbitrary payloads are never echoed.
    assert!(!stderr.contains(secret_marker), "stderr: {stderr}");
    Ok(())
}

#[test]
fn missing_credentials_startup_records_stage_and_safe_message() -> Result<(), BoxError> {
    let temp = tempfile::tempdir()?;
    let temp = temp.path();
    let config = temp.join("bridge.toml");
    fs::create_dir_all(temp.join("root"))?;
    fs::write(
        &config,
        format!(
            "[workspace]\nroot = {:?}\ncwd = {:?}\nstate_dir = {:?}\n[access]\nmode = 'restricted'\nallowed_open_ids = ['test-user']\n[codex]\nexecutable = 'codex'\nargs = ['app-server']\nsandbox = 'workspaceWrite'\n[feishu]\napp_id_env = 'BRIDGE_TEST_MISSING_APP_ID'\napp_secret_env = 'BRIDGE_TEST_MISSING_SECRET'\n",
            temp.join("root"),
            temp.join("root"),
            temp.join("state"),
        ),
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_bridge"))
        .args(["run", "--config"])
        .arg(&config)
        .env_remove("BRIDGE_TEST_MISSING_APP_ID")
        // The workspace root must exist so validation reaches the credential
        // stage that this test exercises.
        .env_remove("BRIDGE_TEST_MISSING_SECRET")
        .output()?;
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("\"event\":\"startup_failed\",\"stage\":\"credentials\""),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("错误：凭据环境变量不可用"),
        "stderr: {stderr}"
    );
    Ok(())
}
