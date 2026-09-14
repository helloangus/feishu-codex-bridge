//! Configuration validation and native service assembly.
use bridge_local::workspace::Workspace;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub workspace: WorkspaceConfig,
    pub access: AccessConfig,
    pub codex: CodexConfig,
    pub feishu: Option<FeishuConfig>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FeishuConfig {
    pub app_id_env: String,
    pub app_secret_env: String,
    pub proxy_env: Option<String>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub state_dir: PathBuf,
}

pub mod bootstrap;
pub mod credentials;
mod descendants;
pub mod health;
pub mod logging;
pub mod service_control;
pub mod setup;
pub mod supervisor;
pub mod supervisor_state;
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccessConfig {
    pub mode: AccessMode,
    #[serde(default)]
    pub allowed_open_ids: BTreeSet<String>,
    pub pairing_code_env: Option<String>,
}
#[derive(Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    Restricted,
    Open,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CodexConfig {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub sandbox: Sandbox,
}
#[derive(Deserialize, Serialize)]
pub enum Sandbox {
    #[serde(rename = "workspaceWrite")]
    WorkspaceWrite,
    #[serde(rename = "dangerFullAccess")]
    DangerFullAccess,
}

/// Configuration failures keep a safe operator-facing message while retaining
/// the underlying cause for callers that need it.
#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    Invalid(&'static str),
}
impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => write!(f, "配置文件无法读取"),
            Self::Parse(_) => write!(f, "配置 TOML 格式无效"),
            Self::Invalid(message) => f.write_str(message),
        }
    }
}
impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Parse(error) => Some(error),
            Self::Invalid(_) => None,
        }
    }
}
impl From<std::io::Error> for ConfigError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<toml::de::Error> for ConfigError {
    fn from(value: toml::de::Error) -> Self {
        Self::Parse(value)
    }
}

/// Environment variable names are validated identically at every boundary.
pub fn valid_env_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

impl From<Sandbox> for bridge_app::ports::Sandbox {
    fn from(value: Sandbox) -> Self {
        match value {
            Sandbox::WorkspaceWrite => Self::WorkspaceWrite,
            Sandbox::DangerFullAccess => Self::DangerFullAccess,
        }
    }
}

impl Config {
    /// Read only the explicit TOML file. Does not source .env or create files.
    pub fn read(path: &Path) -> Result<Self, ConfigError> {
        Ok(toml::from_str(&fs::read_to_string(path)?)?)
    }
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.workspace.root.is_absolute()
            || !self.workspace.cwd.is_absolute()
            || !self.workspace.state_dir.is_absolute()
        {
            return Err(ConfigError::Invalid(
                "工作区、初始目录和状态目录必须为绝对路径",
            ));
        }
        let workspace = Workspace::new(&self.workspace.root)
            .map_err(|_| ConfigError::Invalid("工作区根目录无效"))?;
        workspace
            .resolve_existing(&self.workspace.cwd, &self.workspace.cwd)
            .map_err(|_| ConfigError::Invalid("初始工作目录无效或越出工作区"))?;
        if self
            .access
            .allowed_open_ids
            .iter()
            .any(|id| id.trim().is_empty())
        {
            return Err(ConfigError::Invalid("白名单不能包含空用户 ID"));
        }
        if let Some(name) = &self.access.pairing_code_env {
            if !valid_env_name(name) {
                return Err(ConfigError::Invalid("配对码环境变量名无效"));
            }
        }
        let feishu = self
            .feishu
            .as_ref()
            .ok_or(ConfigError::Invalid("缺少 [feishu] 配置段"))?;
        for (label, name) in [
            ("App ID", &feishu.app_id_env),
            ("App Secret", &feishu.app_secret_env),
        ] {
            if !valid_env_name(name) {
                return Err(ConfigError::Invalid(match label {
                    "App ID" => "飞书 App ID 环境变量名无效",
                    _ => "飞书 App Secret 环境变量名无效",
                }));
            }
        }
        if feishu
            .proxy_env
            .as_ref()
            .is_some_and(|name| !valid_env_name(name))
        {
            return Err(ConfigError::Invalid("飞书代理环境变量名无效"));
        }
        if self.access.mode == AccessMode::Restricted
            && self.access.allowed_open_ids.is_empty()
            && self.access.pairing_code_env.is_none()
        {
            return Err(ConfigError::Invalid(
                "restricted 模式需要白名单或 pairing_code_env",
            ));
        }
        if self.codex.executable.as_os_str().is_empty() || self.codex.args.is_empty() {
            return Err(ConfigError::Invalid("Codex executable 和 args 不能为空"));
        }
        Ok(())
    }
}
