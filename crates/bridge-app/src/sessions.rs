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
    /// Persist authorization before exposing it to the runtime. Never store code.
    fn pair(&self, _user: String, _code: String) -> StoreFuture<'_, bool> {
        Box::pin(async { Ok(false) })
    }
    /// Archive invalidates every local reference to this backend thread.
    fn clear_thread(&self, thread: String) -> StoreFuture<'_, ()>;
    fn preferences(&self, session: SessionKey) -> StoreFuture<'_, Preferences>;
    fn set_preference(&self, session: SessionKey, change: PreferenceChange) -> StoreFuture<'_, ()>;
    fn thread(&self, session: SessionKey) -> StoreFuture<'_, Option<String>>;
    fn bind(&self, session: SessionKey, thread: String) -> StoreFuture<'_, ()>;
    /// Remove only this user's directory binding; never archive the backend thread.
    fn clear(&self, session: SessionKey) -> StoreFuture<'_, ()>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preferences {
    pub model: Option<String>,
    pub plan: bool,
}

#[derive(Debug, Clone)]
pub enum PreferenceChange {
    Model(Option<String>),
    Plan(bool),
}

pub async fn change_preference<S: SessionStore + ?Sized>(
    backend: &dyn AgentBackend,
    store: &S,
    session: SessionKey,
    change: PreferenceChange,
) -> Result<(), StartError> {
    if let PreferenceChange::Model(Some(id)) = &change {
        if id.is_empty() || id.len() > 256 || !backend.models().await?.iter().any(|m| &m.id == id) {
            return Err(BackendError::Incompatible.into());
        }
    }
    store.set_preference(session, change).await?;
    Ok(())
}

/// Settings changes hold the global idle gate, so queued tasks cannot change mode.
pub async fn prepare_configured<S: SessionStore + ?Sized>(
    backend: &dyn AgentBackend,
    store: &S,
    task: &TaskSpec,
    sandbox: Sandbox,
) -> Result<TurnInput, StartError> {
    let preferences = store.preferences(task.session.clone()).await?;
    let mut configured = task.clone();
    configured.model = preferences.model;
    configured.mode = if preferences.plan {
        bridge_core::ExecutionMode::Plan
    } else {
        bridge_core::ExecutionMode::Execute
    };
    prepare(backend, store, &configured, vec![], sandbox).await
}

/// Async counterpart of the offline MessageJournal. True means durable claim;
/// false means an already claimed message, not permission to execute again.
pub trait DurableJournal: Send + Sync {
    fn claim(&self, message: String) -> StoreFuture<'_, bool>;
}

