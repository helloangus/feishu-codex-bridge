//! Bounded blocking access to the existing single JSON writer.
use crate::state::JsonStore;
use bridge_app::{
    MessageJournal,
    directories::{DirectoryStore, DirectoryView},
    sessions::{
        DurableJournal, PreferenceChange, Preferences, SessionStore, SessionStoreError, StoreFuture,
    },
};
use bridge_core::SessionKey;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct AsyncState {
    pairing_code: Option<Arc<String>>,
    store: Arc<Mutex<JsonStore>>,
    slots: Arc<Semaphore>,
}

impl AsyncState {
    pub fn with_pairing(mut self, code: Option<String>) -> Self {
        self.pairing_code = code.map(Arc::new);
        self
    }
    /// Startup owns the state lock and has not admitted work yet.
    pub async fn bound_threads(
        &self,
    ) -> Result<std::collections::BTreeSet<String>, SessionStoreError> {
        self.run(|store| Ok(store.state().sessions.values().cloned().collect()))
            .await
    }

    /// Publish all positively observed archive removals in one durable commit.
    pub async fn clear_archived_bindings(
        &self,
        archived: std::collections::BTreeSet<String>,
    ) -> Result<usize, SessionStoreError> {
        self.run(move |store| {
            let mut next = store.state().clone();
            let before = next.sessions.len();
            next.sessions.retain(|_, thread| !archived.contains(thread));
            let removed = before - next.sessions.len();
            if removed > 0 {
                store.replace(next).map_err(|_| SessionStoreError)?;
            }
            Ok(removed)
        })
        .await
    }

    /// Transfer the locked store from startup; no second writer is created.
    pub fn new(store: JsonStore) -> Self {
        Self {
            pairing_code: None,
            store: Arc::new(Mutex::new(store)),
            slots: Arc::new(Semaphore::new(2)),
        }
    }

    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut JsonStore) -> Result<T, SessionStoreError> + Send + 'static,
    ) -> Result<T, SessionStoreError> {
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| SessionStoreError)?;
        let store = self.store.clone();
        // Cancellation of the caller does not cancel an in-flight durable write.
        // The permit remains owned by this worker until the operation completes.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut store = store.lock().map_err(|_| SessionStoreError)?;
            operation(&mut store)
        })
        .await
        .map_err(|_| SessionStoreError)?
    }
}

fn key(session: &SessionKey) -> Result<String, SessionStoreError> {
    let cwd = session.workspace.to_str().ok_or(SessionStoreError)?;
    if session.user.is_empty() || !session.workspace.is_absolute() {
        return Err(SessionStoreError);
    }
    Ok(format!("{}:{cwd}", session.user))
}

fn workspace(root: &Path) -> Result<crate::workspace::Workspace, SessionStoreError> {
    let workspace = crate::workspace::Workspace::new(root).map_err(|_| SessionStoreError)?;
    if workspace.root() != root {
        return Err(SessionStoreError);
    }
    Ok(workspace)
}

fn validate_snapshot(root: &Path, directory: &Path) -> Result<(), SessionStoreError> {
    let resolved = workspace(root)?
        .resolve_existing(root, directory)
        .map_err(|_| SessionStoreError)?;
    if !directory.is_absolute() || resolved != directory {
        return Err(SessionStoreError);
    }
    Ok(())
}

