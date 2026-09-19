//! Resource-limit behaviour under load: slow transports and command floods.
//!
//! Deterministic fakes replace the Codex process: a hanging backend keeps one
//! task active forever and a sleeping messenger makes delivery slow, so the
//! serial owner's responsiveness and the busy-refusal bounds are observable.
use bridge_app::{
    diagnostics::Diagnostics,
    events::Incoming,
    files::{Attachment, PreparedFiles, TaskFiles},
    messaging::{DeliveryError, DeliveryFuture, MessageId, Messenger, ResourceKind},
    ports::{
        AgentBackend, BackendError, BackendFuture, Model, Sandbox, ThreadSummary, TurnInput,
        TurnRef,
    },
    runtime::{self, Input},
};
use bridge_core::view::Panel;
use bridge_local::{async_state::AsyncState, state::JsonStore};
use std::{
    collections::BTreeSet, error::Error, fs::File, path::PathBuf, sync::Arc, time::Duration,
};
use tokio::{sync::mpsc, time::timeout};
use tokio_util::sync::CancellationToken;

type TestResult<T = ()> = std::result::Result<T, Box<dyn Error>>;

/// A backend whose `start_turn` never returns: the admitted task stays active.
struct HangingBackend;

impl AgentBackend for HangingBackend {
    fn models(&self) -> BackendFuture<'_, Vec<Model>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn threads(&self, _: PathBuf, _: bool) -> BackendFuture<'_, Vec<ThreadSummary>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn read_thread(&self, _: String) -> BackendFuture<'_, ThreadSummary> {
        Box::pin(async {
            Ok(ThreadSummary {
                id: "thread".into(),
                title: String::new(),
                directory: None,
                active: true,
            })
        })
    }
    fn start_thread(&self, _: PathBuf) -> BackendFuture<'_, ThreadSummary> {
        self.read_thread(String::new())
    }
    fn resume_thread(&self, _: String, _: PathBuf) -> BackendFuture<'_, ThreadSummary> {
        self.read_thread(String::new())
    }
    fn archive_thread(&self, _: String, _: bool) -> BackendFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn compact(&self, _: String) -> BackendFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn start_turn(&self, _: TurnInput) -> BackendFuture<'_, TurnRef> {
        Box::pin(async {
            std::future::pending::<TurnRef>().await;
            unreachable!()
        })
    }
    fn interrupt(&self, _: TurnRef) -> BackendFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// A messenger whose text and panel delivery each wait one slow tick before
/// recording into the assertion channel.
struct SlowMessenger {
    delay: Duration,
    texts: mpsc::Sender<String>,
}

impl SlowMessenger {
    fn recorded(delay: Duration) -> (Arc<Self>, mpsc::Receiver<String>) {
        let (texts, receive) = mpsc::channel(4096);
        (Arc::new(Self { delay, texts }), receive)
    }
}

impl Messenger for SlowMessenger {
    fn send_text(&self, _: String, text: String) -> DeliveryFuture<'_, ()> {
        let delay = self.delay;
        let texts = self.texts.clone();
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            texts.send(text).await.map_err(|_| DeliveryError::Transport)
        })
    }
    fn send_panel(&self, _: String, panel: Panel) -> DeliveryFuture<'_, MessageId> {
        let delay = self.delay;
        let texts = self.texts.clone();
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            texts
                .send(format!("panel:{}", panel.title))
                .await
                .map_err(|_| DeliveryError::Transport)?;
            Ok(MessageId("slow".into()))
        })
    }
    fn update_panel(&self, _: MessageId, _: Panel) -> DeliveryFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn upload(&self, _: String, _: String, _: File, _: ResourceKind) -> DeliveryFuture<'_, ()> {
        Box::pin(async { Err(DeliveryError::Transport) })
    }
}

/// The no-op task-file lifecycle shared with the actor harness.
struct IdleFiles;

impl TaskFiles for IdleFiles {
    fn bind_files(&self, _: String, _: String) -> DeliveryFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn prepare_files(
        &self,
        _: String,
        _: PathBuf,
        _: Vec<Attachment>,
    ) -> DeliveryFuture<'_, PreparedFiles> {
        Box::pin(async { Ok(PreparedFiles::default()) })
    }
    fn finish_files(&self, _: String, _: String, _: PathBuf) -> DeliveryFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

fn input(id: &str, text: &str) -> Input {
    Input {
        attachments: vec![],
        card: None,
        id: id.into(),
        user: "allowed".into(),
        chat: "chat".into(),
        text: Some(text.into()),
        accept: Box::new(|_| {}),
    }
}

struct Harness {
    _directory: tempfile::TempDir,
    /// The Codex event sender must stay alive: dropping it would end the run
    /// as a closed protocol connection.
    _events: mpsc::Sender<std::result::Result<Incoming, BackendError>>,
    input: mpsc::Sender<Input>,
    cancel: CancellationToken,
    worker: tokio::task::JoinHandle<std::result::Result<(), runtime::RuntimeError>>,
}

