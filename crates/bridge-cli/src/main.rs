use bridge_cli::Config;
use bridge_core::command::Command;
use bridge_local::{
    migration::{LegacyPaths, export_legacy, read_legacy},
    state::JsonStore,
};
use clap::{Parser, Subcommand};
use std::{fs, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(
    name = "bridge",
    version,
    about = "Rust 迁移工具；当前生产机器人仍由 start.sh 管理"
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    /// Rust 外层守护：监督器异常退出后清理后代并有界重启。
    Guard {
        #[arg(long)]
        config: PathBuf,
    },
    /// 后台启动、停止、重启或查询监督阶段。
    Service {
        #[arg(long)]
        config: PathBuf,
        #[command(subcommand)]
        command: ServiceAction,
    },
    /// 前台监督 Rust 桥接，异常退出后有界退避重启。
    Supervise {
        #[arg(long)]
        config: PathBuf,
    },
    /// 查询 Rust 运行锁与最后记录的连接状态，不连接飞书。
    Status {
        #[arg(long)]
        config: PathBuf,
    },
    /// 前台运行 Rust 原生飞书桥接。
    Run {
        #[arg(long)]
        config: PathBuf,
    },
    /// 检查显式 TOML 配置；不读取 .env 或连接外部服务。
    Config {
        #[command(subcommand)]
        command: ConfigAction,
    },
    /// 离线迁移与回退，不修改原 Python 文件。
    Migrate {
        #[command(subcommand)]
        command: MigrationAction,
    },
    /// 验证文本命令语法，不执行命令。
    CheckCommand { text: String },
}
#[derive(Subcommand)]
enum ConfigAction {
    /// 生成私有配置，不覆盖文件、不写凭据、不启动服务。
    Init {
        #[arg(long, default_value = "bridge.toml")]
        output: PathBuf,
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        allowed_user: Vec<String>,
        #[arg(long)]
        danger_full_access: bool,
    },
    Check {
        #[arg(long)]
        file: PathBuf,
    },
}
#[derive(Subcommand)]
enum ServiceAction {
    Start,
    Stop,
    Restart,
    Status,
}
#[derive(Subcommand)]
enum MigrationAction {
    ImportPython {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        dry_run: bool,
        #[arg(long, requires = "legacy_cwd")]
        legacy_user: Option<String>,
        #[arg(long, requires = "legacy_user")]
        legacy_cwd: Option<PathBuf>,
        #[arg(long)]
        session_file: Option<PathBuf>,
        #[arg(long)]
        settings_file: Option<PathBuf>,
        #[arg(long)]
        seen_file: Option<PathBuf>,
        #[arg(long)]
        allowed_file: Option<PathBuf>,
    },
    ExportPython {
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
}
fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Action::Guard { config } => {
            let settings = Config::read(&config).map_err(|_| "配置无法读取或 TOML 格式无效")?;
            settings.validate()?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(bridge_cli::supervisor::guard(
                    &config,
                    &settings.workspace.state_dir,
                ))?;
        }
        Action::Service { config, command } => {
            let settings = Config::read(&config).map_err(|_| "配置无法读取或 TOML 格式无效")?;
            if matches!(command, ServiceAction::Start | ServiceAction::Restart) {
                settings.validate()?;
            }
            let state = &settings.workspace.state_dir;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            if matches!(command, ServiceAction::Status) {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&bridge_cli::health::status(state)?)?
                );
                if bridge_cli::supervisor::is_running(state)? {
                    println!(
                        "监督阶段：{}",
                        runtime.block_on(bridge_cli::service_control::request(state, b's'))?
                    );
                }
            } else {
                let _control_lock = bridge_cli::service_control::command_lock(state)?;
                runtime.block_on(async {
                    match command {
                        ServiceAction::Start => {
                            bridge_cli::service_control::start(&config, state).await
                        }
                        ServiceAction::Stop => bridge_cli::service_control::stop(state).await,
                        ServiceAction::Restart => {
                            bridge_cli::service_control::stop(state).await?;
                            bridge_cli::service_control::start(&config, state).await
                        }
                        ServiceAction::Status => Ok(()),
                    }
                })?;
                println!("服务控制完成；连接状态请通过 service status 查询");
            }
        }
        Action::Supervise { config } => {
            let settings = Config::read(&config).map_err(|_| "配置无法读取或 TOML 格式无效")?;
            settings.validate()?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(bridge_cli::supervisor::run(
                    &config,
                    &settings.workspace.state_dir,
                ))?;
        }
        Action::Status { config } => {
            let config = Config::read(&config).map_err(|_| "配置无法读取或 TOML 格式无效")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&bridge_cli::health::status(
                    &config.workspace.state_dir
                )?)?
            );
        }
        Action::Run { config } => {
            let config = Config::read(&config).map_err(|_| "配置无法读取或 TOML 格式无效")?;
            config.validate()?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(bridge_cli::bootstrap::run(config))?;
        }
        Action::Config {
            command:
                ConfigAction::Init {
                    output,
                    root,
                    cwd,
                    state_dir,
                    allowed_user,
                    danger_full_access,
                },
        } => {
            bridge_cli::setup::initialize(
                &output,
                &root,
                &cwd,
                &state_dir,
                allowed_user.into_iter().collect(),
                danger_full_access,
            )?;
            println!(
                "配置已生成。请设置 FEISHU_APP_ID、FEISHU_APP_SECRET；无白名单时还需 FEISHU_PAIRING_CODE。尚未启动服务。"
            );
        }
        Action::Config {
            command: ConfigAction::Check { file },
        } => {
            let config = Config::read(&file).map_err(|_| "配置无法读取或 TOML 格式无效")?;
            config.validate()?;
            println!("配置检查通过（未连接飞书或 Codex；未校验运行时凭据）");
        }
        Action::CheckCommand { text } => {
            Command::parse(&text)?;
            println!("命令语法有效（未执行）");
        }
        Action::Migrate { command } => match command {
            MigrationAction::ImportPython {
                source,
                destination,
                dry_run,
                legacy_user,
                legacy_cwd,
                session_file,
                settings_file,
                seen_file,
                allowed_file,
            } => {
                if !source.is_dir() {
                    return Err("来源目录不存在".into());
                }
                if destination.exists() {
                    return Err("目标目录必须不存在，拒绝覆盖现有状态".into());
                }
                let key = match (legacy_user, legacy_cwd) {
                    (Some(user), Some(cwd)) => {
                        if user.is_empty() || user.contains(':') {
                            return Err("legacy-user 无效".into());
                        }
                        Some(format!("{}:{}", user, fs::canonicalize(cwd)?.display()))
                    }
                    _ => None,
                };
                let mut paths = LegacyPaths::in_directory(&source);
                if let Some(path) = session_file {
                    paths.sessions = path;
                }
                if let Some(path) = settings_file {
                    paths.settings = path;
                }
                if let Some(path) = seen_file {
                    paths.seen = path;
                }
                if let Some(path) = allowed_file {
                    paths.allowed = path;
                }
                if ![
                    &paths.sessions,
                    &paths.settings,
                    &paths.seen,
                    &paths.allowed,
                ]
                .iter()
                .any(|p| p.exists())
                {
                    return Err("未找到任何旧状态文件".into());
                }
                let imported = read_legacy(&paths, key.as_deref())?;
                if imported.seen.iter().any(|id| id.is_empty()) {
                    return Err("旧去重记录包含空消息 ID".into());
                }
                println!(
                    "校验通过：{} 个会话，{} 个模型偏好，{} 个配对用户，{} 条去重记录",
                    imported.state.sessions.len(),
                    imported.state.models.len(),
                    imported.state.allowed_open_ids.len(),
                    imported.seen.len()
                );
                if dry_run {
                    println!("dry-run：未创建或修改文件");
                    return Ok(());
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    fs::DirBuilder::new().mode(0o700).create(&destination)?;
                }
                #[cfg(not(unix))]
                fs::create_dir(&destination)?;
                let mut store = JsonStore::open(&destination)?;
                let marker = destination.join("migration-in-progress");
                fs::write(&marker, b"Incomplete import; do not start service.\n")?;
                fs::File::open(&marker)?.sync_all()?;
                fs::File::open(&destination)?.sync_all()?;
                store.import_seen(&imported.seen)?;
                store.replace(imported.state)?;
                fs::remove_file(marker)?;
                fs::File::open(&destination)?.sync_all()?;
                println!("导入完成；原文件未修改。配置需单独迁移，尚未切换运行服务。");
            }
            MigrationAction::ExportPython { state_dir, output } => {
                if !state_dir.is_dir() {
                    return Err("状态目录不存在".into());
                }
                let store = JsonStore::open(&state_dir)?;
                export_legacy(&output, store.state(), store.seen_messages())?;
                println!("导出完成；请在停止服务后按迁移文档切换。");
            }
        },
    }
    Ok(())
}
fn main() -> ExitCode {
    use bridge_app::diagnostics::{Event, Status, emit};
    std::panic::set_hook(Box::new(|_| {
        // Panic payloads can contain remote data. Record occurrence without it.
        emit(Event::Panic, Status::Failed, None, 0);
    }));
    match run(Cli::parse()) {
        Ok(()) => {
            emit(Event::RuntimeExit, Status::Ok, None, 0);
            ExitCode::SUCCESS
        }
        Err(error) => {
            emit(Event::RuntimeExit, Status::Failed, None, 0);
            eprintln!("错误：{error}");
            ExitCode::FAILURE
        }
    }
}
