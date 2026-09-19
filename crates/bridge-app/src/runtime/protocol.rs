//! Backend protocol event handling: notifications fold into the active
//! execution; requests become interactions or immediate denials.
use super::RuntimeError;
use super::flow::tell;
use super::limits;
use super::state::{ActiveKind, Done, Runtime, TaskDone};
use crate::{
    events::{AgentEvent, Incoming},
    interactions::Pending,
    ports::BackendError,
    requests::{ApprovalKind, RequestKind},
    sessions,
};
use tokio::{task::JoinSet, time::timeout};

impl Runtime {
    pub(crate) async fn handle_protocol(
        &mut self,
        incoming: Incoming,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        match incoming {
            Incoming::Notification(notification) => {
                self.handle_notification(notification, jobs).await
            }
            Incoming::Request { mut request, reply } => {
                if let RequestKind::Questions { questions, .. } = &request.kind {
                    self.diagnostics.emit(
                        crate::diagnostics::Event::QuestionReceived,
                        crate::diagnostics::Status::Ok,
                        self.tasks
                            .active
                            .as_ref()
                            .map(|active| active.spec.id.as_str()),
                        questions.len(),
                    );
                }
                if let RequestKind::Approval(approval) = &mut request.kind {
                    if approval.kind == ApprovalKind::FileChange {
                        approval.changes = self
                            .approvals
                            .file_changes
                            .get(&(
                                request.turn.epoch,
                                request.turn.thread_id.clone(),
                                request.turn.turn_id.clone(),
                                request.item.clone(),
                            ))
                            .cloned();
                    }
                }
                if let RequestKind::Questions { questions, .. } = &request.kind {
                    let plan_mode_live = self.tasks.active.as_ref().is_some_and(|active| {
                        active.spec.mode == bridge_core::ExecutionMode::Plan
                            && !active.stopping
                            && active
                                .gate
                                .as_ref()
                                .is_some_and(|gate| gate.accepts_request(&request.turn))
                    });
                    let _ = questions;
                    if !plan_mode_live {
                        if let Some(active) = &self.tasks.active {
                            tell(
                                &self.delivery,
                                &active.spec.chat,
                                "普通执行模式不支持 Codex 问答请求；请在 Plan 模式下重新发起。",
                            )?;
                        }
                        return Err(RuntimeError::Interaction(
                            "问答模式或执行身份无效，未提交空答案",
                        ));
                    }
                    let supported = questions.iter().all(crate::cards::question_supported);
                    if !supported {
                        if let Some(active) = &self.tasks.active {
                            tell(
                                &self.delivery,
                                &active.spec.chat,
                                "问答详情超过展示上限，停止本次运行；未提交空答案。",
                            )?;
                        }
                        return Err(RuntimeError::Interaction("问答无法完整展示"));
                    }
                }
                let valid_question_count = matches!(
                    &request.kind,
                    RequestKind::Questions { questions, .. }
                        if questions.is_empty()
                            || questions.len() > limits::QUESTIONS_PER_REQUEST
                );
                if !valid_question_count {
                    let live_active = self
                        .tasks
                        .active
                        .as_ref()
                        .filter(|active| {
                            !active.stopping
                                && active
                                    .gate
                                    .as_ref()
                                    .is_some_and(|gate| gate.accepts_request(&request.turn))
                        })
                        .map(|active| (active.spec.id.clone(), active.spec.chat.clone()));
                    if let Some((task_id, chat)) = live_active {
                        let can_register = !self
                            .approvals
                            .interactions
                            .item_is_open(&request.turn, &request.item)
                            && self.approvals.interactions.len() < limits::INTERACTIONS;
                        if can_register {
                            self.approvals.next_token = self
                                .approvals
                                .next_token
                                .checked_add(1)
                                .ok_or(RuntimeError::Capacity("审批编号耗尽"))?;
                            let token = crate::cards::CardToken::new(format!(
                                "approval-{}-{}",
                                self.settings.epoch, self.approvals.next_token
                            ));
                            let owner = crate::cards::Owner {
                                user: self
                                    .tasks
                                    .active
                                    .as_ref()
                                    .map(|active| active.spec.session.user.clone())
                                    .unwrap_or_default(),
                                chat: self
                                    .tasks
                                    .active
                                    .as_ref()
                                    .map(|active| active.spec.chat.clone())
                                    .unwrap_or_default(),
                                directory: self
                                    .tasks
                                    .active
                                    .as_ref()
                                    .map(|active| active.spec.session.workspace.clone())
                                    .unwrap_or_default(),
                                generation: self
                                    .tasks
                                    .active
                                    .as_ref()
                                    .and_then(|active| {
                                        self.cards
                                            .generations
                                            .get(&active.spec.session.user)
                                            .copied()
                                    })
                                    .unwrap_or(0),
                                stop_snapshot: self.card_snapshot(),
                            };
                            let pending = Pending {
                                waiting_text: false,
                                answers: std::collections::BTreeMap::new(),
                                question: 0,
                                request,
                                reply,
                                task: task_id,
                                owner,
                                deadline: tokio::time::Instant::now() + limits::INTERACTION_TIMEOUT,
                                card_dispatched: false,
                                source: None,
                            };
                            let registered = self.approvals.interactions.insert(token, pending);
                            debug_assert!(registered, "registration was pre-checked");
                            return Ok(());
                        }
                        tell(
                            &self.delivery,
                            &chat,
                            "审批请求重复或待处理数量已满，已拒绝该请求。",
                        )?;
                        // The request was never registered, so request/reply are
                        // still owned here and fall through to the denial reply.
                    }
                } else if let Some(active) = &self.tasks.active {
                    tell(
                        &self.delivery,
                        &active.spec.chat,
                        "问答请求无效，停止本次运行；未提交空答案。",
                    )?;
                }
                let response = match request.kind {
                    RequestKind::Approval(_) => crate::requests::AgentReply::Approve(false),
                    RequestKind::Questions { .. } => {
                        return Err(RuntimeError::Interaction(
                            "问答请求无效或数量超过上限，未提交空答案",
                        ));
                    }
                };
                if jobs.len() >= limits::BACKGROUND_JOBS {
                    return Err(RuntimeError::Capacity("控制回传容量耗尽，停止运行"));
                }
                jobs.spawn(async move {
                    Done::Task(TaskDone::Control {
                        result: timeout(limits::BACKEND_REPLY_TIMEOUT, reply.reply(response))
                            .await
                            .unwrap_or(Err(BackendError::Uncertain)),
                    })
                });
                Ok(())
            }
        }
    }

