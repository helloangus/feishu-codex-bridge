//! The single interaction manager for approval and question flows.
//!
//! One owner per entry: the same user, chat and directory that received the
//! card, bound to the still-running turn that raised the request. Entries are
//! consumed before any asynchronous write, so an uncertain reply can never
//! re-enable an approval. Expired or stale entries still receive their
//! required denial. Vendor request IDs and answer text are never stored
//! beyond the pending answers the user explicitly provided.
use crate::{
    cards::Owner,
    ports::{BackendError, TurnRef},
    requests::{AgentReply, AgentRequest, ReplyHandle, RequestKind},
    runtime::limits,
};
use std::{collections::BTreeMap, path::Path};
use tokio::time::{Instant, timeout};

pub struct Pending {
    pub waiting_text: bool,
    pub answers: BTreeMap<String, Vec<String>>,
    pub question: usize,
    pub request: AgentRequest,
    pub reply: Box<dyn ReplyHandle>,
    pub task: bridge_core::task::TaskId,
    pub owner: Owner,
    pub deadline: Instant,
    /// True once the current question or approval card has been dispatched.
    /// It remains true after delivery to prevent duplicate cards and resets
    /// only when a question advances to the next step.
    pub card_dispatched: bool,
    pub source: Option<String>,
}

/// The question list of a request, when it is a question group.
fn questions_of(request: &AgentRequest) -> Option<&[crate::requests::Question]> {
    match &request.kind {
        RequestKind::Questions { questions, .. } => Some(questions),
        _ => None,
    }
}

impl Pending {
    /// Ownership covers the delivering user, chat, directory and deadline;
    /// turn liveness is supplied by the caller through a closure.
    fn owned_by(&self, user: &str, chat: &str, directory: &Path, now: Instant) -> bool {
        self.owner.user == user
            && self.owner.chat == chat
            && self.owner.directory == directory
            && now < self.deadline
    }
    fn current_question(&self) -> Option<&crate::requests::Question> {
        match &self.request.kind {
            RequestKind::Questions { questions, .. } if self.question < questions.len() => {
                Some(&questions[self.question])
            }
            _ => None,
        }
    }
    pub fn is_questions(&self) -> bool {
        matches!(&self.request.kind, RequestKind::Questions { .. })
    }
}

/// Outcome of a text answer: recorded (possibly completing the group) or
/// invalid and rejected. A completed group returns its consumed entry so the
/// caller can submit the answers.
#[allow(clippy::large_enum_variant)]
pub enum TextOutcome {
    Recorded {
        complete: bool,
        finished: Option<Pending>,
    },
    Invalid,
}

/// Outcome of a button answer: waiting for free text, recorded (possibly
/// completing the group), or invalid and rejected. A completed group returns
/// its consumed entry so the caller can submit the answers.
#[allow(clippy::large_enum_variant)]
pub enum Choice {
    WaitingText,
    Recorded {
        complete: bool,
        finished: Option<Pending>,
    },
    Invalid,
}

/// Result of an asynchronous reply write. An uncertain result must stop the
/// run: the protocol state cannot distinguish "applied" from "not applied".
#[derive(Debug)]
pub struct ReplyOutcome {
    pub questions: bool,
    pub chat: String,
    pub allow: bool,
    pub result: Result<(), BackendError>,
    /// False when the group was incomplete and nothing was written.
    pub submitted: bool,
}

/// The presenting user's identity for one interaction attempt.
pub struct Claim<'a> {
    pub user: &'a str,
    pub chat: &'a str,
    pub directory: &'a Path,
    pub now: Instant,
}

#[derive(Default)]
pub struct Interactions {
    entries: BTreeMap<crate::cards::CardToken, Pending>,
}

