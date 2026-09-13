//! Full minimal actor with fake stdio app-server, real store and fake delivery.
use bridge_app::{
    messaging::{DeliveryFuture, MessageId, Messenger, ResourceKind},
    runtime::{self, Input},
    sessions::SessionStore,
};
use bridge_codex::process::AppServer;
use bridge_core::view::Panel;
use bridge_local::{async_state::AsyncState, state::JsonStore};
use std::{collections::BTreeSet, error::Error, fs::File, path::Path, sync::Arc, time::Duration};
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
        accept: Box::new(move |accepted| {
            let _ = ack.send(accepted);
        }),
    })
    .await
    .map_err(|_| "input closed")?;
    assert!(wait.await?);
    Ok(())
}
async fn until(
    rx: &mut mpsc::Receiver<(String, String)>,
    pattern: &str,
) -> Result<String, Box<dyn Error>> {
    loop {
        let (chat, text) = rx.recv().await.ok_or("delivery closed")?;
        assert_eq!(chat, "chat");
        if text.contains(pattern) {
            return Ok(text);
        }
    }
}

#[tokio::test]
async fn authorized_text_completes_and_only_owner_interrupts_started_turn()
-> Result<(), Box<dyn Error>> {
    scenario(false).await
}

#[tokio::test]
async fn archive_commit_failure_stops_actor_after_reporting_error() -> Result<(), Box<dyn Error>> {
    scenario(true).await
}

#[tokio::test]
async fn compaction_holds_idle_gate_until_terminal_and_deduplicates() -> Result<(), Box<dyn Error>>
{
    compact_scenario("complete").await
}

#[tokio::test]
async fn compaction_completion_before_rpc_response_is_retained() -> Result<(), Box<dyn Error>> {
    compact_scenario("early").await
}

#[tokio::test]
async fn compaction_can_only_be_stopped_by_owner() -> Result<(), Box<dyn Error>> {
    compact_scenario("stop").await
}

#[tokio::test]
async fn compaction_failure_is_not_reported_as_success() -> Result<(), Box<dyn Error>> {
    compact_scenario("failed").await?;
    compact_scenario("rejected").await
}

#[tokio::test]
async fn uncertain_compaction_stops_without_retry() -> Result<(), Box<dyn Error>> {
    compact_scenario("uncertain").await
}

#[tokio::test]
async fn compaction_requires_existing_valid_idle_session() -> Result<(), Box<dyn Error>> {
    for mode in ["empty", "foreign", "active", "wrong_resume"] {
        compact_scenario(mode).await?;
    }
    Ok(())
}

#[tokio::test]
async fn compaction_failed_claim_never_calls_backend() -> Result<(), Box<dyn Error>> {
    compact_scenario("storage").await
}

#[tokio::test]
async fn compaction_stop_during_preparation_never_submits() -> Result<(), Box<dyn Error>> {
    compact_scenario("prepare_stop").await
}

