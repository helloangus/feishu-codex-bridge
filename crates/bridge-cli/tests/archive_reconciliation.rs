//! Startup reconciliation reads archive evidence before one atomic local commit.
use bridge_app::sessions::{PreferenceChange, SessionStore};
use bridge_codex::{backend::CodexBackend, transport::Connection};
use bridge_core::SessionKey;
use bridge_local::{async_state::AsyncState, state::JsonStore};
use serde_json::{Value, json};
use std::{collections::BTreeSet, error::Error, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn scenario(mode: &str) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let temp = tempfile::tempdir()?;
        let store = AsyncState::new(JsonStore::open(temp.path())?);
        let owner = SessionKey::new("owner", "/project");
        let shared = SessionKey::new("shared", "/other");
        let unrelated = SessionKey::new("other", "/project");
        store.bind(owner.clone(), "archived".into()).await?;
        store.bind(shared.clone(), "archived".into()).await?;
        store.bind(unrelated.clone(), "keep".into()).await?;
        store
            .set_preference(owner.clone(), PreferenceChange::Plan(true))
            .await?;
        if mode == "storage" {
            std::fs::remove_file(temp.path().join("state.previous.json"))?;
            std::fs::create_dir(temp.path().join("state.previous.json"))?;
        }
        let (client, remote) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(client);
        let mut connection = Connection::new(read, write, 1, 8192);
        let backend = CodexBackend::new(connection.client.clone());
        let case = mode.to_owned();
        let responder = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(remote);
            let mut lines = BufReader::new(read).lines();
            let mut count = 0;
            while let Some(line) = lines.next_line().await? {
                let request: Value = serde_json::from_str(&line)?;
                assert_eq!(request["method"], "thread/list");
                assert_eq!(request["params"]["archived"], true);
                assert_eq!(request["params"]["modelProviders"], json!([]));
                assert!(matches!(
                    request["params"]["sourceKinds"].as_array(),
                    Some(source_kinds) if source_kinds.contains(&json!("appServer"))
                ));
                assert!(request["params"].get("cwd").is_none());
                assert_eq!(
                    request["params"]["cursor"],
                    if count == 0 {
                        Value::Null
                    } else {
                        json!("page-2")
                    }
                );
                count += 1;
                let result = if count == 1 {
                    json!({"data":[{"id":"archived"}],"nextCursor":"page-2"})
                } else {
                    match case.as_str() {
                        "cycle" => json!({"data":[],"nextCursor":"page-2"}),
                        "malformed" => json!({"data":[]}),
                        "identity" => json!({"data":[{"id":"invalid/id"}],"nextCursor":null}),
                        _ => json!({"data":[{"id":"unbound"}],"nextCursor":null}),
                    }
                };
                let response = if count == 2 && case == "remote" {
                    json!({"id":request["id"],"error":{"code":-32000,"message":"failed"}})
                } else {
                    json!({"id":request["id"],"result":result})
                };
                write.write_all(format!("{response}\n").as_bytes()).await?;
            }
            Ok::<_, Box<dyn Error + Send + Sync>>(count)
        });
        assert!(
            backend
                .archived_bindings(&BTreeSet::new())
                .await?
                .is_empty()
        );
        let found = backend
            .archived_bindings(&store.bound_threads().await?)
            .await;
        if matches!(mode, "success" | "storage") {
            let found = found?;
            assert_eq!(found, BTreeSet::from(["archived".into()]));
            let committed = store.clear_archived_bindings(found).await;
            if mode == "storage" {
                assert!(committed.is_err());
            } else {
                assert_eq!(committed?, 2);
                assert_eq!(
                    store
                        .clear_archived_bindings(BTreeSet::from(["archived".into()]))
                        .await?,
                    0
                );
            }
        } else {
            assert!(found.is_err());
        }
        connection.shutdown().await?;
        assert_eq!(
            responder
                .await?
                .map_err(|e| std::io::Error::other(e.to_string()))?,
            2
        );
        drop(store);
        let reopened = AsyncState::new(JsonStore::open(temp.path())?);
        let expected = if mode == "success" {
            None
        } else {
            Some("archived".into())
        };
        assert_eq!(reopened.thread(owner.clone()).await?, expected);
        assert_eq!(reopened.thread(shared).await?, expected);
        assert_eq!(reopened.thread(unrelated).await?, Some("keep".into()));
        assert!(reopened.preferences(owner).await?.plan);
        Ok::<_, Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn complete_archive_scan_repairs_all_bindings_in_one_commit() -> Result<(), Box<dyn Error>> {
    scenario("success").await
}

#[tokio::test]
async fn partial_or_invalid_scan_never_clears_bindings() -> Result<(), Box<dyn Error>> {
    for mode in ["cycle", "malformed", "identity", "remote"] {
        scenario(mode).await?;
    }
    Ok(())
}

#[tokio::test]
async fn failed_repair_preserves_bindings_for_next_startup() -> Result<(), Box<dyn Error>> {
    scenario("storage").await
}
