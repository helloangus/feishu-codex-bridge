//! Feishu pbbp2.proto field numbers, verified against the official Python SDK.
use super::Error;
use crate::ingress::{Event, MAX_FRAME_BYTES};
use prost::Message;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::time::{Duration, Instant};

#[derive(Clone, PartialEq, Message)]
pub struct Header {
    #[prost(string, required, tag = "1")]
    pub key: String,
    #[prost(string, required, tag = "2")]
    pub value: String,
}
#[derive(Clone, PartialEq, Message)]
pub struct Frame {
    #[prost(uint64, optional, tag = "1")]
    pub seq_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub log_id: Option<u64>,
    #[prost(int32, optional, tag = "3")]
    pub service: Option<i32>,
    #[prost(int32, optional, tag = "4")]
    pub method: Option<i32>,
    #[prost(message, repeated, tag = "5")]
    pub headers: Vec<Header>,
    #[prost(string, optional, tag = "6")]
    pub payload_encoding: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub payload_type: Option<String>,
    #[prost(bytes = "vec", optional, tag = "8")]
    pub payload: Option<Vec<u8>>,
    #[prost(string, optional, tag = "9")]
    pub log_id_new: Option<String>,
}
impl Frame {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(Error::Protocol);
        }
        let frame = Self::decode(bytes).map_err(|_| Error::Protocol)?;
        if frame.seq_id.is_none()
            || frame.log_id.is_none()
            || frame.service.is_none()
            || !matches!(frame.method, Some(0 | 1))
            || frame.headers.len() > 64
        {
            return Err(Error::Protocol);
        }
        let mut keys = std::collections::BTreeSet::new();
        for h in &frame.headers {
            if h.key.len() > 64 || h.value.len() > 4096 || !keys.insert(&h.key) {
                return Err(Error::Protocol);
            }
        }
        Ok(frame)
    }
    pub fn header(&self, name: &str) -> Result<&str, Error> {
        self.headers
            .iter()
            .find(|h| h.key == name)
            .map(|h| h.value.as_str())
            .ok_or(Error::Protocol)
    }
    pub fn ping(service: i32) -> Self {
        Self {
            seq_id: Some(0),
            log_id: Some(0),
            service: Some(service),
            method: Some(0),
            headers: vec![Header {
                key: "type".into(),
                value: "ping".into(),
            }],
            ..Self::default()
        }
    }
    pub fn reply(self, accepted: bool, elapsed: Duration) -> Self {
        let card = self.header("type").ok() == Some("card");
        self.reply_with_card(accepted, elapsed, card)
    }
    pub fn reply_with_card(mut self, accepted: bool, elapsed: Duration, card: bool) -> Self {
        self.headers.retain(|h| h.key != "biz_rt");
        self.headers.push(Header {
            key: "biz_rt".into(),
            value: elapsed.as_millis().to_string(),
        });
        let body = if accepted && card {
            json!({"code":200,"data":"e30="})
        } else {
            json!({"code":if accepted {200} else {500}})
        };
        self.payload = Some(body.to_string().into_bytes());
        self
    }
}
struct Assembly {
    parts: Vec<Option<Vec<u8>>>,
    signature: Vec<String>,
    bytes: usize,
    deadline: Instant,
}
#[derive(Default)]
pub struct Fragments {
    entries: BTreeMap<String, Assembly>,
}
impl Fragments {
    pub fn push(&mut self, mut frame: Frame, now: Instant) -> Result<Option<Frame>, Error> {
        self.entries.retain(|_, a| now < a.deadline);
        let id = frame.header("message_id")?.to_owned();
        let total = frame
            .header("sum")?
            .parse::<usize>()
            .map_err(|_| Error::Protocol)?;
        let index = frame
            .header("seq")?
            .parse::<usize>()
            .map_err(|_| Error::Protocol)?;
        if id.is_empty() || id.len() > 1024 || total == 0 || total > 64 || index >= total {
            return Err(Error::Protocol);
        }
        let signature = vec![
            frame.header("type")?.into(),
            frame.header("trace_id").unwrap_or("").into(),
            frame.service.unwrap_or_default().to_string(),
            frame.payload_encoding.clone().unwrap_or_default(),
            frame.payload_type.clone().unwrap_or_default(),
        ];
        let bytes = frame.payload.take().unwrap_or_default();
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(Error::Protocol);
        }
        if total == 1 {
            if self.entries.remove(&id).is_some() {
                return Err(Error::Protocol);
            }
            frame.payload = Some(bytes);
            return Ok(Some(frame));
        }
        if !self.entries.contains_key(&id) && self.entries.len() >= 64 {
            return Err(Error::Overloaded);
        }
        if self.entries.values().map(|a| a.bytes).sum::<usize>() + bytes.len() > 8 * 1024 * 1024 {
            return Err(Error::Overloaded);
        }
        let a = self.entries.entry(id.clone()).or_insert_with(|| Assembly {
            parts: vec![None; total],
            signature: signature.clone(),
            bytes: 0,
            deadline: now + Duration::from_secs(5),
        });
        if a.parts.len() != total
            || a.signature != signature
            || a.parts[index].as_ref().is_some_and(|old| old != &bytes)
        {
            self.entries.remove(&id);
            return Err(Error::Protocol);
        }
        if a.parts[index].is_none() {
            a.bytes += bytes.len();
            a.parts[index] = Some(bytes);
        }
        if a.bytes > MAX_FRAME_BYTES {
            self.entries.remove(&id);
            return Err(Error::Protocol);
        }
        if a.parts.iter().any(Option::is_none) {
            return Ok(None);
        }
        let a = self.entries.remove(&id).ok_or(Error::Protocol)?;
        frame.payload = Some(a.parts.into_iter().flatten().flatten().collect());
        Ok(Some(frame))
    }
}

fn string(value: &Value, path: &str) -> Result<String, Error> {
    value
        .pointer(path)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty() && s.len() <= 1024)
        .map(str::to_owned)
        .ok_or(Error::Protocol)
}
pub fn event(payload: &[u8]) -> Result<Option<Event>, Error> {
    let value: Value = serde_json::from_slice(payload).map_err(|_| Error::Protocol)?;
    match value.pointer("/header/event_type").and_then(Value::as_str) {
        Some("im.message.receive_v1") => {
            match value
                .pointer("/event/sender/sender_type")
                .and_then(Value::as_str)
            {
                Some("app") => return Ok(None),
                Some("user") => {}
                _ => return Err(Error::Protocol),
            }
            let content = value
                .pointer("/event/message/content")
                .and_then(Value::as_str)
                .ok_or(Error::Protocol)?;
            let content: Value = serde_json::from_str(content).map_err(|_| Error::Protocol)?;
            if !content.is_object() {
                return Err(Error::Protocol);
            }
            Ok(Some(Event::Message {
                message_id: string(&value, "/event/message/message_id")?,
                user_id: string(&value, "/event/sender/sender_id/open_id")?,
                chat_id: string(&value, "/event/message/chat_id")?,
                chat_type: string(&value, "/event/message/chat_type")?,
                message_type: string(&value, "/event/message/message_type")?,
                content,
            }))
        }
        Some("card.action.trigger") => {
            let action = value
                .pointer("/event/action/value")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !action.is_object() {
                return Err(Error::Protocol);
            }
            Ok(Some(Event::Card {
                message_id: string(&value, "/event/context/open_message_id")?,
                user_id: string(&value, "/event/operator/open_id")?,
                chat_id: string(&value, "/event/context/open_chat_id")?,
                action,
            }))
        }
        Some(_) => Ok(None),
        None => Err(Error::Protocol),
    }
}
