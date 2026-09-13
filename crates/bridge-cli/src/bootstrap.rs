//! Foreground MVP composition. No shell config execution or automatic replay.
use crate::{AccessMode, Config, Sandbox};
use bridge_app::{ports, runtime};
use bridge_codex::process::AppServer;
use bridge_feishu::{ingress::Event, rest::FeishuRest, websocket};
use bridge_local::{async_state::AsyncState, state::JsonStore};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;
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

/// Only locally registered opaque actions enter the runtime; raw card commands
/// cannot bypass message/owner validation or become ordinary chat text.
pub fn decode_card_click(
    source: String,
    action: &serde_json::Value,
) -> Option<bridge_app::cards::Click> {
    use bridge_core::view::ButtonAction;
    match bridge_feishu::decode_action(&action.to_string()).ok()? {
        ButtonAction::Interaction { token, choice } if choice == "run" && !source.is_empty() => {
            Some(bridge_app::cards::Click { token, source })
        }
        _ => None,
    }
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
    let app_id = credential(&feishu.app_id_env)?;
    let secret = credential(&feishu.app_secret_env)?;
    let lock_dir = match std::env::var_os("CODEX_SERVICE_GLOBAL_STATE") {
        Some(value) => PathBuf::from(value),
        None => PathBuf::from(std::env::var_os("HOME").ok_or("缺少 HOME，无法取得全局锁位置")?)
            .join(".feishu-codex-bridge"),
    };
    let _lock = app_lock(&lock_dir, &app_id)?;
    let health = Arc::new(std::sync::Mutex::new(crate::health::Health::start(
        &config.workspace.state_dir,
    )?));
    bridge_app::diagnostics::install(Box::new(crate::logging::Log::open(
        &config.workspace.state_dir.join("runtime"),
    )?))
    .map_err(|_| "日志输出已经初始化，拒绝重复运行")?;
    bridge_app::diagnostics::emit(
        bridge_app::diagnostics::Event::RuntimeStarted,
        bridge_app::diagnostics::Status::Ok,
        None,
        0,
    );
    let json = JsonStore::open(&config.workspace.state_dir)?;
    let mut allowed = config.access.allowed_open_ids.clone();
    allowed.extend(json.state().allowed_open_ids.iter().cloned());
    let pairing_code = config
        .access
        .pairing_code_env
        .as_deref()
        .map(credential)
        .transpose()?;
    if pairing_code.as_ref().is_some_and(|code| {
        code.len() < 16 || code.len() > 256 || code.chars().any(char::is_whitespace)
    }) {
        return Err("配对码需为 16–256 字节且不含空白，请修改指定的环境变量".into());
    }
    if config.access.mode == AccessMode::Restricted && allowed.is_empty() && pairing_code.is_none()
    {
        return Err("restricted 模式需要白名单、已配对用户或配对码".into());
    }
    let store = Arc::new(AsyncState::new(json).with_pairing(pairing_code));
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
    let connection = websocket::Client::new(app_id, secret, proxy.as_deref())?;
    let root = fs::canonicalize(&config.workspace.root)?;
    let mut server = AppServer::spawn(
        &config.codex.executable,
        &config.codex.args,
        &directory,
        epoch,
    )
    .await?;
    let backend = Arc::new(server.backend());
    // Reconcile before Feishu connects or any business task is admitted. A failed
    // scan leaves bindings untouched; a failed commit prevents runtime startup.
    let reconciliation = tokio::time::timeout(Duration::from_secs(60), async {
        let bindings = store
            .bound_threads()
            .await
            .map_err(|_| "无法读取待核对会话绑定")?;
        let archived = backend
            .archived_bindings(&bindings)
            .await
            .map_err(|_| "归档状态核对失败，未启动业务接收")?;
        store
            .clear_archived_bindings(archived)
            .await
            .map_err(|_| "归档绑定修复保存失败，未启动业务接收")
    })
    .await;
    match reconciliation {
        Ok(Ok(removed)) => {
            eprintln!("{{\"event\":\"archive_reconciled\",\"removed_bindings\":{removed}}}")
        }
        failure => {
            let _ = server.shutdown().await;
            return Err(match failure {
                Ok(Err(message)) => message,
                _ => "归档状态核对超时，未启动业务接收",
            }
            .into());
        }
    }
    let cancel = CancellationToken::new();
    let heartbeat = tokio::spawn(crate::health::heartbeat(health.clone(), cancel.clone()));
    let (incoming_tx, mut incoming_rx) = mpsc::channel(128);
    let (input_tx, input_rx) = mpsc::channel(64);
    let (event_tx, event_rx) = mpsc::channel(256);
    let stop = cancel.clone();
    let transport = tokio::spawn(async move {
        let result = connection.run(incoming_tx, stop.clone()).await;
        stop.cancel();
        result
    });
    let stop = cancel.clone();
    let connection_health = health.clone();
    let gateway = tokio::spawn(async move {
        let mut healthy = true;
        while let Some(received) = incoming_rx.recv().await {
            let (id, user, chat, text, card, attachments) = match received.event {
                Event::Connection { state } => {
                    eprintln!("{{\"event\":\"feishu_connection\",\"state\":\"{state:?}\"}}");
                    bridge_app::diagnostics::emit(
                        match state {
                            bridge_feishu::ingress::ConnectionState::Starting => {
                                bridge_app::diagnostics::Event::ConnectionStarting
                            }
                            bridge_feishu::ingress::ConnectionState::Connected => {
                                bridge_app::diagnostics::Event::ConnectionEstablished
                            }
                            bridge_feishu::ingress::ConnectionState::Reconnecting => {
                                bridge_app::diagnostics::Event::ConnectionReconnecting
                            }
                        },
                        bridge_app::diagnostics::Status::Ok,
                        None,
                        0,
                    );
                    let phase = match state {
                        bridge_feishu::ingress::ConnectionState::Starting => {
                            crate::health::Phase::Starting
                        }
                        bridge_feishu::ingress::ConnectionState::Connected => {
                            crate::health::Phase::Connected
                        }
                        bridge_feishu::ingress::ConnectionState::Reconnecting => {
                            crate::health::Phase::Reconnecting
                        }
                    };
                    if connection_health
                        .lock()
                        .map_err(|_| ())
                        .and_then(|mut h| h.set(phase).map_err(|_| ()))
                        .is_err()
                    {
                        eprintln!("{{\"event\":\"health_write_failed\"}}");
                        healthy = false;
                        stop.cancel();
                        break;
                    }
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
                    let text = bridge_feishu::message_text(&message_type, &content);
                    let attachments =
                        bridge_feishu::attachments(&message_id, &message_type, &content);
                    (message_id, user_id, chat_id, text, None, attachments)
                }
                Event::Card {
                    message_id,
                    user_id,
                    chat_id,
                    action,
                } => {
                    let card = decode_card_click(message_id.clone(), &action);
                    (
                        format!("card:{message_id}"),
                        user_id,
                        chat_id,
                        None,
                        card,
                        vec![],
                    )
                }
            };
            let accept = Box::new(move |accepted| {
                if let Some(receipt) = received.acceptance {
                    receipt.complete(accepted);
                }
            });
            if input_tx
                .try_send(runtime::Input {
                    attachments,
                    card,
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
        healthy
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
            root,
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
        Arc::new(
            bridge_local::delivery::Delivery::new(messenger.clone(), messenger)
                .excluding(vec![config.workspace.state_dir.clone()])
                .generated_images(
                    std::env::var_os("CODEX_GENERATED_IMAGES")
                        .map(PathBuf::from)
                        .or_else(|| {
                            std::env::var_os("CODEX_HOME")
                                .map(PathBuf::from)
                                .or_else(|| {
                                    std::env::var_os("HOME")
                                        .map(|home| PathBuf::from(home).join(".codex"))
                                })
                                .map(|home| home.join("generated_images"))
                        }),
                ),
        ),
        input_rx,
        event_rx,
        cancel.clone(),
    )
    .await;
    cancel.cancel();
    let transport_result = transport.await;
    let gateway_result = gateway.await;
    let agent_result = agent.await;
    let heartbeat_result = heartbeat.await;
    signal.abort();
    let _ = signal.await;
    health.lock().map_err(|_| "健康状态锁异常")?.finish(
        result.is_ok()
            && matches!(&transport_result, Ok(Ok(())))
            && matches!(&gateway_result, Ok(true))
            && matches!(&heartbeat_result, Ok(Ok(())))
            && matches!(&agent_result, Ok(Ok(()))),
    )?;
    result?;
    if !matches!(transport_result, Ok(Ok(())))
        || !matches!(gateway_result, Ok(true))
        || !matches!(heartbeat_result, Ok(Ok(())))
        || !matches!(agent_result, Ok(Ok(())))
    {
        return Err("连接或后台任务异常退出；未自动重启任务".into());
    }
    Ok(())
}
