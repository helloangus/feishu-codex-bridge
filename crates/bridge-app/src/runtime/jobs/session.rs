//! Durable claim chains over session state: directory selection and creation,
//! preferences, thread resume/archive, reset, compaction and archive sync.
//!
//! Every handler follows one shape: the chain acquired the scheduler's
//! session-mutation gate when its command was accepted, so its terminal event
//! here releases the gate exactly once (`end_session_mutation`) before
//! reporting success or failure. Results unknown to the durable store reject
//! the input and never replay side effects.
use super::super::RuntimeError;
use super::super::flow::{can_spawn, tell};
use super::super::limits;
use super::super::state::{Active, ActiveKind, Done, Runtime, SessionDone};
use crate::execution::Execution;
use crate::sessions;
use bridge_core::ExecutionMode;
use bridge_core::task::{TaskId, TaskSpec};
use tokio::task::JoinSet;
use tokio::time::Instant;

impl Runtime {
    pub(crate) async fn handle_session_done(
        &mut self,
        done: SessionDone,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        match done {
            SessionDone::ArchiveSynced { result } => self.archive_synced(result),
            SessionDone::DirectoryClaim {
                input,
                current,
                target,
                result,
            } => self.directory_claim(input, current, target, result, jobs).await,
            SessionDone::DirectoryProposed {
                user,
                chat,
                current,
                target,
                result,
            } => self.directory_proposed(user, chat, current, target, result, jobs).await,
            SessionDone::CreationClaim {
                input,
                creation,
                result,
            } => self.creation_claim(input, creation, result, jobs).await,
            SessionDone::DirectoryCreated { user, chat, result } => {
                self.directory_finished(
                    user,
                    chat,
                    result,
                    "目录已创建并切换：",
                    "",
                    "目录创建或设置保存失败，当前目录未切换。路径可能已变化；若已创建部分目录会保留，不会自动删除或重试。请检查后重新发送 /cd <路径>。",
                )
            }
            SessionDone::DirectoryChanged { user, chat, result } => {
                self.directory_finished(
                    user,
                    chat,
                    result,
                    "已切换目录：",
                    "\n后续消息使用此目录的会话、模型和 Plan 设置。",
                    "切换目录失败：路径可能已变化或状态保存失败。当前目录未切换；请检查后重新发送 /cd <路径>。",
                )
            }
            SessionDone::DirectoryListed { chat, result } => {
                self.directory_listed(chat, result)
            }
            SessionDone::CompactClaim {
                input,
                session,
                result,
            } => self.compact_claim(input, session, result, jobs).await,
            SessionDone::CompactPrepared { id, result } => {
                self.compact_prepared(id, result, jobs).await
            }
            SessionDone::CompactSubmitted { id, result } => {
                self.compact_submitted(id, result).await
            }
            SessionDone::PreferenceClaim {
                input,
                session,
                change,
                result,
            } => self.preference_claim(input, session, change, result, jobs).await,
            SessionDone::PreferenceChanged { chat, result } => {
                self.preference_changed(chat, result)
            }
            SessionDone::ThreadClaim {
                input,
                session,
                thread,
                action,
                result,
            } => self.thread_claim(input, session, thread, action, result, jobs).await,
            SessionDone::SessionChanged {
                chat,
                action,
                result,
            } => self.session_changed(chat, action, result),
            SessionDone::Reset { input, result } => self.reset(input, result),
        }
    }

    /// One archived-thread sync finished; the invalidation gate reopens.
    fn archive_synced(
        &mut self,
        result: Result<(), sessions::SessionStoreError>,
    ) -> Result<(), RuntimeError> {
        result.map_err(RuntimeError::from)?;
        self.scheduler.end_session_mutation();
        Ok(())
    }

    /// The directory-change claim persisted; propose the target and, when it
    /// does not exist yet, fall through to a creation confirmation card.
    async fn directory_claim(
        &mut self,
        mut input: super::super::Input,
        current: std::path::PathBuf,
        target: String,
        result: Result<bool, ()>,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        match result {
            Ok(true) => {
                input.ack.settle(true);
                if !can_spawn(jobs, false) {
                    self.scheduler.end_session_mutation();
                    tell(&self.delivery, &input.chat, "系统繁忙，请稍后重新发送。")?;
                    return Ok(());
                }
                let store = self.store.clone();
                let root = self.settings.root.clone();
                let user = input.user.clone();
                let chat = input.chat.clone();
                jobs.spawn(async move {
                    Done::Session(SessionDone::DirectoryProposed {
                        result: store
                            .propose_directory(root, current.clone(), target.clone())
                            .await,
                        user,
                        chat,
                        current,
                        target,
                    })
                });
            }
            Ok(false) => {
                self.scheduler.end_session_mutation();
                input.ack.settle(true);
            }
            Err(()) => {
                self.scheduler.end_session_mutation();
                input.ack.settle(false);
                tell(
                    &self.delivery,
                    &input.chat,
                    "目录请求保存失败，未切换；请重新发送。",
                )?;
            }
        }
        Ok(())
    }

