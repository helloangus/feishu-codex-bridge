//! Protocol-independent commands, identities and state transitions.
pub mod command;
pub mod interaction;
pub mod task;

use std::path::PathBuf;

/// A user and canonical workspace identify a conversation binding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionKey {
    pub user: String,
    pub workspace: PathBuf,
}

impl SessionKey {
    pub fn new(user: impl Into<String>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            user: user.into(),
            workspace: workspace.into(),
        }
    }
}

/// Agent collaboration mode. Every task captures an explicit mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExecutionMode {
    #[default]
    Execute,
    Plan,
}

pub mod view;
