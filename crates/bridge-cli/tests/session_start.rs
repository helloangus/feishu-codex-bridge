//! Cross-crate use case checks with real JSON persistence and in-memory RPC.
use bridge_app::{
    ports::Sandbox,
    sessions::{self, StartError},
};
use bridge_codex::{backend::CodexBackend, transport::Connection};
use bridge_core::{ExecutionMode, SessionKey, task::TaskSpec};
use bridge_local::{async_state::AsyncState, state::JsonStore};
use serde_json::{Value, json};
use std::{error::Error, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn scenario(
    fail_save: bool,
    fail_turn: bool,
    existing: Option<&'static str>,
) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let temp = tempfile::tempdir()?;
        let directory = temp.path().join("state");
        let mut json_store = JsonStore::open(&directory)?;
        if let Some(thread) = existing {
            let mut state = json_store.state().clone();
            state
                .sessions
                .insert("user:/tmp/project".into(), thread.into());
            json_store.replace(state)?;
        }
        let store = AsyncState::new(json_store);
        if fail_save {
            std::fs::create_dir(directory.join("state.json"))?;
        }
        let (client, remote) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(client);
        let mut connection = Connection::new(read, write, 3, 8192);
        let backend = CodexBackend::new(connection.client.clone());
        let state_path = directory.join("state.json");
        let remote_task = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(remote);
            let mut lines = BufReader::new(read).lines();
            let mut methods = Vec::new();
            while let Some(line) = lines.next_line().await? {
                let value: Value = serde_json::from_str(&line)?;
                let method = value["method"].as_str().ok_or("missing method")?;
                methods.push(method.to_owned());
                let result = match method {
                    "model/list" => json!({"data":[{"id":"model","isDefault":true}]}),
                    "thread/start" => json!({"thread":{"id":"thread","cwd":"/tmp/project"}}),
                    "thread/resume" => {
                        assert_eq!(
                            value["params"]["threadId"],
                            existing.ok_or("unexpected resume")?
                        );
                        json!({"thread":{"id":"thread","cwd":"/tmp/project"}})
                    }
                    "turn/start" => {
                        // This read occurs before acknowledging turn/start.
                        let saved: Value = serde_json::from_slice(&std::fs::read(&state_path)?)?;
                        assert_eq!(saved["sessions"]["user:/tmp/project"], "thread");
                        assert_eq!(value["params"]["collaborationMode"]["mode"], "default");
                        if fail_turn {
                            let mut reply =
                                json!({"id":value["id"],"error":{"code":-1,"message":"failed"}})
                                    .to_string();
                            reply.push('\n');
                            write.write_all(reply.as_bytes()).await?;
                            continue;
                        }
                        json!({"turn":{"id":"turn"}})
                    }
                    _ => return Err("unexpected RPC".into()),
                };
                let mut reply = json!({"id":value["id"],"result":result}).to_string();
                reply.push('\n');
                write.write_all(reply.as_bytes()).await?;
            }
            Ok::<_, Box<dyn Error + Send + Sync>>(methods)
        });
        let task = TaskSpec {
            id: "task".into(),
            session: SessionKey::new("user", "/tmp/project"),
            chat: "chat".into(),
            prompt: "test".into(),
            model: None,
            mode: ExecutionMode::Execute,
        };
        let result =
            sessions::start(&backend, &store, &task, vec![], Sandbox::WorkspaceWrite).await;
        if fail_save {
            assert!(matches!(result, Err(StartError::Storage(_))));
        } else if fail_turn || existing == Some("other") {
            assert!(matches!(result, Err(StartError::Backend(_))));
        } else {
            assert_eq!(result?.turn_id, "turn");
        }
        connection.shutdown().await?;
        let methods = remote_task.await?.map_err(|e| -> Box<dyn Error> { e })?;
        let prepare_method = if existing.is_some() {
            "thread/resume"
        } else {
            "thread/start"
        };
        assert_eq!(
            methods,
            if fail_save || existing == Some("other") {
                vec!["model/list", prepare_method]
            } else {
                vec!["model/list", prepare_method, "turn/start"]
            }
        );
        if existing == Some("other") {
            let saved: Value =
                serde_json::from_slice(&std::fs::read(directory.join("state.json"))?)?;
            assert_eq!(saved["sessions"]["user:/tmp/project"], "other");
        }
        Ok::<_, Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn binding_is_durable_before_turn_starts() -> Result<(), Box<dyn Error>> {
    scenario(false, false, None).await
}

#[tokio::test]
async fn failed_binding_save_prevents_turn_start() -> Result<(), Box<dyn Error>> {
    scenario(true, false, None).await
}

#[tokio::test]
async fn turn_failure_keeps_binding_and_is_not_retried() -> Result<(), Box<dyn Error>> {
    scenario(false, true, None).await
}

#[tokio::test]
async fn stored_thread_is_resumed_instead_of_creating_another() -> Result<(), Box<dyn Error>> {
    scenario(false, false, Some("thread")).await
}

#[tokio::test]
async fn mismatched_resume_preserves_binding_and_prevents_execution() -> Result<(), Box<dyn Error>>
{
    scenario(false, false, Some("other")).await
}