    async fn directory_proposed(
        &mut self,
        user: String,
        chat: String,
        current: std::path::PathBuf,
        target: String,
        result: Result<Option<std::path::PathBuf>, sessions::SessionStoreError>,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        match result {
            Ok(None) => {
                if !can_spawn(jobs, false) {
                    self.scheduler.end_session_mutation();
                    tell(&self.delivery, &chat, "系统繁忙，请稍后重新发送。")?;
                    return Ok(());
                }
                let store = self.store.clone();
                let root = self.settings.root.clone();
                jobs.spawn(async move {
                    Done::Session(SessionDone::DirectoryChanged {
                        result: store
                            .change_directory(user.clone(), root, current, target)
                            .await,
                        user,
                        chat,
                    })
                });
            }
            Ok(Some(path)) => {
                self.scheduler.end_session_mutation();
                self.next_confirmation = self
                    .next_confirmation
                    .checked_add(1)
                    .ok_or(RuntimeError::Capacity("确认编号耗尽"))?;
                let token = format!("cd-{}-{}", self.settings.epoch, self.next_confirmation);
                let prompt = format!(
                    "目录不存在：{}\n确认后将创建该目录及缺失的父目录，并切换到此目录。\n\n/cd-confirm {token}\n\n仅限当前用户在此聊天确认，10 分钟内有效；超时或切换目录后失效，不会自动创建。",
                    path.display()
                );
                let entry = crate::directories::Creation {
                    user: user.clone(),
                    chat: chat.clone(),
                    current: current.clone(),
                    input: target,
                    target: path,
                    deadline: Instant::now() + limits::INTERACTION_TIMEOUT,
                };
                if self.confirmations.insert(token.clone(), entry) {
                    if !self.can_spawn(jobs, false) {
                        tell(
                            &self.delivery,
                            &chat,
                            "系统繁忙，请稍后重新发送 /cd <路径>。",
                        )?;
                        return Ok(());
                    }
                    self.next_panel = self
                        .next_panel
                        .checked_add(1)
                        .ok_or(RuntimeError::Capacity("卡片编号耗尽"))?;
                    let (panel, commands) = crate::cards::panel(
                        "创建目录确认",
                        prompt,
                        vec![("确认创建并切换".into(), format!("/cd-confirm {token}"))],
                        &format!("panel-{}-{}", self.settings.epoch, self.next_panel),
                    );
                    let owner = crate::cards::Owner {
                        generation: *self.card_generations.get(&user).unwrap_or(&0),
                        user,
                        chat: chat.clone(),
                        directory: current,
                        stop_snapshot: (self.next_task, None),
                    };
                    super::super::flow::send_panel(
                        &self.diagnostics,
                        None,
                        jobs,
                        self.messenger.clone(),
                        owner,
                        panel,
                        commands,
                    );
                } else {
                    tell(
                        &self.delivery,
                        &chat,
                        "待确认请求已满，未创建目录；请稍后重新发送 /cd <路径>。",
                    )?;
                }
            }
            Err(_) => {
                self.scheduler.end_session_mutation();
                tell(
                    &self.delivery,
                    &chat,
                    "目录路径无效或无法读取；目标须位于工作区内，不能是文件。未创建或切换目录。",
                )?;
            }
        }
        Ok(())
    }

