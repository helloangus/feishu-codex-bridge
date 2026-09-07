//! Reset uses the durable journal and local store; it must not call Codex.
use bridge_app::{
    messaging::{DeliveryError, DeliveryFuture, MessageId, Messenger, ResourceKind},
    ports::Sandbox,
    runtime::{self, Input},
    sessions::SessionStore,
};
use bridge_codex::{backend::CodexBackend, transport::Connection};
use bridge_core::{SessionKey, view::Panel};
use bridge_local::{async_state::AsyncState, state::JsonStore};
use std::{collections::BTreeSet, error::Error, fs::File, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

struct Delivery(mpsc::Sender<String>);
impl Messenger for Delivery {
    fn send_text(&self, _: String, text: String) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            self.0
                .send(text)
                .await
                .map_err(|_| DeliveryError::Transport)
        })
    }
    fn send_panel(&self, _: String, _: Panel) -> DeliveryFuture<'_, MessageId> {
        Box::pin(async { panic!("unexpected card") })
    }
    fn update_panel(&self, _: MessageId, _: Panel) -> DeliveryFuture<'_, ()> {
        Box::pin(async { panic!("unexpected card") })
    }
    fn upload(&self, _: String, _: String, _: File, _: ResourceKind) -> DeliveryFuture<'_, ()> {
        Box::pin(async { panic!("unexpected upload") })
    }
}

async fn send(tx: &mpsc::Sender<Input>, id: &str, text: &str) -> Result<bool, Box<dyn Error>> {
    let (ack, wait) = oneshot::channel();
    tx.send(Input {
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
            let (delivery, mut replies) = mpsc::channel(8);
            let cancel = CancellationToken::new();
            let worker = tokio::spawn(runtime::run(
                runtime::Settings {
                    directory: temp.path().into(),
                    allowed: BTreeSet::from(["owner".into()]),
                    open_access: false,
                    sandbox: Sandbox::WorkspaceWrite,
                    epoch: 1,
                },
                backend,
                store.clone(),
                Arc::new(Delivery(delivery)),
                inputs,
                events,
                cancel.clone(),
            ));
            assert_eq!(send(&tx, "same-reset", "/new").await?, fail_at.is_none());
            if fail_at.is_some() {
                assert!(
                    replies
                        .recv()
                        .await
                        .ok_or("missing failure")?
                        .contains("新建会话失败")
                );
                assert_eq!(store.thread(owner.clone()).await?, Some("old".into()));
            } else if round == 0 {
                assert!(
                    replies
                        .recv()
                        .await
                        .ok_or("missing success")?
                        .contains("下次提问时自动创建")
                );
                assert_eq!(store.thread(owner.clone()).await?, None);
                store.bind(owner.clone(), "newer".into()).await?;
            } else {
                assert_eq!(store.thread(owner.clone()).await?, Some("newer".into()));
                assert!(send(&tx, "status", "/status").await?);
                assert!(
                    replies
                        .recv()
                        .await
                        .ok_or("missing status")?
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
