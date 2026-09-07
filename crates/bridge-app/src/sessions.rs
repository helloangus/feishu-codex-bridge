//! Session preparation commits the binding before starting side effects.
use crate::ports::{AgentBackend, BackendError, Sandbox, TurnInput, TurnRef};
use bridge_core::{SessionKey, task::TaskSpec};
use std::{future::Future, path::PathBuf, pin::Pin};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
#[error("状态保存或读取失败")]
pub struct SessionStoreError;

pub type StoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, SessionStoreError>> + Send + 'a>>;

pub trait SessionStore: Send + Sync {
    fn thread(&self, session: SessionKey) -> StoreFuture<'_, Option<String>>;
    fn bind(&self, session: SessionKey, thread: String) -> StoreFuture<'_, ()>;
}

/// Async counterpart of the offline MessageJournal. True means durable claim;
/// false means an already claimed message, not permission to execute again.
pub trait DurableJournal: Send + Sync {
    fn claim(&self, message: String) -> StoreFuture<'_, bool>;
}

#[derive(Debug, Error)]
pub enum StartError {
    #[error(transparent)]
    Storage(#[from] SessionStoreError),
    #[error(transparent)]
    Backend(#[from] BackendError),
}

/// The scheduler must reserve global execution before calling this use case.
/// Never retry this whole operation automatically after an uncertain result.
pub async fn start<S: SessionStore + ?Sized>(
    backend: &dyn AgentBackend,
    store: &S,
    task: &TaskSpec,
    images: Vec<PathBuf>,
    sandbox: Sandbox,
) -> Result<TurnRef, StartError> {
    let input = prepare(backend, store, task, images, sandbox).await?;
    Ok(backend.start_turn(input).await?)
}

/// The actor can install its execution identity gate before submitting the
/// returned input. Preparation persists state but never starts a turn.
pub async fn prepare<S: SessionStore + ?Sized>(
    backend: &dyn AgentBackend,
    store: &S,
    task: &TaskSpec,
    images: Vec<PathBuf>,
    sandbox: Sandbox,
) -> Result<TurnInput, StartError> {
    if !task.session.workspace.is_absolute()
        || task.session.user.is_empty()
        || images.iter().any(|path| !path.is_absolute())
    {
        return Err(BackendError::Incompatible.into());
    }
    let model = match &task.model {
        Some(model) if !model.is_empty() => model.clone(),
        Some(_) => return Err(BackendError::Incompatible.into()),
        None => {
            backend
                .models()
                .await?
                .into_iter()
                .find(|m| m.is_default)
                .filter(|m| !m.id.is_empty())
                .ok_or(BackendError::Incompatible)?
                .id
        }
    };
    let previous = store.thread(task.session.clone()).await?;
    let thread = match &previous {
        Some(id) => {
            backend
                .resume_thread(id.clone(), task.session.workspace.clone())
                .await?
        }
        None => backend.start_thread(task.session.workspace.clone()).await?,
    };
    if thread.id.is_empty()
        || previous.as_ref().is_some_and(|id| id != &thread.id)
        || thread
            .directory
            .as_ref()
            .is_some_and(|cwd| cwd != &task.session.workspace)
    {
        return Err(BackendError::Incompatible.into());
    }
    store.bind(task.session.clone(), thread.id.clone()).await?;
    Ok(TurnInput {
        thread_id: thread.id,
        directory: task.session.workspace.clone(),
        prompt: task.prompt.clone(),
        images,
        model,
        mode: task.mode,
        sandbox,
    })
}
