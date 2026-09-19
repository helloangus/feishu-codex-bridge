//! Notifications mapped against the pinned protocol baseline (`crate::protocol`);
//! no current-task fallback.
use bridge_app::{
    events::{AgentEvent, TurnOutcome},
    ports::{BackendError, TurnRef},
};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Delta {
    thread_id: String,
    turn_id: String,
    item_id: String,
    delta: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Completed {
    thread_id: String,
    turn: Turn,
}
#[derive(Deserialize)]
struct Turn {
    id: String,
    status: String,
    error: Option<TurnError>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnError {
    message: String,
    additional_details: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemEvent {
    thread_id: String,
    turn_id: String,
    item: Value,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Archived {
    thread_id: String,
}

fn identifier(value: String) -> Result<String, BackendError> {
    if value.trim().is_empty() {
        Err(BackendError::Incompatible)
    } else {
        Ok(value)
    }
}
fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, BackendError> {
    serde_json::from_value(value).map_err(|_| BackendError::Incompatible)
}

/// File-change snapshots above these bounds are emptied (never an error):
/// the approval card cannot render them completely, and an incomplete
/// display must not offer "allow".
const MAX_FILE_CHANGES: usize = 20;
const MAX_FILE_CHANGE_BYTES: usize = 16 * 1024;

/// Only call for notifications. Requests must retain their ID for an explicit
/// response; ignoring an unknown notification never permits ignoring a request.
/// The application still compares the full TurnRef to its active execution.
pub fn notification(
    epoch: u64,
    method: &str,
    params: Value,
) -> Result<Option<AgentEvent>, BackendError> {
    match method {
        "item/started" => file_changes(epoch, params),
        "turn/started" => turn_started(epoch, params).map(Some),
        "item/agentMessage/delta" => agent_message_delta(epoch, params).map(Some),
        "turn/completed" => turn_completed(epoch, params).map(Some),
        "item/completed" => plan_completed(epoch, params),
        "thread/archived" => thread_archived(epoch, params).map(Some),
        _ => Ok(None),
    }
}

fn turn(epoch: u64, thread_id: String, turn_id: String) -> Result<TurnRef, BackendError> {
    Ok(TurnRef {
        epoch,
        thread_id: identifier(thread_id)?,
        turn_id: identifier(turn_id)?,
    })
}

/// `item/started` with a `fileChange` item: collect the per-file diffs the
/// approval card will show. Other item types are ignored.
fn file_changes(epoch: u64, params: Value) -> Result<Option<AgentEvent>, BackendError> {
    let data: ItemEvent = parse(params)?;
    if data.item.get("type").and_then(Value::as_str) != Some("fileChange") {
        return Ok(None);
    }
    let item = identifier(
        data.item
            .get("id")
            .and_then(Value::as_str)
            .ok_or(BackendError::Incompatible)?
            .into(),
    )?;
    let entries = data
        .item
        .get("changes")
        .and_then(Value::as_array)
        .ok_or(BackendError::Incompatible)?;
    let mut changes = Vec::new();
    let mut size = 0usize;
    for entry in entries {
        let path = entry
            .get("path")
            .and_then(Value::as_str)
            .ok_or(BackendError::Incompatible)?;
        let diff = entry
            .get("diff")
            .and_then(Value::as_str)
            .ok_or(BackendError::Incompatible)?;
        let kind = entry.get("kind").ok_or(BackendError::Incompatible)?;
        let operation = kind
            .get("type")
            .and_then(Value::as_str)
            .ok_or(BackendError::Incompatible)?;
        if !matches!(operation, "add" | "delete" | "update") {
            return Err(BackendError::Incompatible);
        }
        let move_path = match kind.get("movePath") {
            None | Some(Value::Null) => None,
            Some(Value::String(path)) => Some(path.clone()),
            _ => return Err(BackendError::Incompatible),
        };
        size = size
            .saturating_add(path.len())
            .saturating_add(diff.len())
            .saturating_add(move_path.as_ref().map_or(0, String::len));
        if entries.len() > MAX_FILE_CHANGES || size > MAX_FILE_CHANGE_BYTES {
            changes.clear();
            break;
        }
        changes.push(bridge_app::requests::FileChange {
            path: path.into(),
            diff: diff.into(),
            operation: operation.into(),
            move_path,
        });
    }
    Ok(Some(AgentEvent::FileChanges {
        turn: turn(epoch, data.thread_id, data.turn_id)?,
        item,
        changes,
    }))
}

/// `turn/started`: the turn identity becomes the execution gate's bind point.
fn turn_started(epoch: u64, params: Value) -> Result<AgentEvent, BackendError> {
    let data: Completed = parse(params)?;
    if data.turn.status != "inProgress" {
        return Err(BackendError::Incompatible);
    }
    Ok(AgentEvent::Started {
        turn: turn(epoch, data.thread_id, data.turn.id)?,
    })
}

/// `item/agentMessage/delta`: streaming output for the progress preview.
fn agent_message_delta(epoch: u64, params: Value) -> Result<AgentEvent, BackendError> {
    let data: Delta = parse(params)?;
    Ok(AgentEvent::Output {
        turn: turn(epoch, data.thread_id, data.turn_id)?,
        item: identifier(data.item_id)?,
        delta: data.delta,
    })
}

/// `turn/completed`: the authoritative terminal outcome of a turn.
fn turn_completed(epoch: u64, params: Value) -> Result<AgentEvent, BackendError> {
    let data: Completed = parse(params)?;
    let outcome = match data.turn.status.as_str() {
        "completed" => TurnOutcome::Completed,
        "failed" => TurnOutcome::Failed {
            message: data.turn.error.as_ref().map(|e| e.message.clone()),
            details: data.turn.error.and_then(|e| e.additional_details),
        },
        "interrupted" => TurnOutcome::Interrupted,
        _ => return Err(BackendError::Incompatible),
    };
    Ok(AgentEvent::Finished {
        turn: turn(epoch, data.thread_id, data.turn.id)?,
        outcome,
    })
}

/// `item/completed` with a `plan` item: the authoritative plan text. Other
/// item types are ignored.
fn plan_completed(epoch: u64, params: Value) -> Result<Option<AgentEvent>, BackendError> {
    let data: ItemEvent = parse(params)?;
    if data.item.get("type").and_then(Value::as_str) != Some("plan") {
        return Ok(None);
    }
    #[derive(Deserialize)]
    struct Plan {
        id: String,
        text: String,
    }
    let plan: Plan = parse(data.item)?;
    Ok(Some(AgentEvent::Plan {
        turn: turn(epoch, data.thread_id, data.turn_id)?,
        item: identifier(plan.id)?,
        text: plan.text,
    }))
}

/// `thread/archived`: the thread's local binding must be cleared later.
fn thread_archived(epoch: u64, params: Value) -> Result<AgentEvent, BackendError> {
    let data: Archived = parse(params)?;
    Ok(AgentEvent::Archived {
        epoch,
        thread: identifier(data.thread_id)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn file_details_preserve_identity_and_never_partially_approve_large_changes()
    -> Result<(), BackendError> {
        let mut params = json!({"threadId":"thread","turnId":"turn","item":{"id":"item","type":"fileChange","changes":[{"path":"/a","kind":{"type":"add"},"diff":"+hello"},{"path":"/b","kind":{"type":"delete"},"diff":"-bye"}]}});
        let Some(AgentEvent::FileChanges {
            turn,
            item,
            changes,
        }) = notification(3, "item/started", params.clone())?
        else {
            return Err(BackendError::Incompatible);
        };
        assert_eq!(turn.epoch, 3);
        assert_eq!(turn.turn_id, "turn");
        assert_eq!(item, "item");
        assert_eq!(changes.len(), 2);
        params["item"]["changes"][1]["diff"] = json!("x".repeat(17 * 1024));
        let Some(AgentEvent::FileChanges { changes, .. }) =
            notification(3, "item/started", params.clone())?
        else {
            return Err(BackendError::Incompatible);
        };
        assert!(changes.is_empty());
        params["item"]["changes"][0]["kind"]["type"] = json!("unknown");
        assert!(notification(3, "item/started", params).is_err());
        Ok(())
    }

    #[test]
    fn started_retains_identity_and_rejects_invalid_status() -> Result<(), BackendError> {
        assert_eq!(
            notification(
                7,
                "turn/started",
                json!({"threadId":"th","turn":{"id":"tu","status":"inProgress"}})
            )?,
            Some(AgentEvent::Started {
                turn: TurnRef {
                    epoch: 7,
                    thread_id: "th".into(),
                    turn_id: "tu".into()
                }
            })
        );
        for params in [
            json!({"threadId":"th","turn":{"id":"tu","status":"completed"}}),
            json!({"threadId":"th","turn":{"id":"","status":"inProgress"}}),
            json!({"turn":{"id":"tu","status":"inProgress"}}),
        ] {
            assert!(notification(7, "turn/started", params).is_err());
        }
        Ok(())
    }

    #[test]
    fn output_retains_all_routing_identity() -> Result<(), BackendError> {
        assert_eq!(
            notification(
                7,
                "item/agentMessage/delta",
                json!({"threadId":"th","turnId":"tu","itemId":"i","delta":"中文","future":true})
            )?,
            Some(AgentEvent::Output {
                turn: TurnRef {
                    epoch: 7,
                    thread_id: "th".into(),
                    turn_id: "tu".into()
                },
                item: "i".into(),
                delta: "中文".into()
            })
        );
        assert!(notification(7, "item/agentMessage/delta", json!({"delta":"orphan"})).is_err());
        Ok(())
    }

    #[test]
    fn malformed_completion_cannot_become_success() -> Result<(), BackendError> {
        for status in ["inProgress", "unknown", ""] {
            assert!(
                notification(
                    1,
                    "turn/completed",
                    json!({"threadId":"th","turn":{"id":"tu","status":status}})
                )
                .is_err()
            );
        }
        for (status, outcome) in [
            ("completed", TurnOutcome::Completed),
            (
                "failed",
                TurnOutcome::Failed {
                    message: None,
                    details: None,
                },
            ),
            ("interrupted", TurnOutcome::Interrupted),
        ] {
            assert_eq!(
                notification(
                    1,
                    "turn/completed",
                    json!({"threadId":"th","turn":{"id":"tu","status":status}})
                )?,
                Some(AgentEvent::Finished {
                    turn: TurnRef {
                        epoch: 1,
                        thread_id: "th".into(),
                        turn_id: "tu".into()
                    },
                    outcome
                })
            );
        }
        assert_eq!(notification(1, "future/notification", json!({}))?, None);
        assert!(notification(1, "thread/archived", json!({"threadId":""})).is_err());
        Ok(())
    }
}

#[cfg(test)]
mod detail_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn failure_details_and_authoritative_plan_survive_mapping() -> Result<(), BackendError> {
        let result = notification(
            1,
            "turn/completed",
            json!({"threadId":"t","turn":{"id":"u","status":"failed","error":{"message":"failure","additionalDetails":"detail"}}}),
        )?;
        assert!(
            matches!(result,Some(AgentEvent::Finished {outcome:TurnOutcome::Failed {message:Some(message),details:Some(details)},..}) if message == "failure" && details == "detail")
        );
        let plan = notification(
            1,
            "item/completed",
            json!({"threadId":"t","turnId":"u","item":{"type":"plan","id":"i","text":"authoritative plan"}}),
        )?;
        assert!(matches!(plan,Some(AgentEvent::Plan {text,..}) if text == "authoritative plan"));
        assert!(
            notification(
                1,
                "item/completed",
                json!({"threadId":"t","turnId":"u","item":{"type":"plan","id":"i"}})
            )
            .is_err()
        );
        Ok(())
    }
}
