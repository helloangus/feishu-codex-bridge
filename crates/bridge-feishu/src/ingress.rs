//! Versioned ingress events; malformed or discontinuous streams fail closed.
use serde::Deserialize;
use serde_json::Value;

pub const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;

pub struct Received {
    pub event: Event,
    pub acceptance: Option<Acceptance>,
}
/// One-use disposition from the application, after its durable admission decision.
pub struct Acceptance(pub(crate) tokio::sync::oneshot::Sender<bool>);
impl Acceptance {
    pub fn complete(self, accepted: bool) {
        let _ = self.0.send(accepted);
    }
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Message {
        message_id: String,
        user_id: String,
        chat_id: String,
        chat_type: String,
        message_type: String,
        content: Value,
    },
    Card {
        message_id: String,
        user_id: String,
        chat_id: String,
        action: Value,
    },
    Connection {
        state: ConnectionState,
    },
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Starting,
    Connected,
    Reconnecting,
}
