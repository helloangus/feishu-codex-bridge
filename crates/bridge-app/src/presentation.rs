//! Serial answer and preview delivery. Platform JSON stays in the messenger adapter.
use crate::messaging::{DeliveryError, MessageId, Messenger};
use bridge_core::view::{Panel, Tone};
use tokio::time::{Duration, timeout};

pub enum Request {
    Text(String, String),
    Answer {
        task: String,
        chat: String,
        text: String,
    },
    Progress {
        task: String,
        chat: String,
        text: String,
    },
}

/// Bounded UTF-8 parts; split code blocks are closed and reopened with their language.
pub fn markdown_parts(text: &str) -> Vec<String> {
    const PAYLOAD: usize = 5500;
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut fence: Option<(String, String)> = None;
    fn flush(parts: &mut Vec<String>, part: &mut String, fence: &Option<(String, String)>) {
        if let Some((close, _)) = fence {
            part.push('\n');
            part.push_str(close);
        }
        parts.push(std::mem::take(part));
        if let Some((_, open)) = fence {
            part.push_str(open);
            part.push('\n');
        }
    }
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start_matches(' ');
        let leading = line.len() - trimmed.len();
        let marker = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'));
        let run = marker
            .map(|c| trimmed.chars().take_while(|v| *v == c).count())
            .unwrap_or(0);
        let delimiter = leading <= 3 && run >= 3 && line.len() <= 128;
        if delimiter && part.len() + line.len() > PAYLOAD {
            flush(&mut parts, &mut part, &fence);
        }
        let mut rest = line;
        while !rest.is_empty() {
            let mut count = (PAYLOAD - part.len()).min(rest.len());
            while !rest.is_char_boundary(count) {
                count -= 1;
            }
            if count == 0 {
                flush(&mut parts, &mut part, &fence);
                continue;
            }
            part.push_str(&rest[..count]);
            rest = &rest[count..];
            if !rest.is_empty() {
                flush(&mut parts, &mut part, &fence);
            }
        }
        if delimiter {
            if let Some((close, _)) = &fence {
                if trimmed.starts_with(close.as_str()) && trimmed[run..].trim().is_empty() {
                    fence = None;
                }
            } else {
                fence = Some((trimmed[..run].into(), trimmed.trim_end().into()));
            }
        }
    }
    if !part.is_empty() {
        if let Some((close, _)) = fence {
            part.push('\n');
            part.push_str(&close);
        }
        parts.push(part);
    }
    if parts.is_empty() {
        parts.push("（无文本输出）".into());
    }
    parts
}

