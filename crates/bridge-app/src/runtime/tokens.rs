//! Capability token formats, minted in exactly one place.
//!
//! Every token this runtime issues is scoped to the process epoch plus a
//! monotonic sequence (or a task id), so tokens from an earlier run or an
//! earlier task can never be replayed. Button tokens append `-<index>` to a
//! panel prefix; nothing outside this module may format a token.

use crate::cards::CardToken;
use bridge_core::task::TaskId;

/// Panel action prefix: `panel-{epoch}-{sequence}`. Each minted panel bumps
/// the sequence; its buttons become `{prefix}-{index}`.
pub(crate) fn panel_prefix(epoch: u64, sequence: u64) -> String {
    format!("panel-{epoch}-{sequence}")
}

/// Directory creation confirmation token: `cd-{epoch}-{sequence}`.
pub(crate) fn confirmation(epoch: u64, sequence: u64) -> String {
    format!("cd-{epoch}-{sequence}")
}

/// Approval or question card token: `approval-{epoch}-{sequence}`.
pub(crate) fn approval(epoch: u64, sequence: u64) -> CardToken {
    CardToken::new(format!("approval-{epoch}-{sequence}"))
}

/// Plan offer token: `plan-{task}`; one offer per task.
pub(crate) fn plan(task: &TaskId) -> CardToken {
    CardToken::new(format!("plan-{task}"))
}

/// Durable input id for a card click, so journal claims cannot collide with
/// message ids: `card:{token}`.
pub(crate) fn click_receipt(token: &CardToken) -> String {
    format!("card:{token}")
}
