//! Bounded, single-owner interaction registry. Never stores vendor request IDs.
use crate::{
    ports::BackendFuture,
    requests::{AgentReply, ReplyHandle, RequestKind},
};
use bridge_core::interaction::{Interaction, InteractionError, InteractionState, Owner};
use std::collections::BTreeMap;
use thiserror::Error;

struct Entry {
    state: Interaction,
    kind: RequestKind,
    handle: Box<dyn ReplyHandle>,
    answers: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RegistryError {
    #[error("操作与交互类型不匹配")]
    InvalidReply,
    #[error(transparent)]
    Interaction(#[from] InteractionError),
}

pub struct Registry {
    capacity: usize,
    entries: BTreeMap<String, Entry>,
}
impl Registry {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: BTreeMap::new(),
        }
    }
    /// Token generation belongs to the caller's random ID source. A failed
    /// insertion returns the handle so the caller can decline the request.
    pub fn insert(
        &mut self,
        token: String,
        owner: Owner,
        deadline_ms: u64,
        kind: RequestKind,
        handle: Box<dyn ReplyHandle>,
    ) -> Result<(), Box<dyn ReplyHandle>> {
        if token.is_empty()
            || self.entries.len() >= self.capacity
            || self.entries.contains_key(&token)
        {
            return Err(handle);
        }
        self.entries.insert(
            token,
            Entry {
                state: Interaction {
                    owner,
                    deadline_ms,
                    state: InteractionState::Pending,
                },
                kind,
                handle,
                answers: BTreeMap::new(),
            },
        );
        Ok(())
    }
    /// Synchronous claim/removal precedes asynchronous I/O. An uncertain write
    /// cannot re-enable approval; ownership failures leave the entry untouched.
    pub fn resolve(
        &mut self,
        token: &str,
        owner: &Owner,
        now_ms: u64,
        response: AgentReply,
    ) -> Result<BackendFuture<'static, ()>, RegistryError> {
        let entry = self
            .entries
            .get_mut(token)
            .ok_or(InteractionError::Unavailable)?;
        if &entry.state.owner != owner {
            return Err(InteractionError::WrongOwner.into());
        }
        // Keep expired entries for expire() to send their required denial.
        if now_ms >= entry.state.deadline_ms {
            return Err(InteractionError::Unavailable.into());
        }
        match (&entry.kind, &response) {
            (RequestKind::Approval(approval), AgentReply::Approve(allow))
                if !allow || approval.can_allow => {}
            (RequestKind::Questions { questions, .. }, AgentReply::Answers(answers))
                if answers
                    .keys()
                    .all(|id| questions.iter().any(|q| &q.id == id)) => {}
            _ => return Err(RegistryError::InvalidReply),
        }
        entry.state.claim(owner, now_ms)?;
        let entry = self
            .entries
            .remove(token)
            .ok_or(InteractionError::Unavailable)?;
        let response = match response {
            AgentReply::Answers(answers) => {
                let mut merged = entry.answers;
                merged.extend(answers);
                AgentReply::Answers(merged)
            }
            approval => approval,
        };
        Ok(entry.handle.reply(response))
    }
    /// Save one question locally, leaving the reply handle pending. The same
    /// ownership and deadline checks apply to text answers and button answers.
    pub fn answer(
        &mut self,
        token: &str,
        owner: &Owner,
        now_ms: u64,
        question: &str,
        answers: Vec<String>,
    ) -> Result<(), RegistryError> {
        let entry = self
            .entries
            .get_mut(token)
            .ok_or(InteractionError::Unavailable)?;
        if &entry.state.owner != owner {
            return Err(InteractionError::WrongOwner.into());
        }
        if now_ms >= entry.state.deadline_ms {
            return Err(InteractionError::Unavailable.into());
        }
        let RequestKind::Questions { questions, .. } = &entry.kind else {
            return Err(RegistryError::InvalidReply);
        };
        if !questions.iter().any(|q| q.id == question) {
            return Err(RegistryError::InvalidReply);
        }
        entry.answers.insert(question.into(), answers);
        Ok(())
    }

    /// Caller must poll every returned future, including during shutdown.
    pub fn expire(&mut self, now_ms: u64) -> Vec<BackendFuture<'static, ()>> {
        let tokens: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, e)| e.state.deadline_ms <= now_ms)
            .map(|(t, _)| t.clone())
            .collect();
        self.reject_tokens(tokens)
    }
    pub fn cancel_epoch(&mut self, epoch: u64) -> Vec<BackendFuture<'static, ()>> {
        let tokens: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, e)| e.state.owner.connection_epoch == epoch)
            .map(|(t, _)| t.clone())
            .collect();
        self.reject_tokens(tokens)
    }
    fn reject_tokens(&mut self, tokens: Vec<String>) -> Vec<BackendFuture<'static, ()>> {
        tokens
            .into_iter()
            .filter_map(|token| self.entries.remove(&token))
            .map(|entry| {
                let response = match entry.kind {
                    RequestKind::Approval(_) => AgentReply::Approve(false),
                    RequestKind::Questions { .. } => AgentReply::Answers(entry.answers),
                };
                entry.handle.reply(response)
            })
            .collect()
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::requests::{Approval, ApprovalKind};
    use bridge_core::SessionKey;
    use std::sync::{Arc, Mutex};
    struct Handle(Arc<Mutex<Vec<bool>>>);
    impl ReplyHandle for Handle {
        fn reply(self: Box<Self>, response: AgentReply) -> BackendFuture<'static, ()> {
            if let AgentReply::Approve(allow) = response {
                if let Ok(mut items) = self.0.lock() {
                    items.push(allow);
                }
            }
            Box::pin(async { Ok(()) })
        }
    }
    fn owner() -> Owner {
        Owner {
            session: SessionKey::new("user", "/tmp"),
            chat: "chat".into(),
            card: "card".into(),
            task: Some("task".into()),
            connection_epoch: 1,
        }
    }
    fn kind() -> RequestKind {
        RequestKind::Approval(Approval {
            kind: ApprovalKind::Command,
            command: None,
            directory: None,
            reason: None,
            grant_root: None,
            can_allow: true,
        })
    }
    #[test]
    fn wrong_owner_and_reply_do_not_consume_but_success_does() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new(2);
        assert!(
            registry
                .insert(
                    "token".into(),
                    owner(),
                    100,
                    kind(),
                    Box::new(Handle(calls.clone()))
                )
                .is_ok()
        );
        let mut wrong = owner();
        wrong.card = "other".into();
        assert!(
            registry
                .resolve("token", &wrong, 1, AgentReply::Approve(true))
                .is_err()
        );
        assert!(
            registry
                .resolve("token", &owner(), 1, AgentReply::Answers(BTreeMap::new()))
                .is_err()
        );
        assert_eq!(registry.len(), 1);
        drop(registry.resolve("token", &owner(), 99, AgentReply::Approve(true)));
        assert!(
            registry
                .resolve("token", &owner(), 99, AgentReply::Approve(true))
                .is_err()
        );
        assert!(registry.expire(100).is_empty());
        assert_eq!(
            calls.lock().ok().as_deref().map(|v| v.as_slice()),
            Some([true].as_slice())
        );
    }
    #[test]
    fn deadline_and_reconnect_remove_handles_once() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new(1);
        assert!(
            registry
                .insert(
                    "a".into(),
                    owner(),
                    10,
                    kind(),
                    Box::new(Handle(calls.clone()))
                )
                .is_ok()
        );
        assert!(
            registry
                .insert(
                    "b".into(),
                    owner(),
                    10,
                    kind(),
                    Box::new(Handle(calls.clone()))
                )
                .is_err()
        );
        assert!(
            registry
                .resolve("a", &owner(), 10, AgentReply::Approve(true))
                .is_err()
        );
        assert_eq!(registry.expire(10).len(), 1);
        assert!(registry.cancel_epoch(1).is_empty());
        assert!(registry.is_empty());
        assert_eq!(
            calls.lock().ok().as_deref().map(|v| v.as_slice()),
            Some([false].as_slice())
        );
    }
}

