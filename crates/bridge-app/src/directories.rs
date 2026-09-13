//! Directory navigation stays behind a bounded local filesystem/storage port.
use crate::sessions::StoreFuture;
use std::{collections::BTreeMap, path::PathBuf};
use tokio::time::Instant;

#[derive(Clone)]
pub struct Creation {
    pub user: String,
    pub chat: String,
    pub current: PathBuf,
    pub input: String,
    pub target: PathBuf,
    pub deadline: Instant,
}

#[derive(Default)]
pub struct Confirmations(BTreeMap<String, Creation>);

impl Confirmations {
    pub fn insert(&mut self, token: String, entry: Creation) -> bool {
        self.invalidate(&entry.user);
        if self.0.len() >= 100 || token.is_empty() || self.0.contains_key(&token) {
            return false;
        }
        self.0.insert(token, entry);
        true
    }
    pub fn get(
        &self,
        token: &str,
        user: &str,
        chat: &str,
        current: &std::path::Path,
        now: Instant,
    ) -> Option<Creation> {
        self.0
            .get(token)
            .filter(|c| {
                c.user == user && c.chat == chat && c.current == current && now < c.deadline
            })
            .cloned()
    }
    pub fn remove(&mut self, token: &str) {
        self.0.remove(token);
    }
    pub fn invalidate(&mut self, user: &str) {
        self.0.retain(|_, c| c.user != user);
    }
    pub fn expire(&mut self, now: Instant) -> Vec<Creation> {
        let expired = self
            .0
            .values()
            .filter(|c| now >= c.deadline)
            .cloned()
            .collect();
        self.0.retain(|_, c| now < c.deadline);
        expired
    }
}

#[derive(Debug)]
pub struct DirectoryView {
    pub path: PathBuf,
    pub children: Vec<String>,
}

impl DirectoryView {
    pub fn text(&self) -> String {
        let mut text = format!("当前目录：{}\n子目录（最多 20 项）：", self.path.display());
        for child in &self.children {
            text.push_str(&format!("\n/cd {child}"));
        }
        text.push_str("\n\n/cd <路径> 切换目录；支持相对路径、绝对路径和空格。\n/cd .. 返回上级（限工作区内）。目标不存在时先请求创建确认。");
        text
    }
}

pub trait DirectoryStore: Send + Sync {
    /// Some(target) means confirmation is required; this never creates anything.
    fn propose_directory(
        &self,
        root: PathBuf,
        current: PathBuf,
        input: String,
    ) -> StoreFuture<'_, Option<PathBuf>>;
    fn create_directory(
        &self,
        user: String,
        root: PathBuf,
        current: PathBuf,
        input: String,
        target: PathBuf,
    ) -> StoreFuture<'_, PathBuf>;
    fn directory_preferences(&self) -> StoreFuture<'_, BTreeMap<String, PathBuf>>;
    /// Reject removed, moved or redirected directory snapshots before backend work.
    fn validate_directory(&self, root: PathBuf, directory: PathBuf) -> StoreFuture<'_, ()>;
    fn inspect_directory(&self, root: PathBuf, current: PathBuf) -> StoreFuture<'_, DirectoryView>;
    /// Resolve and persist in one serialized local operation, preserving other state.
    fn change_directory(
        &self,
        user: String,
        root: PathBuf,
        current: PathBuf,
        input: String,
    ) -> StoreFuture<'_, PathBuf>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::Path, time::Duration};
    #[test]
    fn confirmation_requires_owner_chat_directory_and_unexpired_token() {
        let now = Instant::now();
        let mut pending = Confirmations::default();
        let entry = Creation {
            user: "owner".into(),
            chat: "chat".into(),
            current: "/root".into(),
            input: "new".into(),
            target: "/root/new".into(),
            deadline: now + Duration::from_secs(600),
        };
        assert!(pending.insert("token".into(), entry.clone()));
        for (user, chat, path) in [
            ("other", "chat", "/root"),
            ("owner", "elsewhere", "/root"),
            ("owner", "chat", "/other"),
        ] {
            assert!(
                pending
                    .get("token", user, chat, Path::new(path), now)
                    .is_none()
            );
        }
        assert!(
            pending
                .get("token", "owner", "chat", Path::new("/root"), now)
                .is_some()
        );
        assert!(
            pending
                .get("token", "owner", "chat", Path::new("/root"), entry.deadline)
                .is_none()
        );
        assert_eq!(pending.expire(entry.deadline).len(), 1);
        assert!(pending.expire(entry.deadline).is_empty());
        assert!(pending.insert("next".into(), entry));
        pending.invalidate("owner");
        assert!(
            pending
                .get("next", "owner", "chat", Path::new("/root"), now)
                .is_none()
        );
    }
}
