use std::{
    error::Error,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn executable(path: &Path, body: &str) -> Result<(), Box<dyn Error>> {
    fs::write(path, body)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn invoke(script: &str, action: &str) -> Result<(Output, String), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let binary = temp.path().join("fake bridge");
    executable(&binary, "#!/bin/bash\nprintf '%s\\n' \"$@\"\n")?;
    for name in ["cargo", "cc", "rustup", "codex"] {
        executable(&temp.path().join(name), "#!/bin/bash\nexit 0\n")?;
    }
    let config = temp.path().join("private config.toml");
    let output = Command::new("bash")
        .arg(repo().join(script))
        .arg(action)
        .current_dir(temp.path())
        .env_clear()
        .env("PATH", format!("{}:/usr/bin:/bin", temp.path().display()))
        .env("HOME", temp.path())
        .env("BRIDGE_RUST_BIN", &binary)
        .env("BRIDGE_RUST_CONFIG", &config)
        .env("FEISHU_APP_SECRET", "offline-secret-must-not-leak")
        .stdin(Stdio::null())
        .output()?;
    assert!(!String::from_utf8_lossy(&output.stdout).contains("offline-secret-must-not-leak"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("offline-secret-must-not-leak"));
    Ok((output, config.to_string_lossy().into_owned()))
}

#[test]
fn service_actions_and_foreground_preserve_config() -> Result<(), Box<dyn Error>> {
    for action in ["status", "stop", "start", "restart"] {
        let (output, config) = invoke("start.sh", action)?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout)?
                .lines()
                .collect::<Vec<_>>(),
            ["service", "--config", &config, action]
        );
    }
    let (output, config) = invoke("start.sh", "foreground")?;
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)?
            .lines()
            .collect::<Vec<_>>(),
        ["guard", "--config", &config]
    );
    Ok(())
}

#[test]
fn invalid_action_and_pairing_code_fail_before_start() -> Result<(), Box<dyn Error>> {
    let (output, _) = invoke("start.sh", "invalid")?;
    assert_eq!(output.status.code(), Some(2));
    let temp = tempfile::tempdir()?;
    let binary = temp.path().join("bridge");
    executable(&binary, "#!/bin/bash\nexit 99\n")?;
    let output = Command::new(repo().join("start.sh"))
        .arg("start")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("BRIDGE_RUST_BIN", binary)
        .env("FEISHU_PAIRING_CODE", "too short")
        .output()?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("16–256 字节"));
    Ok(())
}

#[test]
fn setup_check_and_noninteractive_defaults_use_native_cli() -> Result<(), Box<dyn Error>> {
    let (output, config) = invoke("setup.sh", "--check")?;
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)?
            .lines()
            .collect::<Vec<_>>(),
        ["config", "check", "--file", &config]
    );

    let (output, config) = invoke("setup.sh", "--no-start")?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains(&format!("config\ninit\n--output\n{config}")));
    assert!(stdout.contains("准备完成。启动：./start.sh start；状态：./start.sh status。"));
    Ok(())
}
