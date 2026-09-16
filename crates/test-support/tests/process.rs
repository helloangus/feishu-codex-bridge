//! A disposable Rust protocol fake; no Codex login, network or working tree.
use bridge_app::ports::AgentBackend;
use bridge_codex::process::AppServer;
use std::time::Duration;
use test_support::fake_codex_process;

#[tokio::test]
async fn initializes_uses_port_and_reaps_fake_child() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let args = vec!["simple".into()];
    let executable = fake_codex_process!();
    let mut server = tokio::time::timeout(
        Duration::from_secs(5),
        AppServer::spawn(&executable, &args, temp.path(), 42),
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
    tokio::time::timeout(Duration::from_secs(5), async {
        let temp = tempfile::tempdir()?;
        let args = vec!["events".into()];
        let executable = fake_codex_process!();
        let mut server = AppServer::spawn(&executable, &args, temp.path(), 8).await?;
        let bridge_app::events::Incoming::Request { request, reply } = server.next_event().await?
        else {
            return Err("expected request".into());
        };
        assert_eq!(request.turn.epoch, 8);
        assert_eq!(request.turn.turn_id, "u");
        reply
            .reply(bridge_app::requests::AgentReply::Approve(false))
            .await?;
        let bridge_app::events::Incoming::Notification(bridge_app::events::AgentEvent::Finished {
            outcome,
            ..
        }) = server.next_event().await?
        else {
            return Err("expected completion".into());
        };
        assert_eq!(outcome, bridge_app::events::TurnOutcome::Interrupted);
        server.shutdown().await?;
        assert!(server.is_disconnected()?);
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn shutdown_terminates_tools_in_owned_process_group() -> Result<(), Box<dyn std::error::Error>>
{
    tokio::time::timeout(Duration::from_secs(5), async {
        let temp = tempfile::tempdir()?;
        let pid_file = temp.path().join("tool.pid");
        let args = vec!["parent".into(), pid_file.to_string_lossy().into_owned()];
        let executable = fake_codex_process!();
        let mut server = AppServer::spawn(&executable, &args, temp.path(), 7).await?;
        let pid = std::fs::read_to_string(pid_file)?.parse::<i32>()?;
        let pid = rustix::process::Pid::from_raw(pid).ok_or("invalid tool pid")?;
        assert!(rustix::process::test_kill_process(pid).is_ok());
        server.shutdown().await?;
        assert!(rustix::process::test_kill_process(pid).is_err());
        Ok::<_, Box<dyn std::error::Error>>(())
    })
    .await??;
    Ok(())
}
