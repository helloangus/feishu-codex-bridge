//! Foreground supervision. Each attempt is a new process with its own runtime.
use crate::supervisor_state::{Phase as StoredPhase, Recorder};
use fs2::FileExt;
use std::{
    fs::{self, OpenOptions},
    io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    process::Stdio,
    time::Duration,
};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

/// Observe ownership without signalling a PID or creating runtime files.
pub fn is_running(state: &Path) -> io::Result<bool> {
    Ok(guard_running(state)? || lock_running(state, "supervisor.lock")?)
}
pub fn guard_running(state: &Path) -> io::Result<bool> {
    lock_running(state, "guard.lock")
}
fn lock_running(state: &Path, name: &str) -> io::Result<bool> {
    let directory = state.join("runtime");
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(io::Error::other("runtime must be a directory"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    }
    let lock = match OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(directory.join(name))
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    validate_lock(&lock)?;
    match lock.try_lock_exclusive() {
        Ok(()) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error),
    }
}

fn validate_lock(lock: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = lock.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(io::Error::other(
            "supervisor lock must be a private regular file",
        ));
    }
    Ok(())
}

#[derive(Default)]
struct Backoff {
    failures: u32,
}

struct HeartbeatWatch {
    started: tokio::time::Instant,
    progress: tokio::time::Instant,
    last: Option<u64>,
}
impl HeartbeatWatch {
    fn new(now: tokio::time::Instant) -> Self {
        Self {
            started: now,
            progress: now,
            last: None,
        }
    }
    fn expired(&mut self, now: tokio::time::Instant, beat: Option<u64>) -> bool {
        if let Some(beat) = beat {
            if self.last != Some(beat) {
                self.last = Some(beat);
                self.progress = now;
            }
        }
        if self.last.is_none() {
            now.duration_since(self.started) >= Duration::from_secs(120)
        } else {
            now.duration_since(self.progress) >= Duration::from_secs(45)
        }
    }
}

async fn watch_heartbeat(state: &Path, pid: Option<u32>) {
    let mut watch = HeartbeatWatch::new(tokio::time::Instant::now());
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        // Probe on the blocking pool; local filesystem failure must not block
        // control cancellation. One probe at a time, no unbounded retry workers.
        let path = state.to_owned();
        let probe = tokio::task::spawn_blocking(move || crate::health::status(&path));
        let report = tokio::time::timeout(Duration::from_secs(2), probe).await;
        let beat = match report {
            Ok(Ok(Ok(report))) if report.running => report
                .snapshot
                .filter(|s| Some(s.pid) == pid)
                .and_then(|s| s.heartbeat_unix_ms),
            Err(_) => return,
            _ => None,
        };
        if watch.expired(tokio::time::Instant::now(), beat) {
            return;
        }
    }
}
impl Backoff {
    fn after_exit(&mut self, success: bool, elapsed: Duration) -> Option<Duration> {
        if success {
            return None;
        }
        if elapsed >= Duration::from_secs(60) {
            self.failures = 0;
        }
        self.failures += 1;
        // Stop a persistent startup/configuration failure rather than retry forever.
        if self.failures > 10 {
            return None;
        }
        Some(Duration::from_secs(
            (2u64 << (self.failures - 1).min(4)).min(30),
        ))
    }
}

async fn stop_child(child: &mut Child) -> io::Result<()> {
    stop_child_with_grace(child, Duration::from_secs(20)).await
}
async fn stop_child_with_grace(child: &mut Child, grace: Duration) -> io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    if let Some(pid) = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .and_then(rustix::process::Pid::from_raw)
    {
        // The unreaped child is owned here; no PID read from a state file is used.
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(result) => {
            result?;
        }
        Err(_) => {
            child.start_kill()?;
            child.wait().await?;
            return Err(io::Error::other(
                "bridge forced to exit; inspect Codex descendants before restarting",
            ));
        }
    }
    Ok(())
}

