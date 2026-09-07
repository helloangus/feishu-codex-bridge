//! Platform-independent agent interactions. Reply handles are single use.
use crate::ports::{BackendFuture, TurnRef};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalKind {
    Command,
    WriteStdin,
    FileChange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub kind: ApprovalKind,
    pub command: Option<String>,
    pub directory: Option<String>,
    pub reason: Option<String>,
    pub grant_root: Option<String>,
    pub can_allow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub text: String,
    pub other: bool,
    pub secret: bool,
    pub options: Vec<QuestionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKind {
    Approval(Approval),
    Questions {
        blocking: bool,
        questions: Vec<Question>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRequest {
    pub turn: TurnRef,
    pub item: String,
    pub kind: RequestKind,
}

pub enum AgentReply {
    Approve(bool),
    Answers(BTreeMap<String, Vec<String>>),
}

/// Ownership is consumed before the asynchronous write, including uncertain
/// failures. Application authorization must precede calling this port.
pub trait ReplyHandle: Send {
    fn reply(self: Box<Self>, response: AgentReply) -> BackendFuture<'static, ()>;
}
