//! Messaging ports keep HTTP, platform card JSON and SDK types out of use cases.
use bridge_core::view::Panel;
use std::{fs::File, future::Future, pin::Pin};
use thiserror::Error;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum DeliveryError {
    #[error("消息接口请求失败")]
    Transport,
    #[error("消息平台拒绝请求（code={0}）")]
    Rejected(i64),
    #[error("消息平台响应格式不兼容")]
    Incompatible,
    #[error("文件超过大小限制")]
    TooLarge,
    #[error("本地文件读写失败")]
    LocalIo,
    #[error("认证配置无效")]
    Authentication,
}
pub type DeliveryFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, DeliveryError>> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageId(pub String);
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceKind {
    Image,
    File,
}
#[derive(Debug, Clone)]
pub struct ResourceRef {
    pub message_id: String,
    pub key: String,
    pub kind: ResourceKind,
}

pub trait Messenger: Send + Sync {
    /// Text-only adapters can retain the existing delivery path.
    fn rich_output(&self) -> bool {
        false
    }
    fn bind_files(&self, _task: String, _thread: String) -> DeliveryFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn prepare_files(
        &self,
        _task: String,
        _directory: std::path::PathBuf,
        attachments: Vec<Attachment>,
    ) -> DeliveryFuture<'_, PreparedFiles> {
        Box::pin(async move {
            if attachments.is_empty() {
                Ok(PreparedFiles::default())
            } else {
                Err(DeliveryError::Incompatible)
            }
        })
    }
    fn finish_files(
        &self,
        _task: String,
        _chat: String,
        _directory: std::path::PathBuf,
    ) -> DeliveryFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
    fn send_panel(&self, chat: String, panel: Panel) -> DeliveryFuture<'_, MessageId>;
    fn update_panel(&self, id: MessageId, panel: Panel) -> DeliveryFuture<'_, ()>;
    fn send_text(&self, chat: String, text: String) -> DeliveryFuture<'_, ()>;
    /// Caller supplies an already validated regular file, not an arbitrary path.
    fn upload(
        &self,
        chat: String,
        name: String,
        file: File,
        kind: ResourceKind,
    ) -> DeliveryFuture<'_, ()>;
}

pub trait ResourceFetcher: Send + Sync {
    /// Caller owns the temporary file and commits it only after success.
    fn download(&self, resource: ResourceRef, destination: File) -> DeliveryFuture<'_, u64>;
}

#[derive(Debug, Clone)]
pub struct Attachment {
    pub resource: ResourceRef,
    pub name: String,
}
#[derive(Default)]
pub struct PreparedFiles {
    pub prompt: String,
    pub images: Vec<std::path::PathBuf>,
}