impl Interactions {
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn contains(&self, token: &crate::cards::CardToken) -> bool {
        self.entries.contains_key(token)
    }
    pub fn get(&self, token: &crate::cards::CardToken) -> Option<&Pending> {
        self.entries.get(token)
    }
    pub fn get_mut(&mut self, token: &crate::cards::CardToken) -> Option<&mut Pending> {
        self.entries.get_mut(token)
    }
    pub fn remove(&mut self, token: &crate::cards::CardToken) -> Option<Pending> {
        self.entries.remove(token)
    }
    /// Registration is bounded and refuses a second open request for the same
    /// turn item; duplicates are ambiguous and revoke what came before.
    pub fn insert(&mut self, token: crate::cards::CardToken, pending: Pending) -> bool {
        if token.as_str().is_empty()
            || self.entries.len() >= limits::INTERACTIONS
            || self.entries.contains_key(&token)
        {
            return false;
        }
        if self.entries.values().any(|existing| {
            existing.request.turn == pending.request.turn
                && existing.request.item == pending.request.item
        }) {
            return false;
        }
        self.entries.insert(token, pending);
        true
    }
    /// A request is a duplicate when the same turn item already has one.
    pub fn item_is_open(&self, turn: &TurnRef, item: &str) -> bool {
        self.entries
            .values()
            .any(|pending| pending.request.turn == *turn && pending.request.item == item)
    }
    /// Revoke every open request for a turn by expiring it immediately.
    pub fn expire_turn(&mut self, turn: &TurnRef, item: &str) {
        for pending in self.entries.values_mut() {
            if pending.request.turn == *turn && pending.request.item == item {
                pending.deadline = Instant::now();
            }
        }
    }
    /// Tokens whose deadline passed or whose owning task is no longer alive.
    /// Expired entries stay registered until `expire` removes them so their
    /// required denial is sent exactly once.
    pub fn stale_tokens(
        &self,
        now: Instant,
        alive: impl Fn(&Pending) -> bool,
    ) -> Vec<crate::cards::CardToken> {
        self.entries
            .iter()
            .filter(|(_, pending)| now >= pending.deadline || !alive(pending))
            .map(|(token, _)| token.clone())
            .collect()
    }
    /// Tokens whose card still needs to be sent for a live turn.
    pub fn unsent_tokens(&self, valid: impl Fn(&Pending) -> bool) -> Vec<crate::cards::CardToken> {
        self.entries
            .iter()
            .filter(|(_, pending)| !pending.card_dispatched && valid(pending))
            .map(|(token, _)| token.clone())
            .collect()
    }
    /// Record a text answer for the question the card is waiting on. Early,
    /// late, foreign or non-waiting answers are rejected; a completed group is
    /// removed and returned so the caller can submit the answers.
    pub fn answer_text(
        &mut self,
        token: &crate::cards::CardToken,
        claim: Claim<'_>,
        index: usize,
        answer: &str,
        skip: bool,
        turn_live: impl Fn(&Pending) -> bool,
    ) -> TextOutcome {
        let turn_live = |pending: &Pending| !skip && turn_live(pending);
        let Some(pending) = self.entries.get_mut(token) else {
            return TextOutcome::Invalid;
        };
        if !pending.waiting_text
            || index != pending.question
            || !pending.owned_by(claim.user, claim.chat, claim.directory, claim.now)
            || !turn_live(pending)
        {
            return TextOutcome::Invalid;
        }
        let Some(question_id) = (match &pending.request.kind {
            RequestKind::Questions { questions, .. } if pending.question < questions.len() => {
                Some(questions[pending.question].id.clone())
            }
            _ => None,
        }) else {
            return TextOutcome::Invalid;
        };
        pending.answers.insert(question_id, vec![answer.into()]);
        pending.waiting_text = false;
        pending.question += 1;
        pending.card_dispatched = false;
        pending.source = None;
        let complete = questions_of(&pending.request)
            .is_some_and(|questions| pending.question == questions.len());
        let finished = if complete {
            self.entries.remove(token)
        } else {
            None
        };
        TextOutcome::Recorded { complete, finished }
    }
    /// Record a button choice for the current question. The click must come
    /// from the card the entry last delivered. `other` switches the entry to
    /// text mode; anything else must name a valid option index.
    pub fn answer_choice(
        &mut self,
        token: &crate::cards::CardToken,
        claim: Claim<'_>,
        source: &str,
        index: usize,
        choice: &str,
        turn_live: impl Fn(&Pending) -> bool,
    ) -> Choice {
        let Some(pending) = self.entries.get_mut(token) else {
            return Choice::Invalid;
        };
        if !pending.owned_by(claim.user, claim.chat, claim.directory, claim.now)
            || pending.source.as_deref() != Some(source)
            || index != pending.question
            || !turn_live(pending)
        {
            return Choice::Invalid;
        }
        let Some(question) = pending.current_question() else {
            return Choice::Invalid;
        };
        if choice == "other" {
            if !question.other && !question.options.is_empty() && !question.secret {
                return Choice::Invalid;
            }
            pending.waiting_text = true;
            return Choice::WaitingText;
        }
        let Some(selected) = choice
            .parse::<usize>()
            .ok()
            .and_then(|position| question.options.get(position))
            .map(|option| vec![option.label.clone()])
        else {
            return Choice::Invalid;
        };
        let question_id = question.id.clone();
        let total = questions_of(&pending.request)
            .map(|questions| questions.len())
            .unwrap_or_default();
        pending.answers.insert(question_id, selected);
        pending.question += 1;
        pending.card_dispatched = false;
        pending.source = None;
        let complete = pending.question == total;
        let finished = if complete {
            self.entries.remove(token)
        } else {
            None
        };
        Choice::Recorded { complete, finished }
    }
    /// Consume an approval decision. The click must reference the card the
    /// entry last delivered; the entry is removed before the async write.
    pub fn approve(
        &mut self,
        token: &crate::cards::CardToken,
        claim: Claim<'_>,
        source: &str,
        turn_live: impl Fn(&Pending) -> bool,
    ) -> Option<Pending> {
        let pending = self.entries.get(token)?;
        if pending.is_questions()
            || pending.source.as_deref() != Some(source)
            || !pending.owned_by(claim.user, claim.chat, claim.directory, claim.now)
            || !turn_live(pending)
        {
            return None;
        }
        self.entries.remove(token)
    }
    /// Remove and return every entry whose deadline has passed; the caller
    /// turns each into the required denial.
    pub fn expire(&mut self, now: Instant) -> Vec<Pending> {
        let tokens: Vec<crate::cards::CardToken> = self
            .entries
            .iter()
            .filter(|(_, pending)| now >= pending.deadline)
            .map(|(token, _)| token.clone())
            .collect();
        self.remove_all(tokens)
    }
    /// Remove every entry regardless of state; used on shutdown.
    pub fn drain(&mut self) -> Vec<Pending> {
        self.remove_all(self.entries.keys().cloned().collect())
    }
    fn remove_all(&mut self, tokens: Vec<crate::cards::CardToken>) -> Vec<Pending> {
        tokens
            .into_iter()
            .filter_map(|token| self.entries.remove(&token))
            .collect()
    }
}

