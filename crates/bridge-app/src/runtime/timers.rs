//! Periodic maintenance: progress previews, confirmation expiry and the
//! per-task time budget.
use super::flow::tell;
use super::state::{Done, Runtime};
use crate::presentation::Request as DeliveryRequest;
use std::{sync::atomic::Ordering, time::Duration};
use tokio::task::JoinSet;

impl Runtime {
    pub(crate) async fn handle_tick(&mut self, jobs: &mut JoinSet<Done>) -> Result<(), String> {
        let _ = jobs;
        if self.messenger.rich_output() && self.last_progress.elapsed() >= Duration::from_secs(3) {
            self.last_progress = tokio::time::Instant::now();
            if let Some(active) = self.active.as_ref().filter(|active| !active.compact) {
                if self.delivery.capacity() > 8 && !self.progress_busy.swap(true, Ordering::AcqRel) {
                    let output = active
                        .plan
                        .as_ref()
                        .map(|(text, _)| text.as_str())
                        .unwrap_or(&active.output);
                    let preview: String = output.chars().take(1000).collect();
                    let text = format!(
                        "{} · 已用 {} 秒\n\n{}{}",
                        if active.stopping { "正在停止" } else { "正在执行" },
                        active.started.elapsed().as_secs(),
                        if preview.is_empty() { "等待 Codex 输出…" } else { &preview },
                        if preview.len() < output.len() {
                            "\n\n（进度预览，完整内容将在结束后发送）"
                        } else {
                            ""
                        }
                    );
                    if self
                        .delivery
                        .try_send(DeliveryRequest::Progress {
                            task: active.spec.id.clone(),
                            chat: active.spec.chat.clone(),
                            text,
                        })
                        .is_err()
                    {
                        self.progress_busy.store(false, Ordering::Release);
                    }
                }
            }
        }
        for creation in self.confirmations.expire(tokio::time::Instant::now()) {
            tell(
                &self.delivery,
                &creation.chat,
                format!(
                    "目录创建确认已过期：{}\n未自动创建；如需继续，请重新发送 /cd <路径>。",
                    creation.target.display()
                ),
            )?;
        }
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.started.elapsed() > Duration::from_secs(3600))
        {
            return Err("任务超过一小时上限，停止本次运行".into());
        }
        Ok(())
    }
}
