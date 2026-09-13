//! Directory persistence, filesystem boundaries and runtime routing stay offline.
use bridge_app::{
    directories::DirectoryStore,
    sessions::{PreferenceChange, SessionStore},
};
use bridge_core::SessionKey;
use bridge_local::{async_state::AsyncState, state::JsonStore};
use std::{error::Error, path::Path};

#[tokio::test]
async fn creation_requires_proposal_and_preserves_directory_on_save_failure()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("root");
    std::fs::create_dir(&root)?;
    let state = temp.path().join("state");
    let store = AsyncState::new(JsonStore::open(&state)?);
    let target = store
        .propose_directory(root.clone(), root.clone(), "new parent/child".into())
        .await?
        .ok_or("missing proposal")?;
    assert!(!target.exists());
    assert!(store.directory_preferences().await?.is_empty());
    assert_eq!(
        store
            .create_directory(
                "owner".into(),
                root.clone(),
                root.clone(),
                "new parent/child".into(),
                target.clone()
            )
            .await?,
        target
    );
    assert!(target.is_dir());
    assert_eq!(
        store.directory_preferences().await?.get("owner"),
        Some(&target)
    );
    // A consumed confirmation cannot create again; existing paths use normal /cd.
    assert!(
        store
            .create_directory(
                "owner".into(),
                root.clone(),
                root.clone(),
                "new parent/child".into(),
                target.clone()
            )
            .await
            .is_err()
    );
    let next = store
        .propose_directory(root.clone(), root.clone(), "another".into())
        .await?
        .ok_or("missing proposal")?;
    std::fs::create_dir(state.join("state.previous.json"))?;
    assert!(
        store
            .create_directory(
                "owner".into(),
                root.clone(),
                root.clone(),
                "another".into(),
                next.clone()
            )
            .await
            .is_err()
    );
    assert!(next.is_dir());
    assert_eq!(
        store.directory_preferences().await?.get("owner"),
        Some(&target)
    );
    Ok(())
}

#[tokio::test]
async fn creation_revalidates_proposed_path_and_rejects_symlink_replacement()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("root");
    std::fs::create_dir(&root)?;
    let parent = root.join("parent");
    std::fs::create_dir(&parent)?;
    let store = AsyncState::new(JsonStore::open(&temp.path().join("state"))?);
    let workspace = bridge_local::workspace::Workspace::new(&root)?;
    assert!(
        workspace
            .create_confirmed(&root.join("unexpected/../other"))
            .is_err()
    );
    assert!(!root.join("unexpected").exists());
    assert!(
        store
            .propose_directory(root.clone(), root.clone(), "../outside".into())
            .await
            .is_err()
    );
    let target = store
        .propose_directory(root.clone(), root.clone(), "parent/new".into())
        .await?
        .ok_or("missing proposal")?;
    #[cfg(unix)]
    {
        std::fs::rename(&parent, root.join("old-parent"))?;
        std::os::unix::fs::symlink(temp.path(), &parent)?;
        assert!(
            store
                .create_directory(
                    "owner".into(),
                    root.clone(),
                    root.clone(),
                    "parent/new".into(),
                    target.clone()
                )
                .await
                .is_err()
        );
        assert!(!temp.path().join("new").exists());
        // Direct creator also refuses links, independently of proposal validation.
        assert!(
            bridge_local::workspace::Workspace::new(&root)?
                .create_confirmed(&target)
                .is_err()
        );
    }
    assert!(store.directory_preferences().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn directory_change_persists_only_owner_and_preserves_session_settings()
-> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("root");
    std::fs::create_dir(&root)?;
    let child = root.join("dir with spaces");
    std::fs::create_dir(&child)?;
    let state = temp.path().join("state");
    let store = AsyncState::new(JsonStore::open(&state)?);
    let key = SessionKey::new("owner", &root);
    store.bind(key.clone(), "old-thread".into()).await?;
    store
        .set_preference(key.clone(), PreferenceChange::Plan(true))
        .await?;
    assert_eq!(
        store
            .change_directory(
                "owner".into(),
                root.clone(),
                root.clone(),
                "dir with spaces".into()
            )
            .await?,
        child
    );
    assert_eq!(
        store
            .inspect_directory(root.clone(), root.clone())
            .await?
            .children,
        vec!["dir with spaces"]
    );
    assert_eq!(store.thread(key.clone()).await?, Some("old-thread".into()));
    assert!(store.preferences(key).await?.plan);
    drop(store);
    let reopened = AsyncState::new(JsonStore::open(&state)?);
    let saved = reopened.directory_preferences().await?;
    assert_eq!(saved.get("owner"), Some(&child));
    assert!(!saved.contains_key("other"));
    assert_eq!(
        reopened
            .change_directory("owner".into(), root.clone(), child, "..".into())
            .await?,
        root
    );
    Ok(())
}

