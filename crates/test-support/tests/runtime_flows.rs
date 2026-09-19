//! Full runtime actor with fake stdio app-server, real store and fake delivery.
use bridge_app::messaging::{DeliveryError, DeliveryFuture, MessageId, Messenger, ResourceKind};
use bridge_app::{ports::Sandbox, sessions::SessionStore};
use bridge_core::view::Panel;
use bridge_local::{async_state::AsyncState, state::JsonStore};
use std::{
    collections::BTreeSet, error::Error, fs::File, path::PathBuf, sync::Arc, time::Duration,
};
use test_support::{
    actor::{Actor, ActorConfig},
    diagnostics,
    input::submit,
    messenger::{MessengerHandles, MessengerOptions, RecordingMessenger, expect_chat_text},
};

fn options() -> MessengerOptions {
    MessengerOptions {
        panels_fail: true,
        ..MessengerOptions::new()
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
        let temp = tempfile::tempdir()?;
        let state_path: PathBuf = temp.path().join("state");
        let key = bridge_core::SessionKey::new("allowed", temp.path());
        let initial = AsyncState::new(JsonStore::open(&state_path)?);
        if mode != "empty" {
            initial.bind(key.clone(), "thread".into()).await?;
        }
        drop(initial);
        let store = Arc::new(AsyncState::new(JsonStore::open(&state_path)?));
        if mode == "storage" {
            // The `storage` scenario is implemented here, not in the fake:
            // replacing the journal file with a directory makes every durable
            // write fail.
            std::fs::create_dir(state_path.join("seen-messages.json"))?;
        }
        let (messenger, mut output) = RecordingMessenger::recorded("", options());
        let mut actor = Actor::start(
            ActorConfig {
                executable: test_support::fake_codex_runtime!(),
                args: vec!["compact".into(), mode.into()],
                server_cwd: temp.path().into(),
                epoch: 19,
                root: temp.path().into(),
                directory: temp.path().into(),
                allowed: BTreeSet::from(["allowed".into(), "other".into()]),
                open_access: false,
                sandbox: Sandbox::WorkspaceWrite,
            },
            store.clone(),
            messenger.clone(),
            bridge_app::diagnostics::Diagnostics::noop(),
        )
        .await?;
        if mode == "storage" {
            assert_storage_failure(&mut actor, &mut output, &temp).await?;
            return Ok(());
        }
        actor.send_text("compact", "allowed", "/compact").await?;
        match mode {
            "prepare_stop" => {
                assert_prepare_stop(&mut actor, &mut output, &temp).await?;
            }
            "empty" | "foreign" | "active" | "wrong_resume" => {
                assert_prepare_failed(&mut output, &temp).await?;
            }
            "uncertain" => {
                assert_uncertain(&mut actor, &mut output, &temp).await?;
                return Ok(());
            }
            "rejected" => {
                expect_chat_text(&mut output, "chat", "压缩请求被拒绝").await?;
            }
            "early" => {
                expect_chat_text(&mut output, "chat", "上下文压缩完成").await?;
            }
            _ => {
                assert_running_session(&mut actor, &mut output, &temp, mode).await?;
            }
        }
        assert_returns_to_idle(&mut actor, &mut output, &store, &key, mode).await?;
        assert_signal_files(&temp, mode)?;
        actor.cancel.cancel();
        let (worker, server) = actor.stop().await;
        worker??;
        server??;
        drop(store);
        let reopened = AsyncState::new(JsonStore::open(&state_path)?);
        assert!(!bridge_app::sessions::DurableJournal::claim(&reopened, "compact".into()).await?);
        Ok::<_, Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}

type CompactTemp = tempfile::TempDir;

async fn assert_storage_failure(
    actor: &mut Actor,
    output: &mut MessengerHandles,
    temp: &CompactTemp,
) -> Result<(), Box<dyn Error>> {
    let accepted = submit(
        &actor.input,
        "compact",
        "allowed",
        "chat",
        Some("/compact".into()),
        None,
        Vec::new(),
    )
    .await?;
    assert!(!accepted);
    expect_chat_text(output, "chat", "压缩请求保存失败").await?;
    assert!(!temp.path().join("preparation").exists());
    let (worker, server) = actor.stop().await;
    worker??;
    server??;
    Ok(())
}

async fn assert_prepare_stop(
    actor: &mut Actor,
    output: &mut MessengerHandles,
    temp: &CompactTemp,
) -> Result<(), Box<dyn Error>> {
    while !tokio::fs::try_exists(temp.path().join("preparation")).await? {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    actor.send_text("prepare-stop", "allowed", "/stop").await?;
    expect_chat_text(output, "chat", "已请求停止当前任务").await?;
    actor
        .send_text("release-read", "allowed", "/models")
        .await?;
    expect_chat_text(output, "chat", "准备阶段已停止，未启动压缩").await?;
    assert!(!temp.path().join("compactions").exists());
    Ok(())
}

async fn assert_prepare_failed(
    output: &mut MessengerHandles,
    temp: &CompactTemp,
) -> Result<(), Box<dyn Error>> {
    expect_chat_text(output, "chat", "压缩准备失败").await?;
    assert!(!temp.path().join("compactions").exists());
    Ok(())
}

async fn assert_uncertain(
    actor: &mut Actor,
    output: &mut MessengerHandles,
    temp: &CompactTemp,
) -> Result<(), Box<dyn Error>> {
    expect_chat_text(output, "chat", "压缩启动结果不确定").await?;
    let mut worker = std::mem::replace(
        &mut actor.worker,
        tokio::task::spawn(std::future::ready(Ok(()))),
    );
    assert!((&mut worker).await?.is_err());
    actor.cancel.cancel();
    let mut server = std::mem::replace(
        &mut actor.server,
        tokio::task::spawn(std::future::ready(Ok(()))),
    );
    (&mut server).await??;
    assert_eq!(
        std::fs::read_to_string(temp.path().join("compactions"))?,
        "compact\n"
    );
    Ok(())
}

/// The happy path plus every guard that must refuse interference while the
/// compaction is running.
async fn assert_running_session(
    actor: &mut Actor,
    output: &mut MessengerHandles,
    temp: &CompactTemp,
    mode: &str,
) -> Result<(), Box<dyn Error>> {
    expect_chat_text(output, "chat", "正在等待完成").await?;
    actor.send_text("status", "allowed", "/status").await?;
    let status = expect_chat_text(output, "chat", "上下文压缩中").await?;
    assert!(!status.contains("上下文压缩完成"));
    actor.send_text("busy", "allowed", "/new").await?;
    expect_chat_text(output, "chat", "有任务执行中").await?;
    actor.send_text("again", "allowed", "/compact").await?;
    expect_chat_text(output, "chat", "有任务执行中").await?;
    let accepted = submit(
        &actor.input,
        "blocked-task",
        "allowed",
        "chat",
        Some("must not run".into()),
        None,
        Vec::new(),
    )
    .await?;
    assert!(!accepted);
    expect_chat_text(output, "chat", "任务队列繁忙").await?;
    actor.send_text("other-stop", "other", "/stop").await?;
    expect_chat_text(output, "chat", "没有可停止").await?;
    assert!(!temp.path().join("compact-interrupts").exists());
    if mode == "stop" {
        actor.send_text("stop", "allowed", "/stop").await?;
        expect_chat_text(output, "chat", "上下文压缩已停止").await?;
        assert_eq!(
            std::fs::read_to_string(temp.path().join("compact-interrupts"))?,
            "interrupt\n"
        );
    } else {
        actor.send_text("finish", "allowed", "/models").await?;
        expect_chat_text(
            output,
            "chat",
            if mode == "failed" {
                "上下文压缩失败：fake compact error"
            } else {
                "上下文压缩完成"
            },
        )
        .await?;
    }
    Ok(())
}

async fn assert_returns_to_idle(
    actor: &mut Actor,
    output: &mut MessengerHandles,
    store: &Arc<AsyncState>,
    key: &bridge_core::SessionKey,
    mode: &str,
) -> Result<(), Box<dyn Error>> {
    actor.send_text("compact", "allowed", "/compact").await?;
    actor.send_text("idle", "allowed", "/status").await?;
    expect_chat_text(output, "chat", "空闲").await?;
    assert_eq!(
        store.thread(key.clone()).await?,
        if mode == "empty" {
            None
        } else {
            Some("thread".into())
        }
    );
    Ok(())
}

fn assert_signal_files(temp: &CompactTemp, mode: &str) -> Result<(), Box<dyn Error>> {
    if !matches!(
        mode,
        "empty" | "foreign" | "active" | "wrong_resume" | "prepare_stop"
    ) {
        assert_eq!(
            std::fs::read_to_string(temp.path().join("compactions"))?,
            "compact\n"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("preparation"))?,
            "thread/read\nthread/resume\n"
        );
    }
    Ok(())
}

async fn scenario(fail_archive_commit: bool) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let temp = tempfile::tempdir()?;
        let state_path: PathBuf = temp.path().join("state");
        let store = Arc::new(AsyncState::new(JsonStore::open(&state_path)?));
        let (messenger, mut output) = RecordingMessenger::recorded("", options());
        let actor = Actor::start(
            ActorConfig {
                executable: test_support::fake_codex_runtime!(),
                args: vec!["runtime".into()],
                server_cwd: temp.path().into(),
                epoch: 9,
                root: temp.path().into(),
                directory: temp.path().into(),
                allowed: BTreeSet::from(["allowed".into(), "other-allowed".into()]),
                open_access: false,
                sandbox: Sandbox::WorkspaceWrite,
            },
            store.clone(),
            messenger.clone(),
            bridge_app::diagnostics::Diagnostics::noop(),
        )
        .await?;
        actor.send_text("unauthorized", "stranger", "hello").await?;
        actor.send_text("models", "allowed", "/models").await?;
        expect_chat_text(&mut output, "chat", "/model selected").await?;
        actor
            .send_text("bad-model", "allowed", "/model nonexistent")
            .await?;
        expect_chat_text(&mut output, "chat", "设置失败").await?;
        actor
            .send_text("model", "allowed", "/model selected")
            .await?;
        expect_chat_text(&mut output, "chat", "设置已保存").await?;
        actor.send_text("plan-on", "allowed", "/plan on").await?;
        expect_chat_text(&mut output, "chat", "设置已保存").await?;
        actor.send_text("plan-query", "allowed", "/plan").await?;
        expect_chat_text(&mut output, "chat", "已开启").await?;
        actor.send_text("one", "allowed", "hello").await?;
        let completed = expect_chat_text(&mut output, "chat", "执行完成").await?;
        assert!(completed.contains("authoritative plan"));
        assert!(!completed.contains("fake answer"));
        actor.send_text("two", "allowed", "keep working").await?;
        expect_chat_text(&mut output, "chat", "已开始执行").await?;
        // The fake writes this only after receiving turn/start and flushing its
        // response. Poll a concrete acknowledgement, not an assumed delay.
        while !tokio::fs::try_exists(temp.path().join("started")).await? {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        actor.send_text("busy-new", "allowed", "/new").await?;
        expect_chat_text(&mut output, "chat", "有任务执行中").await?;
        actor.send_text("busy-plan", "allowed", "/plan off").await?;
        expect_chat_text(&mut output, "chat", "有任务执行中").await?;
        actor
            .send_text("busy-resume", "allowed", "/resume thread")
            .await?;
        expect_chat_text(&mut output, "chat", "有任务执行中").await?;
        actor
            .send_text("busy-archive", "allowed", "/archive thread")
            .await?;
        expect_chat_text(&mut output, "chat", "有任务执行中").await?;
        assert_eq!(
            store
                .thread(bridge_core::SessionKey::new("allowed", temp.path()))
                .await?,
            Some("thread".into())
        );
        actor
            .send_text("other-stop", "other-allowed", "/stop")
            .await?;
        expect_chat_text(&mut output, "chat", "没有可停止的当前任务").await?;
        actor.send_text("status", "allowed", "/status").await?;
        assert!(
            !expect_chat_text(&mut output, "chat", "运行中")
                .await?
                .contains("正在停止")
        );
        assert!(!tokio::fs::try_exists(temp.path().join("interrupts.jsonl")).await?);
        actor.send_text("stop", "allowed", "/stop").await?;
        expect_chat_text(&mut output, "chat", "任务已停止").await?;
        actor.send_text("list", "allowed", "/resume").await?;
        let listing = expect_chat_text(&mut output, "chat", "/resume thread").await?;
        assert!(!listing.contains("foreign"));
        actor.send_text("idle-new", "allowed", "/new").await?;
        expect_chat_text(&mut output, "chat", "下次提问时自动创建").await?;
        actor
            .send_text("resume", "allowed", "/resume thread")
            .await?;
        expect_chat_text(&mut output, "chat", "会话已恢复").await?;
        assert_eq!(
            store
                .thread(bridge_core::SessionKey::new("allowed", temp.path()))
                .await?,
            Some("thread".into())
        );
        // A replay cannot restore an old binding over a newer local selection.
        store
            .bind(
                bridge_core::SessionKey::new("allowed", temp.path()),
                "newer".into(),
            )
            .await?;
        actor
            .send_text("resume", "allowed", "/resume thread")
            .await?;
        actor
            .send_text("after-replay", "allowed", "/status")
            .await?;
        expect_chat_text(&mut output, "chat", "空闲").await?;
        assert_eq!(
            store
                .thread(bridge_core::SessionKey::new("allowed", temp.path()))
                .await?,
            Some("newer".into())
        );
        actor.send_text("plan-off", "allowed", "/plan off").await?;
        expect_chat_text(&mut output, "chat", "设置已保存").await?;
        actor.send_text("plan-on", "allowed", "/plan on").await?;
        actor
            .send_text("default-model", "allowed", "/model default")
            .await?;
        expect_chat_text(&mut output, "chat", "设置已保存").await?;
        let preferences = store
            .preferences(bridge_core::SessionKey::new("allowed", temp.path()))
            .await?;
        assert!(
            !preferences.plan,
            "old command replay must not re-enable Plan"
        );
        assert!(preferences.model.is_none());
        let own = bridge_core::SessionKey::new("allowed", temp.path());
        store.bind(own.clone(), "thread".into()).await?;
        if fail_archive_commit {
            let backup = state_path.join("state.previous.json");
            if backup.is_file() {
                std::fs::remove_file(&backup)?;
            }
            std::fs::create_dir(backup)?;
        }
        actor
            .send_text("archive", "allowed", "/archive thread")
            .await?;
        if fail_archive_commit {
            expect_chat_text(&mut output, "chat", "归档状态结果不确定").await?;
            assert!(actor.worker.await?.is_err());
            actor.cancel.cancel();
            actor.server.await??;
            assert_eq!(store.thread(own).await?, Some("thread".into()));
            return Ok::<_, Box<dyn Error>>(());
        }
        expect_chat_text(&mut output, "chat", "会话已归档").await?;
        assert!(store.thread(own.clone()).await?.is_none());
        actor
            .send_text("archived-list", "allowed", "/archived")
            .await?;
        expect_chat_text(&mut output, "chat", "/unarchive thread").await?;
        actor
            .send_text("unarchive", "allowed", "/unarchive thread")
            .await?;
        expect_chat_text(&mut output, "chat", "已取消归档").await?;
        assert!(store.thread(own.clone()).await?.is_none());
        store.bind(own.clone(), "thread".into()).await?;
        actor
            .send_text("archive", "allowed", "/archive thread")
            .await?;
        actor
            .send_text("archive-replay-status", "allowed", "/status")
            .await?;
        expect_chat_text(&mut output, "chat", "空闲").await?;
        assert_eq!(store.thread(own).await?, Some("thread".into()));
        assert_eq!(
            std::fs::read_to_string(temp.path().join("archive-actions"))?,
            "thread/archive\nthread/unarchive\n"
        );
        actor.cancel.cancel();
        actor.worker.await??;
        actor.server.await??;
        let trace = std::fs::read_to_string(temp.path().join("interrupts.jsonl"))?;
        let interrupts: Vec<serde_json::Value> = trace
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        assert_eq!(
            interrupts,
            vec![serde_json::json!({"threadId":"thread","turnId":"2"})]
        );
        Ok::<_, Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}

/// A messenger whose texts also fail, so the card fallback cannot mask the
/// transport failure the diagnostics must expose.
struct SilentMessenger;

impl Messenger for SilentMessenger {
    fn send_text(&self, _: String, _: String) -> DeliveryFuture<'_, ()> {
        Box::pin(async { Err(DeliveryError::Transport) })
    }
    fn send_panel(&self, _: String, _: Panel) -> DeliveryFuture<'_, MessageId> {
        Box::pin(async { Err(DeliveryError::Transport) })
    }
    fn update_panel(&self, _: MessageId, _: Panel) -> DeliveryFuture<'_, ()> {
        Box::pin(async { Err(DeliveryError::Transport) })
    }
    fn upload(&self, _: String, _: String, _: File, _: ResourceKind) -> DeliveryFuture<'_, ()> {
        Box::pin(async { Err(DeliveryError::Transport) })
    }
}