impl DirectoryStore for AsyncState {
    fn propose_directory(
        &self,
        root: PathBuf,
        current: PathBuf,
        input: String,
    ) -> StoreFuture<'_, Option<PathBuf>> {
        Box::pin(async move {
            self.run(move |_| {
                if input.is_empty() || input.len() > 4096 || input.chars().any(char::is_control) {
                    return Err(SessionStoreError);
                }
                let workspace = workspace(&root)?;
                if !Path::new(&input).is_absolute() {
                    validate_snapshot(&root, &current)?;
                }
                if workspace
                    .resolve_existing(&current, Path::new(&input))
                    .is_ok()
                {
                    return Ok(None);
                }
                let target = workspace
                    .resolve_proposed(&current, Path::new(&input))
                    .map_err(|_| SessionStoreError)?;
                // Only a missing path can request creation, never files or denied I/O.
                match std::fs::symlink_metadata(&target) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    _ => return Err(SessionStoreError),
                }
                target.to_str().ok_or(SessionStoreError)?;
                Ok(Some(target))
            })
            .await
        })
    }

    fn create_directory(
        &self,
        user: String,
        root: PathBuf,
        current: PathBuf,
        input: String,
        target: PathBuf,
    ) -> StoreFuture<'_, PathBuf> {
        Box::pin(async move {
            self.run(move |store| {
                if user.is_empty()
                    || input.is_empty()
                    || input.len() > 4096
                    || input.chars().any(char::is_control)
                {
                    return Err(SessionStoreError);
                }
                let workspace = workspace(&root)?;
                validate_snapshot(&root, &current)?;
                if workspace
                    .resolve_proposed(&current, Path::new(&input))
                    .map_err(|_| SessionStoreError)?
                    != target
                {
                    return Err(SessionStoreError);
                }
                match std::fs::symlink_metadata(&target) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    _ => return Err(SessionStoreError),
                }
                let value = target.to_str().ok_or(SessionStoreError)?.to_owned();
                workspace
                    .create_confirmed(&target)
                    .map_err(|_| SessionStoreError)?;
                validate_snapshot(&root, &target)?;
                let mut next = store.state().clone();
                next.directories.insert(user, value);
                store.replace(next).map_err(|_| SessionStoreError)?;
                Ok(target)
            })
            .await
        })
    }
    fn directory_preferences(&self) -> StoreFuture<'_, BTreeMap<String, PathBuf>> {
        Box::pin(async move {
            self.run(|store| {
                Ok(store
                    .state()
                    .directories
                    .iter()
                    .map(|(user, path)| (user.clone(), PathBuf::from(path)))
                    .collect())
            })
            .await
        })
    }

    fn validate_directory(&self, root: PathBuf, directory: PathBuf) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            self.run(move |_| validate_snapshot(&root, &directory))
                .await
        })
    }

    fn inspect_directory(&self, root: PathBuf, current: PathBuf) -> StoreFuture<'_, DirectoryView> {
        Box::pin(async move {
            self.run(move |_| {
                validate_snapshot(&root, &current)?;
                let workspace = workspace(&root)?;
                let mut children = Vec::new();
                // Bound filesystem work even in directories containing many entries.
                for entry in std::fs::read_dir(&current)
                    .map_err(|_| SessionStoreError)?
                    .take(1000)
                {
                    let entry = entry.map_err(|_| SessionStoreError)?;
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else {
                        continue;
                    };
                    if name.trim() != name || name.chars().any(char::is_control) {
                        continue;
                    }
                    if workspace
                        .resolve_existing(&current, Path::new(name))
                        .is_ok()
                    {
                        children.push(name.to_owned());
                    }
                }
                children.sort();
                children.truncate(20);
                Ok(DirectoryView {
                    path: current,
                    children,
                })
            })
            .await
        })
    }

    fn change_directory(
        &self,
        user: String,
        root: PathBuf,
        current: PathBuf,
        input: String,
    ) -> StoreFuture<'_, PathBuf> {
        Box::pin(async move {
            self.run(move |store| {
                if user.is_empty()
                    || input.is_empty()
                    || input.len() > 4096
                    || input.chars().any(char::is_control)
                {
                    return Err(SessionStoreError);
                }
                let input = Path::new(&input);
                if !input.is_absolute() {
                    validate_snapshot(&root, &current)?;
                }
                let path = workspace(&root)?
                    .resolve_existing(&current, input)
                    .map_err(|_| SessionStoreError)?;
                let value = path.to_str().ok_or(SessionStoreError)?.to_owned();
                let mut next = store.state().clone();
                next.directories.insert(user, value);
                store.replace(next).map_err(|_| SessionStoreError)?;
                Ok(path)
            })
            .await
        })
    }
}

