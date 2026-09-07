//! Application boundaries and bounded serial admission. No vendor SDK types.
use bridge_core::{SessionKey, task::TaskSpec};
use std::collections::VecDeque;
use thiserror::Error;

/// Durable acceptance is deliberately separate from executing side effects.
pub trait MessageJournal {
    type Error;
    /// Returns false for a duplicate. Must not report success before persistence.
    fn claim_message(&mut self, id: &str) -> Result<bool, Self::Error>;
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AdmissionError<E> {
    #[error("任务队列已满，请稍后重新发送")]
    Full,
    #[error("对话操作中，请稍后重新发送")]
    SessionMutation,
    #[error("消息 ID 不能为空")]
    MissingId,
    #[error("消息状态保存失败")]
    Persistence(E),
}

/// Owned by one application task; callers do not mutate it concurrently.
pub struct Scheduler {
    capacity: usize,
    queue: VecDeque<TaskSpec>,
    active: Option<TaskSpec>,
    mutating: bool,
}

impl Scheduler {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: VecDeque::new(),
            active: None,
            mutating: false,
        }
    }

    pub fn admit<J: MessageJournal>(
        &mut self,
        journal: &mut J,
        message: &str,
        task: TaskSpec,
    ) -> Result<bool, AdmissionError<J::Error>> {
        if message.is_empty() {
            return Err(AdmissionError::MissingId);
        }
        if self.mutating {
            return Err(AdmissionError::SessionMutation);
        }
        if self.queue.len() >= self.capacity {
            return Err(AdmissionError::Full);
        }
        if !journal
            .claim_message(message)
            .map_err(AdmissionError::Persistence)?
        {
            return Ok(false);
        }
        self.queue.push_back(task);
        Ok(true)
    }

    pub fn start_next(&mut self) -> Option<&TaskSpec> {
        if self.active.is_some() || self.mutating {
            return None;
        }
        self.active = self.queue.pop_front();
        self.active.as_ref()
    }

    /// A stale completion cannot free a newer task's execution slot.
    pub fn finish(&mut self, task_id: &str) -> bool {
        if self.active.as_ref().is_some_and(|t| t.id == task_id) {
            self.active = None;
            return true;
        }
        false
    }

    pub fn cancel_queued(&mut self, session: &SessionKey) -> usize {
        let before = self.queue.len();
        self.queue.retain(|task| &task.session != session);
        before - self.queue.len()
    }

    pub fn begin_session_mutation(&mut self) -> bool {
        if self.mutating || self.active.is_some() || !self.queue.is_empty() {
            return false;
        }
        self.mutating = true;
        true
    }

    pub fn end_session_mutation(&mut self) {
        self.mutating = false;
    }
    pub fn queued(&self) -> usize {
        self.queue.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bridge_core::ExecutionMode;
    #[derive(Default)]
    struct Journal {
        ids: Vec<String>,
        fail: bool,
    }
    impl MessageJournal for Journal {
        type Error = &'static str;
        fn claim_message(&mut self, id: &str) -> Result<bool, Self::Error> {
            if self.fail {
                return Err("disk full");
            }
            if self.ids.iter().any(|i| i == id) {
                return Ok(false);
            }
            self.ids.push(id.into());
            Ok(true)
        }
    }
    fn task(id: &str) -> TaskSpec {
        TaskSpec {
            id: id.into(),
            session: SessionKey::new("user", "/tmp/project"),
            chat: "chat".into(),
            prompt: "hello".into(),
            model: None,
            mode: ExecutionMode::Plan,
        }
    }
    #[test]
    fn full_and_failed_storage_do_not_accept_or_execute() {
        let mut scheduler = Scheduler::new(1);
        let mut journal = Journal {
            fail: true,
            ..Journal::default()
        };
        assert!(matches!(
            scheduler.admit(&mut journal, "m1", task("1")),
            Err(AdmissionError::Persistence(_))
        ));
        assert_eq!(scheduler.queued(), 0);
        journal.fail = false;
        assert_eq!(scheduler.admit(&mut journal, "m1", task("1")), Ok(true));
        assert_eq!(
            scheduler.admit(&mut journal, "m2", task("2")),
            Err(AdmissionError::Full)
        );
        assert_eq!(journal.ids, ["m1"]);
    }
    #[test]
    fn fifo_duplicate_and_stale_completion() {
        let mut scheduler = Scheduler::new(4);
        let mut journal = Journal::default();
        assert_eq!(scheduler.admit(&mut journal, "m1", task("1")), Ok(true));
        assert_eq!(
            scheduler.admit(&mut journal, "m1", task("duplicate")),
            Ok(false)
        );
        assert_eq!(scheduler.admit(&mut journal, "m2", task("2")), Ok(true));
        assert!(!scheduler.begin_session_mutation());
        assert_eq!(scheduler.start_next().map(|t| t.id.as_str()), Some("1"));
        assert!(scheduler.start_next().is_none());
        assert!(!scheduler.finish("old"));
        assert!(scheduler.finish("1"));
        assert_eq!(scheduler.start_next().map(|t| t.id.as_str()), Some("2"));
    }
    #[test]
    fn session_mutation_rejects_without_claiming() {
        let mut scheduler = Scheduler::new(4);
        let mut journal = Journal::default();
        assert!(scheduler.begin_session_mutation());
        assert_eq!(
            scheduler.admit(&mut journal, "m", task("1")),
            Err(AdmissionError::SessionMutation)
        );
        assert!(journal.ids.is_empty());
        scheduler.end_session_mutation();
        assert_eq!(scheduler.admit(&mut journal, "m", task("1")), Ok(true));
        assert_eq!(
            scheduler.cancel_queued(&SessionKey::new("other", "/tmp/project")),
            0
        );
        assert_eq!(scheduler.cancel_queued(&task("1").session), 1);
    }
}

pub mod messaging;
pub mod ports;

pub mod events;

pub mod requests;

pub mod interactions;

pub mod execution;
