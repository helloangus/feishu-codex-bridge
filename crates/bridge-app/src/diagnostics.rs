//! Metadata-only diagnostics. Callers cannot pass prompts, answers or remote errors.
use std::{
    io::Write,
    sync::Arc,
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
    DeliveryFailed,
    CardFailed,
    Overloaded,
    HealthWriteFailed,
    ArchiveReconciled,
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

/// Cloneable diagnostics handle owned by one assembled runtime. A missing
/// sink records nothing, which is the default for tests and early startup.
#[derive(Clone, Default)]
pub struct Diagnostics {
    sink: Option<Arc<dyn Sink>>,
}

impl Diagnostics {
    /// Bind the persistent infrastructure sink for one run.
    pub fn new(sink: Box<dyn Sink>) -> Self {
        Self {
            sink: Some(Arc::from(sink)),
        }
    }
    /// Discard every record; used by tests and components without a run.
    pub fn noop() -> Self {
        Self { sink: None }
    }
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

impl Diagnostics {
    pub fn emit(&self, event: Event, status: Status, task: Option<&str>, count: usize) {
        let record = record(event, status, task, count);
        if let Some(sink) = &self.sink {
            if sink.write(&record, matches!(event, Event::Panic)).is_err() {
                // The terminal is for human-facing command output.  Do not
                // expose machine-oriented records there, even when persistent
                // logging fails.
                let _ = writeln!(std::io::stderr().lock(), "警告：无法写入运行诊断日志。");
            }
        }
    }
}

/// Bounded record for failures before any sink exists: only a static stage
/// name and process metadata reach the terminal, never the error text.
pub fn startup_record(stage: &str) {
    let stage_ok = !stage.is_empty()
        && stage.len() <= 32
        && stage.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let _ = writeln!(
        std::io::stderr().lock(),
        "{{\"event\":\"startup_failed\",\"stage\":\"{}\",\"unix_ms\":{time},\"pid\":{}}}",
        if stage_ok { stage } else { "unknown" },
        std::process::id()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failing_sinks_never_panic_and_missing_sinks_record_nothing() {
        struct Failing;
        impl Sink for Failing {
            fn write(&self, _: &str, _: bool) -> std::io::Result<()> {
                Err(std::io::Error::other("closed"))
            }
        }
        Diagnostics::new(Box::new(Failing)).emit(Event::Panic, Status::Failed, None, 0);
        Diagnostics::default().emit(Event::Panic, Status::Failed, None, 0);
    }

    #[test]
    fn startup_records_are_bounded_to_static_stage_names() {
        // Only the shape matters here; both calls must be side-effect free
        // for the caller and accept arbitrary caller text without echoing it
        // unfiltered.
        startup_record("config");
        startup_record("bad stage name with spaces and 故障详情");
    }

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
