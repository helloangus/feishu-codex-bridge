//! Protocol contracts proven against production serialization and decoding.
//!
//! Everything here runs offline: the pinned schema snapshot and recorded
//! fixtures under `schemas/` and `fixtures/` are the only inputs, and the
//! wire tests speak over in-memory duplex streams.
use bridge_app::{
    ports::{AgentBackend, Sandbox, TurnInput, TurnRef},
    requests::{AgentReply, ApprovalKind, RequestKind},
};
use bridge_codex::{
    RpcId,
    backend::CodexBackend,
    protocol::{CODEX_SCHEMA_BASELINE, fixture_dir, schema_dir},
    requests::{decode, prepare},
    transport::Connection,
};
use bridge_core::ExecutionMode;
use serde_json::{Value, json};
use std::{error::Error, fs, path::Path, time::Duration};

/// Spawned wire tasks require `Send + Sync` errors; plain tests convert from
/// this alias through `Box<dyn Error>`, so helpers work in both contexts.
type Failure = Box<dyn Error + Send + Sync>;

fn read_json(path: impl AsRef<Path>) -> Result<Value, Failure> {
    Ok(serde_json::from_slice(&fs::read(path.as_ref())?)?)
}

fn validate(schema: &Value, instance: &Value) -> Result<(), Failure> {
    let validator = jsonschema::validator_for(schema)?;
    validator
        .validate(instance)
        .map_err(|error| error.to_string().into())
}

fn request_cases() -> Result<Vec<Value>, Failure> {
    let cases: Vec<Value> =
        serde_json::from_slice(&fs::read(fixture_dir().join("server-requests.json"))?)?;
    if cases.is_empty() {
        return Err("fixture must contain request cases".into());
    }
    Ok(cases)
}

fn drop_field(params: &Value, field: &str) -> Result<Value, Failure> {
    let mut value = params.clone();
    value
        .as_object_mut()
        .ok_or("fixture params are objects")?
        .remove(field);
    Ok(value)
}

fn approval(kind: RequestKind) -> Result<bridge_app::requests::Approval, Failure> {
    match kind {
        RequestKind::Approval(approval) => Ok(approval),
        RequestKind::Questions { .. } => Err("expected an approval request".into()),
    }
}

fn questions(kind: RequestKind) -> Result<(bool, Vec<bridge_app::requests::Question>), Failure> {
    match kind {
        RequestKind::Questions {
            blocking,
            questions,
        } => Ok((blocking, questions)),
        RequestKind::Approval(_) => Err("expected a question request".into()),
    }
}

/// Every recorded request decodes through the real entry point into the typed
/// application port, with the routing identity and decision inputs preserved.
#[test]
fn fixture_requests_decode_into_typed_application_requests() -> Result<(), Failure> {
    for case in request_cases()? {
        let method = case["method"].as_str().ok_or("missing method")?;
        let request = decode(9, method, case["params"].clone())?;
        assert_eq!(
            request.turn,
            TurnRef {
                epoch: 9,
                thread_id: "t".into(),
                turn_id: "u".into()
            }
        );
        let schema = case["schema"].as_str().ok_or("missing schema")?;
        let item = case["params"]["itemId"].as_str().ok_or("missing itemId")?;
        assert_eq!(request.item, item);
        match (schema, item) {
            ("CommandExecutionRequestApproval", "permissions") => {
                let decision = approval(request.kind)?;
                assert_eq!(decision.kind, ApprovalKind::Command);
                assert_eq!(decision.command.as_deref(), Some("fetch-and-save"));
                assert_eq!(decision.directory.as_deref(), Some("/workspace"));
                assert!(decision.permissions.is_some());
                assert!(decision.network_context.is_some());
                assert!(decision.can_allow);
            }
            ("CommandExecutionRequestApproval", "network") => {
                let decision = approval(request.kind)?;
                assert_eq!(decision.kind, ApprovalKind::Command);
                assert_eq!(decision.command, None);
                assert_eq!(decision.directory, None);
                assert!(decision.permissions.is_none());
                assert!(decision.network_context.is_some());
                assert!(decision.can_allow);
            }
            ("CommandExecutionRequestApproval", "i") => {
                let decision = approval(request.kind)?;
                assert_eq!(decision.kind, ApprovalKind::Command);
                assert_eq!(decision.command.as_deref(), Some("echo test"));
                assert!(decision.permissions.is_none());
                assert!(decision.network_context.is_none());
                assert!(decision.can_allow);
            }
            ("FileChangeRequestApproval", _) => {
                let decision = approval(request.kind)?;
                assert_eq!(decision.kind, ApprovalKind::FileChange);
                assert_eq!(decision.command, None);
                assert!(decision.can_allow);
            }
            ("ToolRequestUserInput", _) => {
                let (blocking, questions) = questions(request.kind)?;
                assert!(blocking);
                assert_eq!(questions.len(), 1);
                let question = &questions[0];
                assert_eq!(question.id, "q");
                assert_eq!(question.header, "h");
                assert_eq!(question.text, "Choose");
                assert!(question.other);
                assert!(!question.secret);
                assert!(question.options.is_empty());
            }
            other => return Err(format!("unexpected fixture case {other:?}").into()),
        }
    }
    Ok(())
}

