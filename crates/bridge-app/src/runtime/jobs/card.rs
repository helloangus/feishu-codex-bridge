//! Card and panel delivery receipts: approval cards, interaction replies,
//! panel updates, list panels and the refresh queue.
use super::super::RuntimeError;
use super::super::flow::{send_panel, spawn_reply, tell};
use super::super::limits;
use super::super::state::{CardDone, Done, ListedContent, PanelRefresh, Runtime};
use bridge_core::task::TaskId;
use bridge_core::view::Panel;
use tokio::task::JoinSet;
use tokio::time::Instant;

impl Runtime {
    pub(crate) async fn handle_card_done(
        &mut self,
        done: CardDone,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        match done {
            CardDone::ApprovalSent {
                token,
                panel,
                commands,
                result,
            } => {
                self.approval_sent(token, panel, commands, result, jobs)
                    .await
            }
            CardDone::ApprovalReplied { outcome } => self.approval_replied(outcome),
            CardDone::PanelUpdated => {
                self.updating_panel = false;
                Ok(())
            }
            CardDone::PanelSent {
                refreshed,
                panel,
                mut entries,
                result,
            } => {
                self.panel_sent(refreshed, panel, &mut entries, result);
                Ok(())
            }
            CardDone::Listed {
                refresh,
                chat,
                user,
                directory,
                generation,
                stop_snapshot,
                result,
            } => self.listed(
                refresh,
                chat,
                user,
                directory,
                generation,
                stop_snapshot,
                result,
                jobs,
            ),
        }
    }

    /// An approval or question card was delivered (or failed). Registration
    /// binds its buttons to the delivered message; a failed or stale delivery
    /// replies with the safe default (deny) instead of leaving Codex waiting.
    async fn approval_sent(
        &mut self,
        token: crate::cards::CardToken,
        panel: Panel,
        commands: Vec<(crate::cards::CardToken, String)>,
        result: Result<crate::messaging::MessageId, crate::messaging::DeliveryError>,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        if panel.title == "Codex 问答" {
            self.diagnostics.emit(
                crate::diagnostics::Event::QuestionSent,
                if result.is_ok() {
                    crate::diagnostics::Status::Ok
                } else {
                    crate::diagnostics::Status::Failed
                },
                self.approvals
                    .get(&token)
                    .map(|pending| pending.task.as_str()),
                panel.buttons.len(),
            );
        } else if result.is_err() {
            self.diagnostics.emit(
                crate::diagnostics::Event::CardFailed,
                crate::diagnostics::Status::Failed,
                None,
                0,
            );
        }
        if let Ok(id) = &result {
            self.card_views.insert(id.0.clone(), panel);
        }
        if let Some(pending) = self.approvals.get_mut(&token) {
            let valid = Instant::now() < pending.deadline
                && self.active.as_ref().is_some_and(|active| {
                    !active.stopping
                        && active.spec.id == pending.task
                        && active.turn.as_ref() == Some(&pending.request.turn)
                });
            let registered = if let Ok(id) = result {
                pending.source = Some(id.0.clone());
                valid
                    && self.card_actions.insert(
                        commands
                            .into_iter()
                            .map(|(token, command)| {
                                (
                                    token,
                                    crate::cards::Action {
                                        user: pending.owner.user.clone(),
                                        chat: pending.owner.chat.clone(),
                                        directory: pending.owner.directory.clone(),
                                        generation: pending.owner.generation,
                                        deadline: pending.deadline,
                                        source: id.0.clone(),
                                        command,
                                        stop_snapshot: None,
                                    },
                                )
                            })
                            .collect(),
                        Instant::now(),
                    )
            } else {
                false
            };
            if !registered {
                let Some(pending) = self.approvals.remove(&token) else {
                    return Ok(());
                };
                if pending.is_questions() {
                    tell(
                        &self.delivery,
                        &pending.owner.chat,
                        "问答卡片发送失败或失效，停止本次运行；未提交空答案。",
                    )?;
                    return Err(RuntimeError::Interaction("问答无法交付"));
                }
                tell(
                    &self.delivery,
                    &pending.owner.chat,
                    "审批卡片发送失败、已失效或登记已满，正在拒绝请求。",
                )?;
                spawn_reply(&self.diagnostics, jobs, pending, false)?;
            }
        }
        Ok(())
    }

