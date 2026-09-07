//! Notifications mapped against the 0.153.4 schema; no current-task fallback.
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

/// Only call for notifications. Requests must retain their ID for an explicit
/// response; ignoring an unknown notification never permits ignoring a request.
/// The application still compares the full TurnRef to its active execution.
pub fn notification(
    epoch: u64,
    method: &str,
    params: Value,
) -> Result<Option<AgentEvent>, BackendError> {
    Ok(Some(match method {
        "item/agentMessage/delta" => {
            let data: Delta = parse(params)?;
            AgentEvent::Output {
                turn: TurnRef {
                    epoch,
                    thread_id: identifier(data.thread_id)?,
                    turn_id: identifier(data.turn_id)?,
                },
                item: identifier(data.item_id)?,
                delta: data.delta,
            }
        }
        "turn/completed" => {
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
            AgentEvent::Finished {
                turn: TurnRef {
                    epoch,
                    thread_id: identifier(data.thread_id)?,
                    turn_id: identifier(data.turn.id)?,
                },
                outcome,
            }
        }
        "item/completed" => {
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
            AgentEvent::Plan {
                turn: TurnRef {
                    epoch,
                    thread_id: identifier(data.thread_id)?,
                    turn_id: identifier(data.turn_id)?,
                },
                item: identifier(plan.id)?,
                text: plan.text,
            }
        }
        "thread/archived" => {
            let data: Archived = parse(params)?;
            AgentEvent::Archived {
                epoch,
                thread: identifier(data.thread_id)?,
            }
        }
        _ => return Ok(None),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
