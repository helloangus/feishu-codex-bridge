//! Durable metadata under the exclusive supervisor lock.
use bridge_app::diagnostics::Sink;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Starting,
    Running,
    Backoff,
    Stopping,
    Stopped,
    Failed,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub pid: u32,
    pub unix_ms: u64,
    pub phase: Phase,
    pub retry_delay_seconds: u64,
}
pub struct Recorder {
    directory: PathBuf,
    log: crate::logging::Log,
    finished: std::sync::atomic::AtomicBool,
}
impl Recorder {
    pub fn open(state: &Path) -> io::Result<Self> {
        let directory = state.join("runtime/supervisor");
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&directory)?;
        let log = crate::logging::Log::open(&directory)?;
        Ok(Self {
            directory,
            log,
            finished: std::sync::atomic::AtomicBool::new(false),
        })
    }
    pub fn finish(&self, success: bool) -> io::Result<()> {
        self.publish(
            if success {
                Phase::Stopped
            } else {
                Phase::Failed
            },
            0,
        )?;
        self.finished
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
    pub fn publish(&self, phase: Phase, delay: u64) -> io::Result<()> {
        let snapshot = Snapshot {
            version: 1,
            pid: std::process::id(),
            unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
            phase,
            retry_delay_seconds: delay,
        };
        let record = serde_json::to_string(&snapshot)?;
        let temporary = self.directory.join("snapshot.pending");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
            )
            .open(&temporary)?;
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.nlink() != 1 {
            return Err(io::Error::other("invalid supervisor snapshot file"));
        }
        file.set_len(0)?;
        file.write_all(record.as_bytes())?;
        file.sync_all()?;
        fs::rename(temporary, self.directory.join("snapshot.json"))?;
        File::open(&self.directory)?.sync_all()?;
        self.log.write(&record, false)
    }
}
impl Drop for Recorder {
    fn drop(&mut self) {
        if !self.finished.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = self.publish(Phase::Failed, 0);
        }
    }
}
pub fn read(state: &Path) -> io::Result<Option<Snapshot>> {
    let directory = state.join("runtime/supervisor");
    match fs::symlink_metadata(&directory) {
        Ok(meta) if !meta.is_dir() => return Err(io::Error::other("invalid supervisor directory")),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    }
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(directory.join("snapshot.json"))
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("invalid supervisor snapshot"));
    }
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Err(io::Error::other("supervisor snapshot too large"));
    }
    let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
    if snapshot.version != 1 {
        return Err(io::Error::other("unsupported supervisor snapshot"));
    }
    Ok(Some(snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normal_exit_and_early_failure_leave_distinct_terminal_records() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let recorder = Recorder::open(tmp.path())?;
        recorder.finish(true)?;
        drop(recorder);
        assert!(matches!(
            read(tmp.path())?.map(|s| s.phase),
            Some(Phase::Stopped)
        ));
        let recorder = Recorder::open(tmp.path())?;
        recorder.publish(Phase::Starting, 0)?;
        drop(recorder);
        assert!(matches!(
            read(tmp.path())?.map(|s| s.phase),
            Some(Phase::Failed)
        ));
        Ok(())
    }
    #[test]
    fn state_and_log_survive_recorder_exit() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        assert!(read(tmp.path())?.is_none());
        assert!(!tmp.path().join("runtime").exists());
        let recorder = Recorder::open(tmp.path())?;
        recorder.publish(Phase::Backoff, 8)?;
        // Simulate an abrupt exit that bypasses Drop's best-effort failure record.
        recorder
            .finished
            .store(true, std::sync::atomic::Ordering::Relaxed);
        drop(recorder);
        let snapshot = read(tmp.path())?.ok_or_else(|| io::Error::other("missing"))?;
        assert!(matches!(snapshot.phase, Phase::Backoff));
        assert_eq!(snapshot.retry_delay_seconds, 8);
        assert_eq!(
            fs::read_to_string(tmp.path().join("runtime/supervisor/events.jsonl"))?
                .lines()
                .count(),
            1
        );
        Ok(())
    }
    #[test]
    fn publication_rejects_link_and_retains_previous_snapshot() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let recorder = Recorder::open(tmp.path())?;
        recorder.publish(Phase::Starting, 0)?;
        let target = tmp.path().join("keep");
        fs::write(&target, b"keep")?;
        std::os::unix::fs::symlink(
            &target,
            tmp.path().join("runtime/supervisor/snapshot.pending"),
        )?;
        assert!(recorder.publish(Phase::Running, 0).is_err());
        assert_eq!(fs::read(target)?, b"keep");
        assert!(matches!(
            read(tmp.path())?.map(|s| s.phase),
            Some(Phase::Starting)
        ));
        Ok(())
    }
}
