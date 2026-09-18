//! Explicit task lifecycle; delivery failure never rewrites execution outcome.
use crate::{ExecutionMode, SessionKey};
use std::fmt;
use thiserror::Error;

/// Opaque per-run task identifier of the form `{epoch}:{counter}`. A dedicated
/// type keeps task ids, thread ids and card tokens apart at every boundary.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(String);

impl TaskId {
    pub fn new(epoch: u64, counter: u64) -> Self {
        Self(format!("{epoch}:{counter}"))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    pub id: TaskId,
    pub session: SessionKey,
    pub chat: String,
    pub prompt: String,
    pub model: Option<String>,
    pub mode: ExecutionMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Queued,
    Preparing,
    Running,
    WaitingApproval,
    WaitingInput,
    Stopping,
    Delivering(Outcome),
    Finished(Outcome),
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("无效的任务状态转换")]
pub struct InvalidTransition;

impl TaskState {
    pub fn transition(&mut self, next: Self) -> Result<(), InvalidTransition> {
        use TaskState::*;
        let valid = matches!(
            (*self, next),
            (Queued, Preparing | Finished(Outcome::Interrupted))
                | (Preparing, Running | Stopping | Delivering(Outcome::Failed))
                | (
                    Running,
                    WaitingApproval | WaitingInput | Stopping | Delivering(_)
                )
                | (
                    WaitingApproval | WaitingInput,
                    Running | Stopping | Delivering(_)
                )
                | (Stopping, Delivering(_))
        ) || matches!((*self, next), (Delivering(a), Finished(b)) if a == b);
        if !valid {
            return Err(InvalidTransition);
        }
        *self = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn final_outcome_cannot_be_replaced_by_late_progress_or_delivery_failure()
    -> Result<(), InvalidTransition> {
        let mut state = TaskState::Running;
        state.transition(TaskState::Delivering(Outcome::Completed))?;
        assert!(state.transition(TaskState::Running).is_err());
        assert!(
            state
                .transition(TaskState::Finished(Outcome::Failed))
                .is_err()
        );
        state.transition(TaskState::Finished(Outcome::Completed))?;
        Ok(())
    }
}
