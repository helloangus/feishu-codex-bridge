//! Versioned ingress events; malformed or discontinuous streams fail closed.
use bridge_app::{cards::Click, runtime};
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

/// Only locally registered opaque actions enter the runtime; raw card commands
/// cannot bypass message/owner validation or become ordinary chat text.
pub fn decode_card_click(source: String, action: &Value) -> Option<Click> {
    match crate::decode_action(&action.to_string()).ok()? {
        bridge_core::view::ButtonAction::Interaction { token, choice }
            if choice == "run" && !source.is_empty() =>
        {
            Some(Click {
                token: bridge_app::cards::CardToken::new(token),
                source,
            })
        }
        _ => None,
    }
}

impl Received {
    /// Convert one received Feishu message or card click into application
    /// input; the runtime settles the carried [`Ack`](runtime::Ack) exactly
    /// once and the receipt completes the Feishu event acknowledgement.
    /// Connection events are lifecycle-only and convert to nothing.
    pub fn into_runtime_input(self) -> Option<runtime::Input> {
        let ack = runtime::Ack::from_receipt(self.acceptance.map(|receipt| receipt.0));
        match self.event {
            Event::Message {
                message_id,
                user_id,
                chat_id,
                message_type,
                content,
                ..
            } => Some(runtime::Input {
                attachments: crate::attachments(&message_id, &message_type, &content),
                card: None,
                id: message_id,
                user: user_id,
                chat: chat_id,
                text: crate::message_text(&message_type, &content),
                ack,
            }),
            Event::Card {
                message_id,
                user_id,
                chat_id,
                action,
            } => Some(runtime::Input {
                attachments: vec![],
                card: decode_card_click(message_id.clone(), &action),
                id: format!("card:{message_id}"),
                user: user_id,
                chat: chat_id,
                text: None,
                ack,
            }),
            Event::Connection { .. } => None,
        }
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
