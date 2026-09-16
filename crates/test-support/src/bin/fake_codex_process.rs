use serde_json::{Value, json};
use std::{
    env, fs,
    io::{self, BufRead, Write},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

fn emit(value: Value) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, &value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

fn request_loop(mode: &str) -> Result<(), Box<dyn std::error::Error>> {
    for line in io::stdin().lock().lines() {
        let message: Value = serde_json::from_str(&line?)?;
        let Some(id) = message.get("id").cloned() else {
            if mode == "events" && message.get("method") == Some(&json!("initialized")) {
                emit(
                    json!({"id":"approve-string","method":"item/fileChange/requestApproval","params":{"threadId":"t","turnId":"u","itemId":"i","startedAtMs":1}}),
                )?;
            }
            continue;
        };
        if mode == "events" && id == json!("approve-string") {
            if message.pointer("/result/decision") != Some(&json!("decline")) {
                return Err("unexpected approval decision".into());
            }
            emit(
                json!({"method":"turn/completed","params":{"threadId":"t","turn":{"id":"u","status":"interrupted"}}}),
            )?;
            continue;
        }
        let result = if message.get("method") == Some(&json!("model/list")) {
            json!({"data":[{"id":"fake-model","isDefault":true}]})
        } else {
            json!({})
        };
        emit(json!({"id":id,"result":result}))?;
    }
    Ok(())
}

fn parent(pid_file: &str) -> Result<(), Box<dyn std::error::Error>> {
    let child = Command::new(env::current_exe()?)
        .arg("tool")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    fs::write(pid_file, child.id().to_string())?;
    request_loop("simple")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("simple") => request_loop("simple"),
        Some("events") => request_loop("events"),
        Some("parent") => parent(&args.next().ok_or("missing pid file")?),
        Some("tool") => {
            thread::sleep(Duration::from_secs(30));
            Ok(())
        }
        _ => Err("unknown fake mode".into()),
    }
}
