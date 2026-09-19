//! `status` and `service status` coverage through the real CLI binary.
//! Each case uses a temporary state directory with local locks and sockets;
//! no long-running service is started.
use bridge_cli::Config;
use fs2::FileExt;
use std::{
    fs,
    io::{self, Read, Write},
    os::unix::net::UnixListener,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
};

type BoxError = Box<dyn std::error::Error>;

fn write_config(temp: &std::path::Path) -> Result<std::path::PathBuf, BoxError> {
    let config = temp.join("bridge.toml");
    fs::write(
        &config,
        format!(
            "[workspace]\nroot = {:?}\ncwd = {:?}\nstate_dir = {:?}\n[access]\nmode = 'restricted'\nallowed_open_ids = ['test-user']\n[codex]\nexecutable = 'codex'\nargs = ['app-server']\nsandbox = 'workspaceWrite'\n[feishu]\napp_id_env = 'FEISHU_APP_ID'\napp_secret_env = 'FEISHU_APP_SECRET'\n",
            temp.join("root"),
            temp.join("root"),
            temp.join("state"),
        ),
    )?;
    fs::create_dir_all(temp.join("root"))?;
    Ok(config)
}

fn run_status(args: &[&str]) -> Result<(i32, String, String), BoxError> {
    let output = Command::new(env!("CARGO_BIN_EXE_bridge"))
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    Ok((
        output.status.code().ok_or("bridge was terminated")?,
        String::from_utf8(output.stdout)?,
        String::from_utf8(output.stderr)?,
    ))
}

fn hold_lock(state: &std::path::Path, name: &str) -> Result<fs::File, BoxError> {
    fs::create_dir_all(state.join("runtime"))?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state.join("runtime").join(name))?;
    lock.lock_exclusive()?;
    Ok(lock)
}

/// Answer exactly one phase query; the completion result is delivered after
/// the subprocess run so a failed exchange fails the test with its cause.
fn serve_once(
    socket: &std::path::Path,
    phase: String,
) -> Result<mpsc::Receiver<io::Result<()>>, BoxError> {
    let listener = UnixListener::bind(socket)?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| -> io::Result<()> {
            let (mut stream, _) = listener.accept()?;
            let mut command = [0_u8; 1];
            stream.read_exact(&mut command)?;
            if command[0] != b's' {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "status must use the phase command",
                ));
            }
            stream.write_all(phase.as_bytes())?;
            Ok(())
        })();
        let _ = sender.send(result);
    });
    Ok(receiver)
}

#[test]
fn absent_service_reports_idle_without_errors() -> Result<(), BoxError> {
    let temp = tempfile::tempdir()?;
    let config = write_config(temp.path())?;
    let (code, stdout, stderr) =
        run_status(&["status", "--config", config.to_string_lossy().as_ref()])?;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("未运行"), "{stdout}");
    assert!(stderr.is_empty(), "{stderr}");
    Ok(())
}

#[test]
fn stalled_control_socket_times_out_instead_of_panicking() -> Result<(), BoxError> {
    // Regression: the standalone status command built a runtime without
    // timers, so the control-socket timeout panicked while a supervisor
    // lock was visible. The subprocess must still exit successfully.
    let temp = tempfile::tempdir()?;
    let config = write_config(temp.path())?;
    let state = temp.path().join("state");
    let _lock = hold_lock(&state, "supervisor.lock")?;
    let (code, stdout, stderr) =
        run_status(&["status", "--config", config.to_string_lossy().as_ref()])?;
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
    assert!(!stderr.contains("timer"), "{stderr}");
    assert!(stdout.contains("监督器正在恢复服务"), "{stdout}");
    Ok(())
}

#[test]
fn supervisor_control_socket_phase_is_displayed() -> Result<(), BoxError> {
    let temp = tempfile::tempdir()?;
    let config = write_config(temp.path())?;
    let state = temp.path().join("state");
    let _lock = hold_lock(&state, "supervisor.lock")?;
    let replies = serve_once(
        &state.join("runtime/control.sock"),
        "{\"phase\":\"running\"}".into(),
    )?;
    let (code, stdout, stderr) =
        run_status(&["status", "--config", config.to_string_lossy().as_ref()])?;
    replies.recv()??;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("监督器正在运行"), "{stdout}");
    Ok(())
}

#[test]
fn guard_control_socket_phase_is_displayed() -> Result<(), BoxError> {
    let temp = tempfile::tempdir()?;
    let config = write_config(temp.path())?;
    let state = temp.path().join("state");
    let _lock = hold_lock(&state, "guard.lock")?;
    let replies = serve_once(
        &state.join("runtime/guard.sock"),
        "{\"phase\":\"backoff\",\"retry_delay_seconds\":5}".into(),
    )?;
    let (code, stdout, stderr) =
        run_status(&["status", "--config", config.to_string_lossy().as_ref()])?;
    replies.recv()??;
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("将在 5 秒后重试"), "{stdout}");
    Ok(())
}

#[test]
fn service_status_and_standalone_status_agree_when_idle() -> Result<(), BoxError> {
    let temp = tempfile::tempdir()?;
    let config = write_config(temp.path())?;
    Config::read(&config)?.validate()?;
    let standalone = run_status(&["status", "--config", config.to_string_lossy().as_ref()])?;
    assert_eq!(standalone.0, 0, "{}", standalone.2);
    let service = run_status(&[
        "service",
        "--config",
        config.to_string_lossy().as_ref(),
        "status",
    ])?;
    assert_eq!(service.0, 0, "{}", service.2);
    assert_eq!(standalone.1, service.1);
    assert!(standalone.1.contains("未运行"));
    Ok(())
}