pub async fn run(config: &Path, state: &Path) -> io::Result<()> {
    run_layer(config, state, false).await
}
pub async fn guard(config: &Path, state: &Path) -> io::Result<()> {
    run_layer(config, state, true).await
}
async fn run_layer(config: &Path, state: &Path, outer: bool) -> io::Result<()> {
    let directory = state.join("runtime");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)?;
    if !fs::symlink_metadata(&directory)?.is_dir() {
        return Err(io::Error::other("runtime must be a directory"));
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(directory.join(if outer {
            "guard.lock"
        } else {
            "supervisor.lock"
        }))?;
    validate_lock(&lock)?;
    lock.try_lock_exclusive()?;
    let guard_state = state.join("guard");
    let recorder = Recorder::open(if outer { &guard_state } else { state })?;
    recorder.publish(StoredPhase::Starting, 0)?;
    crate::descendants::enable()?;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    let socket = directory.join(if outer { "guard.sock" } else { "control.sock" });
    match fs::symlink_metadata(&socket) {
        Ok(meta) if meta.file_type().is_socket() => fs::remove_file(&socket)?,
        Ok(_) => return Err(io::Error::other("control path is not a socket")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = tokio::net::UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let config = fs::canonicalize(config)?;
    let executable = std::env::current_exe()?;
    // Register signals before creating any child.
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let cancel = CancellationToken::new();
    let phase = std::sync::Arc::new(std::sync::Mutex::new("starting".to_string()));
    let control_cancel = cancel.clone();
    let control_phase = phase.clone();
    let control = tokio::spawn(async move {
        let result =
            crate::service_control::serve(listener, control_cancel.clone(), control_phase).await;
        control_cancel.cancel();
        result
    });
    let stop = cancel.clone();
    let signal = tokio::spawn(async move {
        tokio::select! { _ = term.recv() => {}, _ = interrupt.recv() => {} }
        stop.cancel();
    });
    let result = supervise(
        &executable,
        &config,
        cancel.clone(),
        phase.clone(),
        Some(&recorder),
        if outer { "supervise" } else { "run" },
        Some(state),
    )
    .await;
    cancel.cancel();
    let control_result = control.await;
    signal.abort();
    let _ = signal.await;
    let socket_result = fs::remove_file(socket);
    let success = result.is_ok() && socket_result.is_ok() && matches!(&control_result, Ok(Ok(())));
    recorder.finish(success)?;
    socket_result?;
    if !matches!(control_result, Ok(Ok(()))) {
        return Err(io::Error::other("supervisor control failed"));
    }
    result
}

async fn supervise(
    executable: &Path,
    config: &Path,
    cancel: CancellationToken,
    phase: std::sync::Arc<std::sync::Mutex<String>>,
    recorder: Option<&Recorder>,
    action: &str,
    state: Option<&Path>,
) -> io::Result<()> {
    let mut backoff = Backoff::default();
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let mut command = Command::new(executable);
        command
            .arg(action)
            .arg("--config")
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = command.spawn()?;
        if let Some(recorder) = recorder {
            if let Err(error) = recorder.publish(StoredPhase::Running, 0) {
                let _ = stop_child(&mut child).await;
                crate::descendants::clean().await?;
                return Err(error);
            }
        }
        *phase
            .lock()
            .map_err(|_| io::Error::other("phase poisoned"))? = "running".into();
        let started = tokio::time::Instant::now();
        let child_id = child.id();
        let watchdog = async {
            if action != "run" {
                std::future::pending::<()>().await;
            }
            if let Some(state) = state {
                watch_heartbeat(state, child_id).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        let status = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                let recorded = recorder.map(|r| r.publish(StoredPhase::Stopping, 0)).transpose();
                *phase.lock().map_err(|_| io::Error::other("phase poisoned"))? = "stopping".into();
                let stopped = stop_child_with_grace(&mut child, Duration::from_secs(if action == "supervise" {45} else {20})).await;
                crate::descendants::clean().await?;
                recorded?;
                return stopped;
            },
            status = child.wait() => status?,
            _ = watchdog => {
                eprintln!("{{\"event\":\"heartbeat_stalled\"}}");
                let _ = stop_child(&mut child).await;
                if child.try_wait()?.is_none() {return Err(io::Error::other("bridge did not exit after stalled heartbeat"));}
                crate::descendants::clean().await?;
                if backoff.after_exit(false, Duration::ZERO).is_none() {return Err(io::Error::other("heartbeat recovery budget exhausted"));}
                if let Some(recorder) = recorder {recorder.publish(StoredPhase::Backoff, 30)?;}
                *phase.lock().map_err(|_| io::Error::other("phase poisoned"))? = "backoff:30".into();
                tokio::select! {_ = cancel.cancelled() => return Ok(()), _ = tokio::time::sleep(Duration::from_secs(30)) => {}}
                continue;
            },
        };
        crate::descendants::clean().await?;
        if status.success() {
            return Ok(());
        }
        let Some(delay) = backoff.after_exit(
            false,
            if action == "supervise" {
                Duration::ZERO
            } else {
                started.elapsed()
            },
        ) else {
            return Err(io::Error::other(
                "bridge repeatedly exited; restart budget exhausted",
            ));
        };
        *phase
            .lock()
            .map_err(|_| io::Error::other("phase poisoned"))? =
            format!("backoff:{}", delay.as_secs());
        if let Some(recorder) = recorder {
            recorder.publish(StoredPhase::Backoff, delay.as_secs())?;
        }
        eprintln!(
            "{{\"event\":\"supervisor_retry\",\"delay_seconds\":{}}}",
            delay.as_secs()
        );
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn heartbeat_requires_progress_and_has_startup_grace() {
        let start = tokio::time::Instant::now();
        let mut watch = HeartbeatWatch::new(start);
        assert!(!watch.expired(start + Duration::from_secs(119), None));
        assert!(watch.expired(start + Duration::from_secs(120), None));
        let mut watch = HeartbeatWatch::new(start);
        assert!(!watch.expired(start, Some(10)));
        assert!(!watch.expired(start + Duration::from_secs(44), Some(10)));
        assert!(watch.expired(start + Duration::from_secs(45), Some(10)));
        assert!(!watch.expired(start + Duration::from_secs(46), Some(11)));
        assert!(!watch.expired(start + Duration::from_secs(80), None));
        assert!(watch.expired(start + Duration::from_secs(91), None));
    }

    #[test]
    fn guard_lock_is_visible_during_inner_supervisor_absence() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        fs::create_dir(temp.path().join("runtime"))?;
        let lock = fs::File::create(temp.path().join("runtime/guard.lock"))?;
        lock.lock_exclusive()?;
        assert!(guard_running(temp.path())?);
        assert!(is_running(temp.path())?);
        assert!(!lock_running(temp.path(), "supervisor.lock")?);
        drop(lock);
        assert!(!is_running(temp.path())?);
        Ok(())
    }
    #[test]
    fn supervisor_ownership_is_visible_without_child_and_clears_on_release() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        assert!(!is_running(tmp.path())?);
        assert!(!tmp.path().join("runtime").exists());
        fs::create_dir(tmp.path().join("runtime"))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(tmp.path().join("runtime/supervisor.lock"))?;
        lock.try_lock_exclusive()?;
        let report = crate::health::status(tmp.path())?;
        assert!(report.supervisor_running);
        assert!(!report.running);
        assert!(report.snapshot.is_none());
        drop(lock);
        assert!(!crate::health::status(tmp.path())?.supervisor_running);
        Ok(())
    }

    #[test]
    fn status_rejects_redirected_and_hard_linked_locks() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        fs::create_dir(tmp.path().join("runtime"))?;
        let target = tmp.path().join("preserve");
        fs::write(&target, b"preserve")?;
        let lock = tmp.path().join("runtime/supervisor.lock");
        std::os::unix::fs::symlink(&target, &lock)?;
        assert!(is_running(tmp.path()).is_err());
        fs::remove_file(&lock)?;
        fs::hard_link(&target, &lock)?;
        assert!(is_running(tmp.path()).is_err());
        assert_eq!(fs::read(target)?, b"preserve");
        Ok(())
    }
    #[test]
    fn retries_are_bounded_and_stable_run_resets_delay() {
        let mut backoff = Backoff::default();
        for seconds in [2, 4, 8, 16, 30, 30, 30, 30, 30, 30] {
            assert_eq!(
                backoff.after_exit(false, Duration::ZERO),
                Some(Duration::from_secs(seconds))
            );
        }
        assert_eq!(backoff.after_exit(false, Duration::ZERO), None);
        assert_eq!(
            backoff.after_exit(false, Duration::from_secs(60)),
            Some(Duration::from_secs(2))
        );
        assert_eq!(backoff.after_exit(true, Duration::ZERO), None);
    }
    #[tokio::test]
    async fn cancelled_supervision_never_spawns() -> io::Result<()> {
        let cancel = CancellationToken::new();
        cancel.cancel();
        supervise(
            Path::new("/nonexistent/bridge"),
            Path::new("/nonexistent/config"),
            cancel,
            std::sync::Arc::new(std::sync::Mutex::new("starting".into())),
            None,
            "run",
            None,
        )
        .await
    }
}