#[tokio::test]
async fn invalid_paths_and_redirected_snapshots_are_rejected() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("root");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("file"), "not a directory")?;
    let store = AsyncState::new(JsonStore::open(&temp.path().join("state"))?);
    for input in ["../", "missing", "file", "bad\nname", ""] {
        assert!(
            store
                .change_directory("owner".into(), root.clone(), root.clone(), input.into())
                .await
                .is_err(),
            "{input}"
        );
    }
    assert!(store.directory_preferences().await?.is_empty());
    assert!(!root.join("missing").exists());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(temp.path(), root.join("escape"))?;
        assert!(
            store
                .change_directory("owner".into(), root.clone(), root.clone(), "escape".into())
                .await
                .is_err()
        );
        let child = root.join("child");
        std::fs::create_dir(&child)?;
        store
            .change_directory("owner".into(), root.clone(), root.clone(), "child".into())
            .await?;
        std::fs::rename(&child, root.join("moved"))?;
        std::os::unix::fs::symlink(temp.path(), &child)?;
        assert!(
            store
                .validate_directory(root.clone(), child.clone())
                .await
                .is_err()
        );
        assert!(
            store
                .change_directory("owner".into(), root.clone(), child.clone(), "..".into())
                .await
                .is_err()
        );
        assert_eq!(
            store
                .change_directory(
                    "owner".into(),
                    root.clone(),
                    child,
                    root.to_string_lossy().into_owned()
                )
                .await?,
            root
        );
        assert!(
            !store
                .inspect_directory(root.clone(), root)
                .await?
                .children
                .contains(&"escape".to_owned())
        );
    }
    Ok(())
}

#[tokio::test]
async fn failed_directory_save_keeps_previous_choice() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().join("root");
    std::fs::create_dir(&root)?;
    let child = root.join("child");
    std::fs::create_dir(&child)?;
    let state = temp.path().join("state");
    let store = AsyncState::new(JsonStore::open(&state)?);
    store
        .change_directory("owner".into(), root.clone(), root.clone(), ".".into())
        .await?;
    std::fs::create_dir(state.join("state.previous.json"))?;
    assert!(
        store
            .change_directory("owner".into(), root.clone(), root.clone(), "child".into())
            .await
            .is_err()
    );
    assert_eq!(
        store.directory_preferences().await?.get("owner"),
        Some(&root)
    );
    drop(store);
    let reopened = AsyncState::new(JsonStore::open(&state)?);
    assert_eq!(
        reopened.directory_preferences().await?.get("owner"),
        Some(&root)
    );
    Ok(())
}

use bridge_app::{
    messaging::{DeliveryFuture, MessageId, Messenger, ResourceKind},
    runtime::{self, Input},
};
use bridge_codex::process::AppServer;
use bridge_core::view::Panel;
use std::{collections::BTreeSet, fs::File, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

struct Messages(mpsc::Sender<(String, String)>);
impl Messenger for Messages {
    fn send_text(&self, chat: String, text: String) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            self.0
                .send((chat, text))
                .await
                .map_err(|_| bridge_app::messaging::DeliveryError::Transport)
        })
    }
    fn send_panel(&self, _: String, _: Panel) -> DeliveryFuture<'_, MessageId> {
        Box::pin(async { Err(bridge_app::messaging::DeliveryError::Transport) })
    }
    fn update_panel(&self, _: MessageId, _: Panel) -> DeliveryFuture<'_, ()> {
        Box::pin(async { Err(bridge_app::messaging::DeliveryError::Transport) })
    }
    fn upload(&self, _: String, _: String, _: File, _: ResourceKind) -> DeliveryFuture<'_, ()> {
        Box::pin(async { panic!("unexpected upload") })
    }
}
async fn send(
    tx: &mpsc::Sender<Input>,
    id: &str,
    user: &str,
    text: &str,
) -> Result<(), Box<dyn Error>> {
    let (ack, wait) = oneshot::channel();
    tx.send(Input {
        attachments: vec![],
        card: None,
        id: id.into(),
        user: user.into(),
        chat: "chat".into(),
        text: Some(text.into()),
        accept: Box::new(move |value| {
            let _ = ack.send(value);
        }),
    })
    .await?;
    assert!(wait.await?);
    Ok(())
}
async fn until(
    rx: &mut mpsc::Receiver<(String, String)>,
    pattern: &str,
) -> Result<String, Box<dyn Error>> {
    loop {
        let (_, text) = rx.recv().await.ok_or("delivery closed")?;
        if text.contains(pattern) {
            return Ok(text);
        }
    }
}

fn confirmation(text: &str) -> Result<String, Box<dyn Error>> {
    text.lines()
        .find(|line| line.starts_with("/cd-confirm "))
        .map(str::to_owned)
        .ok_or_else(|| "missing confirmation command".into())
}

