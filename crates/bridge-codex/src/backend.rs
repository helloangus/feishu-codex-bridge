//! Version-local RPC shapes mapped into the application port.
use crate::transport::{RpcClient, RpcError};
use bridge_app::ports::{
    AgentBackend, BackendError, BackendFuture, Model, Sandbox, ThreadSummary, TurnInput, TurnRef,
};
use bridge_core::ExecutionMode;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};

#[derive(Clone)]
pub struct CodexBackend {
    rpc: RpcClient,
}

impl From<RpcError> for BackendError {
    fn from(error: RpcError) -> Self {
        match error {
            RpcError::Timeout => Self::Uncertain,
            RpcError::Closed | RpcError::Overloaded => Self::Disconnected,
            RpcError::Protocol | RpcError::StaleConnection => Self::Incompatible,
            RpcError::Remote { code } => Self::Rejected(code),
        }
    }
}

#[derive(Deserialize)]
struct WireThread {
    id: String,
    name: Option<String>,
    title: Option<String>,
    preview: Option<String>,
    cwd: Option<PathBuf>,
    status: Option<Value>,
}

fn thread(value: Value) -> Result<ThreadSummary, BackendError> {
    let wire: WireThread = serde_json::from_value(value).map_err(|_| BackendError::Incompatible)?;
    if wire.id.is_empty() {
        return Err(BackendError::Incompatible);
    }
    let title = [wire.name, wire.title, wire.preview]
        .into_iter()
        .flatten()
        .find(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "未命名".into());
    let active = wire
        .status
        .as_ref()
        .and_then(|s| s.get("type"))
        .and_then(Value::as_str)
        == Some("active");
    Ok(ThreadSummary {
        id: wire.id,
        title,
        directory: wire.cwd,
        active,
    })
}

impl CodexBackend {
    /// Read every archive page before returning evidence for local reconciliation.
    /// Missing IDs are never interpreted as proof that a thread is active.
    pub async fn archived_bindings(
        &self,
        bindings: &std::collections::BTreeSet<String>,
    ) -> Result<std::collections::BTreeSet<String>, BackendError> {
        use std::collections::BTreeSet;
        if bindings.is_empty() {
            return Ok(BTreeSet::new());
        }
        if bindings
            .iter()
            .any(|id| !bridge_app::sessions::valid_thread_id(id))
        {
            return Err(BackendError::Incompatible);
        }
        let mut found = BTreeSet::new();
        let mut cursors = BTreeSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..100 {
            let result = self.call("thread/list", json!({
                "archived":true, "cursor":cursor, "limit":100,
                "sortKey":"updated_at", "sortDirection":"desc", "modelProviders":[],
                "sourceKinds":["cli","vscode","exec","appServer","subAgent",
                    "subAgentReview","subAgentCompact","subAgentThreadSpawn","subAgentOther","unknown"]
            })).await?;
            let data = result
                .get("data")
                .and_then(Value::as_array)
                .ok_or(BackendError::Incompatible)?;
            if data.len() > 100 {
                return Err(BackendError::Incompatible);
            }
            for entry in data {
                let id = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or(BackendError::Incompatible)?;
                if !bridge_app::sessions::valid_thread_id(id) {
                    return Err(BackendError::Incompatible);
                }
                if bindings.contains(id) {
                    found.insert(id.to_owned());
                }
            }
            match result.get("nextCursor") {
                Some(Value::Null) => return Ok(found),
                Some(Value::String(next)) if !next.is_empty() && next.len() <= 4096 => {
                    if !cursors.insert(next.clone()) {
                        return Err(BackendError::Incompatible);
                    }
                    cursor = Some(next.clone());
                }
                _ => return Err(BackendError::Incompatible),
            }
        }
        Err(BackendError::Incompatible)
    }

    pub fn new(rpc: RpcClient) -> Self {
        Self { rpc }
    }
    pub async fn initialize(&self) -> Result<(), BackendError> {
        self.call("initialize", json!({"clientInfo":{"name":"feishu-codex-bridge", "version":env!("CARGO_PKG_VERSION")}, "capabilities":{"experimentalApi":true}})).await?;
        self.rpc.notify("initialized", json!({})).await?;
        Ok(())
    }
    async fn call(&self, method: &str, params: Value) -> Result<Value, BackendError> {
        Ok(self
            .rpc
            .request(method, params, Duration::from_secs(30))
            .await?)
    }
}

