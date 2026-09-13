//! Real staging, snapshot delivery and RPC adapter, with in-memory transports.
use bridge_app::{
    events::{AgentEvent, Incoming, TurnOutcome},
    messaging::*,
    ports::{BackendError, TurnRef},
    runtime::{self, Input},
};
use bridge_core::view::Panel;
use bridge_local::{async_state::AsyncState, delivery::Delivery, state::JsonStore};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::File,
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Semaphore, mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;
type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
struct Messages {
    output: mpsc::Sender<String>,
    downloads: AtomicUsize,
    release: Semaphore,
}
impl Messenger for Messages {
    fn send_text(&self, _: String, text: String) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            self.output
                .send(text)
                .await
                .map_err(|_| DeliveryError::Transport)
        })
    }
    fn send_panel(&self, chat: String, panel: Panel) -> DeliveryFuture<'_, MessageId> {
        Box::pin(async move {
            self.send_text(chat, format!("panel:{}:{}", panel.title, panel.body))
                .await?;
            Ok(MessageId("card".into()))
        })
    }
    fn update_panel(&self, _: MessageId, panel: Panel) -> DeliveryFuture<'_, ()> {
        self.send_text(
            "chat".into(),
            format!("update:{}:{}", panel.title, panel.body),
        )
    }
    fn upload(&self, chat: String, _: String, _: File, _: ResourceKind) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            self.send_text(chat, "upload waiting".into()).await?;
            self.release
                .acquire()
                .await
                .map_err(|_| DeliveryError::Transport)?
                .forget();
            Ok(())
        })
    }
}
impl ResourceFetcher for Messages {
    fn download(&self, _: ResourceRef, mut file: File) -> DeliveryFuture<'_, u64> {
        Box::pin(async move {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            let bytes = b"\x89PNG\r\n\x1a\nimage";
            file.write_all(bytes).map_err(|_| DeliveryError::LocalIo)?;
            Ok(bytes.len() as u64)
        })
    }
}
async fn send(tx: &mpsc::Sender<Input>, id: &str, user: &str, text: &str, files: bool) -> Result {
    let (ack, wait) = oneshot::channel();
    tx.send(Input {
        id: id.into(),
        user: user.into(),
        chat: "chat".into(),
        text: Some(text.into()),
        card: None,
        attachments: if files {
            vec![Attachment {
                name: "image.png".into(),
                resource: ResourceRef {
                    message_id: id.into(),
                    key: "key".into(),
                    kind: ResourceKind::Image,
                },
            }]
        } else {
            vec![]
        },
        accept: Box::new(move |value| {
            let _ = ack.send(value);
        }),
    })
    .await
    .map_err(|_| "input closed")?;
    assert!(wait.await?);
    Ok(())
}
async fn until(rx: &mut mpsc::Receiver<String>, pattern: &str) -> Result {
    loop {
        if rx.recv().await.ok_or("messages closed")?.contains(pattern) {
            return Ok(());
        }
    }
}
#[tokio::test]
async fn authorized_attachment_reaches_turn_once_and_delivery_blocks_next_task() -> Result {
    tokio::time::timeout(Duration::from_secs(20), async {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_path_buf();
        let store = Arc::new(AsyncState::new(JsonStore::open(&root.join("state"))?));
        let (client, remote) = tokio::io::duplex(16384);
        let (read, write) = tokio::io::split(client);
        let mut connection = bridge_codex::transport::Connection::new(read, write, 81, 16384);
        let (turn_tx, mut turns) = mpsc::channel(8);
        let cwd = root.clone();
        let remote = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(remote);
            let mut lines = BufReader::new(read).lines();
            let mut count = 0;
            while let Some(line) = lines.next_line().await? {
                let request: Value = serde_json::from_str(&line)?;
                let result = match request["method"].as_str().ok_or("method")? {
                    "model/list" => json!({"data":[{"id":"model","isDefault":true}]}),
                    "thread/start" | "thread/read" | "thread/resume" => {
                        json!({"thread":{"id":"thread","cwd":cwd,"status":{"type":"idle"}}})
                    }
                    "turn/start" => {
                        count += 1;
                        turn_tx.send(request["params"].clone()).await?;
                        json!({"turn":{"id":format!("turn{count}")}})
                    }
                    "turn/interrupt" => json!({}),
                    method => return Err(format!("unexpected {method}").into()),
                };
                write
                    .write_all(
                        format!("{}\n", json!({"id":request["id"],"result":result})).as_bytes(),
                    )
                    .await?;
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });
        let (tx, rx) = mpsc::channel(16);
        let (events, event_rx) = mpsc::channel::<std::result::Result<Incoming, BackendError>>(16);
        let (output, mut messages) = mpsc::channel(64);
        let messenger = Arc::new(Messages {
            output,
            downloads: AtomicUsize::new(0),
            release: Semaphore::new(0),
        });
        let delivery = Arc::new(
            Delivery::new(messenger.clone(), messenger.clone()).excluding(vec![root.join("state")]),
        );
        let cancel = CancellationToken::new();
        let worker = tokio::spawn(runtime::run(
            runtime::Settings {
                root: root.clone(),
                directory: root.clone(),
                allowed: BTreeSet::from(["owner".into()]),
                open_access: false,
                sandbox: bridge_app::ports::Sandbox::WorkspaceWrite,
                epoch: 81,
            },
            Arc::new(bridge_codex::backend::CodexBackend::new(
                connection.client.clone(),
            )),
            store,
            delivery,
            rx,
            event_rx,
            cancel.clone(),
        ));
        send(&tx, "foreign", "stranger", "attachment", true).await?;
        assert_eq!(messenger.downloads.load(Ordering::SeqCst), 0);
        send(&tx, "one", "owner", "/stop", true).await?;
        let turn = turns.recv().await.ok_or("missing first turn")?;
        assert!(
            turn["input"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["type"] == "localImage"))
        );
        assert!(turn.to_string().contains("附件说明：/stop"));
        assert_eq!(messenger.downloads.load(Ordering::SeqCst), 1);
        events
            .send(Ok(Incoming::Notification(AgentEvent::Output {
                turn: TurnRef {
                    epoch: 81,
                    thread_id: "thread".into(),
                    turn_id: "turn1".into(),
                },
                item: "answer".into(),
                delta: "**完整流式内容**".into(),
            })))
            .await
            .map_err(|_| "event closed")?;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(4)).await;
        tokio::time::resume();
        until(&mut messages, "panel:Codex 执行进度:").await?;
        send(&tx, "one", "owner", "/stop", true).await?;
        send(&tx, "two", "owner", "next task", false).await?;
        std::fs::write(root.join("report.pdf"), b"report")?;
        events
            .send(Ok(Incoming::Notification(AgentEvent::Finished {
                turn: TurnRef {
                    epoch: 81,
                    thread_id: "thread".into(),
                    turn_id: "turn1".into(),
                },
                outcome: TurnOutcome::Completed,
            })))
            .await
            .map_err(|_| "event closed")?;
        let (mut uploaded, mut ended, mut answered) = (false, false, false);
        while !(uploaded && ended && answered) {
            let message = messages.recv().await.ok_or("delivery closed")?;
            uploaded |= message.contains("upload waiting");
            ended |= message.contains("update:执行完成:本轮已结束");
            answered |=
                message.contains("panel:Codex 回复:") && message.contains("**完整流式内容**");
        }
        send(&tx, "busy", "owner", "/new", false).await?;
        until(&mut messages, "正在整理本轮成果").await?;
        assert!(turns.try_recv().is_err());
        assert_eq!(messenger.downloads.load(Ordering::SeqCst), 1);
        messenger.release.add_permits(1);
        let _ = turns.recv().await.ok_or("missing second turn")?;
        cancel.cancel();
        worker.await??;
        connection.shutdown().await?;
        remote.await?.map_err(|e| e.to_string())?;
        Ok(())
    })
    .await?
}