#[tokio::test]
async fn runtime_uses_user_directory_for_turns_settings_and_restart() -> Result<(), Box<dyn Error>>
{
    runtime_scenario(false).await
}

#[tokio::test]
async fn runtime_failed_save_does_not_switch_cached_directory() -> Result<(), Box<dyn Error>> {
    runtime_scenario(true).await
}

async fn runtime_scenario(fail_directory_save: bool) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(20),async {
        let temp=tempfile::tempdir()?;
        let root=temp.path().join("root");std::fs::create_dir(&root)?;
        let child=root.join("child space");std::fs::create_dir(&child)?;
        let script=temp.path().join("server.py");
        std::fs::write(&script,r#"
import json,sys,os,hashlib
threads={}
counter=0
def emit(v):print(json.dumps(v),flush=True)
for line in sys.stdin:
    m=json.loads(line);method=m.get('method');p=m.get('params',{})
    if method=='initialized':continue
    if method=='initialize':result={}
    elif method=='model/list':result={'data':[{'id':'default','isDefault':True},{'id':'chosen','isDefault':False}]}
    elif method in ('thread/start','thread/resume'):
        cwd=p['cwd'];thread='th-'+hashlib.sha256(cwd.encode()).hexdigest()[:8]
        if method=='thread/resume':assert p['threadId']==thread
        threads[thread]=cwd;result={'thread':{'id':thread,'cwd':cwd,'status':{'type':'idle'}}}
    elif method=='turn/start':
        counter+=1;turn=str(counter);thread=p['threadId'];cwd=threads[thread]
        assert p['cwd']==cwd
        assert p['sandboxPolicy']['writableRoots']==[cwd]
        summary=cwd+'|'+p['model']+'|'+p['collaborationMode']['mode']
        emit({'method':'item/agentMessage/delta','params':{'threadId':thread,'turnId':turn,'itemId':'i','delta':summary}})
        if p['input'][0]['text']!='hold':emit({'method':'turn/completed','params':{'threadId':thread,'turn':{'id':turn,'status':'completed'}}})
        else:
            with open('holding','w') as f:f.write('yes')
        result={'turn':{'id':turn}}
    elif method=='turn/interrupt':
        emit({'method':'turn/completed','params':{'threadId':p['threadId'],'turn':{'id':p['turnId'],'status':'interrupted'}}});result={}
    else:raise RuntimeError(method)
    emit({'id':m['id'],'result':result})
"#)?;
        let mut restart_confirmation=String::new();
        for round in 0..2 {
            let mut server=AppServer::spawn(Path::new("python3"),&["-I".into(),"-u".into(),script.to_string_lossy().into_owned()],&root,27).await?;
            let backend=Arc::new(server.backend());
            let store=Arc::new(AsyncState::new(JsonStore::open(&temp.path().join("state"))?));
            let (tx,inputs)=mpsc::channel(16);let (event_tx,events)=mpsc::channel(16);let (messages,mut output)=mpsc::channel(64);
            let cancel=CancellationToken::new();let stop=cancel.clone();
            let source=tokio::spawn(async move {loop {let event=tokio::select! {_=stop.cancelled()=>break,event=server.next_event()=>event};if event_tx.send(event).await.is_err(){break;}}server.shutdown().await});
            let worker=tokio::spawn(runtime::run(runtime::Settings {root:root.clone(),directory:root.clone(),allowed:BTreeSet::from(["owner".into(),"other".into()]),open_access:false,sandbox:bridge_app::ports::Sandbox::WorkspaceWrite,epoch:27},backend,store.clone(),Arc::new(Messages(messages)),inputs,events,cancel.clone()));
            if round==0 {
                send(&tx,"browse","owner","/cd").await?;
                assert!(until(&mut output,"当前目录").await?.contains("/cd child space"));
                if fail_directory_save {
                    std::fs::create_dir(temp.path().join("state/state.json"))?;
                    send(&tx,"failed-switch","owner","/cd child space").await?;
                    until(&mut output,"切换目录失败").await?;
                    send(&tx,"unchanged","owner","/status").await?;
                    assert!(until(&mut output,"当前目录").await?.contains(&format!("当前目录：{}\n",root.display())));
                    assert!(store.directory_preferences().await?.is_empty());
                    cancel.cancel();worker.await??;source.await??;
                    return Ok(());
                }
                send(&tx,"propose","owner","/cd created/nested").await?;
                let command=confirmation(&until(&mut output,"目录不存在").await?)?;
                assert!(!root.join("created").exists());
                send(&tx,"wrong-user","other",&command).await?;until(&mut output,"创建确认无效").await?;
                let (ack,wait)=oneshot::channel();
                tx.send(Input { attachments:vec![],card: None,id:"wrong-chat".into(),user:"owner".into(),chat:"different-chat".into(),text:Some(command.clone()),accept:Box::new(move|accepted|{let _=ack.send(accepted);})}).await?;
                assert!(wait.await?);until(&mut output,"创建确认无效").await?;
                assert!(!root.join("created").exists());
                send(&tx,"confirm","owner",&command).await?;until(&mut output,"目录已创建并切换").await?;
                assert!(root.join("created/nested").is_dir());
                send(&tx,"confirm-repeat","owner",&command).await?;until(&mut output,"创建确认无效").await?;
                send(&tx,"return-root","owner",&format!("/cd {}",root.display())).await?;until(&mut output,"已切换目录").await?;
                send(&tx,"old-proposal","owner","/cd abandoned").await?;
                let abandoned=confirmation(&until(&mut output,"目录不存在").await?)?;
                send(&tx,"switch","owner","/cd child space").await?;until(&mut output,"已切换目录").await?;
                send(&tx,"old-confirm","owner",&abandoned).await?;until(&mut output,"创建确认无效").await?;
                assert!(!root.join("abandoned").exists());
                send(&tx,"model","owner","/model chosen").await?;until(&mut output,"设置已保存").await?;
                send(&tx,"plan","owner","/plan on").await?;until(&mut output,"设置已保存").await?;
                send(&tx,"child-turn","owner","hello").await?;
                assert!(until(&mut output,"执行完成").await?.contains(&format!("{}|chosen|plan",child.display())));
                send(&tx,"parent","owner","/cd ..").await?;until(&mut output,"已切换目录").await?;
                send(&tx,"root-turn","owner","hello").await?;
                assert!(until(&mut output,"执行完成").await?.contains(&format!("{}|default|default",root.display())));
                send(&tx,"switch","owner","/cd child space").await?;
                send(&tx,"dedup-status","owner","/status").await?;
                assert!(until(&mut output,"当前目录").await?.contains(&format!("当前目录：{}\n",root.display())));
                send(&tx,"back","owner",&format!("/cd {}",child.display())).await?;until(&mut output,"已切换目录").await?;
                send(&tx,"busy-proposal","owner","/cd after-task").await?;
                let after_task=confirmation(&until(&mut output,"目录不存在").await?)?;
                send(&tx,"hold","owner","hold").await?;until(&mut output,"已开始执行").await?;
                while !tokio::fs::try_exists(root.join("holding")).await? {tokio::time::sleep(Duration::from_millis(5)).await;}
                send(&tx,"busy-cd","owner","/cd ..").await?;until(&mut output,"有任务执行中").await?;
                send(&tx,"busy-confirm","owner",&after_task).await?;until(&mut output,"有任务执行中").await?;
                send(&tx,"queued","owner","must be cancelled").await?;until(&mut output,"请求已接收").await?;
                send(&tx,"other-cd","other","/cd child space").await?;until(&mut output,"有任务执行中").await?;
                send(&tx,"stop","owner","/stop").await?;until(&mut output,"任务已停止").await?;
                send(&tx,"idle-confirm","owner",&after_task).await?;until(&mut output,"目录已创建并切换").await?;
                send(&tx,"return-child","owner",&format!("/cd {}",child.display())).await?;until(&mut output,"已切换目录").await?;
                send(&tx,"restart-proposal","owner","/cd never-created").await?;
                restart_confirmation=confirmation(&until(&mut output,"目录不存在").await?)?;
            } else {
                send(&tx,"restart-confirm","owner",&restart_confirmation).await?;until(&mut output,"创建确认无效").await?;
                assert!(!child.join("never-created").exists());
                send(&tx,"parent","owner","/cd ..").await?;
                send(&tx,"restart-status","owner","/status").await?;
                assert!(until(&mut output,"当前目录").await?.contains(&format!("当前目录：{}\n",child.display())));
                send(&tx,"restart-turn","owner","hello").await?;
                assert!(until(&mut output,"执行完成").await?.contains(&format!("{}|chosen|plan",child.display())));
                send(&tx,"other-turn","other","hello").await?;
                assert!(until(&mut output,"执行完成").await?.contains(&format!("{}|default|default",root.display())));
                #[cfg(unix)] {
                    std::fs::rename(&child,root.join("moved-child"))?;
                    std::os::unix::fs::symlink(temp.path(),&child)?;
                    send(&tx,"redirected","owner","must not execute outside root").await?;
                    until(&mut output,"当前目录已失效或越出工作区").await?;
                    send(&tx,"redirected-compact","owner","/compact").await?;
                    until(&mut output,"压缩准备失败").await?;
                    send(&tx,"recover","owner",&format!("/cd {}",root.display())).await?;
                    until(&mut output,"已切换目录").await?;
                }
            }
            cancel.cancel();worker.await??;source.await??;
        }
        Ok::<_,Box<dyn Error>>(())
    }).await??;
    Ok(())
}
