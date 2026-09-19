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

/// Pure message transport: panels, text and one already validated file.
/// Task-file lifecycle lives on [`crate::files::TaskFiles`].
pub trait Messenger: Send + Sync {
    /// Text-only adapters retain the plain-text delivery path.
    fn rich_output(&self) -> bool {
        false
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