    /// The controlled reply to an approval or question reached the backend.
    /// An uncertain result stops the run: the request may or may not be open.
    fn approval_replied(
        &mut self,
        outcome: crate::interactions::ReplyOutcome,
    ) -> Result<(), RuntimeError> {
        if outcome.submitted {
            match outcome.result {
                Ok(()) => tell(
                    &self.delivery,
                    &outcome.chat,
                    if outcome.questions {
                        "已向 Codex 回传答案。"
                    } else if outcome.allow {
                        "已向 Codex 回传本次同意；执行结果请等待任务回复。"
                    } else {
                        "已向 Codex 回传拒绝。"
                    },
                )?,
                Err(_) => {
                    tell(
                        &self.delivery,
                        &outcome.chat,
                        if outcome.questions {
                            "问答回传结果不确定，已停止连接，不会自动重试。"
                        } else {
                            "审批回传结果不确定，已停止连接，不会自动重试。"
                        },
                    )?;
                    return Err(RuntimeError::Interaction("交互回传失败"));
                }
            }
        }
        Ok(())
    }

    /// Register buttons of a delivered panel against the delivered message,
    /// dropping entries whose card generation has since been invalidated.
    fn panel_sent(
        &mut self,
        refreshed: bool,
        panel: Panel,
        entries: &mut Vec<(crate::cards::CardToken, crate::cards::Action)>,
        result: Result<crate::messaging::MessageId, crate::messaging::DeliveryError>,
    ) {
        if refreshed {
            self.updating_panel = false;
        }
        if let Ok(id) = result {
            self.card_views.insert(id.0.clone(), panel);
            entries.retain(|(_, entry)| {
                entry.generation == *self.card_generations.get(&entry.user).unwrap_or(&0)
            });
            for (_, entry) in entries.iter_mut() {
                entry.source = id.0.clone();
            }
            if !self
                .card_actions
                .insert(std::mem::take(entries), Instant::now())
            {
                self.diagnostics.emit(
                    crate::diagnostics::Event::CardFailed,
                    crate::diagnostics::Status::Rejected,
                    None,
                    0,
                );
            }
        }
    }

    /// Deliver a freshly built list panel, refreshing the original card when
    /// the user asked for it.
    #[allow(clippy::too_many_arguments)]
    fn listed(
        &mut self,
        refresh: Option<String>,
        chat: String,
        user: String,
        directory: std::path::PathBuf,
        generation: u64,
        stop_snapshot: (u64, Option<TaskId>),
        result: Result<ListedContent, crate::sessions::StartError>,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        match result {
            Ok(ListedContent::Text(text)) => tell(&self.delivery, &chat, text)?,
            Ok(ListedContent::Threads { entries, archived }) => {
                self.deliver_list(
                    jobs,
                    refresh,
                    chat,
                    user,
                    directory,
                    generation,
                    stop_snapshot,
                    crate::cards::threads(
                        &entries,
                        archived,
                        &format!("panel-{}-{}", self.settings.epoch, self.next_panel + 1),
                    ),
                )?;
            }
            Ok(ListedContent::Models { entries, current }) => {
                self.deliver_list(
                    jobs,
                    refresh,
                    chat,
                    user,
                    directory,
                    generation,
                    stop_snapshot,
                    crate::cards::models(
                        &entries,
                        current.as_deref(),
                        &format!("panel-{}-{}", self.settings.epoch, self.next_panel + 1),
                    ),
                )?;
            }
            Err(error) => tell(&self.delivery, &chat, format!("读取会话列表失败：{error}"))?,
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn deliver_list(
        &mut self,
        jobs: &mut JoinSet<Done>,
        refresh: Option<String>,
        chat: String,
        user: String,
        directory: std::path::PathBuf,
        generation: u64,
        stop_snapshot: (u64, Option<TaskId>),
        built: (Panel, Vec<(crate::cards::CardToken, String)>),
    ) -> Result<(), RuntimeError> {
        self.next_panel = self
            .next_panel
            .checked_add(1)
            .ok_or(RuntimeError::Capacity("卡片编号耗尽"))?;
        let (panel, commands) = built;
        let owner = crate::cards::Owner {
            user,
            chat: chat.clone(),
            directory,
            generation,
            stop_snapshot,
        };
        if let Some(source) = refresh {
            if self.refreshes.len() >= limits::PANEL_REFRESHES {
                tell(
                    &self.delivery,
                    &chat,
                    "卡片刷新队列已满，请重新发送列表命令。",
                )?;
            } else {
                self.refreshes.push_back(PanelRefresh {
                    source,
                    owner,
                    panel,
                    commands,
                });
            }
        } else {
            if !self.can_spawn(jobs, false) {
                tell(&self.delivery, &chat, "系统繁忙，请稍后重新发送列表命令。")?;
                return Ok(());
            }
            send_panel(
                &self.diagnostics,
                None,
                jobs,
                self.messenger.clone(),
                owner,
                panel,
                commands,
            );
        }
        Ok(())
    }
}