    /// The creation confirmation claim persisted; create the directory unless
    /// the confirmation expired while the claim was in flight.
    async fn creation_claim(
        &mut self,
        mut input: super::super::Input,
        creation: crate::directories::Creation,
        result: Result<bool, ()>,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        match result {
            Ok(true) if Instant::now() < creation.deadline => {
                input.ack.settle(true);
                if !can_spawn(jobs, false) {
                    self.scheduler.end_session_mutation();
                    tell(&self.delivery, &input.chat, "系统繁忙，请稍后重新发送。")?;
                    return Ok(());
                }
                let store = self.store.clone();
                let root = self.settings.root.clone();
                jobs.spawn(async move {
                    Done::Session(SessionDone::DirectoryCreated {
                        result: store
                            .create_directory(
                                creation.user.clone(),
                                root,
                                creation.current,
                                creation.input,
                                creation.target,
                            )
                            .await,
                        user: creation.user,
                        chat: creation.chat,
                    })
                });
            }
            Ok(new) => {
                self.scheduler.end_session_mutation();
                input.ack.settle(true);
                if new {
                    tell(
                        &self.delivery,
                        &input.chat,
                        "创建确认已过期，未执行；请重新发送 /cd <路径>。",
                    )?;
                }
            }
            Err(()) => {
                self.scheduler.end_session_mutation();
                input.ack.settle(false);
                tell(
                    &self.delivery,
                    &input.chat,
                    "创建确认保存失败，未执行；原确认已失效，请重新发送 /cd <路径>。",
                )?;
            }
        }
        Ok(())
    }

    /// Shared tail of directory creation and change: both invalidate the
    /// user's cards, bump the generation and publish the selected directory.
    #[allow(clippy::too_many_arguments)]
    fn directory_finished(
        &mut self,
        user: String,
        chat: String,
        result: Result<std::path::PathBuf, sessions::SessionStoreError>,
        success_prefix: &str,
        success_suffix: &str,
        failure: &str,
    ) -> Result<(), RuntimeError> {
        self.scheduler.end_session_mutation();
        match result {
            Ok(path) => {
                self.confirmations.invalidate(&user);
                self.card_actions.invalidate(&user);
                *self.card_generations.entry(user.clone()).or_default() += 1;
                self.directories.insert(user, path.clone());
                tell(
                    &self.delivery,
                    &chat,
                    format!("{success_prefix}{}{success_suffix}", path.display()),
                )?;
            }
            Err(_) => tell(&self.delivery, &chat, failure.to_owned())?,
        }
        Ok(())
    }

    fn directory_listed(
        &mut self,
        chat: String,
        result: Result<crate::directories::DirectoryView, sessions::SessionStoreError>,
    ) -> Result<(), RuntimeError> {
        tell(
            &self.delivery,
            &chat,
            result.map(|view| view.text()).unwrap_or_else(|_| {
                "当前目录已失效或无法读取；请使用 /cd <工作区内绝对路径> 重新选择。".into()
            }),
        )
    }

    async fn compact_claim(
        &mut self,
        mut input: super::super::Input,
        session: bridge_core::SessionKey,
        result: Result<bool, ()>,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        match result {
            Ok(true) => {
                input.ack.settle(true);
                self.next_task = self
                    .next_task
                    .checked_add(1)
                    .ok_or(RuntimeError::Capacity("任务标识耗尽"))?;
                let id = TaskId::new(self.settings.epoch, self.next_task);
                let chat = input.chat.clone();
                self.active = Some(Active {
                    kind: ActiveKind::Compact {
                        acknowledged: false,
                        terminal: None,
                        thread: None,
                    },
                    spec: TaskSpec {
                        id: id.clone(),
                        session: session.clone(),
                        chat,
                        prompt: String::new(),
                        model: None,
                        mode: ExecutionMode::Execute,
                    },
                    gate: None,
                    turn: None,
                    stopping: false,
                    output: String::new(),
                    plan: None,
                    truncated: false,
                    started: Instant::now(),
                });
                tell(
                    &self.delivery,
                    &input.chat,
                    "正在准备压缩上下文；可发送 /status 或 /stop。",
                )?;
                if !can_spawn(jobs, false) {
                    super::super::flow::finish(
                        &self.diagnostics,
                        &mut self.active,
                        &mut self.scheduler,
                        &self.delivery,
                        "系统繁忙，压缩未启动；请稍后重试。".into(),
                    )?;
                    return Ok(());
                }
                let backend = self.backend.clone();
                let store = self.store.clone();
                let root = self.settings.root.clone();
                jobs.spawn(async move {
                    Done::Session(SessionDone::CompactPrepared {
                        id,
                        result: async {
                            store
                                .validate_directory(root, session.workspace.clone())
                                .await?;
                            sessions::prepare_compaction(backend.as_ref(), store.as_ref(), session)
                                .await
                        }
                        .await,
                    })
                });
            }
            Ok(false) => {
                self.scheduler.end_session_mutation();
                input.ack.settle(true);
            }
            Err(()) => {
                self.scheduler.end_session_mutation();
                input.ack.settle(false);
                tell(
                    &self.delivery,
                    &input.chat,
                    "压缩请求保存失败，未执行；请重新发送。",
                )?;
            }
        }
        Ok(())
    }

