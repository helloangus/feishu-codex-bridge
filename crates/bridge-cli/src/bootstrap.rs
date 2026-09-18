//! Foreground composition of the native bridge: Feishu ingress, Codex app
//! server, durable state and the application runtime.
use crate::{AccessMode, Config, credentials::valid_pairing_code, valid_env_name};
use bridge_app::runtime;
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
    if !valid_env_name(name) {
        return Err("凭据环境变量名称无效".into());
    }
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "缺少配置指定的凭据环境变量".into())
}

fn optional_credential(name: &str) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if !valid_env_name(name) {
        return Err("凭据环境变量名称无效".into());
    }
    Ok(std::env::var(name).ok().filter(|value| !value.is_empty()))
}

/// Stable App ID-derived lock shared by every native service entry point.
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
    let fail_early = |stage: &'static str, error: &'static str| -> &'static str {
        bridge_app::diagnostics::startup_record(stage);
        error
    };
    let feishu = config
        .feishu
        .as_ref()
        .ok_or_else(|| fail_early("config", "run 需要 [feishu] 配置"))?;
    let app_id = credential(&feishu.app_id_env)
        .map_err(|_| fail_early("credentials", "凭据环境变量不可用"))?;
    let secret = credential(&feishu.app_secret_env)
        .map_err(|_| fail_early("credentials", "凭据环境变量不可用"))?;
    let lock_dir = match std::env::var_os("CODEX_SERVICE_GLOBAL_STATE") {
        Some(value) => PathBuf::from(value),
        None => PathBuf::from(
            std::env::var_os("HOME")
                .ok_or_else(|| fail_early("lock", "缺少 HOME，无法取得全局锁位置"))?,
        )
        .join(".feishu-codex-bridge"),
    };
    let _lock = app_lock(&lock_dir, &app_id).inspect_err(|_| {
        bridge_app::diagnostics::startup_record("lock");
    })?;
    let health = Arc::new(std::sync::Mutex::new(
        crate::health::Health::start(&config.workspace.state_dir).inspect_err(|_| {
            bridge_app::diagnostics::startup_record("health");
        })?,
    ));
    let diagnostics = bridge_app::diagnostics::Diagnostics::new(Box::new(
        crate::logging::Log::open(&config.workspace.state_dir.join("runtime")).inspect_err(
            |_| {
                bridge_app::diagnostics::startup_record("diagnostics");
            },
        )?,
    ));
    std::panic::set_hook({
        let diagnostics = diagnostics.clone();
        // Panic payloads can contain remote data. Record occurrence without it.
        Box::new(move |_| {
            diagnostics.emit(
                bridge_app::diagnostics::Event::Panic,
                bridge_app::diagnostics::Status::Failed,
                None,
                0,
            )
        })
    });
    diagnostics.emit(
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
        .map(optional_credential)
        .transpose()?;
    let pairing_code = pairing_code.flatten();
    if pairing_code
        .as_ref()
        .is_some_and(|code| !valid_pairing_code(code))
    {
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
    let connection = websocket::Client::new(app_id, secret, proxy.as_deref(), diagnostics.clone())?;
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
        Ok(Ok(removed)) => diagnostics.emit(
            bridge_app::diagnostics::Event::ArchiveReconciled,
            bridge_app::diagnostics::Status::Ok,
            None,
            removed,
        ),
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
    let heartbeat = tokio::spawn(crate::health::heartbeat(
        health.clone(),
        cancel.clone(),
        diagnostics.clone(),
    ));
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
    let gateway_diagnostics = diagnostics.clone();
    let gateway = tokio::spawn(async move {
        let diagnostics = &gateway_diagnostics;
        let mut healthy = true;
        while let Some(received) = incoming_rx.recv().await {
            // Feishu payloads become application inputs inside the ingress;
            // this loop only routes lifecycle state and bounded admission.
            let connection = match received.event {
                Event::Connection { state } => state,
                _ => {
                    if let Some(input) = received.into_runtime_input() {
                        if input_tx.try_send(input).is_err() {
                            diagnostics.emit(
                                bridge_app::diagnostics::Event::Overloaded,
                                bridge_app::diagnostics::Status::Overloaded,
                                None,
                                0,
                            );
                        }
                    }
                    continue;
                }
            };
            diagnostics.emit(
                match connection {
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
            let phase = match connection {
                bridge_feishu::ingress::ConnectionState::Starting => crate::health::Phase::Starting,
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
                diagnostics.emit(
                    bridge_app::diagnostics::Event::HealthWriteFailed,
                    bridge_app::diagnostics::Status::Failed,
                    None,
                    0,
                );
                healthy = false;
                stop.cancel();
                break;
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
    let files = Arc::new(
        bridge_app::files::Deliveries::new(
            diagnostics.clone(),
            Arc::new(bridge_local::workspace_files::WorkspaceFiles),
            messenger.clone(),
            messenger.clone(),
        )
        .excluding(vec![config.workspace.state_dir.clone()])
        .generated_images(
            std::env::var_os("CODEX_GENERATED_IMAGES")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("CODEX_HOME")
                        .map(PathBuf::from)
                        .or_else(|| {
                            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex"))
                        })
                        .map(|home| home.join("generated_images"))
                }),
        ),
    );
    let result = runtime::run(
        runtime::Settings {
            root,
            directory,
            allowed,
            open_access: config.access.mode == AccessMode::Open,
            sandbox: config.codex.sandbox.into(),
            epoch,
        },
        diagnostics.clone(),
        backend,
        store,
        messenger,
        files,
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
        diagnostics.emit(
            bridge_app::diagnostics::Event::RuntimeExit,
            bridge_app::diagnostics::Status::Failed,
            None,
            0,
        );
        return Err("连接或后台任务异常退出；未自动重启任务".into());
    }
    diagnostics.emit(
        bridge_app::diagnostics::Event::RuntimeExit,
        bridge_app::diagnostics::Status::Ok,
        None,
        0,
    );
    Ok(())
}