impl AgentBackend for CodexBackend {
    fn models(&self) -> BackendFuture<'_, Vec<Model>> {
        Box::pin(async move {
            let result = self.call("model/list", json!({})).await?;
            result
                .get("data")
                .and_then(Value::as_array)
                .ok_or(BackendError::Incompatible)?
                .iter()
                .map(|item| {
                    let id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .ok_or(BackendError::Incompatible)?;
                    Ok(Model {
                        id: id.into(),
                        is_default: item
                            .get("isDefault")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                })
                .collect()
        })
    }
    fn threads(&self, directory: PathBuf, archived: bool) -> BackendFuture<'_, Vec<ThreadSummary>> {
        Box::pin(async move {
            let result = self.call("thread/list", json!({"cwd":[directory],"archived":archived,"limit":20,"sortKey":"updated_at","sortDirection":"desc"})).await?;
            result
                .get("data")
                .and_then(Value::as_array)
                .ok_or(BackendError::Incompatible)?
                .iter()
                .cloned()
                .map(thread)
                .collect()
        })
    }
    fn read_thread(&self, id: String) -> BackendFuture<'_, ThreadSummary> {
        Box::pin(async move {
            thread(
                self.call("thread/read", json!({"threadId":id,"includeTurns":false}))
                    .await?
                    .get("thread")
                    .cloned()
                    .ok_or(BackendError::Incompatible)?,
            )
        })
    }
    fn start_thread(&self, directory: PathBuf) -> BackendFuture<'_, ThreadSummary> {
        Box::pin(async move {
            thread(
                self.call("thread/start", json!({"cwd":directory}))
                    .await?
                    .get("thread")
                    .cloned()
                    .ok_or(BackendError::Incompatible)?,
            )
        })
    }
    fn resume_thread(&self, id: String, directory: PathBuf) -> BackendFuture<'_, ThreadSummary> {
        Box::pin(async move {
            thread(
                self.call("thread/resume", json!({"threadId":id,"cwd":directory}))
                    .await?
                    .get("thread")
                    .cloned()
                    .ok_or(BackendError::Incompatible)?,
            )
        })
    }
    fn archive_thread(&self, id: String, archived: bool) -> BackendFuture<'_, ()> {
        Box::pin(async move {
            self.call(
                if archived {
                    "thread/archive"
                } else {
                    "thread/unarchive"
                },
                json!({"threadId":id}),
            )
            .await?;
            Ok(())
        })
    }
    fn compact(&self, id: String) -> BackendFuture<'_, ()> {
        Box::pin(async move {
            let result = self
                .call("thread/compact/start", json!({"threadId":id}))
                .await?;
            if !result.is_object() {
                return Err(BackendError::Incompatible);
            }
            Ok(())
        })
    }
    fn start_turn(&self, input: TurnInput) -> BackendFuture<'_, TurnRef> {
        Box::pin(async move {
            let params = turn_params(&input)?;
            let result = self.call("turn/start", params).await?;
            let id = result
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or(BackendError::Incompatible)?;
            Ok(TurnRef {
                thread_id: input.thread_id,
                turn_id: id.into(),
                epoch: self.rpc.epoch(),
            })
        })
    }
    fn interrupt(&self, turn: TurnRef) -> BackendFuture<'_, ()> {
        Box::pin(async move {
            if turn.epoch != self.rpc.epoch() {
                return Err(BackendError::Incompatible);
            }
            self.call(
                "turn/interrupt",
                json!({"threadId":turn.thread_id,"turnId":turn.turn_id}),
            )
            .await?;
            Ok(())
        })
    }
}

fn turn_params(input: &TurnInput) -> Result<Value, BackendError> {
    // Default collaboration mode needs a real selected/default model too.
    if input.model.is_empty()
        || input.thread_id.is_empty()
        || !input.directory.is_absolute()
        || input.images.iter().any(|path| !path.is_absolute())
    {
        return Err(BackendError::Incompatible);
    }
    let mut content = vec![json!({"type":"text","text":input.prompt})];
    content.extend(
        input
            .images
            .iter()
            .map(|path| json!({"type":"localImage","path":path,"detail":"auto"})),
    );
    let sandbox = match input.sandbox {
        Sandbox::WorkspaceWrite => {
            json!({"type":"workspaceWrite","writableRoots":[input.directory],"networkAccess":false})
        }
        Sandbox::DangerFullAccess => json!({"type":"dangerFullAccess"}),
    };
    Ok(
        json!({"threadId":input.thread_id,"cwd":input.directory,"input":content,"model":input.model,
            "approvalPolicy":"on-request","sandboxPolicy":sandbox,
            "collaborationMode":{"mode":if input.mode == ExecutionMode::Plan { "plan" } else { "default" },"settings":{"model":input.model}}
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_turn_has_explicit_mode_and_sandbox() -> Result<(), BackendError> {
        let mut input = TurnInput {
            thread_id: "t".into(),
            directory: "/tmp/project".into(),
            prompt: "hello".into(),
            images: vec!["/tmp/project/image.png".into()],
            model: "available-model".into(),
            mode: ExecutionMode::Plan,
            sandbox: Sandbox::WorkspaceWrite,
        };
        let plan = turn_params(&input)?;
        let expected: Value = serde_json::from_str(include_str!(
            "../../../fixtures/codex/0.153.4/turn-start-plan.json"
        ))
        .map_err(|_| BackendError::Incompatible)?;
        assert_eq!(plan, expected);
        assert_eq!(plan["collaborationMode"]["mode"], "plan");
        assert_eq!(plan["sandboxPolicy"]["networkAccess"], false);
        assert_eq!(plan["approvalPolicy"], "on-request");
        assert_eq!(plan["input"][1]["type"], "localImage");
        input.mode = ExecutionMode::Execute;
        let expected: Value = serde_json::from_str(include_str!(
            "../../../fixtures/codex/0.153.4/turn-start-default.json"
        ))
        .map_err(|_| BackendError::Incompatible)?;
        assert_eq!(turn_params(&input)?, expected);
        assert_eq!(turn_params(&input)?["collaborationMode"]["mode"], "default");
        input.sandbox = Sandbox::DangerFullAccess;
        assert_eq!(
            turn_params(&input)?["sandboxPolicy"],
            json!({"type":"dangerFullAccess"})
        );
        input.model.clear();
        assert!(turn_params(&input).is_err());
        Ok(())
    }
    #[test]
    fn thread_mapping_does_not_require_all_vendor_fields() -> Result<(), BackendError> {
        let item = thread(
            json!({"id":"t","name":" ","preview":"fallback","cwd":"/tmp","status":{"type":"active"},"future":true}),
        )?;
        assert_eq!(item.title, "fallback");
        assert!(item.active);
        assert!(thread(json!({"id":""})).is_err());
        Ok(())
    }
}