#[derive(Default)]
pub struct Presentation {
    preview: Option<(String, MessageId)>,
    failed_task: Option<String>,
}
impl Presentation {
    pub async fn progress(&mut self, m: &dyn Messenger, task: String, chat: String, text: String) {
        if self.failed_task.as_deref() == Some(&task) {
            return;
        }
        if self.preview.as_ref().is_some_and(|(id, _)| id != &task) {
            self.preview = None;
        }
        let panel = Panel::text(
            "Codex 执行进度",
            markdown_parts(&text).remove(0),
            Tone::Info,
        );
        let result = if let Some((_, id)) = &self.preview {
            timeout(Duration::from_secs(5), m.update_panel(id.clone(), panel))
                .await
                .map(|r| r.map(|_| id.clone()))
        } else {
            timeout(Duration::from_secs(5), m.send_panel(chat, panel)).await
        };
        match result {
            Ok(Ok(id)) => self.preview = Some((task, id)),
            _ => {
                self.failed_task = Some(task);
                eprintln!("{{\"event\":\"progress_delivery_failed\"}}");
            }
        }
    }
    pub async fn answer(
        &mut self,
        m: &dyn Messenger,
        task: String,
        chat: String,
        text: String,
    ) -> Result<(), DeliveryError> {
        if let Some((previous, id)) = self.preview.take() {
            if previous == task {
                let title = text
                    .lines()
                    .next()
                    .filter(|line| line.len() <= 128)
                    .unwrap_or("任务已结束");
                let _ = timeout(
                    Duration::from_secs(5),
                    m.update_panel(
                        id,
                        Panel::text(title, "本轮已结束，完整回复见后续消息。", Tone::Muted),
                    ),
                )
                .await;
            }
        }
        self.failed_task = None;
        let parts = markdown_parts(&text);
        let total = parts.len();
        let tone = if text.starts_with("执行完成") {
            Tone::Success
        } else if text.starts_with("任务已停止") || text.starts_with("桥接已停止") {
            Tone::Warning
        } else if text.starts_with("执行失败")
            || text.starts_with("准备失败")
            || text.starts_with("启动结果未知")
        {
            Tone::Error
        } else {
            Tone::Info
        };
        let mut failed = false;
        for (index, body) in parts.into_iter().enumerate() {
            let title = if total == 1 {
                "Codex 回复".into()
            } else {
                format!("Codex 回复 · {}/{total}", index + 1)
            };
            let result = timeout(
                Duration::from_secs(10),
                m.send_panel(chat.clone(), Panel::text(title, &body, tone)),
            )
            .await;
            if !matches!(result, Ok(Ok(_)))
                && !matches!(
                    timeout(
                        Duration::from_secs(10),
                        m.send_text(
                            chat.clone(),
                            format!(
                                "回复 {}/{}（卡片发送未确认，文字补发）\n{body}",
                                index + 1,
                                total
                            )
                        )
                    )
                    .await,
                    Ok(Ok(()))
                )
            {
                failed = true;
            }
        }
        if failed {
            Err(DeliveryError::Transport)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messaging::{DeliveryFuture, ResourceKind};
    use std::{
        fs::File,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };
    #[derive(Default)]
    struct Fake {
        events: Mutex<Vec<String>>,
        tones: Mutex<Vec<Tone>>,
        fail_panel: AtomicBool,
        fail_update: AtomicBool,
    }
    impl Fake {
        fn record(&self, text: String) {
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(text);
        }
        fn events(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }
    }
    impl Messenger for Fake {
        fn send_panel(&self, _: String, panel: Panel) -> DeliveryFuture<'_, MessageId> {
            Box::pin(async move {
                self.tones
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(panel.tone);
                self.record(format!("panel:{}:{}", panel.title, panel.body));
                if self.fail_panel.load(Ordering::Relaxed) {
                    Err(DeliveryError::Transport)
                } else {
                    Ok(MessageId("preview".into()))
                }
            })
        }
        fn update_panel(&self, id: MessageId, panel: Panel) -> DeliveryFuture<'_, ()> {
            Box::pin(async move {
                self.record(format!("update:{}:{}:{}", id.0, panel.title, panel.body));
                if self.fail_update.load(Ordering::Relaxed) {
                    Err(DeliveryError::Transport)
                } else {
                    Ok(())
                }
            })
        }
        fn send_text(&self, _: String, text: String) -> DeliveryFuture<'_, ()> {
            Box::pin(async move {
                self.record(format!("text:{text}"));
                Ok(())
            })
        }
        fn upload(&self, _: String, _: String, _: File, _: ResourceKind) -> DeliveryFuture<'_, ()> {
            Box::pin(async { Err(DeliveryError::Incompatible) })
        }
    }
    #[tokio::test]
    async fn preview_updates_same_message_then_ends_before_complete_answer()
    -> Result<(), DeliveryError> {
        let m = Fake::default();
        let mut p = Presentation::default();
        p.progress(&m, "task".into(), "chat".into(), "first".into())
            .await;
        p.progress(&m, "task".into(), "chat".into(), "second".into())
            .await;
        p.answer(
            &m,
            "task".into(),
            "chat".into(),
            "执行完成\n**完整回复**".into(),
        )
        .await?;
        let events = m.events();
        assert_eq!(events.len(), 4);
        assert!(events[0].starts_with("panel:Codex 执行进度"));
        assert!(events[1].starts_with("update:preview:Codex 执行进度"));
        assert!(events[2].contains("本轮已结束"));
        assert!(events[3].contains("**完整回复**"));
        assert_eq!(
            m.tones.lock().unwrap_or_else(|e| e.into_inner()).last(),
            Some(&Tone::Success)
        );
        p.answer(
            &m,
            "failed".into(),
            "chat".into(),
            "执行失败：测试错误".into(),
        )
        .await?;
        assert_eq!(
            m.tones.lock().unwrap_or_else(|e| e.into_inner()).last(),
            Some(&Tone::Error)
        );
        Ok(())
    }
    #[tokio::test]
    async fn progress_failure_stops_retries_but_final_answer_still_falls_back()
    -> Result<(), DeliveryError> {
        let m = Fake::default();
        let mut p = Presentation::default();
        m.fail_panel.store(true, Ordering::Relaxed);
        p.progress(&m, "task".into(), "chat".into(), "first".into())
            .await;
        p.progress(&m, "task".into(), "chat".into(), "second".into())
            .await;
        assert_eq!(m.events().len(), 1);
        p.answer(&m, "task".into(), "chat".into(), "final".into())
            .await?;
        assert!(
            m.events()
                .last()
                .is_some_and(|e| e.starts_with("text:") && e.contains("final"))
        );
        m.fail_panel.store(false, Ordering::Relaxed);
        p.progress(&m, "next".into(), "chat".into(), "new".into())
            .await;
        assert!(
            m.events()
                .last()
                .is_some_and(|e| e.contains("执行进度:new"))
        );
        Ok(())
    }
    #[tokio::test]
    async fn failed_preview_update_does_not_suppress_final_parts() -> Result<(), DeliveryError> {
        let m = Fake::default();
        let mut p = Presentation::default();
        p.progress(&m, "task".into(), "chat".into(), "first".into())
            .await;
        m.fail_update.store(true, Ordering::Relaxed);
        p.progress(&m, "task".into(), "chat".into(), "second".into())
            .await;
        p.progress(&m, "task".into(), "chat".into(), "third".into())
            .await;
        assert_eq!(m.events().len(), 2);
        p.answer(&m, "task".into(), "chat".into(), "文".repeat(6000))
            .await?;
        let events = m.events();
        assert_eq!(
            events
                .iter()
                .filter(|e| e.starts_with("panel:Codex 回复"))
                .count(),
            4
        );
        assert_eq!(
            events
                .iter()
                .map(|e| e.matches('文').count())
                .sum::<usize>(),
            6000
        );
        Ok(())
    }
    #[test]
    fn long_unicode_code_line_is_bounded_and_fences_reopen() {
        let text = format!("说明\n```rust\n{}\n```\n结尾", "中".repeat(10000));
        let parts = markdown_parts(&text);
        assert!(parts.len() > 3);
        assert!(
            parts
                .iter()
                .all(|p| p.len() < 6000 && p.matches("```").count() == 2)
        );
        assert_eq!(
            parts.iter().map(|p| p.matches('中').count()).sum::<usize>(),
            10000
        );
        assert!(parts.last().is_some_and(|p| p.ends_with("结尾")));
    }
    #[test]
    fn incomplete_and_tilde_fences_are_closed_without_losing_content() {
        for fence in ["```python", "~~~~text"] {
            let parts = markdown_parts(&format!("{fence}\n{}", "x".repeat(18000)));
            assert_eq!(
                parts.iter().map(|p| p.matches('x').count()).sum::<usize>(),
                18000
                    + if fence.ends_with("text") {
                        parts.len()
                    } else {
                        0
                    }
            );
            assert!(parts.iter().all(|p| p.starts_with(fence) && p.len() < 6000));
        }
    }
}
