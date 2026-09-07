//! Configuration validation for the Rust migration tools.
use bridge_local::workspace::Workspace;
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub workspace: WorkspaceConfig,
    pub access: AccessConfig,
    pub codex: CodexConfig,
    pub feishu: Option<FeishuConfig>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeishuConfig {
    pub app_id_env: String,
    pub app_secret_env: String,
    pub python: PathBuf,
    pub adapter: PathBuf,
    pub proxy_env: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
    pub cwd: PathBuf,
    pub state_dir: PathBuf,
}

pub mod bootstrap;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessConfig {
    pub mode: AccessMode,
    #[serde(default)]
    pub allowed_open_ids: BTreeSet<String>,
    pub pairing_code_env: Option<String>,
}
#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    Restricted,
    Open,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexConfig {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub sandbox: Sandbox,
}
#[derive(Deserialize)]
pub enum Sandbox {
    #[serde(rename = "workspaceWrite")]
    WorkspaceWrite,
    #[serde(rename = "dangerFullAccess")]
    DangerFullAccess,
}
impl Config {
    /// Read only the explicit TOML file. Does not source .env or create files.
    pub fn read(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(toml::from_str(&fs::read_to_string(path)?)?)
    }
    pub fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.workspace.root.is_absolute()
            || !self.workspace.cwd.is_absolute()
            || !self.workspace.state_dir.is_absolute()
        {
            return Err("工作区、初始目录和状态目录必须为绝对路径".into());
        }
        let workspace = Workspace::new(&self.workspace.root)?;
        workspace.resolve_existing(&self.workspace.cwd, &self.workspace.cwd)?;
        if self
            .access
            .allowed_open_ids
            .iter()
            .any(|id| id.trim().is_empty())
        {
            return Err("白名单不能包含空用户 ID".into());
        }
        if let Some(name) = &self.access.pairing_code_env {
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err("配对码环境变量名无效".into());
            }
        }
        if self.access.mode == AccessMode::Restricted
            && self.access.allowed_open_ids.is_empty()
            && self.access.pairing_code_env.is_none()
        {
            return Err("restricted 模式需要白名单或 pairing_code_env".into());
        }
        if self.codex.executable.as_os_str().is_empty() || self.codex.args.is_empty() {
            return Err("Codex executable 和 args 不能为空".into());
        }
        Ok(())
    }
}
