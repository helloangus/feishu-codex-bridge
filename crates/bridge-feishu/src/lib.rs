//! Feishu transport and wire decoding boundary.
use bridge_core::command::{Command, ParseError};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("卡片回调格式无效")]
    Json(#[from] serde_json::Error),
    #[error("卡片命令无效")]
    Command(#[from] ParseError),
    #[error("缺少卡片操作参数")]
    MissingArgument,
}

#[derive(Deserialize)]
struct Action {
    token: Option<String>,
    command: String,
    model: Option<String>,
    thread_id: Option<String>,
    path: Option<String>,
    task_id: Option<String>,
    enabled: Option<bool>,
}

/// Decode simple control-panel actions directly, never concatenate user input.
/// Interactive approval/thread/Plan tokens require their own registry path.
pub fn decode_control_action(json: &str) -> Result<Command, DecodeError> {
    let action: Action = serde_json::from_str(json)?;
    Ok(match action.command.as_str() {
        "/plan-toggle" => Command::Plan(Some(action.enabled.ok_or(DecodeError::MissingArgument)?)),
        "/model" if action.model.is_some() => Command::Model(action.model),
        "/cd" => Command::ChangeDirectory(action.path),
        "/cd-confirm" => Command::ConfirmDirectory(required(action.token)?),
        "/stop" => Command::Stop(action.task_id),
        "/resume" => Command::Resume(action.thread_id),
        "/archive" => Command::Archive(required(action.thread_id)?),
        "/unarchive" => Command::Unarchive(required(action.thread_id)?),
        "/help" | "/status" | "/new" | "/plan" | "/archived" | "/model" | "/models"
        | "/compact" => Command::parse(&action.command)?,
        _ => return Err(ParseError::Unknown.into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn card_toggle_is_explicit_and_path_is_not_reparsed() -> Result<(), DecodeError> {
        assert_eq!(
            decode_control_action(r#"{"command":"/plan-toggle","enabled":false}"#)?,
            Command::Plan(Some(false))
        );
        assert!(decode_control_action(r#"{"command":"/plan-toggle"}"#).is_err());
        assert_eq!(
            decode_control_action(r#"{"command":"/cd","path":"dir /stop"}"#)?,
            Command::ChangeDirectory(Some("dir /stop".into()))
        );
        assert!(decode_control_action(r#"{"command":"/approve 123"}"#).is_err());
        Ok(())
    }
}

pub mod cards;
pub mod rest;

fn required(value: Option<String>) -> Result<String, DecodeError> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or(DecodeError::MissingArgument)
}

/// Parsing never authorizes an interaction; check owner, card and deadline in the application.
pub fn decode_action(json: &str) -> Result<bridge_core::view::ButtonAction, DecodeError> {
    #[derive(Deserialize)]
    struct Callback {
        command: String,
        token: Option<String>,
        choice: Option<String>,
    }
    let callback: Callback = serde_json::from_str(json)?;
    if callback.command == "/interaction" {
        Ok(bridge_core::view::ButtonAction::Interaction {
            token: required(callback.token)?,
            choice: required(callback.choice)?,
        })
    } else {
        decode_control_action(json).map(bridge_core::view::ButtonAction::Command)
    }
}

pub mod ingress;

pub mod proxy;
pub mod websocket;

/// Hard node budget when walking a rich-text post; keeps pathological
/// payloads from dominating decode time.
const MAX_POST_NODES: usize = 4096;
/// One more than the application's `ATTACHMENTS_PER_MESSAGE`: the overflow
/// sentinel makes the application reject the message instead of silently
/// dropping files.
const ATTACHMENT_OVERFLOW_SENTINEL: usize = 11;
/// Max attachment key length, protocol-bounded.
const MAX_ATTACHMENT_KEY: usize = 1024;
/// Max attachment display-name characters.
const MAX_ATTACHMENT_NAME_CHARS: usize = 200;

/// Decode media references without downloading or trusting remote filenames
/// as paths. `kind` is the Feishu message type; for rich-text posts the
/// `img`/`file`/`media` child nodes are walked recursively with dedup by key.
pub fn attachments(
    message_id: &str,
    kind: &str,
    content: &serde_json::Value,
) -> Vec<bridge_app::files::Attachment> {
    if kind == "post" {
        return post_attachments(message_id, post_body(content));
    }
    use bridge_app::files::Attachment;
    use bridge_app::messaging::{ResourceKind, ResourceRef};
    let (field, resource_kind) = match kind {
        "image" => ("image_key", ResourceKind::Image),
        "file" | "audio" | "media" | "video" => ("file_key", ResourceKind::File),
        _ => return vec![],
    };
    content
        .get(field)
        .or_else(|| {
            (kind == "media" || kind == "video")
                .then(|| content.get("media_key"))
                .flatten()
        })
        .and_then(serde_json::Value::as_str)
        .filter(|key| !key.is_empty() && key.len() <= MAX_ATTACHMENT_KEY)
        .map(|key| {
            vec![Attachment {
                resource: ResourceRef {
                    message_id: message_id.into(),
                    key: key.into(),
                    kind: resource_kind,
                },
                name: content
                    .get("file_name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(if kind == "image" {
                        "image.png"
                    } else {
                        "attachment.bin"
                    })
                    .chars()
                    .take(MAX_ATTACHMENT_NAME_CHARS)
                    .collect(),
            }]
        })
        .unwrap_or_default()
}

/// Extract attachments from one rich-text post body: walk at most
/// `MAX_POST_NODES` child nodes, dedup by resource key, and stop at the
/// overflow sentinel (one past the application limit) so an overloaded post
/// is rejected upstream rather than silently truncated.
fn post_attachments(
    message_id: &str,
    post: &serde_json::Value,
) -> Vec<bridge_app::files::Attachment> {
    use bridge_app::files::Attachment;
    let mut result = Vec::new();
    let Some(rows) = post.get("content").and_then(serde_json::Value::as_array) else {
        return result;
    };
    for node in rows
        .iter()
        .filter_map(serde_json::Value::as_array)
        .flatten()
        .take(MAX_POST_NODES)
    {
        let kind = match node.get("tag").and_then(serde_json::Value::as_str) {
            Some("img") => "image",
            Some("file" | "media") => "file",
            _ => continue,
        };
        for attachment in attachments(message_id, kind, node) {
            if !result
                .iter()
                .any(|a: &Attachment| a.resource.key == attachment.resource.key)
            {
                result.push(attachment);
            }
        }
        if result.len() >= ATTACHMENT_OVERFLOW_SENTINEL {
            break;
        }
    }
    result
}

/// Locate the post payload: inline `content`, the `zh_cn`/`en_us` locale
/// objects, or failing those, the first object value. The order matches the
/// official SDK's message shape.
fn post_body(content: &serde_json::Value) -> &serde_json::Value {
    if content.get("content").is_some() {
        return content;
    }
    content
        .get("zh_cn")
        .or_else(|| content.get("en_us"))
        .or_else(|| content.as_object().and_then(|o| o.values().next()))
        .unwrap_or(content)
}

pub fn message_text(kind: &str, content: &serde_json::Value) -> Option<String> {
    if kind == "text" {
        return content
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
    }
    if kind != "post" {
        return None;
    }
    let post = post_body(content);
    let mut parts = vec![
        post.get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(""),
    ];
    if let Some(rows) = post.get("content").and_then(serde_json::Value::as_array) {
        for row in rows
            .iter()
            .filter_map(serde_json::Value::as_array)
            .take(4096)
        {
            for node in row.iter().take(4096) {
                if let Some(text) = node.get("text").and_then(serde_json::Value::as_str) {
                    parts.push(text);
                }
            }
            parts.push("\n");
        }
    }
    let text = parts.join(" ");
    (!text.trim().is_empty()).then_some(text)
}