#[tokio::test]
async fn background_delivery_failures_are_observable_through_injected_diagnostics()
-> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let temp = tempfile::tempdir()?;
        let store = Arc::new(AsyncState::new(JsonStore::open(
            &temp.path().join("state"),
        )?));
        let (diagnostics, records) = diagnostics::recorder();
        let actor = Actor::start(
            ActorConfig {
                executable: test_support::fake_codex_runtime!(),
                args: vec!["runtime".into()],
                server_cwd: temp.path().into(),
                epoch: 9,
                root: temp.path().into(),
                directory: temp.path().into(),
                allowed: BTreeSet::from(["allowed".into()]),
                open_access: false,
                sandbox: Sandbox::WorkspaceWrite,
            },
            store,
            Arc::new(SilentMessenger),
            diagnostics,
        )
        .await?;
        // The help panel and its text fallback both fail; both failures must
        // reach the injected diagnostics.
        actor.send_text("help", "allowed", "/help").await?;
        let mut card_failed = false;
        let mut delivery_failed = false;
        for _ in 0..200 {
            {
                let log = diagnostics::records(&records);
                card_failed |= log.iter().any(|r| r.contains("\"event\":\"CardFailed\""));
                delivery_failed |= log
                    .iter()
                    .any(|r| r.contains("\"event\":\"DeliveryFailed\""));
            }
            if card_failed && delivery_failed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        actor.cancel.cancel();
        actor.worker.await.ok();
        actor.server.await.ok();
        assert!(card_failed, "records: {:?}", diagnostics::records(&records));
        assert!(
            delivery_failed,
            "records: {:?}",
            diagnostics::records(&records)
        );
        Ok(())
    })
    .await?
}