/// Removing any field the pinned schemas mark as required must be rejected by
/// the decoder instead of silently producing a degraded application request.
#[test]
fn fixture_requests_missing_required_fields_are_rejected() -> Result<(), Failure> {
    for case in request_cases()? {
        let method = case["method"].as_str().ok_or("missing method")?;
        for field in ["threadId", "turnId", "itemId"] {
            let removed = drop_field(&case["params"], field)?;
            assert!(
                decode(9, method, removed).is_err(),
                "{method} accepted params without {field}"
            );
        }
    }
    let approvals = [
        "item/commandExecution/requestApproval",
        "item/fileChange/requestApproval",
    ];
    for method in approvals {
        let params = json!({"threadId":"t","turnId":"u","itemId":"i","startedAtMs":1});
        assert!(decode(9, method, drop_field(&params, "startedAtMs")?).is_err());
    }
    let method = "item/tool/requestUserInput";
    let params = json!({"threadId":"t","turnId":"u","itemId":"i","isBlocking":true,"questions":[
        {"id":"q","header":"h","question":"Choose"}
    ]});
    for field in ["isBlocking", "questions"] {
        assert!(decode(9, method, drop_field(&params, field)?).is_err());
    }
    let mut empty = params.clone();
    empty["questions"] = json!([]);
    assert!(decode(9, method, empty).is_err());
    assert!(decode(9, method, drop_field(&params["questions"][0], "id")?).is_err());
    assert!(decode(9, "future/requestApproval", params).is_err());
    Ok(())
}

/// The wire bytes of an outbound turn/start come from the production backend
/// serializer: they must equal the recorded fixture baseline and conform to
/// the pinned TurnStartParams schema.
#[tokio::test]
async fn turn_start_wire_params_come_from_production_serialization() -> Result<(), Failure> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client_side, server_side) = tokio::io::duplex(16 * 1024);
        let (read, write) = tokio::io::split(client_side);
        let mut connection = Connection::new(read, write, 11, 64 * 1024);
        let backend = CodexBackend::new(connection.client.clone());
        let schema = read_json(schema_dir().join("TurnStartParams.json"))?;
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
            let (read, mut write) = tokio::io::split(server_side);
            let mut lines = BufReader::new(read).lines();
            for fixture in ["turn-start-plan.json", "turn-start-default.json"] {
                let line = lines
                    .next_line()
                    .await?
                    .ok_or("missing turn/start request")?;
                let envelope: Value = serde_json::from_str(&line)?;
                assert_eq!(envelope["method"], "turn/start");
                let expected = read_json(fixture_dir().join(fixture))?;
                assert_eq!(
                    envelope["params"], expected,
                    "wire params diverged from {fixture}"
                );
                validate(&schema, &envelope["params"])?;
                let reply = json!({"id": envelope["id"], "result": {"turn": {"id": "u"}}});
                write.write_all(format!("{reply}\n").as_bytes()).await?;
                write.flush().await?;
            }
            // Hold the pipe open so client shutdown cannot race a premature EOF.
            let _ = lines.next_line().await;
            Ok::<(), Failure>(())
        });
        let mut input = TurnInput {
            thread_id: "t".into(),
            directory: "/tmp/project".into(),
            prompt: "hello".into(),
            images: vec!["/tmp/project/image.png".into()],
            model: "available-model".into(),
            mode: ExecutionMode::Plan,
            sandbox: Sandbox::WorkspaceWrite,
        };
        let turn = backend.start_turn(input.clone()).await?;
        assert_eq!(
            turn,
            TurnRef {
                epoch: 11,
                thread_id: "t".into(),
                turn_id: "u".into()
            }
        );
        input.mode = ExecutionMode::Execute;
        backend.start_turn(input).await?;
        connection.shutdown().await?;
        server.await??;
        Ok::<(), Failure>(())
    })
    .await??;
    Ok(())
}

/// An approval reply written by the production handle must be the exact
/// JSON-RPC result frame the pinned response schema describes.
#[tokio::test]
async fn approval_reply_wire_result_matches_pinned_response_schema() -> Result<(), Failure> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (client_side, server_side) = tokio::io::duplex(16 * 1024);
        let (read, write) = tokio::io::split(client_side);
        let mut connection = Connection::new(read, write, 3, 64 * 1024);
        let schema = read_json(schema_dir().join("CommandExecutionRequestApprovalResponse.json"))?;
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let (read, _) = tokio::io::split(server_side);
            let mut lines = BufReader::new(read).lines();
            let line = lines.next_line().await?.ok_or("missing approval reply")?;
            let frame: Value = serde_json::from_str(&line)?;
            assert_eq!(frame["id"], "server-q");
            assert!(frame.get("error").is_none());
            assert_eq!(frame["result"], json!({"decision":"accept"}));
            validate(&schema, &frame["result"])?;
            // Hold the pipe open so client shutdown cannot race a premature EOF.
            let _ = lines.next_line().await;
            Ok::<(), Failure>(())
        });
        let (_, handle) = prepare(
            connection.client.clone(),
            3,
            RpcId::String("server-q".into()),
            "item/commandExecution/requestApproval",
            json!({"threadId":"t","turnId":"u","itemId":"i","startedAtMs":1,"command":"echo test"}),
        )
        .await?;
        handle.reply(AgentReply::Approve(true)).await?;
        connection.shutdown().await?;
        server.await??;
        Ok::<(), Failure>(())
    })
    .await??;
    Ok(())
}

/// The pinned snapshot directory itself must carry the baseline version, so a
/// snapshot bumped without updating the shared constant cannot pass unnoticed.
#[test]
fn pinned_baseline_names_the_schema_snapshot() -> Result<(), Failure> {
    assert!(schema_dir().ends_with(CODEX_SCHEMA_BASELINE));
    assert!(fixture_dir().ends_with(CODEX_SCHEMA_BASELINE));
    let manifest = read_json(schema_dir().join("manifest.json"))?;
    assert_eq!(manifest["cli_version"], CODEX_SCHEMA_BASELINE);
    Ok(())
}
