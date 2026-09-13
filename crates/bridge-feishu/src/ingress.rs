//! Versioned sidecar IPC; malformed or discontinuous streams fail closed.
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

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

#[derive(Deserialize)]
struct Envelope {
    version: u32,
    epoch: String,
    sequence: u64,
    event: Event,
}

#[derive(Debug, Error, PartialEq)]
pub enum IngressError {
    #[error("SDK IPC frame exceeds limit")]
    TooLarge,
    #[error("SDK IPC schema invalid or unsupported")]
    Invalid,
    #[error("SDK IPC generation or sequence mismatch")]
    Discontinuity,
}

/// Create one decoder per child generation. An error requires restarting the
/// connection; validation does not authorize the sender or acknowledge work.
pub struct Decoder {
    epoch: String,
    next: u64,
    failed: bool,
}

impl Decoder {
    pub fn new(epoch: String) -> Self {
        Self {
            epoch,
            next: 0,
            failed: false,
        }
    }
    pub fn decode(&mut self, frame: &[u8]) -> Result<Event, IngressError> {
        if self.failed {
            return Err(IngressError::Discontinuity);
        }
        let result = self.parse(frame);
        self.failed = result.is_err();
        result
    }
    fn parse(&mut self, frame: &[u8]) -> Result<Event, IngressError> {
        if frame.len() > MAX_FRAME_BYTES {
            return Err(IngressError::TooLarge);
        }
        let envelope: Envelope =
            serde_json::from_slice(frame).map_err(|_| IngressError::Invalid)?;
        if envelope.version != 1 {
            return Err(IngressError::Invalid);
        }
        if self.epoch.is_empty() || envelope.epoch != self.epoch || envelope.sequence != self.next {
            return Err(IngressError::Discontinuity);
        }
        match &envelope.event {
            Event::Message {
                message_id,
                user_id,
                chat_id,
                content,
                ..
            } => {
                if !valid_ids(message_id, user_id, chat_id) || !content.is_object() {
                    return Err(IngressError::Invalid);
                }
            }
            Event::Card {
                message_id,
                user_id,
                chat_id,
                action,
            } => {
                if !valid_ids(message_id, user_id, chat_id) || !action.is_object() {
                    return Err(IngressError::Invalid);
                }
            }
            Event::Connection { .. } => {}
        }
        self.next = self
            .next
            .checked_add(1)
            .ok_or(IngressError::Discontinuity)?;
        Ok(envelope.event)
    }
}

fn valid_ids(message: &str, user: &str, chat: &str) -> bool {
    [message, user, chat].iter().all(|id| !id.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn frame(epoch: &str, sequence: u64, event: Value) -> Vec<u8> {
        json!({"version":1,"epoch":epoch,"sequence":sequence,"event":event})
            .to_string()
            .into_bytes()
    }
    #[test]
    fn preserves_card_source_and_rejects_replay() -> Result<(), IngressError> {
        let mut decoder = Decoder::new("g1".into());
        let bytes = frame(
            "g1",
            0,
            json!({"kind":"card","message_id":"card","user_id":"user","chat_id":"chat","action":{"token":"opaque"}}),
        );
        let Event::Card {
            message_id,
            user_id,
            chat_id,
            action,
        } = decoder.decode(&bytes)?
        else {
            panic!("expected card")
        };
        assert_eq!(
            (message_id.as_str(), user_id.as_str(), chat_id.as_str()),
            ("card", "user", "chat")
        );
        assert_eq!(action["token"], "opaque");
        assert_eq!(decoder.decode(&bytes), Err(IngressError::Discontinuity));
        Ok(())
    }
    #[test]
    fn invalid_stream_cannot_resume() {
        let event = json!({"kind":"connection","state":"starting"});
        for bytes in [
            frame("old", 0, event.clone()),
            frame("g1", 1, event.clone()),
            b"{}".to_vec(),
            frame(
                "g1",
                0,
                json!({"kind":"card","message_id":"m","user_id":"","chat_id":"c","action":{}}),
            ),
        ] {
            let mut decoder = Decoder::new("g1".into());
            assert!(decoder.decode(&bytes).is_err());
            assert_eq!(
                decoder.decode(&frame("g1", 0, event.clone())),
                Err(IngressError::Discontinuity)
            );
        }
    }
    #[test]
    fn size_limit_precedes_parsing() {
        assert_eq!(
            Decoder::new("g1".into()).decode(&vec![b' '; MAX_FRAME_BYTES + 1]),
            Err(IngressError::TooLarge)
        );
    }
}