#[tokio::test]
async fn separate_runtime_instances_keep_separate_diagnostics() -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let temp = tempfile::tempdir()?;
        let config = |name: &str| -> Result<(PathBuf, Arc<AsyncState>), Box<dyn Error>> {
            let directory = temp.path().join(name);
            std::fs::create_dir_all(&directory)?;
            let store = Arc::new(AsyncState::new(JsonStore::open(&directory.join("state"))?));
            Ok((directory, store))
        };
        let (one_directory, one_store) = config("one")?;
        let (two_directory, two_store) = config("two")?;
        let (first, first_records) = diagnostics::recorder();
        let (second, second_records) = diagnostics::recorder();
        let start = |epoch: u64,
                     directory: PathBuf,
                     store: Arc<AsyncState>,
                     diagnostics: bridge_app::diagnostics::Diagnostics| async move {
            let (messenger, handles) =
                RecordingMessenger::recorded("test", MessengerOptions::new());
            let actor = Actor::start(
                ActorConfig {
                    executable: test_support::fake_codex_runtime!(),
                    args: vec!["runtime".into()],
                    server_cwd: directory.clone(),
                    epoch,
                    root: directory.clone(),
                    directory,
                    allowed: BTreeSet::from(["allowed".into()]),
                    open_access: false,
                    sandbox: Sandbox::WorkspaceWrite,
                },
                store,
                messenger,
                diagnostics,
            )
            .await?;
            Ok::<_, Box<dyn Error>>((actor, handles))
        };
        let (first_actor, _first_handles) = start(1, one_directory, one_store, first).await?;
        let (second_actor, _second_handles) = start(2, two_directory, two_store, second).await?;
        first_actor
            .send_text("task", "allowed", "answer briefly")
            .await?;
        let mut finished = false;
        for _ in 0..200 {
            finished |= diagnostics::records(&first_records)
                .iter()
                .any(|r| r.contains("\"event\":\"TaskFinished\""));
            if finished {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let first_log = diagnostics::records(&first_records);
        assert!(finished, "first: {first_log:?}");
        assert!(diagnostics::records(&second_records).is_empty());
        first_actor.cancel.cancel();
        second_actor.cancel.cancel();
        let _ = first_actor.worker.await;
        let _ = second_actor.worker.await;
        let _ = first_actor.server.await;
        let _ = second_actor.server.await;
        Ok(())
    })
    .await?
}
