//! Server requests and one-use responses, based on the installed CLI schema.
use crate::{RpcId, transport::RpcClient};
use bridge_app::{
    ports::{BackendError, BackendFuture, TurnRef},
    requests::{
        AgentReply, AgentRequest, Approval, ApprovalKind, Question, QuestionOption, ReplyHandle,
        RequestKind,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireRequest {
    thread_id: String,
    turn_id: String,
    item_id: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireQuestion {
    id: String,
    header: String,
    question: String,
    #[serde(default)]
    is_other: bool,
    #[serde(default)]
    is_secret: bool,
    options: Option<Vec<WireOption>>,
}
#[derive(Deserialize)]
struct WireOption {
    label: String,
    description: String,
}

fn text(value: &Value, field: &str) -> Result<Option<String>, BackendError> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err(BackendError::Incompatible),
    }
}

pub fn decode(epoch: u64, method: &str, params: Value) -> Result<AgentRequest, BackendError> {
    let wire: WireRequest =
        serde_json::from_value(params.clone()).map_err(|_| BackendError::Incompatible)?;
    if [&wire.thread_id, &wire.turn_id, &wire.item_id]
        .iter()
        .any(|id| id.trim().is_empty())
    {
        return Err(BackendError::Incompatible);
    }
    let kind = match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            if params.get("startedAtMs").and_then(Value::as_i64).is_none() {
                return Err(BackendError::Incompatible);
            }
            let command = method == "item/commandExecution/requestApproval";
            // Don't offer approval while requested permission overlays cannot
            // yet be faithfully displayed. Unsupported requests get an error.
            if command
                && ["additionalPermissions", "networkApprovalContext"]
                    .iter()
                    .any(|key| params.get(key).is_some_and(|v| !v.is_null()))
            {
                return Err(BackendError::Incompatible);
            }
            let approval_kind = if command {
                match text(&params, "kind")?.as_deref().unwrap_or("command") {
                    "command" => ApprovalKind::Command,
                    "writeStdin" => ApprovalKind::WriteStdin,
                    _ => return Err(BackendError::Incompatible),
                }
            } else {
                ApprovalKind::FileChange
            };
            let mut can_allow = true;
            if command {
                if let Some(decisions) = params.get("availableDecisions").filter(|v| !v.is_null()) {
                    let decisions = decisions.as_array().ok_or(BackendError::Incompatible)?;
                    can_allow = decisions.iter().any(|v| v == "accept");
                    if !decisions.iter().any(|v| v == "decline") {
                        return Err(BackendError::Incompatible);
                    }
                }
            }
            RequestKind::Approval(Approval {
                kind: approval_kind,
                command: text(&params, "command")?,
                directory: text(&params, "cwd")?,
                reason: text(&params, "reason")?,
                grant_root: text(&params, "grantRoot")?,
                can_allow,
            })
        }
        "item/tool/requestUserInput" => {
            let blocking = params
                .get("isBlocking")
                .and_then(Value::as_bool)
                .ok_or(BackendError::Incompatible)?;
            let questions: Vec<WireQuestion> = serde_json::from_value(
                params
                    .get("questions")
                    .cloned()
                    .ok_or(BackendError::Incompatible)?,
            )
            .map_err(|_| BackendError::Incompatible)?;
            if questions.is_empty() || questions.len() > 32 {
                return Err(BackendError::Incompatible);
            }
            let mut ids = BTreeSet::new();
            let mut result = Vec::new();
            for question in questions {
                if question.id.trim().is_empty() || !ids.insert(question.id.clone()) {
                    return Err(BackendError::Incompatible);
                }
                result.push(Question {
                    id: question.id,
                    header: question.header,
                    text: question.question,
                    other: question.is_other,
                    secret: question.is_secret,
                    options: question
                        .options
                        .unwrap_or_default()
                        .into_iter()
                        .map(|o| QuestionOption {
                            label: o.label,
                            description: o.description,
                        })
                        .collect(),
                });
            }
            RequestKind::Questions {
                blocking,
                questions: result,
            }
        }
        _ => return Err(BackendError::Incompatible),
    };
    Ok(AgentRequest {
        turn: TurnRef {
            epoch,
            thread_id: wire.thread_id,
            turn_id: wire.turn_id,
        },
        item: wire.item_id,
        kind,
    })
}

struct Reply {
    rpc: RpcClient,
    epoch: u64,
    id: RpcId,
    kind: RequestKind,
}