impl SessionStore for AsyncState {
    fn pair(&self, user: String, code: String) -> StoreFuture<'_, bool> {
        Box::pin(async move {
            let Some(expected) = &self.pairing_code else {
                return Ok(false);
            };
            if expected.len() < 16 || expected.len() > 256 || code.len() > 256 {
                return Ok(false);
            }
            if user.is_empty() || user.len() > 256 || user.chars().any(char::is_control) {
                return Ok(false);
            }
            let mut difference = expected.len() ^ code.len();
            for (index, byte) in expected.bytes().enumerate() {
                difference |= usize::from(byte ^ code.as_bytes().get(index).copied().unwrap_or(0));
            }
            if difference != 0 {
                return Ok(false);
            }
            self.run(move |store| {
                if store.state().allowed_open_ids.contains(&user) {
                    return Ok(true);
                }
                let mut next = store.state().clone();
                next.allowed_open_ids.insert(user);
                store.replace(next).map_err(|_| SessionStoreError)?;
                Ok(true)
            })
            .await
        })
    }
    fn clear_thread(&self, thread: String) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            if thread.is_empty() {
                return Err(SessionStoreError);
            }
            self.run(move |store| {
                let mut next = store.state().clone();
                next.clear_thread(&thread);
                store.replace(next).map_err(|_| SessionStoreError)
            })
            .await
        })
    }
    fn preferences(&self, session: SessionKey) -> StoreFuture<'_, Preferences> {
        Box::pin(async move {
            let key = key(&session)?;
            self.run(move |store| {
                Ok(Preferences {
                    model: store.state().models.get(&key).cloned(),
                    plan: store.state().plan_modes.get(&key).copied().unwrap_or(false),
                })
            })
            .await
        })
    }
    fn set_preference(&self, session: SessionKey, change: PreferenceChange) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            let key = key(&session)?;
            self.run(move |store| {
                let mut next = store.state().clone();
                match change {
                    PreferenceChange::Model(Some(model)) => {
                        if model.is_empty() || model.len() > 256 {
                            return Err(SessionStoreError);
                        }
                        next.models.insert(key, model);
                    }
                    PreferenceChange::Model(None) => {
                        next.models.remove(&key);
                    }
                    PreferenceChange::Plan(value) => {
                        next.plan_modes.insert(key, value);
                    }
                }
                store.replace(next).map_err(|_| SessionStoreError)
            })
            .await
        })
    }
    fn clear(&self, session: SessionKey) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            let key = key(&session)?;
            self.run(move |store| {
                let mut next = store.state().clone();
                next.sessions.remove(&key);
                store.replace(next).map_err(|_| SessionStoreError)
            })
            .await
        })
    }
    fn thread(&self, session: SessionKey) -> StoreFuture<'_, Option<String>> {
        Box::pin(async move {
            let key = key(&session)?;
            self.run(move |store| Ok(store.state().sessions.get(&key).cloned()))
                .await
        })
    }
    fn bind(&self, session: SessionKey, thread: String) -> StoreFuture<'_, ()> {
        Box::pin(async move {
            let key = key(&session)?;
            if thread.is_empty() {
                return Err(SessionStoreError);
            }
            self.run(move |store| {
                let mut next = store.state().clone();
                next.sessions.insert(key, thread);
                store.replace(next).map_err(|_| SessionStoreError)
            })
            .await
        })
    }
}

#[cfg(test)]
mod pairing_tests {
    use super::*;
    #[tokio::test]
    async fn pairing_is_durable_idempotent_and_never_stores_secret()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let secret = "synthetic-pairing-code";
        let store =
            AsyncState::new(JsonStore::open(temp.path())?).with_pairing(Some(secret.into()));
        assert!(!store.pair("owner".into(), "wrong".into()).await?);
        assert!(store.pair("owner".into(), secret.into()).await?);
        assert!(store.pair("owner".into(), secret.into()).await?);
        assert!(store.pair("second".into(), secret.into()).await?);
        drop(store);
        let state = JsonStore::open(temp.path())?;
        assert!(state.state().allowed_open_ids.contains("owner"));
        assert!(state.state().allowed_open_ids.contains("second"));
        assert!(!std::fs::read_to_string(temp.path().join("state.json"))?.contains(secret));
        Ok(())
    }
    #[tokio::test]
    async fn pairing_write_failure_never_grants_access() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let store = AsyncState::new(JsonStore::open(temp.path())?)
            .with_pairing(Some("synthetic-pairing-code".into()));
        store
            .bind(SessionKey::new("existing", "/project"), "thread".into())
            .await?;
        let backup = temp.path().join("state.previous.json");
        if backup.is_file() {
            std::fs::remove_file(&backup)?;
        }
        std::fs::create_dir(&backup)?;
        assert!(
            store
                .pair("owner".into(), "synthetic-pairing-code".into())
                .await
                .is_err()
        );
        assert!(
            !store
                .run(|s| Ok(s.state().allowed_open_ids.contains("owner")))
                .await?
        );
        Ok(())
    }
}

