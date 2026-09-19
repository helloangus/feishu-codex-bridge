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
            let (permissions, permissions_supported) = if command {
                crate::permissions::profile(params.get("additionalPermissions"))?
            } else {
                (None, true)
            };
            let network_context = if command {
                crate::permissions::context(params.get("networkApprovalContext"))?
            } else {
                None
            };
            let approval_kind = if command {
                match text(&params, "kind")?.as_deref().unwrap_or("command") {
                    "command" => ApprovalKind::Command,
                    "writeStdin" => ApprovalKind::WriteStdin,
                    _ => return Err(BackendError::Incompatible),
                }
            } else {
                ApprovalKind::FileChange
            };
            // Remote execution environments are not represented by this port.
            let mut can_allow =
                text(&params, "environmentId")?.is_none() && text(&params, "grantRoot")?.is_none();
            can_allow &=
                permissions_supported && network_context.as_ref().is_none_or(|v| v.len() <= 4096);
            if command {
                if let Some(decisions) = params.get("availableDecisions").filter(|v| !v.is_null()) {
                    let decisions = decisions.as_array().ok_or(BackendError::Incompatible)?;
                    can_allow &= decisions.iter().any(|v| v == "accept");
                    if !decisions.iter().any(|v| v == "decline") {
                        return Err(BackendError::Incompatible);
                    }
                }
            }
            RequestKind::Approval(Approval {
                permissions,
                network_context,
                changes: None,
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
        let mut remote = approval();
        remote["environmentId"] = json!("remote");
        remote["availableDecisions"] = json!(["accept", "decline"]);
        let request = decode(2, "item/commandExecution/requestApproval", remote)?;
        assert!(payload(&request.kind, AgentReply::Approve(true)).is_err());
        let mut overlay = approval();
        overlay["additionalPermissions"] = json!({"network":{"enabled":true}});
        let request = decode(2, "item/commandExecution/requestApproval", overlay)?;
        assert_eq!(
            payload(&request.kind, AgentReply::Approve(true))?,
            json!({"decision":"accept"})
        );
        assert!(decode(2, "future/requestApproval", approval()).is_err());
        Ok(())
    }
    #[test]
    fn permission_reply_never_emits_session_or_policy_authorization() -> Result<(), BackendError> {
        for (extra, allowed) in [
            (
                json!({"additionalPermissions":{"fileSystem":{"write":["/output"]}},"networkApprovalContext":{"host":"example.test","protocol":"https"}}),
                true,
            ),
            (
                json!({"additionalPermissions":{"network":{"enabled":false}},"availableDecisions":["acceptForSession","decline"]}),
                false,
            ),
            (
                json!({"networkApprovalContext":{"host":"x".repeat(4096),"protocol":"http"}}),
                false,
            ),
            (
                json!({"additionalPermissions":{"fileSystem":{"entries":[{"access":"write","path":{"type":"special","value":{"kind":"unknown","path":"future"}}}]}}}),
                false,
            ),
        ] {
            let mut params = approval();
            for (key, value) in extra.as_object().ok_or(BackendError::Incompatible)? {
                params[key] = value.clone();
            }
            params["proposedNetworkPolicyAmendments"] =
                json!([{"action":"allow","host":"example.test"}]);
            params["proposedExecpolicyAmendment"] = json!(["echo"]);
            let request = decode(2, "item/commandExecution/requestApproval", params)?;
            assert_eq!(
                payload(&request.kind, AgentReply::Approve(false))?,
                json!({"decision":"decline"})
            );
            let response = payload(&request.kind, AgentReply::Approve(true));
            if allowed {
                assert_eq!(response?, json!({"decision":"accept"}));
            } else {
                assert!(response.is_err());
            }
        }
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
    use crate::protocol::{fixture_dir, testing};
    use std::{collections::BTreeMap, error::Error, fs};
    /// The recorded request fixtures drive the real decoder, and the replies
    /// the production mapping produces must stay equal to the recorded baseline
    /// and conform to the pinned response schemas.
    #[test]
    fn schema_checked_fixtures_are_the_payloads_used_by_rust() -> Result<(), Box<dyn Error>> {
        let cases: Vec<Value> =
            serde_json::from_slice(&fs::read(fixture_dir().join("server-requests.json"))?)?;
        for case in &cases {
            let schema = case["schema"].as_str().ok_or("missing schema")?;
            let request = decode(
                1,
                case["method"].as_str().ok_or("missing method")?,
                case["params"].clone(),
            )?;
            let reply = match &request.kind {
                RequestKind::Approval(_) => AgentReply::Approve(false),
                RequestKind::Questions { .. } => AgentReply::Answers(BTreeMap::new()),
            };
            let produced = payload(&request.kind, reply)?;
            testing::validate(&format!("{schema}Params.json"), &case["params"])?;
            testing::validate(&format!("{schema}Response.json"), &produced)?;
            assert_eq!(produced, case["reply"]);
        }
        Ok(())
    }
}

#[cfg(test)]
mod response_tests {
    use super::*;
    use crate::protocol::testing;
    use std::{collections::BTreeMap, error::Error};
    fn command_approval(extra: Value) -> Result<AgentRequest, BackendError> {
        let mut params =
            json!({"threadId":"t","turnId":"u","itemId":"i","startedAtMs":1,"command":"echo test"});
        let fields = extra.as_object().ok_or(BackendError::Incompatible)?;
        for (key, value) in fields {
            params[key] = value.clone();
        }
        decode(2, "item/commandExecution/requestApproval", params)
    }
    /// Approval replies are emitted only as plain accept/decline; the pinned
    /// response schema validates every wire payload the mapping can produce.
    /// `acceptForSession` is schema-valid but intentionally unreachable.
    #[test]
    fn approval_reply_payloads_stay_within_pinned_response_schemas() -> Result<(), Box<dyn Error>> {
        let request = command_approval(json!({}))?;
        let accept = payload(&request.kind, AgentReply::Approve(true))?;
        assert_eq!(accept, json!({"decision":"accept"}));
        testing::validate("CommandExecutionRequestApprovalResponse.json", &accept)?;
        let decline = payload(&request.kind, AgentReply::Approve(false))?;
        assert_eq!(decline, json!({"decision":"decline"}));
        testing::validate("CommandExecutionRequestApprovalResponse.json", &decline)?;
        let write_stdin = command_approval(json!({"kind":"writeStdin"}))?;
        let decline = payload(&write_stdin.kind, AgentReply::Approve(false))?;
        testing::validate("CommandExecutionRequestApprovalResponse.json", &decline)?;
        let file_change = decode(
            2,
            "item/fileChange/requestApproval",
            json!({"threadId":"t","turnId":"u","itemId":"i","startedAtMs":1}),
        )?;
        let decline = payload(&file_change.kind, AgentReply::Approve(false))?;
        assert_eq!(decline, json!({"decision":"decline"}));
        testing::validate("FileChangeRequestApprovalResponse.json", &decline)?;
        // Schema-valid session-scoped decisions exist, but no AgentReply maps
        // to them, and sessions without the plain accept decision stay
        // decline-only instead of being approved.
        testing::validate(
            "CommandExecutionRequestApprovalResponse.json",
            &json!({"decision":"acceptForSession"}),
        )?;
        let session_only =
            command_approval(json!({"availableDecisions":["acceptForSession","decline"]}))?;
        assert!(payload(&session_only.kind, AgentReply::Approve(true)).is_err());
        let decline = payload(&session_only.kind, AgentReply::Approve(false))?;
        testing::validate("CommandExecutionRequestApprovalResponse.json", &decline)?;
        Ok(())
    }
    /// Per-question answers, including free-text answers for `other` fields,
    /// serialize into the pinned experimental response schema.
    #[test]
    fn question_answer_payloads_stay_within_pinned_response_schema() -> Result<(), Box<dyn Error>> {
        let request = decode(
            2,
            "item/tool/requestUserInput",
            json!({"threadId":"t","turnId":"u","itemId":"i","isBlocking":true,"questions":[
                {"id":"q","header":"h","question":"Choose","options":[{"label":"a","description":"first"}]},
                {"id":"r","header":"h","question":"Describe","isOther":true}
            ]}),
        )?;
        let answers = AgentReply::Answers(BTreeMap::from([
            ("q".into(), vec!["a".into()]),
            ("r".into(), vec!["自由文本".into()]),
        ]));
        let produced = payload(&request.kind, answers)?;
        assert_eq!(
            produced,
            json!({"answers":{"q":{"answers":["a"]},"r":{"answers":["自由文本"]}}})
        );
        testing::validate("ToolRequestUserInputResponse.json", &produced)?;
        // Unanswered questions still produce an empty, schema-valid answer list.
        let produced = payload(&request.kind, AgentReply::Answers(BTreeMap::new()))?;
        testing::validate("ToolRequestUserInputResponse.json", &produced)?;
        assert_eq!(
            produced,
            json!({"answers":{"q":{"answers":[]},"r":{"answers":[]}}})
        );
        Ok(())
    }
}