fn payload(kind: &RequestKind, reply: AgentReply) -> Result<Value, BackendError> {
    match (kind, reply) {
        (RequestKind::Approval(approval), AgentReply::Approve(allow)) => {
            if allow && !approval.can_allow {
                return Err(BackendError::Incompatible);
            }
            Ok(json!({"decision":if allow { "accept" } else { "decline" }}))
        }
        (RequestKind::Questions { questions, .. }, AgentReply::Answers(mut answers)) => {
            if answers
                .keys()
                .any(|id| !questions.iter().any(|q| &q.id == id))
            {
                return Err(BackendError::Incompatible);
            }
            let result: serde_json::Map<String, Value> = questions
                .iter()
                .map(|q| {
                    (
                        q.id.clone(),
                        json!({"answers":answers.remove(&q.id).unwrap_or_default()}),
                    )
                })
                .collect();
            Ok(json!({"answers":result}))
        }
        _ => Err(BackendError::Incompatible),
    }
}

impl ReplyHandle for Reply {
    fn reply(self: Box<Self>, response: AgentReply) -> BackendFuture<'static, ()> {
        Box::pin(async move {
            let value = payload(&self.kind, response)?;
            self.rpc
                .reply(self.epoch, self.id, value)
                .await
                .map_err(Into::into)
        })
    }
}

/// Unknown/malformed requests receive a fixed JSON-RPC error, never acceptance.
/// The caller must treat the returned error as a protocol failure for the turn.
pub async fn prepare(
    rpc: RpcClient,
    epoch: u64,
    id: RpcId,
    method: &str,
    params: Value,
) -> Result<(AgentRequest, Box<dyn ReplyHandle>), BackendError> {
    if epoch != rpc.epoch() {
        return Err(BackendError::Incompatible);
    }
    match decode(epoch, method, params) {
        Ok(request) => {
            let reply = Reply {
                rpc,
                epoch,
                id,
                kind: request.kind.clone(),
            };
            Ok((request, Box::new(reply)))
        }
        Err(error) => {
            rpc.reject(epoch, id).await?;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    fn approval() -> Value {
        json!({"threadId":"t","turnId":"u","itemId":"i","startedAtMs":1,"command":"echo test"})
    }
    #[test]
    fn only_supported_one_time_approval_is_emitted() -> Result<(), BackendError> {
        let request = decode(2, "item/commandExecution/requestApproval", approval())?;
        assert_eq!(
            payload(&request.kind, AgentReply::Approve(true))?,
            json!({"decision":"accept"})
        );
        assert_eq!(
            payload(&request.kind, AgentReply::Approve(false))?,
            json!({"decision":"decline"})
        );
        let mut restricted = approval();
        restricted["availableDecisions"] = json!(["decline", "acceptForSession"]);
        let request = decode(2, "item/commandExecution/requestApproval", restricted)?;
        assert!(payload(&request.kind, AgentReply::Approve(true)).is_err());
        let mut overlay = approval();
        overlay["additionalPermissions"] = json!({"network":{"enabled":true}});
        assert!(decode(2, "item/commandExecution/requestApproval", overlay).is_err());
        assert!(decode(2, "future/requestApproval", approval()).is_err());
        Ok(())
    }
    #[test]
    fn questions_keep_other_secret_and_empty_timeout_answers() -> Result<(), BackendError> {
        let params = json!({"threadId":"t","turnId":"u","itemId":"i","isBlocking":true,"questions":[{"id":"q","header":"h","question":"Choose","isOther":true,"isSecret":true,"options":null}]});
        let request = decode(2, "item/tool/requestUserInput", params.clone())?;
        assert_eq!(
            payload(&request.kind, AgentReply::Answers(BTreeMap::new()))?,
            json!({"answers":{"q":{"answers":[]}}})
        );
        assert!(
            payload(
                &request.kind,
                AgentReply::Answers(BTreeMap::from([("unknown".into(), vec![])]))
            )
            .is_err()
        );
        let mut duplicate = params;
        duplicate["questions"]
            .as_array_mut()
            .ok_or(BackendError::Incompatible)?
            .push(json!({"id":"q","header":"h","question":"second"}));
        assert!(decode(2, "item/tool/requestUserInput", duplicate).is_err());
        Ok(())
    }
}

#[cfg(test)]
mod fixture_tests {
    use super::*;
    use std::collections::BTreeMap;
    #[test]
    fn schema_checked_fixtures_are_the_payloads_used_by_rust()
    -> Result<(), Box<dyn std::error::Error>> {
        let cases: Vec<Value> = serde_json::from_str(include_str!(
            "../../../fixtures/codex/0.153.4/server-requests.json"
        ))?;
        for case in cases {
            let request = decode(
                1,
                case["method"].as_str().ok_or("missing method")?,
                case["params"].clone(),
            )?;
            let reply = match &request.kind {
                RequestKind::Approval(_) => AgentReply::Approve(false),
                RequestKind::Questions { .. } => AgentReply::Answers(BTreeMap::new()),
            };
            assert_eq!(payload(&request.kind, reply)?, case["reply"]);
        }
        Ok(())
    }
}
