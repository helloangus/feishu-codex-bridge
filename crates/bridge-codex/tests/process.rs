//! A disposable Python protocol fake; no Codex login, network or working tree.
use bridge_app::ports::AgentBackend;
use bridge_codex::process::AppServer;
use std::{path::Path, time::Duration};

#[tokio::test]
async fn initializes_uses_port_and_reaps_fake_child() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let script = temp.path().join("fake.py");
    std::fs::write(
        &script,
        r#"
import json, sys
for line in sys.stdin:
    message = json.loads(line)
    if 'id' not in message:
        continue
    method = message['method']
    result = {'data': [{'id':'fake-model','isDefault':True}]} if method == 'model/list' else {}
    print(json.dumps({'id':message['id'],'result':result}), flush=True)
"#,
    )?;
    let args = vec![
        "-I".into(),
        "-u".into(),
        script.to_string_lossy().into_owned(),
    ];
    let mut server = tokio::time::timeout(
        Duration::from_secs(5),
        AppServer::spawn(Path::new("python3"), &args, temp.path(), 42),
    )
    .await??;
    let models = server.backend().models().await?;
    assert_eq!(models[0].id, "fake-model");
    assert!(models[0].is_default);
    tokio::time::timeout(Duration::from_secs(5), server.shutdown()).await??;
    assert!(server.is_disconnected()?);
    Ok(())
}

#[tokio::test]
async fn child_requests_and_notifications_cross_only_typed_boundary()
-> Result<(), Box<dyn std::error::Error>> {
    tokio::time::timeout(Duration::from_secs(5),async {
        let temp=tempfile::tempdir()?;
        let script=temp.path().join("events.py");
        std::fs::write(&script,r#"
import json,sys
for line in sys.stdin:
    message=json.loads(line)
    if message.get('method') == 'initialize':
        print(json.dumps({'id':message['id'],'result':{}}),flush=True)
    elif message.get('method') == 'initialized':
        print(json.dumps({'id':'approve-string','method':'item/fileChange/requestApproval','params':{'threadId':'t','turnId':'u','itemId':'i','startedAtMs':1}}),flush=True)
    elif message.get('id') == 'approve-string':
        assert message['result']['decision'] == 'decline'
        print(json.dumps({'method':'turn/completed','params':{'threadId':'t','turn':{'id':'u','status':'interrupted'}}}),flush=True)
"#)?;
        let args=vec!["-I".into(),"-u".into(),script.to_string_lossy().into_owned()];
        let mut server=AppServer::spawn(Path::new("python3"),&args,temp.path(),8).await?;
        let bridge_app::events::Incoming::Request {request,reply}=server.next_event().await? else {return Err("expected request".into());};
        assert_eq!(request.turn.epoch,8);
        assert_eq!(request.turn.turn_id,"u");
        reply.reply(bridge_app::requests::AgentReply::Approve(false)).await?;
        let bridge_app::events::Incoming::Notification(bridge_app::events::AgentEvent::Finished {outcome,..})=server.next_event().await? else {return Err("expected completion".into());};
        assert_eq!(outcome,bridge_app::events::TurnOutcome::Interrupted);
        server.shutdown().await?;
        assert!(server.is_disconnected()?);
        Ok::<_,Box<dyn std::error::Error>>(())
    }).await??;
    Ok(())
}

#[tokio::test]
async fn shutdown_terminates_tools_in_owned_process_group() -> Result<(), Box<dyn std::error::Error>>
{
    tokio::time::timeout(Duration::from_secs(5), async {
        let temp=tempfile::tempdir()?;
        let script=temp.path().join("parent.py");
        let pid_file=temp.path().join("tool.pid");
        std::fs::write(&script,r#"
import json,sys,subprocess,signal
signal.signal(signal.SIGTERM,signal.SIG_IGN)
tool=subprocess.Popen([sys.executable,'-u','-c','import signal,time; signal.signal(signal.SIGTERM,signal.SIG_DFL); print("ready",flush=True); time.sleep(30)'],stdout=subprocess.PIPE,text=True)
assert tool.stdout.readline().strip()=='ready'
with open(sys.argv[1],'w') as out: out.write(str(tool.pid))
try:
    for line in sys.stdin:
        message=json.loads(line)
        if 'id' in message: print(json.dumps({'id':message['id'],'result':{}}),flush=True)
finally:
    tool.wait()
"#)?;
        let args=vec!["-I".into(),"-u".into(),script.to_string_lossy().into_owned(),pid_file.to_string_lossy().into_owned()];
        let mut server=AppServer::spawn(Path::new("python3"),&args,temp.path(),7).await?;
        let pid=std::fs::read_to_string(pid_file)?.parse::<i32>()?;
        let pid=rustix::process::Pid::from_raw(pid).ok_or("invalid tool pid")?;
        assert!(rustix::process::test_kill_process(pid).is_ok());
        server.shutdown().await?;
        assert!(rustix::process::test_kill_process(pid).is_err());
        Ok::<_,Box<dyn std::error::Error>>(())
    }).await??;
    Ok(())
}
