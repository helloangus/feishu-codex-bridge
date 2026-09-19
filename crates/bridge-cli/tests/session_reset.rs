//! Reset uses the durable journal and local store; it must not call Codex.
use bridge_app::{
    ports::Sandbox,
    runtime::{self, Input},
    sessions::SessionStore,
};
use bridge_codex::{backend::CodexBackend, transport::Connection};
use bridge_core::SessionKey;
use bridge_local::{async_state::AsyncState, state::JsonStore};
use std::{collections::BTreeSet, error::Error, sync::Arc, time::Duration};
use test_support::messenger::{MessengerOptions, RecordingMessenger};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

async fn send(tx: &mpsc::Sender<Input>, id: &str, text: &str) -> Result<bool, Box<dyn Error>> {
    let (ack, wait) = oneshot::channel();
    tx.send(Input {
        attachments: vec![],
        card: None,
        id: id.into(),
        user: "owner".into(),
        chat: "chat".into(),
        text: Some(text.into()),
        accept: Box::new(move |accepted| {
            let _ = ack.send(accepted);
        }),
    })
    .await?;
    Ok(wait.await?)
}

async fn scenario(fail_at: Option<&str>) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let temp = tempfile::tempdir()?;
        let owner = SessionKey::new("owner", temp.path());
        let other = SessionKey::new("other", temp.path());
        let store = AsyncState::new(JsonStore::open(temp.path())?);
        store.bind(owner.clone(), "old".into()).await?;
        store.bind(other.clone(), "preserved".into()).await?;
        drop(store);
        // Two actors and reopened stores exercise replay after a process restart.
        for round in 0..if fail_at.is_none() { 2 } else { 1 } {
            let store = Arc::new(AsyncState::new(JsonStore::open(temp.path())?));
            if let Some(file) = fail_at {
                let path = temp.path().join(file);
                if path.is_file() {
                    std::fs::remove_file(&path)?;
                }
                std::fs::create_dir(path)?;
            }
            let (wire, mut remote) = tokio::io::duplex(4096);
            let (read, write) = tokio::io::split(wire);
            let mut connection = Connection::new(read, write, 1, 4096);
            let backend = Arc::new(CodexBackend::new(connection.client.clone()));
            let (tx, inputs) = mpsc::channel(8);
            let (_events, events) = mpsc::channel(8);
            let (messenger, mut handles) = RecordingMessenger::recorded(
                "",
                MessengerOptions {
                    panels_fail: true,
                    upload_panics: true,
                    ..MessengerOptions::new()
                },
            );
            let cancel = CancellationToken::new();
            let worker = tokio::spawn(runtime::run(
                runtime::Settings {
                    root: temp.path().into(),
                    directory: temp.path().into(),
                    allowed: BTreeSet::from(["owner".into()]),
                    open_access: false,
                    sandbox: Sandbox::WorkspaceWrite,
                    epoch: 1,
                },
                bridge_app::diagnostics::Diagnostics::noop(),
                backend,
                store.clone(),
                messenger.clone(),
                Arc::new(test_support::files::IdleFiles),
                inputs,
                events,
                cancel.clone(),
            ));
            assert_eq!(send(&tx, "same-reset", "/new").await?, fail_at.is_none());
            if fail_at.is_some() {
                assert!(
                    handles
                        .text
                        .recv()
                        .await
                        .ok_or("missing failure")?
                        .1
                        .contains("新建会话失败")
                );
                assert_eq!(store.thread(owner.clone()).await?, Some("old".into()));
            } else if round == 0 {
                assert!(
                    handles
                        .text
                        .recv()
                        .await
                        .ok_or("missing success")?
                        .1
                        .contains("下次提问时自动创建")
                );
                assert_eq!(store.thread(owner.clone()).await?, None);
                store.bind(owner.clone(), "newer".into()).await?;
            } else {
                assert_eq!(store.thread(owner.clone()).await?, Some("newer".into()));
                assert!(send(&tx, "status", "/status").await?);
                assert!(
                    handles
                        .text
                        .recv()
                        .await
                        .ok_or("missing status")?
                        .1
                        .contains("空闲")
                );
            }
            assert_eq!(store.thread(other.clone()).await?, Some("preserved".into()));
            cancel.cancel();
            worker.await?.map_err(std::io::Error::other)?;
            connection.shutdown().await?;
            use tokio::io::AsyncReadExt;
            let mut requests = Vec::new();
            remote.read_to_end(&mut requests).await?;
            assert!(
                requests.is_empty(),
                "reset must not archive or create a Codex thread"
            );
        }
        Ok::<_, Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn reset_is_scoped_and_replay_after_restart_preserves_newer_binding()
