//! Rendered card actions cross the gateway and real runtime without network.
use bridge_app::{
    messaging::{DeliveryFuture, MessageId, Messenger, ResourceKind},
    runtime::{self, Input},
    sessions::SessionStore,
};
use bridge_core::{
    SessionKey,
    view::{Button, Panel},
};
use bridge_local::{async_state::AsyncState, state::JsonStore};
use std::{
    collections::BTreeSet,
    error::Error,
    fs::File,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

struct Messages {
    text: mpsc::Sender<String>,
    panels: mpsc::Sender<(String, Panel)>,
    updates: mpsc::Sender<(String, Panel)>,
    sequence: AtomicU64,
    fail_updates: Arc<AtomicBool>,
}
impl Messenger for Messages {
    fn send_text(&self, _: String, text: String) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            self.text
                .send(text)
                .await
                .map_err(|_| bridge_app::messaging::DeliveryError::Transport)
        })
    }
    fn send_panel(&self, _: String, panel: Panel) -> DeliveryFuture<'_, MessageId> {
        Box::pin(async move {
            let id = format!("message-{}", self.sequence.fetch_add(1, Ordering::Relaxed));
            self.panels
                .send((id.clone(), panel))
                .await
                .map_err(|_| bridge_app::messaging::DeliveryError::Transport)?;
            Ok(MessageId(id))
        })
    }
    fn update_panel(&self, id: MessageId, panel: Panel) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            self.updates
                .send((id.0, panel))
                .await
                .map_err(|_| bridge_app::messaging::DeliveryError::Transport)?;
            // A failed visual update must not replay or undo the command.
            if self.fail_updates.load(Ordering::Relaxed) {
                Err(bridge_app::messaging::DeliveryError::Transport)
            } else {
                Ok(())
            }
        })
    }
    fn upload(&self, _: String, _: String, _: File, _: ResourceKind) -> DeliveryFuture<'_, ()> {
        Box::pin(async { panic!("unexpected upload") })
    }
}
async fn submit(
    tx: &mpsc::Sender<Input>,
    id: &str,
    user: &str,
    text: Option<String>,
    card: Option<bridge_app::cards::Click>,
) -> Result<(), Box<dyn Error>> {
    let (ack, wait) = oneshot::channel();
    tx.send(Input {
        attachments: vec![],
        card,
        id: id.into(),
        user: user.into(),
        chat: "chat".into(),
        text,
        accept: Box::new(move |accepted| {
            let _ = ack.send(accepted);
        }),
    })
    .await?;
    assert!(wait.await?);
    Ok(())
}
async fn send(tx: &mpsc::Sender<Input>, id: &str, text: &str) -> Result<(), Box<dyn Error>> {
    submit(tx, id, "owner", Some(text.into()), None).await
}
async fn click(
    tx: &mpsc::Sender<Input>,
    user: &str,
    source: &str,
    button: &Button,
) -> Result<(), Box<dyn Error>> {
    let card = bridge_cli::bootstrap::decode_card_click(
        source.into(),
        &bridge_feishu::cards::action_value(&button.action),
    )
    .ok_or("invalid callback")?;
    submit(tx, "same-event", user, None, Some(card)).await
}
async fn until(rx: &mut mpsc::Receiver<String>, pattern: &str) -> Result<(), Box<dyn Error>> {
    loop {
        if rx.recv().await.ok_or("delivery closed")?.contains(pattern) {
            return Ok(());
        }
    }
}
#[tokio::test]
async fn help_and_creation_cards_round_trip_with_message_and_owner_checks()
-> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_path_buf();
        let store = Arc::new(AsyncState::new(JsonStore::open(&root.join("state"))?));
        let key = SessionKey::new("owner", &root);
        store.bind(key.clone(), "thread".into()).await?;
        let (client, remote) = tokio::io::duplex(4096);
        let remote_worker = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            let (read, mut write) = tokio::io::split(remote);
            let mut lines = BufReader::new(read).lines();
            let mut count = 0;
            while let Some(line) = lines.next_line().await? {
                let request: serde_json::Value = serde_json::from_str(&line)?;
                count += 1;
                let result = match request["method"].as_str() {
                    Some("model/list") => serde_json::json!({"data":[{"id":format!("model-{count}"),"isDefault":true}]}),
                    Some("thread/list") => serde_json::json!({"data":[]}),
                    _ => return Err("unexpected RPC".into()),
                };
                let reply = serde_json::json!({"id":request["id"],"result":result});
                write.write_all(format!("{reply}\n").as_bytes()).await?;
            }
            Ok::<_, Box<dyn Error + Send + Sync>>(())
        });
        let (read, write) = tokio::io::split(client);
        let mut connection = bridge_codex::transport::Connection::new(read, write, 71, 4096);
        let backend = Arc::new(bridge_codex::backend::CodexBackend::new(
            connection.client.clone(),
        ));
        let (tx, inputs) = mpsc::channel(16);
        let (_events, events) = mpsc::channel(8);
        let (messages, mut output) = mpsc::channel(32);
        let (panels, mut cards) = mpsc::channel(8);
        let (updates, mut updated_cards) = mpsc::channel(16);
        let fail_updates = Arc::new(AtomicBool::new(true));
        let cancel = CancellationToken::new();
        let worker = tokio::spawn(runtime::run(
            runtime::Settings {
                root: root.clone(),
                directory: root.clone(),
                allowed: BTreeSet::from(["owner".into(), "other".into()]),
                open_access: false,
                sandbox: bridge_app::ports::Sandbox::WorkspaceWrite,
                epoch: 71,
            },
            backend,
            store.clone(),
            Arc::new(Messages {
                text: messages,
                panels,
                updates,
                sequence: AtomicU64::new(1),
                fail_updates: fail_updates.clone(),
            }),
            inputs,
            events,
            cancel.clone(),
        ));
        send(&tx, "help", "/help").await?;
        let (source, panel) = cards.recv().await.ok_or("missing help")?;
        assert_eq!(panel.title, "Codex 控制面板");
        assert_eq!(panel.buttons.len(), 9);
        // A local asynchronous query provides a barrier after send_panel returns.
        send(&tx, "barrier", "/model").await?;
        until(&mut output, "当前模型").await?;
        click(&tx, "other", &source, &panel.buttons[0]).await?;
        until(&mut output, "卡片操作无效").await?;
        click(&tx, "owner", "wrong-source", &panel.buttons[0]).await?;
        until(&mut output, "卡片操作无效").await?;
        click(&tx, "owner", &source, &panel.buttons[0]).await?;
        until(&mut output, "当前目录").await?;
        let (updated_source, updated) = updated_cards.recv().await.ok_or("missing update")?;
        assert_eq!(updated_source, source);
        assert_eq!(updated.buttons.len(), 8);
        assert!(
            !updated
                .buttons
                .iter()
                .any(|button| button.action == panel.buttons[0].action)
        );
        click(&tx, "owner", &source, &panel.buttons[0]).await?;
        until(&mut output, "卡片操作无效").await?;
        click(&tx, "owner", &source, &panel.buttons[3]).await?;
        until(&mut output, "已切换到新会话").await?;
        assert!(store.thread(key).await?.is_none());
        send(&tx, "propose-card", "/cd card-created").await?;
        let (creation_source, creation) = cards.recv().await.ok_or("missing creation card")?;
        assert!(creation.body.contains("/cd-confirm"));
        assert!(!root.join("card-created").exists());
        send(&tx, "barrier2", "/model").await?;
        until(&mut output, "当前模型").await?;
        click(&tx, "owner", &creation_source, &creation.buttons[0]).await?;
        until(&mut output, "目录已创建并切换").await?;
        assert!(root.join("card-created").is_dir());
        loop {
            let (id, updated) = updated_cards.recv().await.ok_or("missing invalidation")?;
            if id == source && updated.buttons.is_empty() {
                assert!(updated.body.contains("均已使用或失效"));
                break;
            }
        }
        click(&tx, "owner", &source, &panel.buttons[1]).await?;
        until(&mut output, "卡片操作无效").await?;
        fail_updates.store(false, Ordering::Relaxed);
        send(&tx, "models", "/models").await?;
        let (model_source, models) = cards.recv().await.ok_or("missing models")?;
        send(&tx, "model-barrier", "/model").await?;
        until(&mut output, "当前模型").await?;
        click(&tx, "owner", &model_source, models.buttons.last().ok_or("refresh button")?).await?;
        let refreshed = loop {
            let (id, panel) = updated_cards.recv().await.ok_or("missing refresh")?;
            if id == model_source && panel.buttons.iter().any(|b| b.label.contains("model-2")) { break panel; }
        };
        send(&tx, "refresh-barrier", "/model").await?;
        until(&mut output, "当前模型").await?;
        assert!(cards.try_recv().is_err(), "refresh must reuse the message");
        click(&tx, "owner", &model_source, &models.buttons[1]).await?;
        until(&mut output, "卡片操作无效").await?;
        click(&tx, "owner", &model_source, &refreshed.buttons[1]).await?;
        until(&mut output, "设置已保存").await?;
        fail_updates.store(true, Ordering::Relaxed);
        click(&tx, "owner", &model_source, refreshed.buttons.last().ok_or("refresh button")?).await?;
        until(&mut output, "/model model-3").await?;
        let failed = loop {
            let (id, panel) = updated_cards.recv().await.ok_or("missing failed refresh")?;
            if id == model_source && panel.buttons.iter().any(|b| b.label.contains("model-3")) { break panel; }
        };
        click(&tx, "owner", &model_source, &failed.buttons[0]).await?;
        until(&mut output, "卡片操作无效").await?;
        fail_updates.store(false, Ordering::Relaxed);
        send(&tx, "threads", "/resume").await?;
        let (thread_source, threads) = cards.recv().await.ok_or("missing threads")?;
        send(&tx, "thread-barrier", "/model").await?;
        until(&mut output, "当前模型").await?;
        click(&tx, "owner", &thread_source, threads.buttons.last().ok_or("archive navigation")?).await?;
        loop {
            let (id, panel) = updated_cards.recv().await.ok_or("missing archive navigation update")?;
            if id == thread_source && panel.title == "已归档会话" {
                assert!(panel.body.contains("没有已归档会话"));
                break;
            }
        }
        assert!(cards.try_recv().is_err());
        assert!(
            bridge_cli::bootstrap::decode_card_click(
                source,
                &serde_json::json!({"command":"/new"})
            )
            .is_none()
        );
        cancel.cancel();
        worker.await??;
        connection.shutdown().await?;
        remote_worker.abort();
        Ok::<_, Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}
