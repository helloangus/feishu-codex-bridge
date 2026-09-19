//! Terminal dispositions of task and compaction flows.
//!
//! `Outcome` is pure data: the runtime carries it beside the accumulated
//! output, and the presentation layer derives the user-visible label, the
//! card tone and the diagnostic status from it. Nothing may classify by
//! matching message text.

/// Why one task or bridge flow ended.
pub enum Outcome {
    /// The turn finished normally.
    Completed,
    /// The user stopped this task before it finished.
    Stopped,
    /// The bridge itself is shutting down; the task will not be resumed.
    BridgeStopped,
    /// The backend turn failed and reported a reason.
    Failed { detail: String },
    /// Preparation failed before a turn could start; nothing executed.
    PrepareFailed { detail: String },
    /// The start request's result is unknown; the turn may be running, so the
    /// bridge stops instead of retrying.
    StartUnknown { detail: String },
    /// A compaction-specific terminal state; its wording differs from tasks.
    Compact(Compaction),
    /// The flow was refused or cancelled before any effect. The label is the
    /// full user-visible sentence; no detail is appended.
    NotStarted { label: String },
}

/// Terminal states of a context-compaction flow.
pub enum Compaction {
    Completed,
    Stopped,
    Failed { detail: String },
    PrepareFailed { detail: String },
}
