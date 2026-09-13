//! Remote archive success precedes a single local commit clearing all references.
use bridge_app::sessions::{self, PreferenceChange, SessionStore, StartError, ThreadAction};
use bridge_codex::{backend::CodexBackend, transport::Connection};
use bridge_core::SessionKey;
use bridge_local::{async_state::AsyncState, state::JsonStore};
use serde_json::{Value, json};
use std::{error::Error, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn scenario(case: &str) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let temp=tempfile::tempdir()?;
        let store=AsyncState::new(JsonStore::open(temp.path())?);
        let owner=SessionKey::new("owner","/tmp/project");
        let shared=SessionKey::new("shared","/tmp/another-project");
        let other=SessionKey::new("other","/tmp/project");
        store.bind(owner.clone(),"target".into()).await?;
        store.bind(shared.clone(),"target".into()).await?;
        store.bind(other.clone(),"unrelated".into()).await?;
        store.set_preference(owner.clone(),PreferenceChange::Plan(true)).await?;
        if case=="storage" {
            let backup=temp.path().join("state.previous.json");
            std::fs::remove_file(&backup)?;
            std::fs::create_dir(backup)?;
        }
        let (client,remote)=tokio::io::duplex(8192);
        let (read,write)=tokio::io::split(client);
        let mut connection=Connection::new(read,write,1,8192);
        let backend=CodexBackend::new(connection.client.clone());
        let mode=case.to_owned();
        let responder=tokio::spawn(async move {
            let (read,mut write)=tokio::io::split(remote);
            let mut lines=BufReader::new(read).lines();let mut calls=Vec::new();
            while let Some(line)=lines.next_line().await? {
                let request:Value=serde_json::from_str(&line)?;
                let method=request["method"].as_str().ok_or("method")?;
                calls.push(method.to_owned());
                assert_eq!(request["params"]["threadId"],"target");
                let response=if method=="thread/read" {
                    let cwd=if mode=="directory" {"/tmp/foreign"} else {"/tmp/project"};
                    let status=if mode=="active" {"active"} else {"idle"};
                    let id=if mode=="identity" {"different-thread"} else {"target"};
                    let cwd=if mode=="missing_directory" {None} else {Some(cwd)};
                    json!({"id":request["id"],"result":{"thread":{"id":id,"cwd":cwd,"status":{"type":status}}}})
                } else {
                    assert_eq!(method,if mode=="unarchive" {"thread/unarchive"} else {"thread/archive"});
                    if mode=="uncertain" {break;}
                    if mode=="rejected" {json!({"id":request["id"],"error":{"code":-32000,"message":"rejected"}})}
                    else {json!({"id":request["id"],"result":{}})}
                };
                write.write_all(format!("{response}\n").as_bytes()).await?;
            }
            Ok::<_,Box<dyn Error+Send+Sync>>(calls)
        });
        let action=if case=="unarchive" {ThreadAction::Unarchive} else {ThreadAction::Archive};
        let result=sessions::change_thread(&backend,&store,owner.clone(),"target".into(),action).await;
        assert_eq!(result.is_ok(),matches!(case,"success"|"unarchive"));
        assert_eq!(matches!(result,Err(StartError::Reconcile)),matches!(case,"storage"|"uncertain"));
        drop(store);
        let reopened=AsyncState::new(JsonStore::open(temp.path())?);
        let expected=if case=="success" {None} else {Some("target".into())};
        assert_eq!(reopened.thread(owner.clone()).await?,expected);
        assert_eq!(reopened.thread(shared).await?,expected);
        assert_eq!(reopened.thread(other).await?,Some("unrelated".into()));
        assert!(reopened.preferences(owner).await?.plan);
        let shutdown = connection.shutdown().await;
        if case == "uncertain" {
            assert!(matches!(shutdown, Ok(()) | Err(bridge_codex::transport::RpcError::Closed)));
        } else {shutdown?;}
        let calls=responder.await?.map_err(|e|std::io::Error::other(e.to_string()))?;
        assert_eq!(calls.len(),if matches!(case,"active"|"directory"|"identity"|"missing_directory") {1} else {2});
        Ok::<_,Box<dyn Error>>(())
    }).await??;
    Ok(())
}

#[tokio::test]
async fn archive_clears_all_matching_bindings_and_preserves_preferences()
-> Result<(), Box<dyn Error>> {
    scenario("success").await
}
#[tokio::test]
async fn unarchive_does_not_switch_or_clear_bindings() -> Result<(), Box<dyn Error>> {
    scenario("unarchive").await
}
#[tokio::test]
async fn rejected_archive_preserves_bindings() -> Result<(), Box<dyn Error>> {
    scenario("rejected").await
}
#[tokio::test]
async fn active_or_foreign_thread_is_rejected_before_mutation() -> Result<(), Box<dyn Error>> {
    scenario("active").await?;
    scenario("directory").await
}
#[tokio::test]
async fn failed_local_commit_requires_reconciliation() -> Result<(), Box<dyn Error>> {
    scenario("storage").await
}
#[tokio::test]
async fn lost_archive_response_requires_reconciliation() -> Result<(), Box<dyn Error>> {
    scenario("uncertain").await
}

#[tokio::test]
async fn mismatched_identity_or_missing_directory_never_archives() -> Result<(), Box<dyn Error>> {
    scenario("identity").await?;
    scenario("missing_directory").await
}