async fn start(messenger: Arc<dyn Messenger>) -> TestResult<Harness> {
    let directory = tempfile::tempdir()?;
    std::fs::create_dir_all(directory.path().join("root"))?;
    let store = Arc::new(AsyncState::new(JsonStore::open(
        &directory.path().join("state"),
    )?));
    let (input, input_rx) = mpsc::channel(64);
    let (events, event_rx) = mpsc::channel::<std::result::Result<Incoming, BackendError>>(64);
    let cancel = CancellationToken::new();
    let worker = tokio::spawn(runtime::run(
        runtime::Settings {
            root: directory.path().join("root"),
            directory: directory.path().join("root"),
            allowed: BTreeSet::from(["allowed".into()]),
            open_access: false,
            sandbox: Sandbox::WorkspaceWrite,
            epoch: 5,
        },
        Diagnostics::noop(),
        Arc::new(HangingBackend),
        store,
        messenger,
        Arc::new(IdleFiles),
        input_rx,
        event_rx,
        cancel.clone(),
    ));
    Ok(Harness {
        _directory: directory,
        _events: events,
        input,
        cancel,
        worker,
    })
}

#[tokio::test]
async fn slow_delivery_keeps_the_serial_owner_responsive() -> TestResult {
    timeout(Duration::from_secs(10), async {
        let (messenger, mut texts) = SlowMessenger::recorded(Duration::from_millis(20));
        let harness = start(messenger).await?;
        // One active task plus a control command: the owner must answer the
        // command even while the presenter is still draining slowly.
        harness.input.send(input("task", "hello")).await?;
        harness.input.send(input("status", "/status")).await?;
        let mut received = 0;
        while received < 2 {
            texts.recv().await.ok_or("delivery channel closed")?;
            received += 1;
        }
        harness.cancel.cancel();
        let result = harness
            .worker
            .await
            .map_err(|e| Box::new(e) as Box<dyn Error>)?;
        if let Err(error) = &result {
            eprintln!("runtime error: {error}");
        }
        result?;
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn slow_transport_flood_fails_bounded_instead_of_growing_memory() -> TestResult {
    timeout(Duration::from_secs(15), async {
        let (messenger, _texts) = SlowMessenger::recorded(Duration::from_millis(50));
        let harness = start(messenger).await?;
        // The delivery queue holds 128 items. Each wave of 64 commands is
        // drained from the input channel faster than the presenter delivers,
        // so successive waves accumulate past the bound and the run must stop
        // with the bounded capacity failure instead of growing memory.
        'waves: for wave in 0..6 {
            for index in 0..64 {
                let id = format!("status-{wave}-{index}");
                if harness.input.try_send(input(&id, "/status")).is_err() {
                    break 'waves;
                }
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        harness.cancel.cancel();
        match harness.worker.await? {
            Ok(()) => Err("expected a capacity stop, got a clean exit".into()),
            Err(runtime::RuntimeError::Capacity(_)) => Ok(()),
            Err(other) => Err(format!("expected a capacity stop, got {other}").into()),
        }
    })
    .await?
}

#[tokio::test]
async fn command_flood_stays_bounded_and_busy_refusals_protect_control() -> TestResult {
    timeout(Duration::from_secs(10), async {
        let (messenger, mut texts) = SlowMessenger::recorded(Duration::from_millis(0));
        let harness = start(messenger).await?;
        // One task occupies the execution slot; the flood fills the scheduler
        // queue and later commands must receive busy refusals.
        harness.input.send(input("task", "hello")).await?;
        for index in 0..200 {
            let _ = harness
                .input
                .send(input(
                    &format!("flood-{index}"),
                    &format!("flood prompt {index}"),
                ))
                .await;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut busy_seen = false;
        let transport_closed = loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .ok_or("waiting for flood outcomes")?;
            let text = timeout(remaining, texts.recv())
                .await
                .map_err(|_| "timed out waiting for flood outcomes")?;
            match text {
                Some(text) => {
                    busy_seen |= text.contains("任务队列繁忙");
                    if busy_seen {
                        break false;
                    }
                }
                None => break true,
            }
        };
        // The control path stays alive after the flood: while the run is
        // still going, /status must produce an answer of its own. A run that
        // ended concurrently is handled by the bounded-outcome match below.
        if !transport_closed {
            let _ = harness
                .input
                .send(input("status-after-flood", "/status"))
                .await;
            let status_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let remaining = status_deadline
                    .checked_duration_since(tokio::time::Instant::now())
                    .ok_or("control command never answered")?;
                match timeout(remaining, texts.recv()).await {
                    Ok(Some(text)) if text.contains("任务队列繁忙") => continue,
                    Ok(Some(_)) => break,
                    Ok(None) => break,
                    Err(_) => return Err("control command never answered".into()),
                }
            }
        }
        harness.cancel.cancel();
        assert!(busy_seen, "no busy refusal observed");
        // The run either survives the flood and exits cleanly on cancel, or
        // stops with the bounded capacity failure. Anything else is unbounded
        // or misclassified behaviour.
        match harness.worker.await? {
            Ok(()) => {}
            Err(runtime::RuntimeError::Capacity(_)) => {}
            Err(other) => {
                return Err(format!("unexpected flood outcome: {other}").into());
            }
        }
        Ok(())
    })
    .await?
}
