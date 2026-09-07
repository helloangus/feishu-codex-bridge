//! Agent-facing application port. Vendor JSON values cannot cross this boundary.
use bridge_core::ExecutionMode;
use std::{future::Future, path::PathBuf, pin::Pin};
use thiserror::Error;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum BackendError {
    #[error("代理连接已关闭")]
    Disconnected,
    #[error("代理操作结果未知；请勿自动重试")]
    Uncertain,
    #[error("代理协议不兼容")]
    Incompatible,
    #[error("代理拒绝请求（code={0}）")]
    Rejected(i64),
}
pub type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, BackendError>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub id: String,
    pub is_default: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    pub id: String,
    pub title: String,
    pub directory: Option<PathBuf>,
    pub active: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sandbox {
    WorkspaceWrite,
    DangerFullAccess,
}
#[derive(Debug, Clone)]
pub struct TurnInput {
    pub thread_id: String,
    pub directory: PathBuf,
    pub prompt: String,
    pub images: Vec<PathBuf>,
    pub model: String,
    pub mode: ExecutionMode,
    pub sandbox: Sandbox,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRef {
    pub thread_id: String,
    pub turn_id: String,
    pub epoch: u64,
}

/// Object-safe asynchronous port, usable with deterministic fake implementations.
pub trait AgentBackend: Send + Sync {
    fn models(&self) -> BackendFuture<'_, Vec<Model>>;
    fn threads(&self, directory: PathBuf, archived: bool) -> BackendFuture<'_, Vec<ThreadSummary>>;
    fn read_thread(&self, id: String) -> BackendFuture<'_, ThreadSummary>;
    fn start_thread(&self, directory: PathBuf) -> BackendFuture<'_, ThreadSummary>;
    fn resume_thread(&self, id: String, directory: PathBuf) -> BackendFuture<'_, ThreadSummary>;
    fn archive_thread(&self, id: String, archived: bool) -> BackendFuture<'_, ()>;
    fn compact(&self, id: String) -> BackendFuture<'_, ()>;
    fn start_turn(&self, input: TurnInput) -> BackendFuture<'_, TurnRef>;
    fn interrupt(&self, turn: TurnRef) -> BackendFuture<'_, ()>;
}
