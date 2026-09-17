//! Full runtime actor with fake stdio app-server, real store and fake delivery.
use bridge_app::{ports::Sandbox, sessions::SessionStore};
use bridge_local::{async_state::AsyncState, state::JsonStore};
use std::{collections::BTreeSet, error::Error, path::PathBuf, sync::Arc, time::Duration};
use test_support::{
    actor::{Actor, ActorConfig},
    input::submit,
    messenger::{MessengerOptions, RecordingMessenger, expect_chat_text},
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
            std::fs::create_dir(state_path.join("seen-messages.json"))?;
        }
        let (messenger, mut output) = RecordingMessenger::recorded("", options());
        let actor = Actor::start(
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
        )
        .await?;
        if mode == "storage" {
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
            expect_chat_text(&mut output, "chat", "压缩请求保存失败").await?;
            assert!(!temp.path().join("preparation").exists());
            actor.cancel.cancel();
            actor.worker.await??;
            actor.server.await??;
            return Ok(());
        }
        actor.send_text("compact", "allowed", "/compact").await?;
        if mode == "prepare_stop" {
            while !tokio::fs::try_exists(temp.path().join("preparation")).await? {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            actor.send_text("prepare-stop", "allowed", "/stop").await?;
            expect_chat_text(&mut output, "chat", "已请求停止当前任务").await?;
            actor
                .send_text("release-read", "allowed", "/models")
                .await?;
            expect_chat_text(&mut output, "chat", "准备阶段已停止，未启动压缩").await?;
            assert!(!temp.path().join("compactions").exists());
        } else if matches!(mode, "empty" | "foreign" | "active" | "wrong_resume") {
            expect_chat_text(&mut output, "chat", "压缩准备失败").await?;
            assert!(!temp.path().join("compactions").exists());
        } else if mode == "uncertain" {
            expect_chat_text(&mut output, "chat", "压缩启动结果不确定").await?;
            assert!(actor.worker.await?.is_err());
            actor.cancel.cancel();
            actor.server.await??;
            assert_eq!(
                std::fs::read_to_string(temp.path().join("compactions"))?,
                "compact\n"
            );
            return Ok(());
        } else if mode == "rejected" {
            expect_chat_text(&mut output, "chat", "压缩请求被拒绝").await?;
        } else if mode == "early" {
            expect_chat_text(&mut output, "chat", "上下文压缩完成").await?;
        } else {
            expect_chat_text(&mut output, "chat", "正在等待完成").await?;
            actor.send_text("status", "allowed", "/status").await?;
            let status = expect_chat_text(&mut output, "chat", "上下文压缩中").await?;
            assert!(!status.contains("上下文压缩完成"));
            actor.send_text("busy", "allowed", "/new").await?;
            expect_chat_text(&mut output, "chat", "有任务执行中").await?;
            actor.send_text("again", "allowed", "/compact").await?;
            expect_chat_text(&mut output, "chat", "有任务执行中").await?;
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
            expect_chat_text(&mut output, "chat", "任务队列繁忙").await?;
            actor.send_text("other-stop", "other", "/stop").await?;
            expect_chat_text(&mut output, "chat", "没有可停止").await?;
            assert!(!temp.path().join("compact-interrupts").exists());
            if mode == "stop" {
                actor.send_text("stop", "allowed", "/stop").await?;
                expect_chat_text(&mut output, "chat", "上下文压缩已停止").await?;
                assert_eq!(
                    std::fs::read_to_string(temp.path().join("compact-interrupts"))?,
                    "interrupt\n"
                );
            } else {
                actor.send_text("finish", "allowed", "/models").await?;
                expect_chat_text(
                    &mut output,
                    "chat",
                    if mode == "failed" {
                        "上下文压缩失败：fake compact error"
                    } else {
                        "上下文压缩完成"
                    },
                )
                .await?;
            }
        }
        actor.send_text("compact", "allowed", "/compact").await?;
        actor.send_text("idle", "allowed", "/status").await?;
        expect_chat_text(&mut output, "chat", "空闲").await?;
        assert_eq!(
            store.thread(key).await?,
            if mode == "empty" {
                None
            } else {
                Some("thread".into())
            }
        );
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
        actor.cancel.cancel();
        actor.worker.await??;
        actor.server.await??;
        drop(store);
        let reopened = AsyncState::new(JsonStore::open(&state_path)?);
        assert!(!bridge_app::sessions::DurableJournal::claim(&reopened, "compact".into()).await?);
        Ok::<_, Box<dyn Error>>(())
    })
    .await??;
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