    async fn handle_notification(
        &mut self,
        notification: AgentEvent,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        if let AgentEvent::Archived { epoch, thread } = &notification {
            if *epoch != self.settings.epoch {
                return Ok(());
            }
            if !sessions::valid_thread_id(thread) {
                return Err(RuntimeError::Internal("归档通知会话 ID 无效"));
            }
            if !self.session_book.archived_threads.contains(thread)
                && self.session_book.archived_threads.len() >= limits::ARCHIVED_THREADS
            {
                return Err(RuntimeError::Capacity("待同步归档通知超过上限，停止运行"));
            }
            self.session_book.archived_threads.insert(thread.clone());
            self.cards.plan_offer = None;
            return Ok(());
        }
        if let AgentEvent::FileChanges {
            turn,
            item,
            changes,
        } = &notification
        {
            if self.turn_is_live(turn) {
                let key = (
                    turn.epoch,
                    turn.thread_id.clone(),
                    turn.turn_id.clone(),
                    item.clone(),
                );
                // Duplicate item snapshots are ambiguous: revoke pending approvals.
                if self.approvals.file_changes.contains_key(&key)
                    || self.approvals.interactions.item_is_open(turn, item)
                {
                    self.approvals.interactions.expire_turn(turn, item);
                    if let Some(recorded) = self.approvals.file_changes.get_mut(&key) {
                        recorded.clear();
                    }
                } else if self.approvals.file_changes.len() < limits::FILE_CHANGE_ITEMS {
                    self.approvals.file_changes.insert(key, changes.clone());
                }
            }
            return Ok(());
        }
        if let AgentEvent::Started { turn } = &notification {
            let compacting = self.tasks.active.as_mut().filter(|active| {
                let expected = match &active.kind {
                    ActiveKind::Compact { thread, .. } => thread.as_ref(),
                    ActiveKind::Task => None,
                };
                active.gate.is_some()
                    && turn.epoch == self.settings.epoch
                    && expected == Some(&turn.thread_id)
            });
            if let Some(active) = compacting {
                if active.turn.as_ref().is_some_and(|current| current != turn) {
                    return Err(RuntimeError::Maintenance("压缩期间出现其他执行，停止运行"));
                }
                if active.turn.is_none() {
                    active.turn = Some(turn.clone());
                    let early = active
                        .gate
                        .as_mut()
                        .ok_or(RuntimeError::Internal("缺少压缩状态"))?
                        .bind(turn.clone())
                        .map_err(|_| RuntimeError::Interaction("压缩执行身份不匹配"))?;
                    let stopping = active.stopping;
                    if stopping {
                        let backend = self.backend.clone();
                        let stop_turn = turn.clone();
                        if jobs.len() >= limits::BACKGROUND_JOBS {
                            return Err(RuntimeError::Capacity("控制容量耗尽，无法停止压缩"));
                        }
                        jobs.spawn(async move {
                            Done::Task(TaskDone::Control {
                                result: backend.interrupt(stop_turn).await,
                            })
                        });
                    }
                    for item in early {
                        if let Some(offer) = super::flow::event(
                            &self.diagnostics,
                            &mut self.tasks.active,
                            &mut self.tasks.scheduler,
                            &self.delivery,
                            item,
                        )? {
                            self.cards.plan_offer = Some(offer);
                        }
                    }
                }
            }
            return Ok(());
        }
        // Remaining notifications belong to the active execution gate; early
        // events are buffered by the gate and replayed in order.
        let early = match self
            .tasks
            .active
            .as_mut()
            .and_then(|active| active.gate.as_mut())
            .map(|gate| {
                gate.event(notification)
                    .map_err(|_| RuntimeError::Backend("Codex 提前事件过多"))
            }) {
            Some(result) => result?,
            None => return Ok(()),
        };
        if let Some(notification) = early {
            if let Some(offer) = super::flow::event(
                &self.diagnostics,
                &mut self.tasks.active,
                &mut self.tasks.scheduler,
                &self.delivery,
                notification,
            )? {
                self.cards.plan_offer = Some(offer);
            }
        }
        Ok(())
    }
}
