//! Application boundaries and bounded serial admission. No vendor SDK types.
pub mod cards;
pub mod diagnostics;
pub mod directories;
pub mod plans;
pub mod presentation;
use bridge_core::{SessionKey, task::TaskSpec};
use std::collections::{BTreeMap, VecDeque};
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
    #[error("该消息正在保存接收状态")]
    Pending,
    #[error("消息状态保存失败")]
    Persistence(E),
}

/// Owned by one application task; callers do not mutate it concurrently.
pub struct Scheduler {
    capacity: usize,
    queue: VecDeque<TaskSpec>,
    active: Option<TaskSpec>,
    mutating: bool,
    pending: BTreeMap<u64, (String, TaskSpec, Option<bool>)>,
    next_admission: u64,
}

/// Opaque identity for a pending disk operation. The owner must commit or abort
/// it when the supervised storage operation finishes, including cancellation.
pub struct AdmissionTicket(u64);

impl Scheduler {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            queue: VecDeque::new(),
            active: None,
            mutating: false,
            pending: BTreeMap::new(),
            next_admission: 0,
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
        if self.queue.len() + self.pending.len() >= self.capacity {
            return Err(AdmissionError::Full);
        }
        if !self.pending.is_empty() {
            return Err(AdmissionError::Pending);
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

    /// Reserve bounded capacity synchronously, then perform disk I/O outside
    /// this state owner. Stop and status remain available while persistence runs.
    pub fn reserve(
        &mut self,
        message: String,
        task: TaskSpec,
    ) -> Result<AdmissionTicket, AdmissionError<()>> {
        if message.is_empty() {
            return Err(AdmissionError::MissingId);
        }
        if self.mutating {
            return Err(AdmissionError::SessionMutation);
        }
        if self.pending.values().any(|(id, _, _)| id == &message) {
            return Err(AdmissionError::Pending);
        }
        if self.queue.len() + self.pending.len() >= self.capacity {
            return Err(AdmissionError::Full);
        }
        let id = self.next_admission;
        self.next_admission = id.checked_add(1).ok_or(AdmissionError::Full)?;
        self.pending.insert(id, (message, task, None));
        Ok(AdmissionTicket(id))
    }

    /// Call only with the result of the journal claim. An absent ticket may have
    /// been cancelled by the user; its late disk completion cannot enqueue work.
    pub fn commit_admission(&mut self, ticket: AdmissionTicket, newly_claimed: bool) -> bool {
        if let Some((_, _, ready)) = self.pending.get_mut(&ticket.0) {
            *ready = Some(newly_claimed);
            self.drain_admissions();
            return newly_claimed;
        }
        false
    }

    pub fn abort_admission(&mut self, ticket: AdmissionTicket) {
        self.pending.remove(&ticket.0);
        self.drain_admissions();
    }

    fn drain_admissions(&mut self) {
        // Disk jobs may finish out of order; preserve original admission FIFO.
        while self
            .pending
            .first_key_value()
            .is_some_and(|(_, (_, _, ready))| ready.is_some())
        {
            if let Some((_, (_, task, Some(true)))) = self.pending.pop_first() {
                self.queue.push_back(task);
            }
        }
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
        let before = self.queue.len() + self.pending.len();
        self.queue.retain(|task| &task.session != session);
        self.pending
            .retain(|_, (_, task, _)| &task.session != session);
        self.drain_admissions();
        before - self.queue.len() - self.pending.len()
    }

    pub fn begin_session_mutation(&mut self) -> bool {
        if self.mutating
            || self.active.is_some()
            || !self.queue.is_empty()
            || !self.pending.is_empty()
        {
            return false;
        }
        self.mutating = true;
        true
    }

    pub fn end_session_mutation(&mut self) {
        self.mutating = false;
    }
    /// Backend invalidations precede queued work, but never race active preparation
    /// or another session mutation. Pending admissions only persist task claims.
    pub fn begin_invalidation(&mut self) -> bool {
        if self.mutating || self.active.is_some() {
            return false;
        }
        self.mutating = true;
        true
    }
    pub fn queued(&self) -> usize {
        self.queue.len()
    }
    pub fn has_task(&self, id: &str) -> bool {
        self.active.as_ref().is_some_and(|s| s.id == id)
            || self.queue.iter().any(|s| s.id == id)
            || self.pending.values().any(|(_, s, _)| s.id == id)
    }
    pub fn pending_admissions(&self) -> usize {
        self.pending.len()
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
    fn invalidation_precedes_queue_and_pending_admissions_without_racing_active_work() {
        let mut scheduler = Scheduler::new(4);
        let ticket = match scheduler.reserve("message".into(), task("1")) {
            Ok(ticket) => ticket,
            Err(error) => panic!("unexpected reservation failure: {error:?}"),
        };
        assert!(scheduler.begin_invalidation());
        assert!(!scheduler.begin_invalidation());
        assert!(scheduler.commit_admission(ticket, true));
        assert!(scheduler.start_next().is_none());
        scheduler.end_session_mutation();
        assert!(scheduler.begin_invalidation());
        scheduler.end_session_mutation();
        assert_eq!(scheduler.start_next().map(|t| t.id.as_str()), Some("1"));
        assert!(!scheduler.begin_invalidation());
        assert!(scheduler.finish("1"));
        assert!(scheduler.begin_session_mutation());
        assert!(!scheduler.begin_invalidation());
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

    #[test]
    fn asynchronous_claims_preserve_fifo_when_disk_results_arrive_out_of_order()
    -> Result<(), AdmissionError<()>> {
        let mut scheduler = Scheduler::new(2);
        let first = scheduler.reserve("m1".into(), task("1"))?;
        let second = scheduler.reserve("m2".into(), task("2"))?;
        assert!(matches!(
            scheduler.reserve("m3".into(), task("3")),
            Err(AdmissionError::Full)
        ));
        assert!(!scheduler.begin_session_mutation());
        assert!(scheduler.commit_admission(second, true));
        assert!(scheduler.start_next().is_none());
        assert!(scheduler.commit_admission(first, true));
        assert_eq!(scheduler.start_next().map(|t| t.id.as_str()), Some("1"));
        assert!(scheduler.finish("1"));
        assert_eq!(scheduler.start_next().map(|t| t.id.as_str()), Some("2"));
        Ok(())
    }

    #[test]
    fn cancellation_and_failed_claim_never_enqueue_late_work() -> Result<(), AdmissionError<()>> {
        let mut scheduler = Scheduler::new(2);
        let first = scheduler.reserve("m1".into(), task("1"))?;
        assert_eq!(scheduler.cancel_queued(&task("1").session), 1);
        assert!(!scheduler.commit_admission(first, true));
        let second = scheduler.reserve("m2".into(), task("2"))?;
        scheduler.abort_admission(second);
        let duplicate = scheduler.reserve("m3".into(), task("3"))?;
        assert!(!scheduler.commit_admission(duplicate, false));
        assert!(scheduler.start_next().is_none());
        assert!(scheduler.begin_session_mutation());
        Ok(())
    }
}

pub mod messaging;
pub mod ports;

pub mod events;

pub mod requests;

pub mod interactions;

pub mod execution;
pub mod runtime;
pub mod sessions;
