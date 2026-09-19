//! Serial runtime: text, help, status and owner-scoped stop.
//!
//! The `run` loop only selects events and delegates to handlers; state lives
//! in `state::Runtime`, input handling in `input`, protocol events in
//! `protocol`, background completion in `jobs` and timers in `timers`.
//! Serial admission, ownership checks and bounded channels are preserved.
//!
//! Task and channel topology of one run (see `docs/architecture.md` for the
//! rendered diagram):
//!
//! ```text
//! bridge-cli bootstrap                 runtime::run
//! ─────────────────────                ─────────────────────────────────────
//! transport ──incoming(128)──▶ gateway ──input(64)──▶ ┌───────────────┐
//! agent ──────event(256)─────▶───────────────────────▶│ select! loop  │
//!                                                     │ (state::      │
//! heartbeat ─▶ health file        ┌─delivery(128)──▶  │ Runtime)      │
//! signal ────▶ cancel             │  sender task      └──────┬────────┘
//!                                 ▼                          │ jobs(128)
//!                            Messenger(REST) ◀── spawns ─────┘
//! ```
mod ack;
mod flow;
mod input;
mod jobs;
pub(crate) mod limits;
mod protocol;
mod state;
mod timers;

pub use ack::Ack;
pub use state::{Settings, Store};

use crate::{
    diagnostics::Diagnostics,
    events::Incoming,
    files::TaskFiles,
    messaging::Messenger,
    ports::{AgentBackend, BackendError},
    sessions::SessionStoreError,
};
use std::sync::Arc;
use thiserror::Error;
use tokio::{sync::mpsc, task::JoinSet, time::timeout};
use tokio_util::sync::CancellationToken;