async fn compact_scenario(mode: &str) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let temp=tempfile::tempdir()?;
        let script=temp.path().join("compact.py");
        std::fs::write(&script,r#"
import json,sys,os
mode=sys.argv[1]
pending=False
preparation=None
def emit(v): print(json.dumps(v),flush=True)
def started(thread='thread',turn='compact'):
    emit({'method':'turn/started','params':{'threadId':thread,'turn':{'id':turn,'status':'inProgress'}}})
def finished(status='completed',turn='compact'):
    emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':turn,'status':status,'error':({'message':'fake compact error'} if status=='failed' else None)}}})
for line in sys.stdin:
    m=json.loads(line);method=m.get('method')
    if method=='initialized':continue
    if method=='initialize':result={}
    elif method in ('thread/read','thread/resume'):
        assert m['params']['threadId']=='thread'
        result={'thread':{'id':('wrong' if mode=='wrong_resume' and method=='thread/resume' else 'thread'),'cwd':('/foreign' if mode=='foreign' else os.getcwd()),'status':{'type':('active' if mode=='active' else 'idle')}}}
        with open('preparation','a') as f:f.write(method+'\n')
        if mode=='prepare_stop' and method=='thread/read':
            preparation={'id':m['id'],'result':result};continue
    elif method=='thread/compact/start':
        assert m['params']=={'threadId':'thread'}
        with open('compactions','a') as f:f.write('compact\n')
        if mode=='rejected':
            emit({'id':m['id'],'error':{'code':-32000,'message':'rejected'}});continue
        if mode=='uncertain':
            emit({'id':m['id'],'result':None});continue
        pending=True
        started('unrelated','foreign')
        started()
        finished(turn='stale')
        if mode=='early':finished();pending=False
        result={}
    elif method=='model/list':
        if preparation is not None:emit(preparation);preparation=None
        if pending:finished('failed' if mode=='failed' else 'completed');pending=False
        result={'data':[{'id':'model','isDefault':True}]}
    elif method=='turn/interrupt':
        assert m['params']=={'threadId':'thread','turnId':'compact'}
        with open('compact-interrupts','a') as f:f.write('interrupt\n')
        finished('interrupted');pending=False;result={}
    else:raise RuntimeError(method)
    emit({'id':m['id'],'result':result})
"#)?;
        let mut server=AppServer::spawn(Path::new("python3"),&["-I".into(),"-u".into(),script.to_string_lossy().into_owned(),mode.into()],temp.path(),19).await?;
        let backend=Arc::new(server.backend());
        let state_path=temp.path().join("state");
        let key=bridge_core::SessionKey::new("allowed",temp.path());
        let initial=AsyncState::new(JsonStore::open(&state_path)?);
        if mode!="empty" {initial.bind(key.clone(),"thread".into()).await?;}
        drop(initial);
        let store=Arc::new(AsyncState::new(JsonStore::open(&state_path)?));
        if mode=="storage" {
            std::fs::create_dir(state_path.join("seen-messages.json"))?;
        }
        let (input_tx,input_rx)=mpsc::channel(16);
        let (event_tx,event_rx)=mpsc::channel(16);
        let (messages,mut output)=mpsc::channel(64);
        let cancel=CancellationToken::new();let stop=cancel.clone();
        let source=tokio::spawn(async move {
            loop {let event=tokio::select! {_=stop.cancelled()=>break,event=server.next_event()=>event};if event_tx.send(event).await.is_err(){break;}}
            server.shutdown().await
        });
        let worker=tokio::spawn(runtime::run(runtime::Settings {root:temp.path().into(),directory:temp.path().into(),allowed:BTreeSet::from(["allowed".into(),"other".into()]),open_access:false,sandbox:bridge_app::ports::Sandbox::WorkspaceWrite,epoch:19},backend,store.clone(),Arc::new(Messages(messages)),input_rx,event_rx,cancel.clone()));
        if mode=="storage" {
            let (ack,wait)=oneshot::channel();
            input_tx.send(Input { attachments:vec![],card: None,id:"compact".into(),user:"allowed".into(),chat:"chat".into(),text:Some("/compact".into()),accept:Box::new(move|value|{let _=ack.send(value);})}).await?;
            assert!(!wait.await?);
            until(&mut output,"压缩请求保存失败").await?;
            assert!(!temp.path().join("preparation").exists());
            cancel.cancel();worker.await??;source.await??;
            return Ok(());
        }
        send(&input_tx,"compact","allowed","/compact").await?;
        if mode=="prepare_stop" {
            while !tokio::fs::try_exists(temp.path().join("preparation")).await? {tokio::time::sleep(Duration::from_millis(5)).await;}
            send(&input_tx,"prepare-stop","allowed","/stop").await?;
            until(&mut output,"已请求停止当前任务").await?;
            send(&input_tx,"release-read","allowed","/models").await?;
            until(&mut output,"准备阶段已停止，未启动压缩").await?;
            assert!(!temp.path().join("compactions").exists());
        } else if matches!(mode,"empty"|"foreign"|"active"|"wrong_resume") {
            until(&mut output,"压缩准备失败").await?;
            assert!(!temp.path().join("compactions").exists());
        } else if mode=="uncertain" {
            until(&mut output,"压缩启动结果不确定").await?;
            assert!(worker.await?.is_err());
            cancel.cancel();source.await??;
            assert_eq!(std::fs::read_to_string(temp.path().join("compactions"))?,"compact\n");
            return Ok(());
        } else if mode=="rejected" {
            until(&mut output,"压缩请求被拒绝").await?;
        } else if mode=="early" {
            until(&mut output,"上下文压缩完成").await?;
        } else {
            until(&mut output,"正在等待完成").await?;
            send(&input_tx,"status","allowed","/status").await?;
            let status=until(&mut output,"上下文压缩中").await?;
            assert!(!status.contains("上下文压缩完成"));
            send(&input_tx,"busy","allowed","/new").await?;
            until(&mut output,"有任务执行中").await?;
            send(&input_tx,"again","allowed","/compact").await?;
            until(&mut output,"有任务执行中").await?;
            let (ack,wait)=oneshot::channel();
            input_tx.send(Input { attachments:vec![],card: None,id:"blocked-task".into(),user:"allowed".into(),chat:"chat".into(),text:Some("must not run".into()),accept:Box::new(move|value|{let _=ack.send(value);})}).await?;
            assert!(!wait.await?);
            until(&mut output,"任务队列繁忙").await?;
            send(&input_tx,"other-stop","other","/stop").await?;
            until(&mut output,"没有可停止").await?;
            assert!(!temp.path().join("compact-interrupts").exists());
            if mode=="stop" {
                send(&input_tx,"stop","allowed","/stop").await?;
                until(&mut output,"上下文压缩已停止").await?;
                assert_eq!(std::fs::read_to_string(temp.path().join("compact-interrupts"))?,"interrupt\n");
            } else {
                send(&input_tx,"finish","allowed","/models").await?;
                until(&mut output,if mode=="failed" {"上下文压缩失败：fake compact error"} else {"上下文压缩完成"}).await?;
            }
        }
        send(&input_tx,"compact","allowed","/compact").await?;
        send(&input_tx,"idle","allowed","/status").await?;
        until(&mut output,"空闲").await?;
        assert_eq!(store.thread(key).await?,if mode=="empty" {None} else {Some("thread".into())});
        if !matches!(mode,"empty"|"foreign"|"active"|"wrong_resume"|"prepare_stop") {
            assert_eq!(std::fs::read_to_string(temp.path().join("compactions"))?,"compact\n");
            assert_eq!(std::fs::read_to_string(temp.path().join("preparation"))?,"thread/read\nthread/resume\n");
        }
        cancel.cancel();worker.await??;source.await??;
        drop(store);
        let reopened=AsyncState::new(JsonStore::open(&state_path)?);
        assert!(!bridge_app::sessions::DurableJournal::claim(&reopened,"compact".into()).await?);
        Ok::<_,Box<dyn Error>>(())
    }).await??;
    Ok(())
}

