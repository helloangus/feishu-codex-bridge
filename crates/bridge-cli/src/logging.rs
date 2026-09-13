//! Rotating metadata-only JSONL. The service lock must be held by the caller.
use bridge_app::diagnostics::Sink;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Mutex,
};

pub struct Log(Mutex<Writer>);
struct Writer {
    directory: PathBuf,
    file: Option<File>,
    size: u64,
    limit: u64,
}
const MAX_RECORD: usize = 4096;
const BACKUPS: usize = 3;

impl Log {
    pub fn open(directory: &Path) -> io::Result<Self> {
        Self::with_limit(directory, 2 * 1024 * 1024)
    }
    fn with_limit(directory: &Path, limit: u64) -> io::Result<Self> {
        if !fs::symlink_metadata(directory)?.is_dir() {
            return Err(io::Error::other("log directory must be a directory"));
        }
        let file = open_file(&directory.join("events.jsonl"))?;
        let size = file.metadata()?.len();
        Ok(Self(Mutex::new(Writer {
            directory: directory.into(),
            file: Some(file),
            size,
            limit,
        })))
    }
}
fn open_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    let meta = file.metadata()?;
    use std::os::unix::fs::MetadataExt;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(io::Error::other("log must be a private regular file"));
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}
impl Writer {
    fn write(&mut self, record: &str) -> io::Result<()> {
        if record.len() > MAX_RECORD || record.contains(['\n', '\r']) {
            return Err(io::Error::other("invalid diagnostic record"));
        }
        if self.file.is_none() {
            return Err(io::Error::other("log unavailable after rotation failure"));
        }
        if self.size > 0 && self.size + record.len() as u64 + 1 > self.limit {
            if let Some(file) = self.file.take() {
                file.sync_all()?;
            }
            for index in (1..=BACKUPS).rev() {
                let source = self.directory.join(if index == 1 {
                    "events.jsonl".into()
                } else {
                    format!("events.jsonl.{}", index - 1)
                });
                let destination = self.directory.join(format!("events.jsonl.{index}"));
                match fs::rename(source, destination) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            self.file = Some(open_file(&self.directory.join("events.jsonl"))?);
            self.size = 0;
            File::open(&self.directory)?.sync_all()?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("log unavailable"))?;
        let line = format!("{record}\n");
        file.write_all(line.as_bytes())?;
        self.size += line.len() as u64;
        Ok(())
    }
}
impl Sink for Log {
    fn write(&self, record: &str, panic: bool) -> io::Result<()> {
        let mut writer = if panic {
            self.0
                .try_lock()
                .map_err(|_| io::Error::other("log busy during panic"))?
        } else {
            self.0
                .lock()
                .map_err(|_| io::Error::other("log poisoned"))?
        };
        writer.write(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotation_retains_newest_records_and_reopens_without_truncating() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let log = Log::with_limit(tmp.path(), 10)?;
        for n in 0..8 {
            log.write(&format!("{{\"n\":{n}}}"), false)?;
        }
        drop(log);
        assert_eq!(
            fs::read_to_string(tmp.path().join("events.jsonl"))?,
            "{\"n\":7}\n"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join("events.jsonl.3"))?,
            "{\"n\":4}\n"
        );
        assert_eq!(fs::read_dir(tmp.path())?.count(), 4);
        let log = Log::open(tmp.path())?;
        log.write("{}", false)?;
        assert!(fs::read_to_string(tmp.path().join("events.jsonl"))?.ends_with("{}\n"));
        assert_eq!(
            fs::metadata(tmp.path().join("events.jsonl"))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        Ok(())
    }
    #[test]
    fn invalid_records_and_symlinks_are_rejected() -> io::Result<()> {
        let tmp = tempfile::tempdir()?;
        let log = Log::open(tmp.path())?;
        assert!(log.write("{}\nforged", false).is_err());
        assert!(log.write(&"x".repeat(4097), false).is_err());
        assert_eq!(fs::metadata(tmp.path().join("events.jsonl"))?.len(), 0);
        let other = tempfile::tempdir()?;
        std::os::unix::fs::symlink(
            tmp.path().join("events.jsonl"),
            other.path().join("events.jsonl"),
        )?;
        assert!(Log::open(other.path()).is_err());
        let _guard = log.0.lock().map_err(|_| io::Error::other("lock"))?;
        assert!(log.write("{}", true).is_err());
        Ok(())
    }
}