    async fn compact_prepared(
        &mut self,
        id: TaskId,
        result: Result<String, sessions::StartError>,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        if !can_spawn(jobs, false)
            && self
                .active
                .as_ref()
                .is_some_and(|active| active.is_compact() && active.spec.id == id)
        {
            super::super::flow::finish(
                &self.diagnostics,
                &mut self.active,
                &mut self.scheduler,
                &self.delivery,
                "系统繁忙，压缩未提交；请稍后重试。".into(),
            )?;
            return Ok(());
        }
        let compacting = self
            .active
            .as_mut()
            .filter(|active| active.is_compact() && active.spec.id == id);
        if let Some(active) = compacting {
            if active.stopping {
                super::super::flow::finish(
                    &self.diagnostics,
                    &mut self.active,
                    &mut self.scheduler,
                    &self.delivery,
                    "准备阶段已停止，未启动压缩。".into(),
                )?;
                return Ok(());
            }
            match result {
                Ok(thread) => {
                    active.gate = Some(Execution::starting(
                        self.settings.epoch,
                        thread.clone(),
                        limits::EARLY_PROTOCOL_EVENTS,
                    ));
                    // Record the expected thread before the request can emit notifications.
                    if let ActiveKind::Compact {
                        thread: expected, ..
                    } = &mut active.kind
                    {
                        *expected = Some(thread.clone());
                    }
                    let backend = self.backend.clone();
                    let id = active.spec.id.clone();
                    jobs.spawn(async move {
                        Done::Session(SessionDone::CompactSubmitted {
                            id,
                            result: backend.compact(thread).await,
                        })
                    });
                }
                Err(error) => super::super::flow::finish(
                    &self.diagnostics,
                    &mut self.active,
                    &mut self.scheduler,
                    &self.delivery,
                    format!("压缩准备失败：{error}；不会自动重试。"),
                )?,
            }
        }
        Ok(())
    }

    async fn compact_submitted(
        &mut self,
        id: TaskId,
        result: Result<(), crate::ports::BackendError>,
    ) -> Result<(), RuntimeError> {
        let compacting = self
            .active
            .as_mut()
            .filter(|active| active.is_compact() && active.spec.id == id);
        if let Some(active) = compacting {
            match result {
                Ok(()) => {
                    let terminal = match &mut active.kind {
                        ActiveKind::Compact {
                            acknowledged,
                            terminal,
                            ..
                        } => {
                            *acknowledged = true;
                            terminal.take()
                        }
                        ActiveKind::Task => None,
                    };
                    if let Some(label) = terminal {
                        super::super::flow::finish(
                            &self.diagnostics,
                            &mut self.active,
                            &mut self.scheduler,
                            &self.delivery,
                            label,
                        )?;
                    } else {
                        let chat = active.spec.chat.clone();
                        tell(
                            &self.delivery,
                            &chat,
                            "已请求压缩上下文，正在等待完成；可发送 /status 或 /stop。",
                        )?;
                    }
                }
                Err(crate::ports::BackendError::Rejected(_)) if active.turn.is_none() => {
                    super::super::flow::finish(
                        &self.diagnostics,
                        &mut self.active,
                        &mut self.scheduler,
                        &self.delivery,
                        "压缩请求被拒绝；不会自动重试。".into(),
                    )?;
                }
                Err(error) => {
                    let chat = active.spec.chat.clone();
                    tell(
                        &self.delivery,
                        &chat,
                        format!("压缩启动结果不确定：{error}；桥接将停止，不会自动重试。"),
                    )?;
                    return Err(RuntimeError::Maintenance("压缩启动结果不确定"));
                }
            }
        }
        Ok(())
    }

