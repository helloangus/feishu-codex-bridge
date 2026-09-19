//! Fake Codex app-server process for behavior tests; `argv[1]` selects the
//! scenario, `argv[2]` (for the `compact` scenario) the compaction mode.
//!
//! Scenario → behavior → signal files (written into the process CWD, which
//! tests set to a per-case temp directory):
//!
//! | scenario        | behavior                                        | signals               |
//! |-----------------|-------------------------------------------------|-----------------------|
//! | `thread`        | normal turn; echoes thread `main`               | `started`             |
//! | `storage`       | implemented by the *test*, not this binary: the | (none)                |
//! |                 | test pre-replaces `seen-messages.json` with a   |                       |
//! |                 | directory so persistence fails (see             |                       |
//! |                 | `test-support/tests/runtime_flows.rs`)          |                       |
//! | `hang`          | never answers `startTurn`                       | `holding`             |
//! | `slow`          | delays replies (slow transport)                 | `started`             |
//! | `archive`       | emits `thread/archived` notifications           | `archive-actions`     |
//! | `flood`         | emits a burst of events before the reply        | `interrupts.jsonl`    |
//! | `compact <mode>`| compaction variants below                       | `preparation`,        |
//! |                 |                                                 | `compactions`,        |
//! |                 |                                                 | `compact-interrupts`  |
//!
//! `compact` modes: `wrong_resume` (resume returns a different thread id),
//! `foreign` (thread cwd is outside the workspace), `active` (thread reports
//! busy), `prepare_stop` (stop arrives between prepare and submit),
//! `uncertain` (submit fails with an uncertain transport error), `rejected`
//! (submit is refused), `early` (events arrive before the turn binds),
//! `failed` (prepare fails). The authoritative consumer of these behaviors is
//! `test-support/tests/runtime_flows.rs`; add new modes in both places.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    io::{self, BufRead, Write},
};

fn emit(value: Value) -> io::Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, &value)?;
    out.write_all(b"\n")?;
    out.flush()
}

fn append(path: &str, text: &str) -> io::Result<()> {
    use std::fs::OpenOptions;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(text.as_bytes())
}

fn models() -> Value {
    json!({"data":[{"id":"gpt-5.6-luna","isDefault":false},{"id":"selected","isDefault":false},{"id":"chosen","isDefault":false}]})
}

fn compact(mode: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut pending = false;
    let mut preparation: Option<Value> = None;
    for line in io::stdin().lock().lines() {
        let message: Value = serde_json::from_str(&line?)?;
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        if method == "initialized" {
            continue;
        }
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => json!({}),
            "thread/read" | "thread/resume" => {
                if message.pointer("/params/threadId") != Some(&json!("thread")) {
                    return Err("wrong thread".into());
                }
                let thread = if mode == "wrong_resume" && method == "thread/resume" {
                    "wrong"
                } else {
                    "thread"
                };
                let cwd = if mode == "foreign" {
                    "/foreign".into()
                } else {
                    env::current_dir()?.to_string_lossy().into_owned()
                };
                let result = json!({"thread":{"id":thread,"cwd":cwd,"status":{"type":if mode == "active" {"active"} else {"idle"}}}});
                append("preparation", &format!("{method}\n"))?;
                if mode == "prepare_stop" && method == "thread/read" {
                    preparation = Some(json!({"id":id,"result":result}));
                    continue;
                }
                result
            }
            "thread/compact/start" => {
                append("compactions", "compact\n")?;
                if mode == "rejected" {
                    emit(json!({"id":id,"error":{"code":-32000,"message":"rejected"}}))?;
                    continue;
                }
                if mode == "uncertain" {
                    Value::Null
                } else {
                    pending = true;
                    emit(
                        json!({"method":"turn/started","params":{"threadId":"unrelated","turn":{"id":"foreign","status":"inProgress"}}}),
                    )?;
                    emit(
                        json!({"method":"turn/started","params":{"threadId":"thread","turn":{"id":"compact","status":"inProgress"}}}),
                    )?;
                    emit(
                        json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"stale","status":"completed","error":null}}}),
                    )?;
                    if mode == "early" {
                        emit(finished("completed"))?;
                        pending = false;
                    }
                    json!({})
                }
            }
            "model/list" => {
                if let Some(value) = preparation.take() {
                    emit(value)?;
                }
                if pending {
                    emit(finished(if mode == "failed" {
                        "failed"
                    } else {
                        "completed"
                    }))?;
                    pending = false;
                }
                models()
            }
            "turn/interrupt" => {
                append("compact-interrupts", "interrupt\n")?;
                emit(finished("interrupted"))?;
                pending = false;
                json!({})
            }
            _ => return Err(format!("unexpected method {method}").into()),
        };
        emit(json!({"id":id,"result":result}))?;
    }
    Ok(())
}

