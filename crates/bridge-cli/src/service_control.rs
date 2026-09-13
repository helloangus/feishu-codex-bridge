//! Local control protocol; the supervisor lock owns the socket pathname.
use std::{io, path::Path, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
};
use tokio_util::sync::CancellationToken;

pub fn command_lock(state: &Path) -> io::Result<std::fs::File> {
    use fs2::FileExt;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    let directory = state.join("runtime");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)?;
    if !std::fs::symlink_metadata(&directory)?.is_dir() {
        return Err(io::Error::other("runtime must be a directory"));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(directory.join("control.lock"))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(io::Error::other("invalid control lock"));
    }
    file.try_lock_exclusive()?;
    Ok(file)
}

pub async fn request(state: &Path, command: u8) -> io::Result<String> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let socket = if crate::supervisor::guard_running(state)? {
            "runtime/guard.sock"
        } else {
            "runtime/control.sock"
        };
        let mut stream = UnixStream::connect(state.join(socket)).await?;
        stream.write_all(&[command]).await?;
        let mut bytes = Vec::new();
        stream.take(4097).read_to_end(&mut bytes).await?;
        if bytes.len() > 4096 {
            return Err(io::Error::other("control response too large"));
        }
        String::from_utf8(bytes).map_err(io::Error::other)
    })
    .await
    .map_err(|_| io::Error::other("supervisor control timed out"))?
}

pub async fn serve(
    listener: UnixListener,
    cancel: CancellationToken,
    phase: std::sync::Arc<std::sync::Mutex<String>>,
) -> io::Result<()> {
    loop {
        let (mut stream, _) = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = listener.accept() => result?,
        };
        let result = tokio::time::timeout(Duration::from_secs(1), async {
            let command = stream.read_u8().await?;
            let response = match command {
                b's' => phase
                    .lock()
                    .map_err(|_| io::Error::other("phase poisoned"))?
                    .clone(),
                b'p' => std::process::id().to_string(),
                b'x' => {
                    cancel.cancel();
                    "stopping".into()
                }
                _ => "invalid".into(),
            };
            stream.write_all(response.as_bytes()).await
        })
        .await;
        // A disconnected/slow local caller must not terminate supervision.
        let _ = result;
    }
}

pub async fn stop(state: &Path) -> io::Result<()> {
    let guarded = crate::supervisor::guard_running(state)?;
    if !crate::supervisor::is_running(state)? {
        return Ok(());
    }
    if request(state, b'x').await? != "stopping" {
        return Err(io::Error::other("stop not acknowledged"));
    }
    tokio::time::timeout(Duration::from_secs(60), async {
        while crate::supervisor::is_running(state)? {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let guard_state = state.join("guard");
        if let Some(snapshot) =
            crate::supervisor_state::read(if guarded { &guard_state } else { state })?
        {
            if !matches!(snapshot.phase, crate::supervisor_state::Phase::Stopped) {
                return Err(io::Error::other(
                    "supervisor did not record a clean stop; no restart performed",
                ));
            }
        }
        Ok(())
    })
    .await
    .map_err(|_| io::Error::other("supervisor has not stopped; no restart performed"))?
}

pub async fn start(config: &Path, state: &Path) -> io::Result<()> {
    if crate::supervisor::is_running(state)? {
        return Err(io::Error::other("supervisor already running"));
    }
    if crate::health::status(state)?.running {
        return Err(io::Error::other("foreground bridge is running"));
    }
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .arg("guard")
        .arg("--config")
        .arg(std::fs::canonicalize(config)?)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .kill_on_drop(false)
        .spawn()?;
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if child.try_wait()?.is_some() {
                return Err(io::Error::other("supervisor exited during startup"));
            }
            if crate::supervisor::is_running(state)? {
                if let Ok(pid) = request(state, b'p').await {
                    if Some(pid.parse::<u32>().map_err(io::Error::other)?) == child.id() {
                        return Ok(());
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    match result {
        Ok(Ok(())) => { /* Keep the supervisor alive after this command exits. */ }
        other => {
            // Do not kill a supervisor that may already own descendants. Request
            // orderly shutdown only if the responder is the child we launched.
            if let Ok(pid) = request(state, b'p').await {
                if pid.parse::<u32>().ok() == child.id() {
                    let _ = request(state, b'x').await;
                    let _ = tokio::time::timeout(Duration::from_secs(30), child.wait()).await;
                }
            }
            return match other {
                Ok(Err(e)) => Err(e),
                _ => Err(io::Error::other("supervisor startup timed out")),
            };
        }
    }
    drop(child);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn guard_controls_remain_available_without_inner_supervisor() -> io::Result<()> {
        use fs2::FileExt;
        let tmp = tempfile::tempdir()?;
        std::fs::create_dir(tmp.path().join("runtime"))?;
        let lock = std::fs::File::create(tmp.path().join("runtime/guard.lock"))?;
        lock.lock_exclusive()?;
        let listener = UnixListener::bind(tmp.path().join("runtime/guard.sock"))?;
        let cancel = CancellationToken::new();
        let phase = std::sync::Arc::new(std::sync::Mutex::new("backoff:8".into()));
        let task = tokio::spawn(serve(listener, cancel.clone(), phase));
        assert_eq!(request(tmp.path(), b's').await?, "backoff:8");
        assert_eq!(request(tmp.path(), b'x').await?, "stopping");
        task.await.map_err(io::Error::other)??;
        assert!(cancel.is_cancelled());
        Ok(())
    }
    #[test]
    fn control_commands_are_serialized() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let lock = command_lock(tmp.path())?;
        assert!(command_lock(tmp.path()).is_err());
        drop(lock);
        let _lock = command_lock(tmp.path())?;
        Ok(())
    }
    #[tokio::test]
    async fn phase_query_and_stop_are_acknowledged() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        std::fs::create_dir(tmp.path().join("runtime"))?;
        let listener = UnixListener::bind(tmp.path().join("runtime/control.sock"))?;
        let cancel = CancellationToken::new();
        let phase = std::sync::Arc::new(std::sync::Mutex::new("backoff:4".into()));
        let task = tokio::spawn(serve(listener, cancel.clone(), phase));
        assert_eq!(request(tmp.path(), b's').await?, "backoff:4");
        assert_eq!(request(tmp.path(), b'?').await?, "invalid");
        assert!(!cancel.is_cancelled());
        assert_eq!(request(tmp.path(), b'x').await?, "stopping");
        task.await.map_err(io::Error::other)??;
        assert!(cancel.is_cancelled());
        Ok(())
    }
}