async fn scenario(fail_archive_commit: bool) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(15),async {
        let temp=tempfile::tempdir()?;
        let script=temp.path().join("fake.py");
        std::fs::write(&script,r#"
import json,sys,os
def emit(value):
    print(json.dumps(value),flush=True)
turn=0
archived=False
for line in sys.stdin:
    m=json.loads(line)
    method=m.get('method')
    if method=='initialized':continue
    if method=='initialize':result={}
    elif method=='model/list':result={'data':[{'id':'model','isDefault':True},{'id':'selected','isDefault':False}]}
    elif method in ('thread/start','thread/resume'):result={'thread':{'id':'thread','cwd':os.getcwd()}}
    elif method=='thread/read':result={'thread':{'id':'thread','cwd':os.getcwd(),'status':{'type':'idle'}}}
    elif method=='thread/list':result={'data':([{'id':'thread','cwd':os.getcwd(),'title':'local session'}, {'id':'foreign','cwd':'/foreign','title':'must not display'}] if m['params']['archived']==archived else [])}
    elif method in ('thread/archive','thread/unarchive'):
        assert m['params']['threadId']=='thread'
        archived=method=='thread/archive'
        with open('archive-actions','a') as trace:trace.write(method+'\n')
        result={}
    elif method=='turn/start':
        turn+=1
        assert m['params']['model']=='selected', m
        assert m['params']['collaborationMode']['mode']=='plan', m
        if turn==1:
            emit({'method':'item/agentMessage/delta','params':{'threadId':'thread','turnId':'1','itemId':'i','delta':'fake answer'}})
            emit({'method':'item/completed','params':{'threadId':'thread','turnId':'1','item':{'id':'plan','type':'plan','text':'authoritative plan'}}})
            emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'1','status':'completed'}}})
        result={'turn':{'id':str(turn)}}
    elif method=='turn/interrupt':
        assert m['params']=={'threadId':'thread','turnId':'2'}, m
        with open('interrupts.jsonl','a') as trace:
            trace.write(json.dumps(m['params'])+'\n')
        emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':str(turn),'status':'interrupted'}}})
        result={}
    else:raise RuntimeError('unexpected method')
    emit({'id':m['id'],'result':result})
    if method=='turn/start' and turn==2:
        with open('started','w') as marker:
            marker.write('2')
