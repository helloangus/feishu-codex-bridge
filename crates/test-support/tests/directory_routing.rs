//! Runtime routing turns in the persisted per-user directory, offline.
use bridge_app::{directories::DirectoryStore, ports::Sandbox};
use bridge_local::{async_state::AsyncState, state::JsonStore};
use std::{collections::BTreeSet, error::Error, path::PathBuf, sync::Arc, time::Duration};
use test_support::{
    actor::{Actor, ActorConfig},
    input::submit,
    messenger::{MessengerOptions, RecordingMessenger, expect_text},
};

fn options() -> MessengerOptions {
    MessengerOptions {
        panels_fail: true,
        upload_panics: true,
        ..MessengerOptions::new()
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
    tokio::time::timeout(Duration::from_secs(20), async {
        let temp = tempfile::tempdir()?;
        let state_path: PathBuf = temp.path().join("state");
        let root = temp.path().join("root");
        std::fs::create_dir(&root)?;
        let child = root.join("child space");
        std::fs::create_dir(&child)?;
        let mut restart_confirmation = String::new();
        for round in 0..2 {
            let store = Arc::new(AsyncState::new(JsonStore::open(&state_path)?));
            let (messenger, mut output) = RecordingMessenger::recorded("", options());
            let actor = Actor::start(
                ActorConfig {
                    executable: test_support::fake_codex_runtime!(),
                    args: vec!["directory".into()],
                    server_cwd: root.clone(),
                    epoch: 27,
                    root: root.clone(),
                    directory: root.clone(),
                    allowed: BTreeSet::from(["owner".into(), "other".into()]),
                    open_access: false,
                    sandbox: Sandbox::WorkspaceWrite,
                },
                store.clone(),
                messenger.clone(),
            )
            .await?;
            if round == 0 {
                actor.send_text("browse", "owner", "/cd").await?;
                assert!(
                    expect_text(&mut output, "当前目录")
                        .await?
                        .1
                        .contains("/cd child space")
                );
                if fail_directory_save {
                    std::fs::create_dir(state_path.join("state.json"))?;
                    actor
                        .send_text("failed-switch", "owner", "/cd child space")
                        .await?;
                    expect_text(&mut output, "切换目录失败").await?;
                    actor.send_text("unchanged", "owner", "/status").await?;
                    assert!(
                        expect_text(&mut output, "当前目录")
                            .await?
                            .1
                            .contains(&format!("当前目录：{}\n", root.display()))
                    );
                    assert!(store.directory_preferences().await?.is_empty());
                    actor.cancel.cancel();
                    actor.worker.await??;
                    actor.server.await??;
                    return Ok(());
                }
                actor
                    .send_text("propose", "owner", "/cd created/nested")
                    .await?;
                let command = confirmation(&expect_text(&mut output, "目录不存在").await?.1)?;
                assert!(!root.join("created").exists());
                actor.send_text("wrong-user", "other", &command).await?;
                expect_text(&mut output, "创建确认无效").await?;
                let accepted = submit(
                    &actor.input,
                    "wrong-chat",
                    "owner",
                    "different-chat",
                    Some(command.clone()),
                    None,
                    Vec::new(),
                )
                .await?;
                assert!(accepted);
                expect_text(&mut output, "创建确认无效").await?;
                assert!(!root.join("created").exists());
                actor.send_text("confirm", "owner", &command).await?;
                expect_text(&mut output, "目录已创建并切换").await?;
                assert!(root.join("created/nested").is_dir());
                actor.send_text("confirm-repeat", "owner", &command).await?;
                expect_text(&mut output, "创建确认无效").await?;
                actor
                    .send_text("return-root", "owner", &format!("/cd {}", root.display()))
                    .await?;
                expect_text(&mut output, "已切换目录").await?;
                actor
                    .send_text("old-proposal", "owner", "/cd abandoned")
                    .await?;
                let abandoned = confirmation(&expect_text(&mut output, "目录不存在").await?.1)?;
                actor
                    .send_text("switch", "owner", "/cd child space")
                    .await?;
                expect_text(&mut output, "已切换目录").await?;
                actor.send_text("old-confirm", "owner", &abandoned).await?;
                expect_text(&mut output, "创建确认无效").await?;
                assert!(!root.join("abandoned").exists());
                actor.send_text("model", "owner", "/model chosen").await?;
                expect_text(&mut output, "设置已保存").await?;
                actor.send_text("plan", "owner", "/plan on").await?;
                expect_text(&mut output, "设置已保存").await?;
                actor.send_text("child-turn", "owner", "hello").await?;
                assert!(
                    expect_text(&mut output, "执行完成")
                        .await?
                        .1
                        .contains(&format!("{}|chosen|plan", child.display()))
                );
                actor.send_text("parent", "owner", "/cd ..").await?;
                expect_text(&mut output, "已切换目录").await?;
                actor.send_text("root-turn", "owner", "hello").await?;
                assert!(
                    expect_text(&mut output, "执行完成")
                        .await?
                        .1
                        .contains(&format!("{}|gpt-5.6-luna|default", root.display()))
                );
                actor
                    .send_text("switch", "owner", "/cd child space")
                    .await?;
                actor.send_text("dedup-status", "owner", "/status").await?;
                assert!(
                    expect_text(&mut output, "当前目录")
                        .await?
                        .1
                        .contains(&format!("当前目录：{}\n", root.display()))
                );
                actor
                    .send_text("back", "owner", &format!("/cd {}", child.display()))
                    .await?;
                expect_text(&mut output, "已切换目录").await?;
                actor
                    .send_text("busy-proposal", "owner", "/cd after-task")
                    .await?;
                let after_task = confirmation(&expect_text(&mut output, "目录不存在").await?.1)?;
                actor.send_text("hold", "owner", "hold").await?;
                expect_text(&mut output, "已开始执行").await?;
                while !tokio::fs::try_exists(root.join("holding")).await? {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                actor.send_text("busy-cd", "owner", "/cd ..").await?;
                expect_text(&mut output, "有任务执行中").await?;
                actor
                    .send_text("busy-confirm", "owner", &after_task)
                    .await?;
                expect_text(&mut output, "有任务执行中").await?;
                actor
                    .send_text("queued", "owner", "must be cancelled")
                    .await?;
                expect_text(&mut output, "请求已接收").await?;
                actor
                    .send_text("other-cd", "other", "/cd child space")
                    .await?;
                expect_text(&mut output, "有任务执行中").await?;
                actor.send_text("stop", "owner", "/stop").await?;
                expect_text(&mut output, "任务已停止").await?;
                actor
                    .send_text("idle-confirm", "owner", &after_task)
                    .await?;
                expect_text(&mut output, "目录已创建并切换").await?;
                actor
                    .send_text("return-child", "owner", &format!("/cd {}", child.display()))
                    .await?;
                expect_text(&mut output, "已切换目录").await?;
                actor
                    .send_text("restart-proposal", "owner", "/cd never-created")
                    .await?;
                restart_confirmation =
                    confirmation(&expect_text(&mut output, "目录不存在").await?.1)?;
            } else {
                actor
                    .send_text("restart-confirm", "owner", &restart_confirmation)
                    .await?;
                expect_text(&mut output, "创建确认无效").await?;
                assert!(!child.join("never-created").exists());
                actor.send_text("parent", "owner", "/cd ..").await?;
                actor
                    .send_text("restart-status", "owner", "/status")
                    .await?;
                assert!(
                    expect_text(&mut output, "当前目录")
                        .await?
                        .1
                        .contains(&format!("当前目录：{}\n", child.display()))
                );
                actor.send_text("restart-turn", "owner", "hello").await?;
                assert!(
                    expect_text(&mut output, "执行完成")
                        .await?
                        .1
                        .contains(&format!("{}|chosen|plan", child.display()))
                );
                actor.send_text("other-turn", "other", "hello").await?;
                assert!(
                    expect_text(&mut output, "执行完成")
                        .await?
                        .1
                        .contains(&format!("{}|gpt-5.6-luna|default", root.display()))
                );
                #[cfg(unix)]
                {
                    std::fs::rename(&child, root.join("moved-child"))?;
                    std::os::unix::fs::symlink(temp.path(), &child)?;
                    actor
                        .send_text("redirected", "owner", "must not execute outside root")
                        .await?;
                    expect_text(&mut output, "当前目录已失效或越出工作区").await?;
                    actor
                        .send_text("redirected-compact", "owner", "/compact")
                        .await?;
                    expect_text(&mut output, "压缩准备失败").await?;
                    actor
                        .send_text("recover", "owner", &format!("/cd {}", root.display()))
                        .await?;
                    expect_text(&mut output, "已切换目录").await?;
                }
            }
            actor.cancel.cancel();
            actor.worker.await??;
            actor.server.await??;
        }
        Ok::<_, Box<dyn Error>>(())
    })
    .await??;
    Ok(())
}
