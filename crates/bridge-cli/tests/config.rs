use bridge_cli::Config;
use std::{fs, process::Command};

fn workspace_config(access: &str, sandbox: &str, feishu: &str) -> String {
    format!(
        "[workspace]\nroot = '/tmp'\ncwd = '/tmp'\nstate_dir = '/tmp/bridge-state'\n{access}\n[codex]\nexecutable = 'codex'\nargs = ['app-server']\nsandbox = '{sandbox}'\n{feishu}"
    )
}

fn write_config(
    temp: &std::path::Path,
    body: &str,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let config = temp.join("bridge.toml");
    fs::write(&config, body)?;
    Ok(config)
}

const DEFAULT_FEISHU: &str =
    "[feishu]\napp_id_env = 'FEISHU_APP_ID'\napp_secret_env = 'FEISHU_APP_SECRET'\n";

#[test]
fn restricted_config_requires_explicit_access_and_workspace()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let body = workspace_config(
        "[access]\nmode = 'restricted'\nallowed_open_ids = []\n",
        "workspaceWrite",
        DEFAULT_FEISHU,
    );
    let config = write_config(temp.path(), &body)?;
    assert!(Config::read(&config)?.validate().is_err());
    fs::write(
        &config,
        body.replace("allowed_open_ids = []", "allowed_open_ids = ['test-user']"),
    )?;
    Config::read(&config)?.validate()?;
    assert!(!temp.path().join("bridge-state").exists());
    fs::write(
        &config,
        body.replace("mode = 'restricted'", "mode = 'open'"),
    )?;
    Config::read(&config)?.validate()?;
    fs::write(&config, body.replace("workspaceWrite", "invalid"))?;
    assert!(Config::read(&config).is_err());
    Ok(())
}

#[test]
fn config_check_prints_human_result_without_structured_diagnostics()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let config = write_config(
        temp.path(),
        &workspace_config(
            "[access]\nmode = 'restricted'\nallowed_open_ids = ['test-user']\n",
            "workspaceWrite",
            DEFAULT_FEISHU,
        ),
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_bridge"))
        .args(["config", "check", "--file"])
        .arg(&config)
        .output()?;
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)?,
        "配置检查通过（未连接飞书或 Codex；未校验运行时凭据）\n"
    );
    assert!(String::from_utf8(output.stderr)?.is_empty());
    Ok(())
}

#[test]
fn retired_runtime_fields_and_state_conversion_commands_are_rejected()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let config = write_config(
        temp.path(),
        &workspace_config(
            "[access]\nmode = 'open'\nallowed_open_ids = []\n",
            "workspaceWrite",
            "[feishu]\napp_id_env = 'APP_ID'\napp_secret_env = 'APP_SECRET'\npython = '/removed'\nadapter = '/removed'\n",
        ),
    )?;
    assert!(Config::read(&config).is_err());
    let output = Command::new(env!("CARGO_BIN_EXE_bridge"))
        .arg("migrate")
        .output()?;
    assert_eq!(output.status.code(), Some(2));
    Ok(())
}

#[test]
fn static_validation_covers_feishu_section_and_environment_names()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let base = workspace_config(
        "[access]\nmode = 'restricted'\nallowed_open_ids = ['test-user']\n",
        "workspaceWrite",
        DEFAULT_FEISHU,
    );
    // A missing [feishu] section can never start the bridge; reject it early.
    let config = write_config(temp.path(), &base.replace(DEFAULT_FEISHU, ""))?;
    let error = Config::read(&config)?.validate();
    assert!(error.is_err());
    // Empty or non-identifier environment variable names are invalid.
    for feishu in [
        "[feishu]\napp_id_env = ''\napp_secret_env = 'FEISHU_APP_SECRET'\n",
        "[feishu]\napp_id_env = 'not valid'\napp_secret_env = 'FEISHU_APP_SECRET'\n",
        "[feishu]\napp_id_env = 'A'\napp_secret_env = 'S'\nproxy_env = 'bad-name'\n",
    ] {
        let config = write_config(temp.path(), &base.replace(DEFAULT_FEISHU, feishu))?;
        assert!(Config::read(&config)?.validate().is_err(), "{feishu}");
    }
    // A proxy environment name is optional; a valid one still passes.
    let config = write_config(
        temp.path(),
        &base.replace(
            DEFAULT_FEISHU,
            "[feishu]\napp_id_env = 'A'\napp_secret_env = 'S'\nproxy_env = 'BRIDGE_PROXY'\n",
        ),
    )?;
    Config::read(&config)?.validate()?;
    Ok(())
}