fn finished(status: &str) -> Value {
    json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"compact","status":status,"error":if status == "failed" {json!({"message":"fake compact error"})} else {Value::Null}}}})
}

fn runtime() -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?.to_string_lossy().into_owned();
    let mut turn = 0_u64;
    let mut archived = false;
    for line in io::stdin().lock().lines() {
        let message: Value = serde_json::from_str(&line?)?;
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        if method == "initialized" {
            continue;
        }
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => json!({}),
            "model/list" => models(),
            "thread/start" | "thread/resume" => json!({"thread":{"id":"thread","cwd":cwd}}),
            "thread/read" => json!({"thread":{"id":"thread","cwd":cwd,"status":{"type":"idle"}}}),
            "thread/list" => {
                let requested = message
                    .pointer("/params/archived")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                json!({"data":if requested == archived {json!([{"id":"thread","cwd":cwd,"title":"local session"},{"id":"foreign","cwd":"/foreign","title":"must not display"}])} else {json!([])}})
            }
            "thread/archive" | "thread/unarchive" => {
                archived = method == "thread/archive";
                append("archive-actions", &format!("{method}\n"))?;
                json!({})
            }
            "turn/start" => {
                turn += 1;
                if turn == 1 {
                    emit(
                        json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"1","itemId":"i","delta":"fake answer"}}),
                    )?;
                    emit(
                        json!({"method":"item/completed","params":{"threadId":"thread","turnId":"1","item":{"id":"plan","type":"plan","text":"authoritative plan"}}}),
                    )?;
                    emit(
                        json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":"1","status":"completed"}}}),
                    )?;
                }
                json!({"turn":{"id":turn.to_string()}})
            }
            "turn/interrupt" => {
                append(
                    "interrupts.jsonl",
                    "{\"threadId\":\"thread\",\"turnId\":\"2\"}\n",
                )?;
                emit(
                    json!({"method":"turn/completed","params":{"threadId":"thread","turn":{"id":turn.to_string(),"status":"interrupted"}}}),
                )?;
                json!({})
            }
            _ => return Err(format!("unexpected method {method}").into()),
        };
        emit(json!({"id":id,"result":result}))?;
        if method == "turn/start" && turn == 2 {
            fs::write("started", "2")?;
        }
    }
    Ok(())
}

fn directory() -> Result<(), Box<dyn std::error::Error>> {
    use std::collections::BTreeMap;
    let mut threads = BTreeMap::<String, String>::new();
    let mut counter = 0_u64;
    for line in io::stdin().lock().lines() {
        let message: Value = serde_json::from_str(&line?)?;
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        if method == "initialized" {
            continue;
        }
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        let result = match method {
            "initialize" => json!({}),
            "model/list" => models(),
            "thread/start" | "thread/resume" => {
                let cwd = params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .ok_or("missing cwd")?
                    .to_owned();
                let thread = format!("th-{:x}", Sha256::digest(cwd.as_bytes()))[..11].to_owned();
                if method == "thread/resume" && params.get("threadId") != Some(&json!(thread)) {
                    return Err("wrong resumed thread".into());
                }
                threads.insert(thread.clone(), cwd.clone());
                json!({"thread":{"id":thread,"cwd":cwd,"status":{"type":"idle"}}})
            }
            "turn/start" => {
                counter += 1;
                let thread = params
                    .get("threadId")
                    .and_then(Value::as_str)
                    .ok_or("missing thread")?;
                let cwd = threads.get(thread).ok_or("unknown thread")?;
                let summary = format!(
                    "{}|{}|{}",
                    cwd,
                    params.get("model").and_then(Value::as_str).unwrap_or(""),
                    params
                        .pointer("/collaborationMode/mode")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                );
                emit(
                    json!({"method":"item/agentMessage/delta","params":{"threadId":thread,"turnId":counter.to_string(),"itemId":"i","delta":summary}}),
                )?;
                if params.pointer("/input/0/text") != Some(&json!("hold")) {
                    emit(
                        json!({"method":"turn/completed","params":{"threadId":thread,"turn":{"id":counter.to_string(),"status":"completed"}}}),
                    )?;
                } else {
                    fs::write("holding", "yes")?;
                }
                json!({"turn":{"id":counter.to_string()}})
            }
            "turn/interrupt" => {
                emit(
                    json!({"method":"turn/completed","params":{"threadId":params["threadId"],"turn":{"id":params["turnId"],"status":"interrupted"}}}),
                )?;
                json!({})
            }
            _ => return Err(format!("unexpected method {method}").into()),
        };
        emit(json!({"id":id,"result":result}))?;
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("compact") => compact(&args.next().ok_or("missing compact mode")?),
        Some("runtime") => runtime(),
        Some("directory") => directory(),
        _ => Err("unknown fake mode".into()),
    }
}
