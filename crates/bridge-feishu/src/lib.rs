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

pub mod sidecar;
