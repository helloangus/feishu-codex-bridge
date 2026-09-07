//! Foreground MVP composition. No shell config execution or automatic replay.
use crate::{AccessMode, Config, Sandbox};
use bridge_app::{ports, runtime};
use bridge_codex::process::AppServer;
use bridge_feishu::{ingress::Event, rest::FeishuRest, sidecar};
use bridge_local::{async_state::AsyncState, state::JsonStore};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command, sync::mpsc, time::timeout};
use tokio_util::sync::CancellationToken;

fn credential(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err("凭据环境变量名称无效".into());
    }
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "缺少配置指定的凭据环境变量".into())
}

/// Same directory/hash naming as service.py, so Python and Rust exclude each other.
pub fn app_lock(directory: &Path, app_id: &str) -> Result<File, Box<dyn std::error::Error>> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let digest = format!("{:x}", Sha256::digest(app_id.as_bytes()));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(directory.join(format!("{}.lock", &digest[..24])))?;
    lock.try_lock_exclusive()
        .map_err(|_| "同一飞书 App ID 已有运行实例")?;
    Ok(lock)
}

pub async fn run(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let feishu = config.feishu.as_ref().ok_or("run 需要 [feishu] 配置")?;
    if !feishu.adapter.is_absolute() || !feishu.adapter.is_file() {
        return Err("adapter 必须是存在的绝对文件路径".into());
    }
    let app_id = credential(&feishu.app_id_env)?;
    let secret = credential(&feishu.app_secret_env)?;
    let lock_dir = match std::env::var_os("CODEX_SERVICE_GLOBAL_STATE") {
        Some(value) => PathBuf::from(value),
        None => PathBuf::from(std::env::var_os("HOME").ok_or("缺少 HOME，无法取得全局锁位置")?)
            .join(".feishu-codex-bridge"),
    };
    let _lock = app_lock(&lock_dir, &app_id)?;
    let json = JsonStore::open(&config.workspace.state_dir)?;
    let mut allowed = config.access.allowed_open_ids.clone();
    allowed.extend(json.state().allowed_open_ids.iter().cloned());
    if config.access.mode == AccessMode::Restricted && allowed.is_empty() {
        return Err("最小版暂不提供配对命令，需要配置白名单或导入已配对用户".into());
    }
    let store = Arc::new(AsyncState::new(json));
    let proxy = feishu
        .proxy_env
        .as_ref()
        .and_then(|name| std::env::var(name).ok())
        .filter(|v| !v.is_empty());
    let messenger = Arc::new(
        FeishuRest::new(
            app_id.clone(),
            secret.clone(),
            proxy.as_deref(),
            20 * 1024 * 1024,
        )
        .map_err(|_| "飞书 REST 配置无效")?,
    );
    let epoch = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())
        .unwrap_or_else(|_| u64::from(std::process::id()));
    let directory = fs::canonicalize(&config.workspace.cwd)?;
    let mut server = AppServer::spawn(
        &config.codex.executable,
        &config.codex.args,
        &directory,
        epoch,
    )
    .await?;
    let backend = Arc::new(server.backend());
    let mut command = Command::new(&feishu.python);
    command
        .arg("-u")
        .arg(&feishu.adapter)
        .current_dir(feishu.adapter.parent().ok_or("adapter 目录无效")?)
        .env("FEISHU_APP_ID", app_id)
        .env("FEISHU_APP_SECRET", secret)
        .env("BRIDGE_CONNECTION_EPOCH", epoch.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if let Some(proxy) = proxy {
        command.env("FEISHU_PROXY_URL", proxy);
    } else {
        command.env_remove("FEISHU_PROXY_URL");
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            server.shutdown().await?;
            return Err("无法启动飞书 SDK 进程".into());
        }
    };
    let read = child.stdout.take().ok_or("SDK stdout 不可用")?;
    let write = child.stdin.take().ok_or("SDK stdin 不可用")?;
    let cancel = CancellationToken::new();
    let (incoming_tx, mut incoming_rx) = mpsc::channel(128);
    let (input_tx, input_rx) = mpsc::channel(64);
    let (event_tx, event_rx) = mpsc::channel(256);
    let stop = cancel.clone();
    let ipc = tokio::spawn(async move {
        let result = sidecar::pump(read, write, epoch.to_string(), incoming_tx, stop.clone()).await;
        stop.cancel();
        result
    });
    let stop = cancel.clone();
    let gateway = tokio::spawn(async move {
        while let Some(received) = incoming_rx.recv().await {
            let (id, user, chat, text) = match received.event {
                Event::Connection { state } => {
                    eprintln!("{{\"event\":\"feishu_connection\",\"state\":\"{state:?}\"}}");
                    continue;
                }
                Event::Message {
                    message_id,
                    user_id,
                    chat_id,
                    message_type,
                    content,
                    ..
                } => {
                    let text = if message_type == "text" {
                        content
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned)
                    } else {
                        None
                    };
                    (message_id, user_id, chat_id, text)
                }
                Event::Card {
                    message_id,
                    user_id,
                    chat_id,
                    ..
                } => (format!("card:{message_id}"), user_id, chat_id, None),
            };
            let accept = Box::new(move |accepted| {
                if let Some(receipt) = received.acceptance {
                    receipt.complete(accepted);
                }
            });
            if input_tx
                .try_send(runtime::Input {
                    id,
                    user,
                    chat,
                    text,
                    accept,
                })
                .is_err()
            {
                eprintln!("{{\"event\":\"input_overloaded\"}}");
            }
        }
        stop.cancel();
    });
    let stop = cancel.clone();
    let agent = tokio::spawn(async move {
        let mut unhealthy = false;
        loop {
            let event = tokio::select! {_=stop.cancelled()=>break,event=server.next_event()=>event};
            let failed = event.is_err();
            if event_tx.try_send(event).is_err() || failed {
                unhealthy = true;
                stop.cancel();
                break;
            }
        }
        server.shutdown().await?;
        if unhealthy {
            Err(std::io::Error::other("Codex event connection failed"))
        } else {
            Ok(())
        }
    });
    let stop = cancel.clone();
    let signal = tokio::spawn(async move {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
        stop.cancel();
    });
    eprintln!("{{\"event\":\"rust_runtime_started\"}}");
    let result = runtime::run(
        runtime::Settings {
            directory,
            allowed,
            open_access: config.access.mode == AccessMode::Open,
            sandbox: match config.codex.sandbox {
                Sandbox::WorkspaceWrite => ports::Sandbox::WorkspaceWrite,
                Sandbox::DangerFullAccess => ports::Sandbox::DangerFullAccess,
            },
            epoch,
        },
        backend,
        store,
        messenger,
        input_rx,
        event_rx,
        cancel.clone(),
    )
    .await;
    cancel.cancel();
    let ipc_result = ipc.await;
    let gateway_result = gateway.await;
    let agent_result = agent.await;
    if timeout(Duration::from_secs(2), child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
    signal.abort();
    let _ = signal.await;
    result?;
    if !matches!(ipc_result, Ok(Ok(())))
        || gateway_result.is_err()
        || !matches!(agent_result, Ok(Ok(())))
    {
        return Err("连接或后台任务异常退出；未自动重启任务".into());
    }
    Ok(())
}
