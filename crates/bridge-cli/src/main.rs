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
    /// 前台运行最小版（授权文本、状态、停止；需要 Python SDK 薄进程）。
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
    Check {
        #[arg(long)]
        file: PathBuf,
    },
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
        Action::Run { config } => {
            let config = Config::read(&config).map_err(|_| "配置无法读取或 TOML 格式无效")?;
            config.validate()?;
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(bridge_cli::bootstrap::run(config))?;
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
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("错误：{error}");
            ExitCode::FAILURE
        }
    }
}
