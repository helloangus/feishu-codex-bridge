//! Run ownership is established by a lock, never by a reusable PID.
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Starting,
    Connected,
    Reconnecting,
    Stopped,
    Failed,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub pid: u32,
    pub started_unix_ms: u64,
    pub updated_unix_ms: u64,
    pub phase: Phase,
    #[serde(default)]
    pub heartbeat_unix_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub guard_running: bool,
    pub guard_snapshot: Option<crate::supervisor_state::Snapshot>,
    pub supervisor_snapshot: Option<crate::supervisor_state::Snapshot>,
    pub supervisor_running: bool,
    pub running: bool,
    pub heartbeat_fresh: Option<bool>,
    pub snapshot: Option<Snapshot>,
}

const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
const HEARTBEAT_MAX_AGE_MS: u64 = 30_000;

fn heartbeat_fresh(running: bool, snapshot: Option<&Snapshot>, at: u64) -> Option<bool> {
    if !running {
        return Some(false);
    }
    let snapshot = snapshot?;
    if matches!(snapshot.phase, Phase::Stopped | Phase::Failed) {
        return Some(false);
    }
    let age = at.checked_sub(snapshot.heartbeat_unix_ms?)?;
    Some(age <= HEARTBEAT_MAX_AGE_MS)
}

pub async fn heartbeat(
    health: std::sync::Arc<std::sync::Mutex<Health>>,
    cancel: tokio_util::sync::CancellationToken,
) -> io::Result<()> {
    let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            _ = interval.tick() => {}
        }
        let result = health
            .lock()
            .map_err(|_| io::Error::other("health lock poisoned"))
            .and_then(|mut health| health.pulse());
        if let Err(error) = result {
            eprintln!("{{\"event\":\"health_write_failed\"}}");
            cancel.cancel();
            return Err(error);
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
fn open(path: &Path, create: bool) -> io::Result<File> {
    use std::os::unix::fs::MetadataExt;
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(io::Error::other("invalid health file"));
    }
    Ok(file)
}

pub struct Health {
    directory: PathBuf,
    _lock: File,
    snapshot: Snapshot,
    finished: bool,
}

impl Health {
    pub fn start(state: &Path) -> io::Result<Self> {
        let directory = state.join("runtime");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        if fs::symlink_metadata(&directory)?.file_type().is_symlink() {
            return Err(io::Error::other("runtime directory cannot be a symlink"));
        }
        let lock = open(&directory.join("service.lock"), true)?;
        lock.try_lock_exclusive()?;
        let mut health = Self {
            directory,
            _lock: lock,
            snapshot: Snapshot {
                version: 1,
                pid: std::process::id(),
                started_unix_ms: now(),
                updated_unix_ms: now(),
                phase: Phase::Starting,
                heartbeat_unix_ms: None,
            },
            finished: false,
        };
        health.set(Phase::Starting)?;
        Ok(health)
    }

    pub fn set(&mut self, phase: Phase) -> io::Result<()> {
        self.snapshot.phase = phase;
        self.snapshot.updated_unix_ms = now();
        let temporary = self.directory.join("health.pending");
        // Exclusive service lock serializes publication; no-follow prevents writing through a stale symlink.
        let mut file = open(&temporary, true)?;
        file.set_len(0)?;
        serde_json::to_writer(&mut file, &self.snapshot)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(temporary, self.directory.join("health.json"))?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }

    fn pulse(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.snapshot.heartbeat_unix_ms = Some(now());
        self.set(self.snapshot.phase)
    }

    pub fn finish(&mut self, success: bool) -> io::Result<()> {
        self.set(if success {
            Phase::Stopped
        } else {
            Phase::Failed
        })?;
        self.finished = true;
        Ok(())
    }
}
impl Drop for Health {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.set(Phase::Failed);
        }
    }
}