impl DurableJournal for AsyncState {
    fn claim(&self, message: String) -> StoreFuture<'_, bool> {
        Box::pin(async move {
            self.run(move |store| store.claim_message(&message).map_err(|_| SessionStoreError))
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn preferences_persist_are_scoped_and_survive_new_session()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let store = AsyncState::new(JsonStore::open(temp.path())?);
        let own = SessionKey::new("one", "/tmp/project");
        store
            .set_preference(own.clone(), PreferenceChange::Model(Some("chosen".into())))
            .await?;
        store
            .set_preference(own.clone(), PreferenceChange::Plan(true))
            .await?;
        store.bind(own.clone(), "thread".into()).await?;
        store.clear(own.clone()).await?;
        drop(store);
        let store = AsyncState::new(JsonStore::open(temp.path())?);
        assert_eq!(
            store.preferences(own.clone()).await?,
            Preferences {
                model: Some("chosen".into()),
                plan: true
            }
        );
        assert_eq!(
            store
                .preferences(SessionKey::new("two", "/tmp/project"))
                .await?,
            Preferences::default()
        );
        assert_eq!(
            store
                .preferences(SessionKey::new("one", "/tmp/other"))
                .await?,
            Preferences::default()
        );
        store
            .set_preference(own.clone(), PreferenceChange::Model(None))
            .await?;
        assert_eq!(
            store.preferences(own).await?,
            Preferences {
                model: None,
                plan: true
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn preference_write_failure_preserves_previous_values()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let store = AsyncState::new(JsonStore::open(temp.path())?);
        let own = SessionKey::new("one", "/tmp/project");
        store
            .set_preference(own.clone(), PreferenceChange::Plan(true))
            .await?;
        std::fs::create_dir(temp.path().join("state.previous.json"))?;
        assert!(
            store
                .set_preference(own.clone(), PreferenceChange::Plan(false))
                .await
                .is_err()
        );
        drop(store);
        let store = AsyncState::new(JsonStore::open(temp.path())?);
        assert!(store.preferences(own).await?.plan);
        Ok(())
    }

    #[tokio::test]
    async fn clear_is_scoped_and_survives_reopen() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let store = AsyncState::new(JsonStore::open(temp.path())?);
        let own = SessionKey::new("one", "/tmp/project");
        let other_user = SessionKey::new("two", "/tmp/project");
        let other_directory = SessionKey::new("one", "/tmp/other");
        for session in [&own, &other_user, &other_directory] {
            store.bind(session.clone(), "thread".into()).await?;
        }
        store.clear(own.clone()).await?;
        drop(store);
        let store = AsyncState::new(JsonStore::open(temp.path())?);
        assert_eq!(store.thread(own.clone()).await?, None);
        assert_eq!(store.thread(other_user).await?, Some("thread".into()));
        assert_eq!(store.thread(other_directory).await?, Some("thread".into()));
        store.clear(own).await?;
        Ok(())
    }

    #[tokio::test]
    async fn clear_failure_preserves_existing_binding() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let store = AsyncState::new(JsonStore::open(temp.path())?);
        let own = SessionKey::new("one", "/tmp/project");
        store.bind(own.clone(), "thread".into()).await?;
        std::fs::create_dir(temp.path().join("state.previous.json"))?;
        assert!(store.clear(own.clone()).await.is_err());
        assert_eq!(store.thread(own.clone()).await?, Some("thread".into()));
        drop(store);
        let store = AsyncState::new(JsonStore::open(temp.path())?);
        assert_eq!(store.thread(own).await?, Some("thread".into()));
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_bindings_survive_reopening() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let store = AsyncState::new(JsonStore::open(temp.path())?);
        let one = SessionKey::new("one", "/tmp/project");
        let two = SessionKey::new("two", "/tmp/project");
        let (a, b) = tokio::join!(
            store.bind(one.clone(), "a".into()),
            store.bind(two.clone(), "b".into())
        );
        a?;
        b?;
        assert_eq!(store.thread(one).await?, Some("a".into()));
        assert_eq!(store.thread(two).await?, Some("b".into()));
        drop(store);
        let reopened = JsonStore::open(temp.path())?;
        assert_eq!(reopened.state().sessions.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_caller_does_not_abandon_started_commit()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let store = AsyncState::new(JsonStore::open(temp.path())?);
        let worker = store.clone();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let caller = tokio::spawn(async move {
            worker
                .run(move |store| {
                    let _ = started.send(());
                    blocked
                        .recv_timeout(Duration::from_secs(2))
                        .map_err(|_| SessionStoreError)?;
                    let mut next = store.state().clone();
                    next.sessions
                        .insert("user:/tmp/project".into(), "thread".into());
                    store.replace(next).map_err(|_| SessionStoreError)
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), ready).await??;
        caller.abort();
        let _ = caller.await;
        release.send(())?;
        let binding = tokio::time::timeout(
            Duration::from_secs(2),
            store.thread(SessionKey::new("user", "/tmp/project")),
        )
        .await??;
        assert_eq!(binding, Some("thread".into()));
        Ok(())
    }
}