#[cfg(test)]
mod answer_tests {
    use super::*;
    use crate::requests::Question;
    use bridge_core::SessionKey;
    use std::sync::{Arc, Mutex};
    struct Handle(Arc<Mutex<BTreeMap<String, Vec<String>>>>);
    impl ReplyHandle for Handle {
        fn reply(self: Box<Self>, reply: AgentReply) -> BackendFuture<'static, ()> {
            if let AgentReply::Answers(answers) = reply {
                if let Ok(mut saved) = self.0.lock() {
                    *saved = answers;
                }
            }
            Box::pin(async { Ok(()) })
        }
    }
    #[test]
    fn partial_answers_survive_timeout() -> Result<(), RegistryError> {
        let saved = Arc::new(Mutex::new(BTreeMap::new()));
        let owner = Owner {
            session: SessionKey::new("u", "/tmp"),
            chat: "c".into(),
            card: "card".into(),
            task: None,
            connection_epoch: 1,
        };
        let kind = RequestKind::Questions {
            blocking: true,
            questions: vec![Question {
                id: "q".into(),
                header: "h".into(),
                text: "text".into(),
                other: true,
                secret: false,
                options: vec![],
            }],
        };
        let mut registry = Registry::new(1);
        assert!(
            registry
                .insert(
                    "token".into(),
                    owner.clone(),
                    10,
                    kind,
                    Box::new(Handle(saved.clone()))
                )
                .is_ok()
        );
        registry.answer("token", &owner, 9, "q", vec!["answer".into()])?;
        assert!(
            registry
                .answer("token", &owner, 10, "q", vec!["late".into()])
                .is_err()
        );
        assert_eq!(registry.expire(10).len(), 1);
        assert_eq!(
            saved.lock().ok().and_then(|m| m.get("q").cloned()),
            Some(vec!["answer".into()])
        );
        Ok(())
    }
}