pub fn status(state: &Path) -> io::Result<Report> {
    let guard_running = crate::supervisor::guard_running(state)?;
    let guard_snapshot = crate::supervisor_state::read(&state.join("guard"))?;
    let supervisor_running = crate::supervisor::is_running(state)?;
    let supervisor_snapshot = crate::supervisor_state::read(state)?;
    let directory = state.join("runtime");
    let lock = match open(&directory.join("service.lock"), false) {
        Ok(lock) => lock,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Report {
                guard_running,
                guard_snapshot,
                supervisor_snapshot,
                supervisor_running,
                running: false,
                heartbeat_fresh: Some(false),
                snapshot: None,
            });
        }
        Err(e) => return Err(e),
    };
    let running = match lock.try_lock_exclusive() {
        Ok(()) => false,
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => true,
        Err(e) => return Err(e),
    };
    let snapshot = match open(&directory.join("health.json"), false) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(4097).read_to_end(&mut bytes)?;
            if bytes.len() > 4096 {
                return Err(io::Error::other("health file exceeds limit"));
            }
            let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
            if snapshot.version != 1 {
                return Err(io::Error::other("unsupported health version"));
            }
            Some(snapshot)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    Ok(Report {
        guard_running,
        guard_snapshot,
        supervisor_snapshot,
        supervisor_running,
        running,
        heartbeat_fresh: heartbeat_fresh(running, snapshot.as_ref(), now()),
        snapshot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn freshness_distinguishes_expiry_legacy_clock_reversal_and_exit() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut health = Health::start(tmp.path())?;
        assert_eq!(heartbeat_fresh(true, Some(&health.snapshot), 50_000), None);
        health.snapshot.heartbeat_unix_ms = Some(10_000);
        assert_eq!(
            heartbeat_fresh(true, Some(&health.snapshot), 40_000),
            Some(true)
        );
        assert_eq!(
            heartbeat_fresh(true, Some(&health.snapshot), 40_001),
            Some(false)
        );
        assert_eq!(heartbeat_fresh(true, Some(&health.snapshot), 9_999), None);
        assert_eq!(
            heartbeat_fresh(false, Some(&health.snapshot), 10_000),
            Some(false)
        );
        health.snapshot.phase = Phase::Failed;
        assert_eq!(
            heartbeat_fresh(true, Some(&health.snapshot), 10_000),
            Some(false)
        );
        Ok(())
    }

    #[test]
    fn pulse_preserves_connection_and_cannot_overwrite_final_state() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut health = Health::start(tmp.path())?;
        health.set(Phase::Reconnecting)?;
        health.pulse()?;
        let report = status(tmp.path())?;
        assert_eq!(report.heartbeat_fresh, Some(true));
        assert_eq!(report.snapshot.map(|s| s.phase), Some(Phase::Reconnecting));
        health.finish(true)?;
        let path = tmp.path().join("runtime/health.json");
        let before = fs::read(&path)?;
        health.pulse()?;
        assert_eq!(fs::read(path)?, before);
        assert_eq!(status(tmp.path())?.heartbeat_fresh, Some(false));
        Ok(())
    }

    #[tokio::test]
    async fn heartbeat_failure_cancels_runtime_and_pre_cancel_does_not_write() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let health = std::sync::Arc::new(std::sync::Mutex::new(Health::start(tmp.path())?));
        let path = tmp.path().join("runtime/health.json");
        let before = fs::read(&path)?;
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        heartbeat(health.clone(), cancel).await?;
        assert_eq!(fs::read(path)?, before);
        fs::create_dir(tmp.path().join("runtime/health.pending"))?;
        let cancel = tokio_util::sync::CancellationToken::new();
        assert!(heartbeat(health, cancel.clone()).await.is_err());
        assert!(cancel.is_cancelled());
        Ok(())
    }

    #[test]
    fn lock_and_snapshot_follow_lifecycle_without_pid_signals() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        assert!(!status(tmp.path())?.running);
        assert!(!tmp.path().join("runtime").exists());
        let mut health = Health::start(tmp.path())?;
        assert!(Health::start(tmp.path()).is_err());
        health.set(Phase::Connected)?;
        assert!(status(tmp.path())?.running);
        assert_eq!(
            status(tmp.path())?.snapshot.map(|s| s.phase),
            Some(Phase::Connected)
        );
        health.finish(true)?;
        drop(health);
        assert!(!status(tmp.path())?.running);
        assert_eq!(
            status(tmp.path())?.snapshot.map(|s| s.phase),
            Some(Phase::Stopped)
        );
        let health = Health::start(tmp.path())?;
        drop(health);
        assert_eq!(
            status(tmp.path())?.snapshot.map(|s| s.phase),
            Some(Phase::Failed)
        );
        Ok(())
    }
    #[test]
    fn stale_connected_snapshot_is_not_live_and_corruption_is_reported() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut health = Health::start(tmp.path())?;
        health.set(Phase::Connected)?;
        health.finished = true; // Simulate death before final publication.
        drop(health);
        assert!(!status(tmp.path())?.running);
        fs::write(tmp.path().join("runtime/health.json"), b"invalid")?;
        assert!(status(tmp.path()).is_err());
        Ok(())
    }
    #[test]
    fn publication_rejects_symlink_without_touching_target() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut health = Health::start(tmp.path())?;
        let target = tmp.path().join("keep");
        fs::write(&target, b"preserve")?;
        std::os::unix::fs::symlink(&target, tmp.path().join("runtime/health.pending"))?;
        assert!(health.set(Phase::Connected).is_err());
        assert_eq!(fs::read(target)?, b"preserve");
        Ok(())
    }
}