-> Result<(), Box<dyn Error>> {
    scenario(None).await
}

#[tokio::test]
async fn reset_clear_failure_is_reported_without_losing_binding() -> Result<(), Box<dyn Error>> {
    scenario(Some("state.previous.json")).await
}

#[tokio::test]
async fn reset_journal_failure_does_not_clear_binding() -> Result<(), Box<dyn Error>> {
    scenario(Some("seen-messages.json")).await
}

async fn archive_notification_scenario(fail: bool) -> Result<(), Box<dyn Error>> {
    use bridge_app::events::{AgentEvent, Incoming};
    tokio::time::timeout(Duration::from_secs(10), async {
        let temp = tempfile::tempdir()?;
        let store = Arc::new(AsyncState::new(JsonStore::open(temp.path())?));
        let owner = SessionKey::new("owner", temp.path());
        let shared = SessionKey::new("shared", temp.path().join("other"));
        let preserved = SessionKey::new("preserved", temp.path());
        store.bind(owner.clone(), "target".into()).await?;
        store.bind(shared.clone(), "target".into()).await?;
        store.bind(preserved.clone(), "keep".into()).await?;
        if fail {
            std::fs::remove_file(temp.path().join("state.previous.json"))?;
            std::fs::create_dir(temp.path().join("state.previous.json"))?;
        }
        let (wire, mut remote) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(wire);
        let mut connection = Connection::new(read, write, 1, 4096);
        let backend = Arc::new(CodexBackend::new(connection.client.clone()));
        let (_tx, inputs) = mpsc::channel(8);
        let (events_tx, events) = mpsc::channel(8);
        let (messenger, _handles) = RecordingMessenger::recorded(
            "",
            MessengerOptions {
                panels_fail: true,
                upload_panics: true,
                ..MessengerOptions::new()
            },
        );
        let cancel = CancellationToken::new();
        let worker = tokio::spawn(runtime::run(
            runtime::Settings {
                root: temp.path().into(),
                directory: temp.path().into(),
                allowed: BTreeSet::from(["owner".into()]),
                open_access: false,
                sandbox: Sandbox::WorkspaceWrite,
                epoch: 1,
            },
            bridge_app::diagnostics::Diagnostics::noop(),
            backend,
            store.clone(),
            messenger.clone(),
            Arc::new(test_support::files::IdleFiles),
            inputs,
            events,
            cancel.clone(),
        ));
        // A stale connection must not invalidate an unrelated live binding.
        for (epoch, thread) in [(0, "keep"), (1, "target"), (1, "target")] {
            events_tx
                .send(Ok(Incoming::Notification(AgentEvent::Archived {
                    epoch,
                    thread: thread.into(),
                })))
                .await?;
        }
        if fail {
            let result = worker.await?;
            match result {
                Err(error) => assert!(matches!(
                    error,
                    bridge_app::runtime::RuntimeError::Storage(_)
                )),
                Ok(()) => panic!("archive notification failure unexpectedly completed"),
            }
            assert_eq!(store.thread(owner.clone()).await?, Some("target".into()));
        } else {
            while store.thread(owner.clone()).await?.is_some() {
                tokio::task::yield_now().await;
            }
            assert_eq!(store.thread(shared.clone()).await?, None);
            cancel.cancel();
            worker.await?.map_err(std::io::Error::other)?;
        }
        assert_eq!(store.thread(preserved.clone()).await?, Some("keep".into()));
        connection.shutdown().await?;
        use tokio::io::AsyncReadExt;
        let mut requests = Vec::new();
        remote.read_to_end(&mut requests).await?;
        assert!(
            requests.is_empty(),
            "archive notifications must not issue backend mutations"
        );
        drop(store);
        let reopened = AsyncState::new(JsonStore::open(temp.path())?);
        assert_eq!(
            reopened.thread(owner).await?,
            if fail { Some("target".into()) } else { None }
        );
        assert_eq!(reopened.thread(preserved).await?, Some("keep".into()));
        Ok::<_, Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn archive_notifications_clear_shared_bindings_and_ignore_stale_epochs()
-> Result<(), Box<dyn Error>> {
    archive_notification_scenario(false).await
}

#[tokio::test]
async fn archive_notification_commit_failure_stops_runtime() -> Result<(), Box<dyn Error>> {
    archive_notification_scenario(true).await
}