"#)?;
        let mut server=AppServer::spawn(Path::new("python3"),&["-I".into(),"-u".into(),script.to_string_lossy().into_owned()],temp.path(),9).await?;
        let backend=Arc::new(server.backend());
        let store=Arc::new(AsyncState::new(JsonStore::open(&temp.path().join("state"))?));
        let (input_tx,input_rx)=mpsc::channel(16);
        let (event_tx,event_rx)=mpsc::channel(16);
        let (messages,mut output)=mpsc::channel(64);
        let cancel=CancellationToken::new();let stop=cancel.clone();
        let source=tokio::spawn(async move {
            loop {let event=tokio::select! {_=stop.cancelled()=>break,event=server.next_event()=>event};if event_tx.send(event).await.is_err(){break;}}
            server.shutdown().await
        });
        let worker=tokio::spawn(runtime::run(runtime::Settings {root:temp.path().into(),directory:temp.path().into(),allowed:BTreeSet::from(["allowed".into(),"other-allowed".into()]),open_access:false,sandbox:bridge_app::ports::Sandbox::WorkspaceWrite,epoch:9},backend,store.clone(),Arc::new(Messages(messages)),input_rx,event_rx,cancel.clone()));
        send(&input_tx,"unauthorized","stranger","hello").await?;
        send(&input_tx,"models","allowed","/models").await?;
        until(&mut output,"/model selected").await?;
        send(&input_tx,"bad-model","allowed","/model nonexistent").await?;
        until(&mut output,"设置失败").await?;
        send(&input_tx,"model","allowed","/model selected").await?;
        until(&mut output,"设置已保存").await?;
        send(&input_tx,"plan-on","allowed","/plan on").await?;
        until(&mut output,"设置已保存").await?;
        send(&input_tx,"plan-query","allowed","/plan").await?;
        until(&mut output,"已开启").await?;
        send(&input_tx,"one","allowed","hello").await?;
        let completed=until(&mut output,"执行完成").await?;
        assert!(completed.contains("authoritative plan"));
        assert!(!completed.contains("fake answer"));
        send(&input_tx,"two","allowed","keep working").await?;
        until(&mut output,"已开始执行").await?;
        // The fake writes this only after receiving turn/start and flushing its
        // response. Poll a concrete acknowledgement, not an assumed delay.
        while !tokio::fs::try_exists(temp.path().join("started")).await? {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        send(&input_tx,"busy-new","allowed","/new").await?;
        until(&mut output,"有任务执行中").await?;
        send(&input_tx,"busy-plan","allowed","/plan off").await?;
        until(&mut output,"有任务执行中").await?;
        send(&input_tx,"busy-resume","allowed","/resume thread").await?;
        until(&mut output,"有任务执行中").await?;
        send(&input_tx,"busy-archive","allowed","/archive thread").await?;
        until(&mut output,"有任务执行中").await?;
        assert_eq!(store.thread(bridge_core::SessionKey::new("allowed",temp.path())).await?,Some("thread".into()));
        send(&input_tx,"other-stop","other-allowed","/stop").await?;
        until(&mut output,"没有可停止的当前任务").await?;
        send(&input_tx,"status","allowed","/status").await?;
        assert!(!until(&mut output,"运行中").await?.contains("正在停止"));
        assert!(!tokio::fs::try_exists(temp.path().join("interrupts.jsonl")).await?);
        send(&input_tx,"stop","allowed","/stop").await?;
        until(&mut output,"任务已停止").await?;
        send(&input_tx,"list","allowed","/resume").await?;
        let listing=until(&mut output,"/resume thread").await?;
        assert!(!listing.contains("foreign"));
        send(&input_tx,"idle-new","allowed","/new").await?;
        until(&mut output,"下次提问时自动创建").await?;
        send(&input_tx,"resume","allowed","/resume thread").await?;
        until(&mut output,"会话已恢复").await?;
        assert_eq!(store.thread(bridge_core::SessionKey::new("allowed",temp.path())).await?,Some("thread".into()));
        // A replay cannot restore an old binding over a newer local selection.
        store.bind(bridge_core::SessionKey::new("allowed",temp.path()),"newer".into()).await?;
        send(&input_tx,"resume","allowed","/resume thread").await?;
        send(&input_tx,"after-replay","allowed","/status").await?;
        until(&mut output,"空闲").await?;
        assert_eq!(store.thread(bridge_core::SessionKey::new("allowed",temp.path())).await?,Some("newer".into()));
        send(&input_tx,"plan-off","allowed","/plan off").await?;
        until(&mut output,"设置已保存").await?;
        send(&input_tx,"plan-on","allowed","/plan on").await?;
        send(&input_tx,"default-model","allowed","/model default").await?;
        until(&mut output,"设置已保存").await?;
        let preferences=store.preferences(bridge_core::SessionKey::new("allowed",temp.path())).await?;
        assert!(!preferences.plan,"old command replay must not re-enable Plan");
        assert!(preferences.model.is_none());
        let own=bridge_core::SessionKey::new("allowed",temp.path());
        store.bind(own.clone(),"thread".into()).await?;
        if fail_archive_commit {
            let backup=temp.path().join("state/state.previous.json");
            if backup.is_file() {std::fs::remove_file(&backup)?;}
            std::fs::create_dir(backup)?;
        }
        send(&input_tx,"archive","allowed","/archive thread").await?;
        if fail_archive_commit {
            until(&mut output,"归档状态结果不确定").await?;
            assert!(worker.await?.is_err());
            cancel.cancel();source.await??;
            assert_eq!(store.thread(own).await?,Some("thread".into()));
            return Ok::<_,Box<dyn Error>>(());
        }
        until(&mut output,"会话已归档").await?;
        assert!(store.thread(own.clone()).await?.is_none());
        send(&input_tx,"archived-list","allowed","/archived").await?;
        until(&mut output,"/unarchive thread").await?;
        send(&input_tx,"unarchive","allowed","/unarchive thread").await?;
        until(&mut output,"已取消归档").await?;
        assert!(store.thread(own.clone()).await?.is_none());
        store.bind(own.clone(),"thread".into()).await?;
        send(&input_tx,"archive","allowed","/archive thread").await?;
        send(&input_tx,"archive-replay-status","allowed","/status").await?;
        until(&mut output,"空闲").await?;
        assert_eq!(store.thread(own).await?,Some("thread".into()));
        assert_eq!(std::fs::read_to_string(temp.path().join("archive-actions"))?,"thread/archive\nthread/unarchive\n");
        cancel.cancel();worker.await?.map_err(|e|->Box<dyn Error>{e.into()})?;source.await??;
        let trace=std::fs::read_to_string(temp.path().join("interrupts.jsonl"))?;
        let interrupts:Vec<serde_json::Value>=trace.lines().map(serde_json::from_str).collect::<Result<_,_>>()?;
        assert_eq!(interrupts,vec![serde_json::json!({"threadId":"thread","turnId":"2"})]);
        Ok::<_,Box<dyn Error>>(())
    }).await??;
    Ok(())
}

#[test]
fn global_app_lock_matches_python_and_excludes_second_instance() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let lock = bridge_cli::bootstrap::app_lock(temp.path(), "test-app")?;
    assert!(bridge_cli::bootstrap::app_lock(temp.path(), "test-app").is_err());
    drop(lock);
    assert!(bridge_cli::bootstrap::app_lock(temp.path(), "test-app").is_ok());
    Ok(())
}
