//! One-use interactions with explicit ownership and connection generations.
use crate::SessionKey;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    pub session: SessionKey,
    pub chat: String,
    pub card: String,
    pub task: Option<String>,
    pub connection_epoch: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionState {
    Pending,
    Resolving,
    Resolved,
    Expired,
    Invalidated,
}

#[derive(Debug)]
pub struct Interaction {
    pub owner: Owner,
    pub deadline_ms: u64,
    pub state: InteractionState,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InteractionError {
    #[error("操作不属于当前用户、聊天、任务或连接")]
    WrongOwner,
    #[error("操作已处理或失效")]
    Unavailable,
}

impl Interaction {
    /// Failed ownership checks never consume another user's operation.
    pub fn claim(&mut self, owner: &Owner, now_ms: u64) -> Result<(), InteractionError> {
        if &self.owner != owner {
            return Err(InteractionError::WrongOwner);
        }
        self.expire(now_ms);
        if self.state != InteractionState::Pending {
            return Err(InteractionError::Unavailable);
        }
        self.state = InteractionState::Resolving;
        Ok(())
    }

    pub fn expire(&mut self, now_ms: u64) -> bool {
        if self.state == InteractionState::Pending && now_ms >= self.deadline_ms {
            self.state = InteractionState::Expired;
            return true;
        }
        false
    }

    pub fn resolve(&mut self) -> Result<(), InteractionError> {
        if self.state != InteractionState::Resolving {
            return Err(InteractionError::Unavailable);
        }
        self.state = InteractionState::Resolved;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_connection_and_wrong_user_cannot_consume_interaction() {
        let owner = Owner {
            session: SessionKey::new("u", "/tmp/project"),
            chat: "c".into(),
            card: "card".into(),
            task: Some("task".into()),
            connection_epoch: 2,
        };
        let mut item = Interaction {
            owner: owner.clone(),
            deadline_ms: 100,
            state: InteractionState::Pending,
        };
        let mut old = owner.clone();
        old.connection_epoch = 1;
        assert_eq!(item.claim(&old, 1), Err(InteractionError::WrongOwner));
        assert_eq!(item.state, InteractionState::Pending);
        assert!(item.claim(&owner, 99).is_ok());
        assert!(!item.expire(100));
        assert!(item.claim(&owner, 99).is_err());
        assert!(item.resolve().is_ok());
    }
    #[test]
    fn deadline_is_inclusive() {
        let owner = Owner {
            session: SessionKey::new("u", "/tmp"),
            chat: "c".into(),
            card: "c".into(),
            task: None,
            connection_epoch: 1,
        };
        let mut item = Interaction {
            owner: owner.clone(),
            deadline_ms: 100,
            state: InteractionState::Pending,
        };
        assert!(item.claim(&owner, 100).is_err());
        assert_eq!(item.state, InteractionState::Expired);
    }
}
