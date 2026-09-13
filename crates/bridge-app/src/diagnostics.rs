//! Metadata-only diagnostics. Callers cannot pass prompts, answers or remote errors.
use std::{
    io::Write,
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Copy)]
pub enum Event {
    RuntimeStarted,
    ConnectionStarting,
    ConnectionEstablished,
    ConnectionReconnecting,
    ArtifactSent,
    AnswerDelivered,
    TaskPrepared,
    TaskStarted,
    TaskFinished,
    FilesPrepared,
    FilesFinished,
    QuestionReceived,
    QuestionSent,
    AnswerReturned,
    RuntimeExit,
    Panic,
    Reconnect,
    HeartbeatTimeout,
}

pub trait Sink: Send + Sync {
    fn write(&self, record: &str, panic: bool) -> std::io::Result<()>;
}
static SINK: OnceLock<Box<dyn Sink>> = OnceLock::new();

pub fn install(sink: Box<dyn Sink>) -> Result<(), Box<dyn Sink>> {
    SINK.set(sink)
}
#[derive(Debug, Clone, Copy)]
pub enum Status {
    Ok,
    Failed,
    Rejected,
    Transport,
    Protocol,
    Authentication,
    Proxy,
    Overloaded,
    Exhausted,
}

fn record(event: Event, status: Status, task: Option<&str>, count: usize) -> String {
    // Task identifiers are generated internally; do not serialize arbitrary strings.
    let task = task
        .filter(|id| {
            !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_digit() || b == b':')
        })
        .unwrap_or("");
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!(
        "{{\"event\":\"{event:?}\",\"status\":\"{status:?}\",\"unix_ms\":{time},\"pid\":{},\"task\":\"{task}\",\"count\":{count}}}",
        std::process::id()
    )
}

pub fn emit(event: Event, status: Status, task: Option<&str>, count: usize) {
    let record = record(event, status, task, count);
    if let Some(sink) = SINK.get() {
        if sink.write(&record, matches!(event, Event::Panic)).is_err() {
            let _ = writeln!(
                std::io::stderr().lock(),
                "{{\"event\":\"log_write_failed\"}}"
            );
        }
    }
    // Logging failures must not panic while reporting another failure.
    let _ = writeln!(std::io::stderr().lock(), "{}", record);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostics_reject_payloads_and_preserve_internal_task_ids() {
        let valid = record(Event::QuestionSent, Status::Ok, Some("123:4"), 2);
        assert!(valid.contains("\"task\":\"123:4\""));
        assert!(valid.contains("\"count\":2"));
        for value in ["secret", "123\nforged", "\"token\"", "", &"1".repeat(65)] {
            assert!(
                record(Event::QuestionSent, Status::Failed, Some(value), 0)
                    .contains("\"task\":\"\"")
            );
        }
    }
}