#[derive(Debug, Error)]
pub enum StartError {
    #[error("当前目录没有已绑定会话，请先提问或使用 /resume 恢复会话")]
    NoSession,
    #[error("归档状态结果不确定；请核对并修复本地会话绑定后再重启，不能直接重发任务")]
    Reconcile,
    #[error(transparent)]
    Storage(#[from] SessionStoreError),
    #[error(transparent)]
    Backend(#[from] BackendError),
}

/// Load an existing idle thread after restart without creating or rebinding one.
/// The caller holds the global mutation slot until compaction's terminal event.
pub async fn prepare_compaction<S: SessionStore + ?Sized>(
    backend: &dyn AgentBackend,
    store: &S,
    session: SessionKey,
) -> Result<String, StartError> {
    let id = store
        .thread(session.clone())
        .await?
        .ok_or(StartError::NoSession)?;
    if !valid_thread_id(&id) || !session.workspace.is_absolute() || session.user.is_empty() {
        return Err(BackendError::Incompatible.into());
    }
    let before = backend.read_thread(id.clone()).await?;
    if before.id != id || before.active || before.directory.as_ref() != Some(&session.workspace) {
        return Err(BackendError::Incompatible.into());
    }
    let loaded = backend
        .resume_thread(id.clone(), session.workspace.clone())
        .await?;
    if loaded.id != id || loaded.active || loaded.directory.as_ref() != Some(&session.workspace) {
        return Err(BackendError::Incompatible.into());
    }
    Ok(id)
}

/// A directory is the legacy visibility boundary, not per-user thread ownership.
pub async fn list(
    backend: &dyn AgentBackend,
    session: &SessionKey,
    archived: bool,
) -> Result<String, StartError> {
    let entries = list_entries(backend, session, archived).await?;
    let mut lines = Vec::new();
    for thread in &entries {
        let title = &thread.title;
        lines.push(format!(
            "{}{}\n{} {}{}",
            title,
            if thread.active { "（执行中）" } else { "" },
            if archived { "/unarchive" } else { "/resume" },
            thread.id,
            if archived {
                String::new()
            } else {
                format!("\n/archive {}", thread.id)
            }
        ));
    }
    Ok(if lines.is_empty() {
        if archived {
            "当前目录没有已归档会话。".into()
        } else {
            "当前目录没有可恢复的会话。".into()
        }
    } else {
        format!(
            "当前目录{}会话（最多 8 项），复制对应命令操作：\n\n{}",
            if archived { "已归档" } else { "最近" },
            lines.join("\n\n")
        )
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedThread {
    pub id: String,
    pub title: String,
    pub active: bool,
}

pub async fn list_entries(
    backend: &dyn AgentBackend,
    session: &SessionKey,
    archived: bool,
) -> Result<Vec<ListedThread>, StartError> {
    Ok(backend
        .threads(session.workspace.clone(), archived)
        .await?
        .into_iter()
        .filter(|thread| thread.directory.as_ref() == Some(&session.workspace))
        .filter(|thread| valid_thread_id(&thread.id))
        .take(8)
        .map(|thread| ListedThread {
            id: thread.id,
            title: thread
                .title
                .chars()
                .filter(|c| !c.is_control())
                .take(160)
                .collect(),
            active: thread.active,
        })
        .collect())
}

#[derive(Clone, Copy)]
pub enum ThreadAction {
    Resume,
    Archive,
    Unarchive,
}

impl ThreadAction {
    pub fn success(self) -> &'static str {
        match self {
            Self::Resume => "会话已恢复，下次提问将继续该会话。",
            Self::Archive => "会话已归档，相关本地绑定已清除；历史记录保留，可用 /archived 查看。",
            Self::Unarchive => "已取消归档；当前会话未切换，可用 /resume 查看并恢复。",
        }
    }
}

pub async fn change_thread<S: SessionStore + ?Sized>(
    backend: &dyn AgentBackend,
    store: &S,
    session: SessionKey,
    id: String,
    action: ThreadAction,
) -> Result<(), StartError> {
    if matches!(action, ThreadAction::Resume) {
        return resume(backend, store, session, id).await;
    }
    if !valid_thread_id(&id) || !session.workspace.is_absolute() || session.user.is_empty() {
        return Err(BackendError::Incompatible.into());
    }
    let before = backend.read_thread(id.clone()).await?;
    if before.id != id || before.active || before.directory.as_ref() != Some(&session.workspace) {
        return Err(BackendError::Incompatible.into());
    }
    let archived = matches!(action, ThreadAction::Archive);
    match backend.archive_thread(id.clone(), archived).await {
        Ok(()) => {}
        Err(error @ BackendError::Rejected(_)) => return Err(error.into()),
        Err(_) => return Err(StartError::Reconcile),
    }
    if archived {
        store
            .clear_thread(id)
            .await
            .map_err(|_| StartError::Reconcile)?;
    }
    Ok(())
}

pub fn valid_thread_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Caller holds the scheduler mutation slot throughout validation and commit.
pub async fn resume<S: SessionStore + ?Sized>(
    backend: &dyn AgentBackend,
    store: &S,
    session: SessionKey,
    id: String,
) -> Result<(), StartError> {
    if !valid_thread_id(&id) || !session.workspace.is_absolute() || session.user.is_empty() {
        return Err(BackendError::Incompatible.into());
    }
    let before = backend.read_thread(id.clone()).await?;
    if before.id != id || before.active || before.directory.as_ref() != Some(&session.workspace) {
        return Err(BackendError::Incompatible.into());
    }
    let resumed = backend
        .resume_thread(id.clone(), session.workspace.clone())
        .await?;
    if resumed.id != id || resumed.active || resumed.directory.as_ref() != Some(&session.workspace)
    {
        return Err(BackendError::Incompatible.into());
    }
    store.bind(session, id).await?;
    Ok(())
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
