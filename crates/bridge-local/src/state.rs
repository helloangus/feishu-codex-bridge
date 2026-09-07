//! A single writer owns all state. Atomic replacements precede in-memory commits.
use bridge_app::MessageJournal;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;
use thiserror::Error;

pub const SCHEMA_VERSION: u32 = 1;
pub const SEEN_LIMIT: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub schema_version: u32,
    pub sessions: BTreeMap<String, String>,
    pub models: BTreeMap<String, String>,
    pub directories: BTreeMap<String, String>,
    pub plan_modes: BTreeMap<String, bool>,
    pub allowed_open_ids: BTreeSet<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            sessions: BTreeMap::new(),
            models: BTreeMap::new(),
            directories: BTreeMap::new(),
            plan_modes: BTreeMap::new(),
            allowed_open_ids: BTreeSet::new(),
        }
    }
}

impl State {
    pub fn empty() -> Self {
        Self::default()
    }

    /// All bindings to an archived thread disappear in the same file commit.
    pub fn clear_thread(&mut self, thread: &str) {
        self.sessions.retain(|_, value| value != thread);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema_version: u32,
    ids: Vec<String>,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("状态文件 I/O 失败")]
    Io(#[from] io::Error),
    #[error("状态 JSON 格式无效")]
    Json(#[from] serde_json::Error),
    #[error("不支持的状态 schema 版本：{0}")]
    Version(u32),
    #[error("状态目录已有写入者")]
    Locked,
    #[error("状态写入结果不确定，必须重新打开存储后再操作")]
    Uncertain,
    #[error("消息 ID 不能为空")]
    EmptyMessage,
}

/// Keep this owner in a bounded blocking worker, never across network awaits.
pub struct JsonStore {
    directory: PathBuf,
    _lock: File,
    state: State,
    journal: Journal,
    healthy: bool,
}

fn read_optional<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn check_version(version: u32) -> Result<(), StoreError> {
    if version != SCHEMA_VERSION {
        return Err(StoreError::Version(version));
    }
    Ok(())
}

/// Write in the destination directory; temp file is private from creation.
pub(crate) fn atomic_json(path: &Path, value: &impl Serialize) -> Result<(), StoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing parent"))?;
    let mut temp = NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temp, value)?;
    temp.write_all(b"\n")?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|error| StoreError::Io(error.error))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

impl JsonStore {
    pub fn open(directory: &Path) -> Result<Self, StoreError> {
        if directory.join("migration-in-progress").exists() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "上次迁移未完成；请使用新的目标目录重新导入",
            )
            .into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory)?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(directory)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(directory.join("state.lock"))?;
        lock.try_lock_exclusive().map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                StoreError::Locked
            } else {
                StoreError::Io(error)
            }
        })?;
        let state =
            read_optional::<State>(&directory.join("state.json"))?.unwrap_or_else(State::empty);
        check_version(state.schema_version)?;
        let journal =
            read_optional::<Journal>(&directory.join("seen-messages.json"))?.unwrap_or(Journal {
                schema_version: SCHEMA_VERSION,
                ids: vec![],
            });
        check_version(journal.schema_version)?;
        if journal.ids.len() > SEEN_LIMIT || journal.ids.iter().any(|id| id.is_empty()) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid journal").into());
        }
        Ok(Self {
            directory: directory.into(),
            _lock: lock,
            state,
            journal,
            healthy: true,
        })
    }

    pub fn state(&self) -> &State {
        &self.state
    }
    pub fn seen_messages(&self) -> &[String] {
        &self.journal.ids
    }

    pub fn replace(&mut self, next: State) -> Result<(), StoreError> {
        if !self.healthy {
            return Err(StoreError::Uncertain);
        }
        check_version(next.schema_version)?;
        let path = self.directory.join("state.json");
        if path.exists() {
            atomic_json(&self.directory.join("state.previous.json"), &self.state)?;
        }
        if let Err(error) = atomic_json(&path, &next) {
            self.healthy = false;
            return Err(error);
        }
        self.state = next;
        Ok(())
    }

    pub fn import_seen(&mut self, ids: &[String]) -> Result<(), StoreError> {
        if !self.healthy {
            return Err(StoreError::Uncertain);
        }
        let mut unique = Vec::new();
        for id in ids {
            if id.is_empty() {
                return Err(StoreError::EmptyMessage);
            }
            unique.retain(|old| old != id);
            unique.push(id.clone());
        }
        let start = unique.len().saturating_sub(SEEN_LIMIT);
        let next = Journal {
            schema_version: SCHEMA_VERSION,
            ids: unique.split_off(start),
        };
        if let Err(error) = atomic_json(&self.directory.join("seen-messages.json"), &next) {
            self.healthy = false;
            return Err(error);
        }
        self.journal = next;
        Ok(())
    }
}

