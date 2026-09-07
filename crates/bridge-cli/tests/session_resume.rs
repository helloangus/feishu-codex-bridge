//! Resume validates remote identity before replacing a durable local binding.
use bridge_app::sessions::{self, SessionStore};
use bridge_codex::{backend::CodexBackend, transport::Connection};
use bridge_core::SessionKey;
use bridge_local::{async_state::AsyncState, state::JsonStore};
use serde_json::{Value, json};
use std::{error::Error, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn scenario(case: &str) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let temp = tempfile::tempdir()?;
        let store = AsyncState::new(JsonStore::open(temp.path())?);
        let session = SessionKey::new("owner", "/tmp/project");
        store.bind(session.clone(), "old".into()).await?;
        if case == "storage" {
            std::fs::create_dir(temp.path().join("state.previous.json"))?;
        }
        let (client, remote) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(client);
        let mut connection = Connection::new(read, write, 1, 8192);
        let backend = CodexBackend::new(connection.client.clone());
        let case_owned = case.to_owned();
        let responder = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(remote);
            let mut lines = BufReader::new(read).lines();
            let mut calls = Vec::new();
            while let Some(line) = lines.next_line().await? {
                let request: Value = serde_json::from_str(&line)?;
                let method = request["method"].as_str().ok_or("method absent")?;
                calls.push(method.to_owned());
                assert!(matches!(method, "thread/read" | "thread/resume"));
                assert_eq!(request["params"]["threadId"], "target");
                let mut thread =
                    json!({"id":"target","cwd":"/tmp/project","status":{"type":"idle"}});
                if method == "thread/read" {
                    match case_owned.as_str() {
                        "directory" => thread["cwd"] = json!("/tmp/elsewhere"),
                        "missing_directory" => {
                            thread.as_object_mut().ok_or("object")?.remove("cwd");
                        }
                        "active" => thread["status"]["type"] = json!("active"),
                        "identity" => thread["id"] = json!("wrong"),
                        _ => {}
                    }
                } else {
                    assert_eq!(request["params"]["cwd"], "/tmp/project");
                    if case_owned == "resume_identity" {
                        thread["id"] = json!("wrong");
                    }
                }
                let response = if case_owned == "rejected" && method == "thread/resume" {
                    json!({"id":request["id"],"error":{"code":-32000,"message":"rejected"}})
                } else {
                    json!({"id":request["id"],"result":{"thread":thread}})
                };
                write.write_all(format!("{response}\n").as_bytes()).await?;
            }
            Ok::<_, Box<dyn Error + Send + Sync>>(calls)
        });
        let result = sessions::resume(&backend, &store, session.clone(), "target".into()).await;
        assert_eq!(result.is_ok(), case == "success");
        let expected = if case == "success" { "target" } else { "old" };
        assert_eq!(store.thread(session.clone()).await?, Some(expected.into()));
        drop(store);
        let reopened = AsyncState::new(JsonStore::open(temp.path())?);
        assert_eq!(reopened.thread(session).await?, Some(expected.into()));
        connection.shutdown().await?;
        let calls = responder
            .await?
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let early = matches!(
            case,
            "directory" | "missing_directory" | "active" | "identity"
        );
        assert_eq!(calls.len(), if early { 1 } else { 2 });
        Ok::<_, Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn valid_resume_persists_binding() -> Result<(), Box<dyn Error>> {
    scenario("success").await
}
#[tokio::test]
async fn foreign_or_missing_directory_is_rejected_before_resume() -> Result<(), Box<dyn Error>> {
    scenario("directory").await?;
    scenario("missing_directory").await
}
#[tokio::test]
async fn active_thread_is_not_resumed() -> Result<(), Box<dyn Error>> {
    scenario("active").await
}
#[tokio::test]
async fn wrong_read_or_resume_identity_preserves_old_binding() -> Result<(), Box<dyn Error>> {
    scenario("identity").await?;
    scenario("resume_identity").await
}
#[tokio::test]
async fn rejected_resume_preserves_old_binding() -> Result<(), Box<dyn Error>> {
    scenario("rejected").await
}
#[tokio::test]
async fn failed_commit_preserves_old_binding() -> Result<(), Box<dyn Error>> {
    scenario("storage").await
}