/// Categorized terminal failure of one serial run. Every message is safe for
/// delivery to users and logs; the variant is the classification operators
/// act on.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// The Feishu transport or an internal sender ended mid-run.
    #[error("{0}")]
    Connection(&'static str),
    /// A bounded capacity was exhausted and a control response could be lost.
    #[error("{0}")]
    Capacity(&'static str),
    /// An interaction invariant was violated; answers are never guessed.
    #[error("{0}")]
    Interaction(&'static str),
    /// Compaction, archival or a task limit requires a controlled stop.
    #[error("{0}")]
    Maintenance(&'static str),
    /// The Codex connection or protocol failed beyond recovery.
    #[error("{0}")]
    Backend(&'static str),
    /// Durable state failed; the cause category stays available on the error.
    #[error("状态保存或读取失败")]
    Storage(#[from] SessionStoreError),
    /// The working directory left the workspace or its settings are unreadable.
    #[error("{0}")]
    Directory(&'static str),
    /// An internal invariant was violated; the run cannot continue safely.
    #[error("{0}")]
    Internal(&'static str),
}

pub struct Input {
    pub attachments: Vec<crate::files::Attachment>,
    pub card: Option<crate::cards::Click>,
    pub id: String,
    pub user: String,
    pub chat: String,
    /// None denotes media or a separately validated card; never an ordinary task.
    pub text: Option<String>,
    /// Settled exactly once per input; dropping rejects. See [`Ack`].
    pub ack: Ack,
}

/// All network and persistence work is spawned; the owner remains responsive
/// to stop/status. Fatal backend failures terminate this run, never replay work.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    settings: Settings,
    diagnostics: Diagnostics,
    backend: Arc<dyn AgentBackend>,
    store: Arc<dyn Store>,
    messenger: Arc<dyn Messenger>,
    task_files: Arc<dyn TaskFiles>,
    mut inputs: mpsc::Receiver<Input>,
    mut events: mpsc::Receiver<Result<Incoming, BackendError>>,
    cancel: CancellationToken,
) -> Result<(), RuntimeError> {
    store
        .validate_directory(settings.root.clone(), settings.directory.clone())
        .await
        .map_err(|_| RuntimeError::Directory("初始目录无效或越出工作区"))?;
    let directories = store.directory_preferences().await?;
    let (delivery, mut deliveries) =
        mpsc::channel::<crate::presentation::Request>(limits::DELIVERY_QUEUE);
    let text_messenger = messenger.clone();
    let sender_diagnostics = diagnostics.clone();
    let progress_busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let busy = progress_busy.clone();
    let mut sender = tokio::spawn(async move {
        let diagnostics = &sender_diagnostics;
        let mut presentation = crate::presentation::Presentation::default();
        while let Some(request) = deliveries.recv().await {
            let result = match request {
                crate::presentation::Request::Text(chat, text) => {
                    timeout(
                        limits::MESSAGE_TIMEOUT,
                        text_messenger.send_text(chat, text),
                    )
                    .await
                }
                crate::presentation::Request::Answer { task, chat, text } => {
                    let id = task.clone();
                    let result = if text_messenger.rich_output() {
                        timeout(
                            limits::ANSWER_DELIVERY_TIMEOUT,
                            presentation.answer(text_messenger.as_ref(), task, chat, text),
                        )
                        .await
                    } else {
                        timeout(
                            limits::MESSAGE_TIMEOUT,
                            text_messenger.send_text(chat, text),
                        )
                        .await
                    };
                    diagnostics.emit(
                        crate::diagnostics::Event::AnswerDelivered,
                        if matches!(&result, Ok(Ok(()))) {
                            crate::diagnostics::Status::Ok
                        } else {
                            crate::diagnostics::Status::Failed
                        },
                        Some(&id),
                        0,
                    );
                    result
                }
                crate::presentation::Request::Progress { task, chat, text } => {
                    if text_messenger.rich_output() {
                        presentation
                            .progress(diagnostics, text_messenger.as_ref(), task, chat, text)
                            .await;
                    }
                    busy.store(false, std::sync::atomic::Ordering::Release);
                    continue;
                }
            };
            if !matches!(result, Ok(Ok(()))) {
                diagnostics.emit(
                    crate::diagnostics::Event::DeliveryFailed,
                    crate::diagnostics::Status::Failed,
                    None,
                    0,
                );
            }
        }
    });
    let shutdown_backend = backend.clone();
    let mut state = state::Runtime::new(
        settings,
        diagnostics.clone(),
        backend,
        store,
        messenger,
        task_files,
        delivery,
        directories,
        progress_busy,
    );
    let mut jobs = JoinSet::new();
    let mut tick = tokio::time::interval(limits::TICK);
    let result = async {
        loop {
            state.maintain(&mut jobs).await?;
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break Ok(()),
                _ = &mut sender => break Err(RuntimeError::Connection("发送器意外退出")),
                input = inputs.recv() => {
                    let Some(input) = input else { break Err(RuntimeError::Connection("飞书连接已关闭")); };
                    state.handle_input(input, &mut jobs).await?;
                }
                done = jobs.join_next(), if !jobs.is_empty() => {
                    let done = done
                        .ok_or(RuntimeError::Internal("后台任务集合异常"))?
                        .map_err(|_| RuntimeError::Internal("后台任务异常退出"))?;
                    state.handle_done(done, &mut jobs).await?;
                }
                incoming = events.recv() => {
                    let incoming = incoming
                        .ok_or(RuntimeError::Connection("Codex 事件连接已关闭"))?
                        .map_err(|_| RuntimeError::Backend("Codex 协议或连接异常"))?;
                    state.handle_protocol(incoming, &mut jobs).await?;
                }
                _ = tick.tick() => state.handle_tick(&mut jobs).await?,
            }
        }
    }
    .await;
    if let Some(active) = state.tasks.active.take() {
        if let Some(turn) = active.turn {
            let _ = timeout(
                limits::SHUTDOWN_INTERRUPT_TIMEOUT,
                shutdown_backend.interrupt(turn),
            )
            .await;
        }
        let _ = state
            .delivery
            .try_send(crate::presentation::Request::Answer {
                task: active.spec.id.as_str().to_owned(),
                chat: active.spec.chat,
                text: "桥接已停止；未完成任务不会自动重跑。".into(),
            });
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    for pending in state.approvals.interactions.drain() {
        // Only a control-capacity failure can abort a shutdown denial; the
        // timeout bounds the whole drain either way.
        if flow::spawn_reply(&diagnostics, &mut jobs, pending, false).is_err() {
            break;
        }
    }
    let _ = timeout(limits::SHUTDOWN_REPLY_TIMEOUT, async {
        while jobs.join_next().await.is_some() {}
    })
    .await;
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    drop(state.delivery);
    if !sender.is_finished()
        && timeout(limits::SHUTDOWN_SENDER_TIMEOUT, &mut sender)
            .await
            .is_err()
    {
        sender.abort();
        let _ = sender.await;
    }
    result
}
