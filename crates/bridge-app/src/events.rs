//! Agent notifications consumed without knowledge of a vendor's wire protocol.
use crate::ports::TurnRef;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    Failed {
        message: Option<String>,
        details: Option<String>,
    },
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    Output {
        turn: TurnRef,
        item: String,
        delta: String,
    },
    Plan {
        turn: TurnRef,
        item: String,
        text: String,
    },
    Finished {
        turn: TurnRef,
        outcome: TurnOutcome,
    },
    Archived {
        epoch: u64,
        thread: String,
    },
}

/// Request reply handles stay opaque to application state and UI code.
pub enum Incoming {
    Notification(AgentEvent),
    Request {
        request: crate::requests::AgentRequest,
        reply: Box<dyn crate::requests::ReplyHandle>,
    },
}
