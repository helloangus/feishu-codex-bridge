//! Bounded blocking access to the existing single JSON writer.
use crate::state::JsonStore;
use bridge_app::{
    MessageJournal,
    sessions::{DurableJournal, SessionStore, SessionStoreError, StoreFuture},
};
use bridge_core::SessionKey;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct AsyncState {
    store: Arc<Mutex<JsonStore>>,
    slots: Arc<Semaphore>,
}

impl AsyncState {
    /// Transfer the locked store from startup; no second writer is created.
    pub fn new(store: JsonStore) -> Self {
        Self {
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

impl SessionStore for AsyncState {
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
