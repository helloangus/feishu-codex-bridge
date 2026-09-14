//! Credential command coverage through the real CLI binary: configured
//! environment variable names, offline validation and non-interactive runs.
use std::{
    fs,
    process::{Command, Output, Stdio},
};

type BoxError = Box<dyn std::error::Error>;

fn write_config(
    temp: &std::path::Path,
    feishu: &str,
    access: &str,
) -> Result<std::path::PathBuf, BoxError> {
    let config = temp.join("bridge.toml");
    fs::write(
        &config,
        format!(
            "[workspace]\nroot = {:?}\ncwd = {:?}\nstate_dir = {:?}\n{access}\n[codex]\nexecutable = 'codex'\nargs = ['app-server']\nsandbox = 'workspaceWrite'\n{feishu}\n",
            temp.join("root"),
            temp.join("root"),
            temp.join("state"),
        ),
    )?;
    fs::create_dir_all(temp.join("root"))?;
    Ok(config)
}

const DEFAULT_FEISHU: &str =
    "[feishu]\napp_id_env = 'FEISHU_APP_ID'\napp_secret_env = 'FEISHU_APP_SECRET'\n";
const PAIRING_ACCESS: &str = "[access]\nmode = 'restricted'\nallowed_open_ids = []\npairing_code_env = 'FEISHU_PAIRING_CODE'\n";
const WHITELIST_ACCESS: &str = "[access]\nmode = 'restricted'\nallowed_open_ids = ['test-user']\n";

fn run_credentials(
    config: &std::path::Path,
    envs: &[(&str, &str)],
    stdin: Option<&[u8]>,
) -> Result<Output, BoxError> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bridge"));
    command
        .args(["credentials", "--file"])
        .arg(config)
        .arg("--print")
        .env_clear()
        .env("PATH", "/usr/bin:/bin");
    for (name, value) in envs {
        command.env(name, value);
    }
    match stdin {
        Some(input) => {
            use std::io::Write;
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn()?;
            child
                .stdin
                .as_mut()
                .ok_or("piped stdin is missing")?
                .write_all(input)?;
            Ok(child.wait_with_output()?)
        }
        None => Ok(command.stdin(Stdio::null()).output()?),
    }
}

#[test]
fn whitelist_config_passes_with_credentials_in_environment() -> Result<(), BoxError> {
    let temp = tempfile::tempdir()?;
    let config = write_config(temp.path(), DEFAULT_FEISHU, WHITELIST_ACCESS)?;
    let output = run_credentials(
        &config,
        &[
            ("FEISHU_APP_ID", "app-id"),
            ("FEISHU_APP_SECRET", "app-secret"),
        ],
        None,
    )?;
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    Ok(())
}

#[test]
fn missing_secret_fails_offline_without_echoing_values() -> Result<(), BoxError> {
    let temp = tempfile::tempdir()?;
    let config = write_config(temp.path(), DEFAULT_FEISHU, WHITELIST_ACCESS)?;
    let output = run_credentials(&config, &[("FEISHU_APP_ID", "app-id")], None)?;
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("FEISHU_APP_SECRET"), "{stderr}");
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn invalid_pairing_codes_are_rejected_including_unicode_whitespace() -> Result<(), BoxError> {
    let temp = tempfile::tempdir()?;
    let config = write_config(temp.path(), DEFAULT_FEISHU, PAIRING_ACCESS)?;
    for (code, label) in [
        ("too short", "short"),
        ("0123456789abcdef\u{00a0}", "nbsp"),
        ("0123456789abcdef\t", "tab"),
        (&"x".repeat(257)[..], "long"),
    ] {
        let output = run_credentials(
            &config,
            &[
                ("FEISHU_APP_ID", "app-id"),
                ("FEISHU_APP_SECRET", "app-secret"),
                ("FEISHU_PAIRING_CODE", code),
            ],
            None,
        )?;
        assert_eq!(output.status.code(), Some(1), "{label}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("16–256 字节"),
            "{label}"
        );
    }
    Ok(())
}

#[test]
fn piped_values_become_quoted_export_lines_and_never_hit_stderr() -> Result<(), BoxError> {
    let temp = tempfile::tempdir()?;
    let config = write_config(temp.path(), DEFAULT_FEISHU, PAIRING_ACCESS)?;
    let output = run_credentials(
        &config,
        &[],
        Some(b"app-id\napp-secret\n0123456789abcdef\n"),
    )?;
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    let stdout = String::from_utf8(output.stdout.clone())?;
    assert!(stdout.contains("export FEISHU_APP_ID='app-id'"), "{stdout}");
    assert!(
        stdout.contains("export FEISHU_APP_SECRET='app-secret'"),
        "{stdout}"
    );
    assert!(
        stdout.contains("export FEISHU_PAIRING_CODE='0123456789abcdef'"),
        "{stdout}"
    );
    assert!(stdout.contains("BRIDGE_RUST_PAIRING_PRESENT=1"), "{stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("app-secret"), "{stderr}");
    assert!(!stderr.contains("0123456789abcdef"), "{stderr}");
    Ok(())
}

#[test]
fn paired_state_keeps_pairing_optional() -> Result<(), BoxError> {
    let temp = tempfile::tempdir()?;
    let config = write_config(temp.path(), DEFAULT_FEISHU, PAIRING_ACCESS)?;
    fs::create_dir_all(temp.path().join("state"))?;
    fs::write(
        temp.path().join("state/state.json"),
        "{\"schema_version\":1,\"sessions\":{},\"models\":{},\"directories\":{},\"plan_modes\":{},\"allowed_open_ids\":[\"ou_paired\"]}",
    )?;
    let output = run_credentials(
        &config,
        &[
            ("FEISHU_APP_ID", "app-id"),
            ("FEISHU_APP_SECRET", "app-secret"),
        ],
        None,
    )?;
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
    Ok(())
}

#[test]
fn custom_environment_variable_names_are_respected() -> Result<(), BoxError> {
    let temp = tempfile::tempdir()?;
    let config = write_config(
        temp.path(),
        "[feishu]\napp_id_env = 'MY_BRIDGE_ID'\napp_secret_env = 'MY_BRIDGE_SECRET'\nproxy_env = 'MY_BRIDGE_PROXY'\n",
        WHITELIST_ACCESS,
    )?;
    let output = run_credentials(
        &config,
        &[("MY_BRIDGE_ID", "id"), ("MY_BRIDGE_SECRET", "secret")],
        None,
    )?;
    assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
    // Default names must not be forced on an existing configuration; a
    // missing configured name is reported instead of the default one.
    let output = run_credentials(
        &config,
        &[("FEISHU_APP_ID", "id"), ("FEISHU_APP_SECRET", "secret")],
        None,
    )?;
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("MY_BRIDGE_ID"), "{stderr}");
    Ok(())
}
