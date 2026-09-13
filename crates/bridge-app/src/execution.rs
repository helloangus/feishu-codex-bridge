//! Execution identity gate. Handles notifications arriving before turn/start's
//! response without assigning them to whatever task happens to be current.
use crate::{
    events::AgentEvent,
    ports::{BackendError, TurnRef},
};
use std::collections::VecDeque;

pub struct Execution {
    epoch: u64,
    thread: String,
    turn: Option<TurnRef>,
    early: VecDeque<AgentEvent>,
    capacity: usize,
    terminal: bool,
}
impl Execution {
    pub fn starting(epoch: u64, thread: String, capacity: usize) -> Self {
        Self {
            epoch,
            thread,
            turn: None,
            early: VecDeque::new(),
            capacity,
            terminal: false,
        }
    }
    /// Events with unrelated identities never enter this execution's buffers.
    pub fn event(&mut self, event: AgentEvent) -> Result<Option<AgentEvent>, BackendError> {
        let identity = match &event {
            AgentEvent::Output { turn, .. }
            | AgentEvent::Plan { turn, .. }
            | AgentEvent::Finished { turn, .. } => turn,
            AgentEvent::Archived { .. }
            | AgentEvent::Started { .. }
            | AgentEvent::FileChanges { .. } => return Ok(None),
        };
        if self.terminal || identity.epoch != self.epoch || identity.thread_id != self.thread {
            return Ok(None);
        }
        if let Some(turn) = &self.turn {
            if identity != turn {
                return Ok(None);
            }
            if matches!(event, AgentEvent::Finished { .. }) {
                self.terminal = true;
            }
            Ok(Some(event))
        } else {
            if self.early.len() >= self.capacity {
                self.terminal = true;
                self.early.clear();
                return Err(BackendError::Incompatible);
            }
            self.early.push_back(event);
            Ok(None)
        }
    }
    /// Bind exactly once to the acknowledged turn, then replay matching events.
    pub fn bind(&mut self, turn: TurnRef) -> Result<Vec<AgentEvent>, BackendError> {
        if self.terminal
            || self.turn.is_some()
            || turn.epoch != self.epoch
            || turn.thread_id != self.thread
            || turn.turn_id.is_empty()
        {
            return Err(BackendError::Incompatible);
        }
        self.turn = Some(turn);
        let buffered = std::mem::take(&mut self.early);
        let mut effects = Vec::new();
        for event in buffered {
            if let Some(event) = self.event(event)? {
                effects.push(event);
            }
        }
        Ok(effects)
    }
    /// An uncertain RPC failure never creates an implicit retry path.
    pub fn disconnect(&mut self) {
        self.terminal = true;
        self.early.clear();
    }
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }
    /// Before the start response, requests may wait but must not be approved.
    pub fn accepts_request(&self, turn: &TurnRef) -> bool {
        !self.terminal
            && turn.epoch == self.epoch
            && turn.thread_id == self.thread
            && self.turn.as_ref().is_none_or(|bound| bound == turn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::TurnOutcome;
    fn turn(id: &str) -> TurnRef {
        TurnRef {
            epoch: 1,
            thread_id: "thread".into(),
            turn_id: id.into(),
        }
    }
    fn output(id: &str) -> AgentEvent {
        AgentEvent::Output {
            turn: turn(id),
            item: "item".into(),
            delta: "text".into(),
        }
    }
    #[test]
    fn completion_before_start_response_is_retained_and_late_output_ignored()
    -> Result<(), BackendError> {
        let mut execution = Execution::starting(1, "thread".into(), 4);
        execution.event(output("old"))?;
        execution.event(output("new"))?;
        execution.event(AgentEvent::Finished {
            turn: turn("new"),
            outcome: TurnOutcome::Completed,
        })?;
        execution.event(output("new"))?;
        let events = execution.bind(turn("new"))?;
        assert_eq!(events.len(), 2);
        assert!(execution.is_terminal());
        assert_eq!(execution.event(output("new"))?, None);
        Ok(())
    }
    #[test]
    fn old_epochs_and_buffer_overflow_cannot_rebind() -> Result<(), BackendError> {
        let mut execution = Execution::starting(1, "thread".into(), 1);
        let mut old = turn("new");
        old.epoch = 0;
        assert_eq!(
            execution.event(AgentEvent::Finished {
                turn: old,
                outcome: TurnOutcome::Failed {
                    message: None,
                    details: None
                }
            })?,
            None
        );
        execution.event(output("new"))?;
        assert!(execution.event(output("new")).is_err());
        assert!(execution.bind(turn("new")).is_err());
        Ok(())
    }
}