    async fn preference_claim(
        &mut self,
        mut input: super::super::Input,
        session: bridge_core::SessionKey,
        change: sessions::PreferenceChange,
        result: Result<bool, ()>,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        match result {
            Ok(true) => {
                input.ack.settle(true);
                if !can_spawn(jobs, false) {
                    self.scheduler.end_session_mutation();
                    tell(&self.delivery, &input.chat, "系统繁忙，请稍后重新发送。")?;
                    return Ok(());
                }
                let backend = self.backend.clone();
                let store = self.store.clone();
                let root = self.settings.root.clone();
                jobs.spawn(async move {
                    Done::Session(SessionDone::PreferenceChanged {
                        chat: input.chat,
                        result: async {
                            store
                                .validate_directory(root, session.workspace.clone())
                                .await?;
                            sessions::change_preference(
                                backend.as_ref(),
                                store.as_ref(),
                                session,
                                change,
                            )
                            .await
                        }
                        .await,
                    })
                });
            }
            Ok(false) => {
                self.scheduler.end_session_mutation();
                input.ack.settle(true);
            }
            Err(()) => {
                self.scheduler.end_session_mutation();
                input.ack.settle(false);
                tell(
                    &self.delivery,
                    &input.chat,
                    "设置请求保存失败，请重新发送。",
                )?;
            }
        }
        Ok(())
    }

    fn preference_changed(
        &mut self,
        chat: String,
        result: Result<(), sessions::StartError>,
    ) -> Result<(), RuntimeError> {
        self.scheduler.end_session_mutation();
        tell(
            &self.delivery,
            &chat,
            match result {
                Ok(()) => "设置已保存，后续任务生效。使用 /model 或 /plan 查看。".into(),
                Err(error) => format!(
                    "设置失败：{error}；模型请从 /models 选择。不会自动重试，请检查后重新发送。"
                ),
            },
        )
    }

    async fn thread_claim(
        &mut self,
        mut input: super::super::Input,
        session: bridge_core::SessionKey,
        thread: String,
        action: sessions::ThreadAction,
        result: Result<bool, ()>,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), RuntimeError> {
        match result {
            Ok(true) => {
                input.ack.settle(true);
                if !can_spawn(jobs, false) {
                    self.scheduler.end_session_mutation();
                    tell(&self.delivery, &input.chat, "系统繁忙，请稍后重新发送。")?;
                    return Ok(());
                }
                let backend = self.backend.clone();
                let store = self.store.clone();
                let root = self.settings.root.clone();
                jobs.spawn(async move {
                    Done::Session(SessionDone::SessionChanged {
                        chat: input.chat,
                        action,
                        result: async {
                            store
                                .validate_directory(root, session.workspace.clone())
                                .await?;
                            sessions::change_thread(
                                backend.as_ref(),
                                store.as_ref(),
                                session,
                                thread,
                                action,
                            )
                            .await
                        }
                        .await,
                    })
                });
            }
            Ok(false) => {
                self.scheduler.end_session_mutation();
                input.ack.settle(true);
            }
            Err(()) => {
                self.scheduler.end_session_mutation();
                input.ack.settle(false);
                tell(
                    &self.delivery,
                    &input.chat,
                    "会话操作请求保存失败，未执行；请重新发送。",
                )?;
            }
        }
        Ok(())
    }

    /// A thread resume/archive/unarchive finished. An uncertain archive result
    /// stops the run: local bindings can no longer be trusted against remote.
    fn session_changed(
        &mut self,
        chat: String,
        action: sessions::ThreadAction,
        result: Result<(), sessions::StartError>,
    ) -> Result<(), RuntimeError> {
        self.scheduler.end_session_mutation();
        let recovery = matches!(&result, Err(sessions::StartError::Reconcile));
        let text = match result {
            Ok(()) => action.success().into(),
            Err(error) => {
                format!("会话操作失败：{error}。目标必须属于当前目录且处于空闲状态；不会自动重试。")
            }
        };
        tell(&self.delivery, &chat, text)?;
        if recovery {
            return Err(RuntimeError::Maintenance(
                "归档结果不确定，已停止运行，需核对本地绑定和远端状态",
            ));
        }
        Ok(())
    }

    /// The reset claim persisted; clear the binding only when this input won
    /// the claim, so a repeated /new can never erase a newer binding.
    fn reset(
        &mut self,
        mut input: super::super::Input,
        result: Result<bool, ()>,
    ) -> Result<(), RuntimeError> {
        self.scheduler.end_session_mutation();
        match result {
            Ok(new) => {
                input.ack.settle(true);
                if new {
                    tell(
                        &self.delivery,
                        &input.chat,
                        "已切换到新会话，下次提问时自动创建。",
                    )?;
                }
            }
            Err(()) => {
                input.ack.settle(false);
                tell(
                    &self.delivery,
                    &input.chat,
                    "新建会话失败，状态结果未确认；请重新发送一条 /new，不会自动重试。",
                )?;
            }
        }
        Ok(())
    }
}
