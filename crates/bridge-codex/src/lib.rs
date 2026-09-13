//! JSON-RPC envelope boundary. Process execution is intentionally not wired yet.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RpcId {
    Integer(i64),
    String(String),
}

#[derive(Debug, PartialEq)]
pub enum Envelope {
    Response {
        id: RpcId,
        result: Value,
    },
    Error {
        id: RpcId,
        error: Value,
    },
    Request {
        id: RpcId,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("JSON-RPC 帧过大")]
    TooLarge,
    #[error("JSON-RPC JSON 无效")]
    Json(#[from] serde_json::Error),
    #[error("JSON-RPC envelope 无效")]
    Invalid,
}

pub fn decode(line: &[u8], max_bytes: usize) -> Result<Envelope, DecodeError> {
    if line.len() > max_bytes {
        return Err(DecodeError::TooLarge);
    }
    let value: Value = serde_json::from_slice(line)?;
    let obj = value.as_object().ok_or(DecodeError::Invalid)?;
    if obj.get("jsonrpc").is_some_and(|v| v != "2.0") {
        return Err(DecodeError::Invalid);
    }
    let id = obj
        .get("id")
        .cloned()
        .map(serde_json::from_value::<RpcId>)
        .transpose()?;
    if let Some(method) = obj.get("method") {
        if obj.contains_key("result") || obj.contains_key("error") {
            return Err(DecodeError::Invalid);
        }
        let method = method.as_str().ok_or(DecodeError::Invalid)?.to_owned();
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        return Ok(match id {
            Some(id) => Envelope::Request { id, method, params },
            None => Envelope::Notification { method, params },
        });
    }
    let id = id.ok_or(DecodeError::Invalid)?;
    match (obj.get("result"), obj.get("error")) {
        (Some(result), None) => Ok(Envelope::Response {
            id,
            result: result.clone(),
        }),
        (None, Some(error)) => Ok(Envelope::Error {
            id,
            error: error.clone(),
        }),
        _ => Err(DecodeError::Invalid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn distinguishes_server_request_from_response_and_accepts_string_ids() -> Result<(), DecodeError>
    {
        assert!(matches!(
            decode(
                br#"{"id":"q","method":"item/tool/requestUserInput","params":{}}"#,
                1024
            )?,
            Envelope::Request {
                id: RpcId::String(_),
                ..
            }
        ));
        assert!(matches!(
            decode(br#"{"id":1,"result":null}"#, 1024)?,
            Envelope::Response { .. }
        ));
        assert!(matches!(
            decode(br#"{"method":"future/event","params":{}}"#, 1024)?,
            Envelope::Notification { .. }
        ));
        assert!(decode(br#"{"id":1,"result":{},"error":{}}"#, 1024).is_err());
        assert!(decode(br#"{"id":null,"result":{}}"#, 1024).is_err());
        assert!(matches!(decode(b"{}", 1), Err(DecodeError::TooLarge)));
        Ok(())
    }
}

pub mod backend;
pub mod process;
pub mod transport;

pub mod events;

mod permissions;
pub mod requests;
