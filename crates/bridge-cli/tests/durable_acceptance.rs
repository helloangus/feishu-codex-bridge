//! Connect SDK IPC acceptance to durable admission without a real SDK/network.
use bridge_app::{Scheduler, sessions::DurableJournal};
use bridge_core::{ExecutionMode, SessionKey, task::TaskSpec};
use bridge_feishu::{ingress::Event, sidecar::pump};
use bridge_local::{async_state::AsyncState, state::JsonStore};
use serde_json::{Value, json};
use std::{error::Error, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

async fn scenario(broken: bool) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let temp = tempfile::tempdir()?;
        let store = AsyncState::new(JsonStore::open(temp.path())?);
        if broken { std::fs::create_dir(temp.path().join("seen-messages.json"))?; }
        let (client, remote) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(client);
        let (remote_read, mut remote_write) = tokio::io::split(remote);
        let mut acknowledgements = BufReader::new(remote_read).lines();
        let (tx, mut rx) = mpsc::channel(2);
        let cancel = CancellationToken::new();
        let worker = tokio::spawn(pump(read, write, "g".into(), tx, cancel.clone()));
        let mut scheduler = Scheduler::new(2);
        for sequence in 0..2 {
            let mut frame = json!({"version":1,"epoch":"g","sequence":sequence,"event":{"kind":"message","message_id":"same-id","user_id":"user","chat_id":"chat","chat_type":"p2p","message_type":"text","content":{"text":"hello"}}}).to_string();
            frame.push('\n');
            remote_write.write_all(frame.as_bytes()).await?;
            let received = rx.recv().await.ok_or("missing event")?;
            let Event::Message {message_id, ..} = received.event else {return Err("expected message".into());};
            // Authorization/content validation would precede reservation in the
            // runtime. This fixture represents an explicitly allowed user.
            let task = TaskSpec {id:format!("task-{sequence}"),session:SessionKey::new("user","/tmp/project"),chat:"chat".into(),prompt:"hello".into(),model:None,mode:ExecutionMode::Execute};
            let ticket = scheduler.reserve(message_id.clone(), task).map_err(|_| "admission failed")?;
            let accepted = match store.claim(message_id).await {
                Ok(newly_claimed) => {
                    scheduler.commit_admission(ticket, newly_claimed);
                    true // Duplicates are deliberately acknowledged, never executed.
                }
                Err(_) => {scheduler.abort_admission(ticket); false}
            };
            received.acceptance.ok_or("missing acceptance")?.complete(accepted);
            let ack: Value = serde_json::from_str(&acknowledgements.next_line().await?.ok_or("missing ACK")?)?;
            assert_eq!(ack["accepted"], !broken);
            assert_eq!(scheduler.queued(), if broken {0} else {1});
        }
        cancel.cancel(); worker.await??;
        Ok::<_, Box<dyn Error>>(())
    }).await??;
    Ok(())
}

#[tokio::test]
async fn persisted_message_is_acknowledged_and_duplicate_is_not_enqueued()
-> Result<(), Box<dyn Error>> {
    scenario(false).await
}

#[tokio::test]
async fn failed_journal_write_rejects_sdk_ack_and_execution() -> Result<(), Box<dyn Error>> {
    scenario(true).await
}
