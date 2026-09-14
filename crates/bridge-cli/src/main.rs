use bridge_cli::Config;
use bridge_cli::credentials;
use bridge_core::command::Command;
use clap::{Parser, Subcommand};
use std::{path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(name = "bridge", version, about = "飞书与 Codex 的原生桥接服务")]
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
    /// 按配置声明的环境变量校验凭据；交互终端支持隐藏输入。
    Credentials {
        #[arg(long)]
        file: PathBuf,
        /// 输出缺失凭据的 export 语句，供脚本 eval 后启动服务。
        #[arg(long)]
        print: bool,
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
fn runtime() -> Result<tokio::runtime::Runtime, Box<dyn std::error::Error>> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}
/// Shared status query for `status` and `service status`; the control socket
/// phase needs a runtime with IO and timers enabled.
fn print_status(settings: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let state = &settings.workspace.state_dir;
    let report = bridge_cli::health::status(state)?;
    let phase = if report.supervisor_running || report.guard_running {
        runtime()?
            .block_on(bridge_cli::service_control::request(
                state,
                bridge_cli::service_control::ControlCommand::Phase,
            ))
            .ok()
            .and_then(|response| response.as_phase().map(str::to_owned))
    } else {
        None
    };
    println!("{}", bridge_cli::health::display(&report, phase.as_deref()));
    Ok(())
}
fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Action::Guard { config } => {
            let settings = Config::read(&config)?;
            settings.validate()?;
            runtime()?.block_on(bridge_cli::supervisor::guard(
                &config,
                &settings.workspace.state_dir,
            ))?;
        }
        Action::Service { config, command } => {
            let settings = Config::read(&config)?;
            let state = &settings.workspace.state_dir;
            if matches!(command, ServiceAction::Status) {
                return print_status(&settings);
            }
            if matches!(command, ServiceAction::Start | ServiceAction::Restart) {
                settings.validate()?;
            }
            let _control_lock = bridge_cli::service_control::command_lock(state)?;
            runtime()?.block_on(async {
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
            println!(
                "服务控制完成。查看运行和飞书连接状态：bridge status --config {}",
                config.display()
            );
        }
        Action::Supervise { config } => {
            let settings = Config::read(&config)?;
            settings.validate()?;
            runtime()?.block_on(bridge_cli::supervisor::run(
                &config,
                &settings.workspace.state_dir,
            ))?;
        }
        Action::Status { config } => {
            let settings = Config::read(&config)?;
            print_status(&settings)?;
        }
        Action::Run { config } => {
            let config = Config::read(&config)?;
            config.validate()?;
            runtime()?.block_on(bridge_cli::bootstrap::run(config))?;
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
            let config = Config::read(&file)?;
            config.validate()?;
            println!("配置检查通过（未连接飞书或 Codex；未校验运行时凭据）");
        }
        Action::Credentials { file, print } => {
            let config = Config::read(&file)?;
            config.validate()?;
            let exports = credentials::ensure(&config, print)?;
            for line in exports {
                println!("{line}");
            }
        }
        Action::CheckCommand { text } => {
            Command::parse(&text)?;
            println!("命令语法有效（未执行）");
        }
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