/// Submit one pending entry's reply. Groups with missing or empty answers are
/// never submitted — dropping the handle writes nothing, including shutdown.
/// The write is bounded; an elapsed bound reports [`BackendError::Uncertain`].
pub async fn deliver_reply(
    diagnostics: &crate::diagnostics::Diagnostics,
    pending: Pending,
    allow: bool,
) -> ReplyOutcome {
    let questions = pending.is_questions();
    if let RequestKind::Questions {
        questions: list, ..
    } = &pending.request.kind
    {
        if list.is_empty()
            || list.iter().any(|question| {
                !pending.answers.get(&question.id).is_some_and(|answers| {
                    !answers.is_empty() && answers.iter().all(|answer| !answer.trim().is_empty())
                })
            })
        {
            return ReplyOutcome {
                questions,
                chat: pending.owner.chat,
                allow,
                result: Ok(()),
                submitted: false,
            };
        }
    }
    let response = match &pending.request.kind {
        RequestKind::Questions { .. } => AgentReply::Answers(pending.answers.clone()),
        _ => AgentReply::Approve(allow),
    };
    let result = timeout(limits::BACKEND_REPLY_TIMEOUT, pending.reply.reply(response))
        .await
        .unwrap_or(Err(BackendError::Uncertain));
    if questions {
        diagnostics.emit(
            crate::diagnostics::Event::AnswerReturned,
            if result.is_ok() {
                crate::diagnostics::Status::Ok
            } else {
                crate::diagnostics::Status::Failed
            },
            Some(pending.task.as_str()),
            0,
        );
    }
    ReplyOutcome {
        questions,
        chat: pending.owner.chat,
        allow,
        result,
        submitted: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cards::CardToken;
    use crate::requests::{Approval, ApprovalKind, Question};
    use std::sync::{Arc, Mutex};

    type BoxError = Box<dyn std::error::Error>;

    struct Handle(Arc<Mutex<Vec<AgentReply>>>);
    impl ReplyHandle for Handle {
        fn reply(
            self: Box<Self>,
            response: AgentReply,
        ) -> crate::ports::BackendFuture<'static, ()> {
            if let Ok(mut recorded) = self.0.lock() {
                recorded.push(response);
            }
            Box::pin(async { Ok(()) })
        }
    }
    struct FailingHandle;
    impl ReplyHandle for FailingHandle {
        fn reply(self: Box<Self>, _: AgentReply) -> crate::ports::BackendFuture<'static, ()> {
            Box::pin(async { Err(BackendError::Rejected(1)) })
        }
    }

    fn owner() -> Owner {
        Owner {
            user: "user".into(),
            chat: "chat".into(),
            directory: "/workspace".into(),
            generation: 3,
            snapshot: crate::cards::TaskSnapshot {
                next_task: 0,
                active: None,
            },
        }
    }
    fn task() -> bridge_core::task::TaskId {
        bridge_core::task::TaskId::new(7, 1)
    }
    fn approval_request(token: &CardToken) -> AgentRequest {
        AgentRequest {
            turn: TurnRef {
                epoch: 1,
                thread_id: "thread".into(),
                turn_id: format!("turn-{token}"),
            },
            item: format!("item-{token}"),
            kind: RequestKind::Approval(Approval {
                permissions: None,
                network_context: None,
                changes: None,
                kind: ApprovalKind::Command,
                command: Some("echo".into()),
                directory: Some("/workspace".into()),
                reason: None,
                grant_root: None,
                can_allow: true,
            }),
        }
    }
    fn question_request(token: &CardToken) -> AgentRequest {
        AgentRequest {
            turn: TurnRef {
                epoch: 1,
                thread_id: "thread".into(),
                turn_id: format!("turn-{token}"),
            },
            item: format!("item-{token}"),
            kind: RequestKind::Questions {
                blocking: true,
                questions: vec![
                    Question {
                        id: "q1".into(),
                        header: "h".into(),
                        text: "t".into(),
                        other: true,
                        secret: false,
                        options: vec![],
                    },
                    Question {
                        id: "q2".into(),
                        header: "h".into(),
                        text: "t".into(),
                        other: false,
                        secret: false,
                        options: vec![],
                    },
                ],
            },
        }
    }
    fn now() -> Instant {
        Instant::now()
    }
    fn live() -> impl Fn(&Pending) -> bool {
        |_| true
    }

    fn insert_approval(
        registry: &mut Interactions,
        token: &CardToken,
        handle: Box<dyn ReplyHandle>,
    ) -> bool {
        registry.insert(
            token.clone(),
            Pending {
                waiting_text: false,
                answers: BTreeMap::new(),
                question: 0,
                request: approval_request(token),
                reply: handle,
                task: task(),
                owner: owner(),
                deadline: now() + std::time::Duration::from_secs(600),
                card_dispatched: false,
                source: Some("card".into()),
            },
        )
    }
    fn insert_questions(
        registry: &mut Interactions,
        token: &CardToken,
        handle: Box<dyn ReplyHandle>,
    ) {
        assert!(registry.insert(
            token.clone(),
            Pending {
                waiting_text: false,
                answers: BTreeMap::new(),
                question: 0,
                request: question_request(token),
                reply: handle,
                task: task(),
                owner: owner(),
                deadline: now() + std::time::Duration::from_secs(600),
                card_dispatched: false,
                source: None,
            },
        ));
    }

    #[test]
    fn approval_storm_is_bounded_and_refuses_duplicate_turn_items() {
        let mut registry = Interactions::default();
        for index in 0..32 {
            assert!(insert_approval(
                &mut registry,
                &CardToken::new(format!("t{index}")),
                Box::new(Handle(Arc::new(Mutex::new(Vec::new()))))
            ));
        }
        assert!(!insert_approval(
            &mut registry,
            &CardToken::new("extra"),
            Box::new(Handle(Arc::new(Mutex::new(Vec::new()))))
        ));
        let mut duplicate = Interactions::default();
        let handle = || Box::new(Handle(Arc::new(Mutex::new(Vec::new())))) as Box<dyn ReplyHandle>;
        assert!(insert_approval(
            &mut duplicate,
            &CardToken::new("first"),
            handle()
        ));
        // The second registration reuses the first request's turn and item.
        let mut second = Pending {
            waiting_text: false,
            answers: BTreeMap::new(),
            question: 0,
            request: approval_request(&CardToken::new("first")),
            reply: handle(),
            task: task(),
            owner: owner(),
            deadline: now() + std::time::Duration::from_secs(600),
            card_dispatched: false,
            source: Some("card".into()),
        };
        second.request.turn.thread_id = "thread".into();
        assert!(!duplicate.insert(CardToken::new("second"), second));
        assert!(!duplicate.item_is_open(
            &TurnRef {
                epoch: 9,
                thread_id: "x".into(),
                turn_id: "y".into()
            },
            "item-first"
        ));
    }

    #[test]
    fn foreign_owners_and_unknown_sources_never_consume() {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Interactions::default();
        insert_approval(
            &mut registry,
            &CardToken::new("token"),
            Box::new(Handle(recorded.clone())),
        );
        let directory = std::path::Path::new("/workspace");
        assert!(
            registry
                .approve(
                    &CardToken::new("token"),
                    Claim {
                        user: "other",
                        chat: "chat",
                        directory,
                        now: now(),
                    },
                    "card",
                    |_| true,
                )
                .is_none()
        );
        assert!(
            registry
                .approve(
                    &CardToken::new("token"),
                    Claim {
                        user: "user",
                        chat: "elsewhere",
                        directory,
                        now: now(),
                    },
                    "card",
                    |_| true,
                )
                .is_none()
        );
        assert!(
            registry
                .approve(
                    &CardToken::new("token"),
                    Claim {
                        user: "user",
                        chat: "chat",
                        directory,
                        now: now(),
                    },
                    "stale",
                    |_| true,
                )
                .is_none()
        );
        assert!(
            registry
                .approve(
                    &CardToken::new("token"),
                    Claim {
                        user: "user",
                        chat: "chat",
                        directory,
                        now: now(),
                    },
                    "card",
                    |_| false,
                )
                .is_none()
        );
        // A questions entry is never consumable through the approval path.
        insert_questions(
            &mut registry,
            &CardToken::new("questions"),
            Box::new(Handle(Arc::new(Mutex::new(Vec::new())))),
        );
        assert!(
            registry
                .approve(
                    &CardToken::new("questions"),
                    Claim {
                        user: "user",
                        chat: "chat",
                        directory,
                        now: now(),
                    },
                    "card",
                    |_| true,
                )
                .is_none()
        );
        assert_eq!(registry.len(), 2);
        assert!(recorded.lock().map(|v| v.is_empty()).unwrap_or_default());
    }

    #[test]
    fn approval_consumes_before_write_and_denial_is_returned() -> Result<(), BoxError> {
        let mut registry = Interactions::default();
        insert_approval(
            &mut registry,
            &CardToken::new("token"),
            Box::new(Handle(Arc::new(Mutex::new(Vec::new())))),
        );
        let directory = std::path::Path::new("/workspace");
        let claim = || Claim {
            user: "user",
            chat: "chat",
            directory,
            now: now(),
        };
        let pending = registry
            .approve(&CardToken::new("token"), claim(), "card", |_| true)
            .ok_or("approval must be consumable")?;
        assert!(registry.is_empty());
        // The second attempt has nothing to consume.
        assert!(
            registry
                .approve(&CardToken::new("token"), claim(), "card", |_| true)
                .is_none()
        );
        drop(pending);
        Ok(())
    }

    #[tokio::test]
    async fn uncertain_reply_is_reported_and_incomplete_answers_are_never_submitted()
    -> Result<(), BoxError> {
        let mut registry = Interactions::default();
        insert_questions(
            &mut registry,
            &CardToken::new("token"),
            Box::new(FailingHandle),
        );
        let mut pending = registry.remove(&CardToken::new("token")).ok_or("entry")?;
        pending.answers.insert("q1".into(), vec!["first".into()]);
        pending.answers.insert("q2".into(), vec!["second".into()]);
        let outcome = deliver_reply(&crate::diagnostics::Diagnostics::noop(), pending, false).await;
        assert_eq!(outcome.result, Err(BackendError::Rejected(1)));
        assert!(outcome.submitted);
        // An expired questions group drops the handle without any write.
        let mut expired = Interactions::default();
        insert_questions(
            &mut expired,
            &CardToken::new("token"),
            Box::new(FailingHandle),
        );
        expired
            .get_mut(&CardToken::new("token"))
            .ok_or("entry")?
            .deadline = now();
        // Expired entries stay registered until expire() removes them once.
        let pending = expired
            .expire(now())
            .into_iter()
            .next()
            .ok_or("expired entry")?;
        assert!(expired.expire(now()).is_empty());
        let outcome = deliver_reply(&crate::diagnostics::Diagnostics::noop(), pending, false).await;
        assert!(!outcome.submitted);
        assert_eq!(outcome.result, Ok(()));
        Ok(())
    }

    #[test]
    fn per_question_answers_progress_and_reject_mismatched_steps() -> Result<(), BoxError> {
        let mut registry = Interactions::default();
        insert_questions(
            &mut registry,
            &CardToken::new("token"),
            Box::new(Handle(Arc::new(Mutex::new(Vec::new())))),
        );
        let directory = std::path::Path::new("/workspace");
        let claim = || Claim {
            user: "user",
            chat: "chat",
            directory,
            now: now(),
        };
        // Buttons only work after the card source has been delivered.
        assert!(matches!(
            registry.answer_choice(&CardToken::new("token"), claim(), "card", 0, "0", |_| true),
            Choice::Invalid
        ));
        registry
            .get_mut(&CardToken::new("token"))
            .ok_or("entry")?
            .source = Some("card".into());
        assert!(matches!(
            registry.answer_choice(&CardToken::new("token"), claim(), "card", 1, "0", |_| true),
            Choice::Invalid
        ));
        // "other" without options waits for text.
        assert!(matches!(
            registry.answer_choice(
                &CardToken::new("token"),
                claim(),
                "card",
                0,
                "other",
                |_| true
            ),
            Choice::WaitingText
        ));
        // The first question has no options; a choice index cannot record.
        assert!(matches!(
            registry.answer_choice(&CardToken::new("token"), claim(), "card", 0, "0", |_| true),
            Choice::Invalid
        ));
        // Text answers only apply to the waiting question.
        assert!(matches!(
            registry.answer_text(&CardToken::new("token"), claim(), 1, "late", false, |_| {
                true
            }),
            TextOutcome::Invalid
        ));
        assert!(matches!(
            registry.answer_text(
                &CardToken::new("token"),
                claim(),
                0,
                "first answer",
                false,
                |_| true
            ),
            TextOutcome::Recorded {
                complete: false,
                finished: None
            }
        ));
        // The next question re-enters text mode through its own card click;
        // the re-delivered card installs a fresh source first.
        registry
            .get_mut(&CardToken::new("token"))
            .ok_or("entry")?
            .source = Some("card".into());
        assert!(matches!(
            registry.answer_choice(
                &CardToken::new("token"),
                claim(),
                "card",
                1,
                "other",
                |_| true
            ),
            Choice::WaitingText
        ));
        assert!(matches!(
            registry.answer_text(
                &CardToken::new("token"),
                claim(),
                1,
                "second answer",
                false,
                |_| true
            ),
            TextOutcome::Recorded {
                complete: true,
                finished: Some(_)
            }
        ));
        assert!(registry.is_empty());
        Ok(())
    }

    #[test]
    fn stale_and_expired_entries_are_reported_once() {
        let mut registry = Interactions::default();
        insert_approval(
            &mut registry,
            &CardToken::new("token"),
            Box::new(Handle(Arc::new(Mutex::new(Vec::new())))),
        );
        let stale = registry.stale_tokens(now(), |_| false);
        assert_eq!(stale, vec![CardToken::new("token")]);
        assert!(registry.stale_tokens(now(), live()).is_empty());
        let removed = registry.expire(now() + std::time::Duration::from_secs(601));
        assert_eq!(removed.len(), 1);
        assert!(registry.is_empty());
        assert!(registry.expire(now()).is_empty());
    }

    #[test]
    fn turn_revocation_marks_entries_expired() {
        let mut registry = Interactions::default();
        insert_approval(
            &mut registry,
            &CardToken::new("token"),
            Box::new(Handle(Arc::new(Mutex::new(Vec::new())))),
        );
        registry.expire_turn(
            &TurnRef {
                epoch: 1,
                thread_id: "thread".into(),
                turn_id: "turn-token".into(),
            },
            "item-token",
        );
        assert_eq!(registry.expire(now()).len(), 1);
    }

    #[test]
    fn dead_turns_are_reported_stale_while_live_ones_stay() {
        // Turn liveness (including the connection epoch) is expressed through
        // the caller-supplied `alive` check; entries of a dead turn must be
        // reported stale without being consumed by the report itself.
        let mut registry = Interactions::default();
        insert_approval(
            &mut registry,
            &CardToken::new("token"),
            Box::new(Handle(Arc::new(Mutex::new(Vec::new())))),
        );
        assert_eq!(
            registry.stale_tokens(now(), |_| false),
            vec![CardToken::new("token")]
        );
        assert_eq!(registry.len(), 1);
        assert!(registry.stale_tokens(now(), live()).is_empty());
    }
}