impl MessageJournal for JsonStore {
    type Error = StoreError;
    fn claim_message(&mut self, id: &str) -> Result<bool, StoreError> {
        if !self.healthy {
            return Err(StoreError::Uncertain);
        }
        if id.is_empty() {
            return Err(StoreError::EmptyMessage);
        }
        if self.journal.ids.iter().any(|old| old == id) {
            return Ok(false);
        }
        let mut ids = self.journal.ids.clone();
        ids.push(id.to_owned());
        self.import_seen(&ids)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persistence_lock_and_restart_dedup() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let mut store = JsonStore::open(temp.path())?;
        assert!(matches!(
            JsonStore::open(temp.path()),
            Err(StoreError::Locked)
        ));
        assert!(store.claim_message("m")?);
        assert!(!store.claim_message("m")?);
        let mut next = State::empty();
        next.sessions
            .insert("user:/project".into(), "thread".into());
        store.replace(next.clone())?;
        drop(store);
        let mut store = JsonStore::open(temp.path())?;
        assert_eq!(store.state(), &next);
        assert!(!store.claim_message("m")?);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(temp.path().join("state.json"))?
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        Ok(())
    }
    #[test]
    fn malformed_and_future_state_are_not_reset() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        fs::write(temp.path().join("state.json"), "broken")?;
        assert!(matches!(
            JsonStore::open(temp.path()),
            Err(StoreError::Json(_))
        ));
        let mut next = State::empty();
        next.schema_version = 2;
        atomic_json(&temp.path().join("state.json"), &next)?;
        assert!(matches!(
            JsonStore::open(temp.path()),
            Err(StoreError::Version(2))
        ));
        Ok(())
    }
    #[test]
    fn failed_commit_never_advances_memory_or_allows_work() -> Result<(), Box<dyn std::error::Error>>
    {
        let temp = tempfile::tempdir()?;
        let mut store = JsonStore::open(temp.path())?;
        fs::create_dir(temp.path().join("seen-messages.json"))?;
        assert!(store.claim_message("m").is_err());
        assert!(store.seen_messages().is_empty());
        assert!(matches!(
            store.claim_message("m"),
            Err(StoreError::Uncertain)
        ));
        Ok(())
    }
    #[test]
    fn journal_is_bounded_and_archive_preserves_unrelated_bindings()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let mut store = JsonStore::open(temp.path())?;
        let ids: Vec<_> = (0..1005).map(|i| i.to_string()).collect();
        store.import_seen(&ids)?;
        assert_eq!(store.seen_messages().len(), 1000);
        assert_eq!(store.seen_messages()[0], "5");
        let mut next = State::empty();
        next.sessions.extend([
            ("a".into(), "t".into()),
            ("b".into(), "t".into()),
            ("c".into(), "other".into()),
        ]);
        next.clear_thread("t");
        store.replace(next)?;
        assert_eq!(store.state().sessions.len(), 1);
        Ok(())
    }
}
