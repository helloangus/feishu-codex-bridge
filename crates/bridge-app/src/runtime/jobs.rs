//! Background job completion handling. Every completion either advances the
//! flow it belongs to or reports a failure to the user; results unknown to the
//! protocol stop the run instead of retrying.
use super::flow::{can_spawn, send_panel, spawn_interrupt, spawn_reply, tell};
use super::limits;
use super::state::{Active, ActiveKind, Done, FileDelivery, ListedContent, PanelRefresh, Runtime};
use crate::{execution::Execution, ports::BackendError, sessions};
use bridge_core::{ExecutionMode, task::TaskSpec};
use std::path::PathBuf;
use tokio::{task::JoinSet, time::Instant};

impl Runtime {
    pub(crate) async fn handle_done(
        &mut self,
        done: Done,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), String> {
        match done {
            Done::PlanAction {
                input,
                task,
                result,
            } => {
                self.scheduler.end_session_mutation();
                (input.accept)(true);
                match result {
                    Ok(true) => {
                        if let Some(task) = task {
                            let ticket = self
                                .scheduler
                                .reserve(input.id, task)
                                .map_err(|_| "计划实施入队失败".to_owned())?;
                            self.scheduler.commit_admission(ticket, true);
                            tell(
                                &self.delivery,
                                &input.chat,
                                "已关闭 Plan 模式，确认的计划已加入实施队列。",
                            )?;
                        } else {
                            tell(
                                &self.delivery,
                                &input.chat,
                                "保持 Plan 模式，可继续发送消息讨论或修改计划。",
                            )?;
                        }
                    }
                    Ok(false) => {
                        tell(
                            &self.delivery,
                            &input.chat,
                            "此计划选择已处理，不会重复执行。",
                        )?;
                    }
                    Err(error) => tell(
                        &self.delivery,
                        &input.chat,
                        format!(
                            "计划选择处理失败：{error}。未启动实施，设置可能已部分保存；请检查 /plan 和当前会话后重新生成计划，不会自动重试。"
                        ),
                    )?,
                }
            }
            Done::FilesDelivered => {
                self.files = FileDelivery::Idle;
            }
            Done::ApprovalReplied { outcome } => {
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
                            return Err("交互回传失败".into());
                        }
                    }
                }
            }
            Done::ApprovalSent {
                token,
                panel,
                commands,
                result,
            } => {
                if panel.title == "Codex 问答" {
                    crate::diagnostics::emit(
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
                            return Err("问答无法交付".into());
                        }
                        tell(
                            &self.delivery,
                            &pending.owner.chat,
                            "审批卡片发送失败、已失效或登记已满，正在拒绝请求。",
                        )?;
                        spawn_reply(jobs, pending, false)?;
                    }
                }
            }
            Done::PanelUpdated => {
                self.updating_panel = false;
            }
            Done::PanelSent {
                refreshed,
                panel,
                mut entries,
                result,
            } => {
                if refreshed {
                    self.updating_panel = false;
                }
                if let Ok(id) = result {
                    self.card_views.insert(id.0.clone(), panel);
                    entries.retain(|(_, entry)| {
                        entry.generation == *self.card_generations.get(&entry.user).unwrap_or(&0)
                    });
                    for (_, entry) in &mut entries {
                        entry.source = id.0.clone();
                    }
                    if !self.card_actions.insert(entries, Instant::now()) {
                        eprintln!("{{\"event\":\"card_actions_full\"}}");
                    }
                }
            }
            Done::DirectoryClaim {
                input,
                current,
                target,
                result,
            } => match result {
                Ok(true) => {
                    (input.accept)(true);
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
                        Done::DirectoryProposed {
                            result: store
                                .propose_directory(root, current.clone(), target.clone())
                                .await,
                            user,
                            chat,
                            current,
                            target,
                        }
                    });
                }
                Ok(false) => {
                    self.scheduler.end_session_mutation();
                    (input.accept)(true);
                }
                Err(()) => {
                    self.scheduler.end_session_mutation();
                    (input.accept)(false);
                    tell(
                        &self.delivery,
                        &input.chat,
                        "目录请求保存失败，未切换；请重新发送。",
                    )?;
                }
            },
            Done::DirectoryProposed {
                user,
                chat,
                current,
                target,
                result,
            } => match result {
                Ok(None) => {
                    if !can_spawn(jobs, false) {
                        self.scheduler.end_session_mutation();
                        tell(&self.delivery, &chat, "系统繁忙，请稍后重新发送。")?;
                        return Ok(());
                    }
                    let store = self.store.clone();
                    let root = self.settings.root.clone();
                    jobs.spawn(async move {
                        Done::DirectoryChanged {
                            result: store
                                .change_directory(user.clone(), root, current, target)
                                .await,
                            user,
                            chat,
                        }
                    });
                }
                Ok(Some(path)) => {
                    self.scheduler.end_session_mutation();
                    self.next_confirmation = self
                        .next_confirmation
                        .checked_add(1)
                        .ok_or("确认编号耗尽".to_owned())?;
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
                            .ok_or("卡片编号耗尽".to_owned())?;
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
                        send_panel(None, jobs, self.messenger.clone(), owner, panel, commands);
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
            },
            Done::CreationClaim {
                input,
                creation,
                result,
            } => match result {
                Ok(true) if Instant::now() < creation.deadline => {
                    (input.accept)(true);
                    if !can_spawn(jobs, false) {
                        self.scheduler.end_session_mutation();
                        tell(&self.delivery, &input.chat, "系统繁忙，请稍后重新发送。")?;
                        return Ok(());
                    }
                    let store = self.store.clone();
                    let root = self.settings.root.clone();
                    jobs.spawn(async move {
                        Done::DirectoryCreated {
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
                        }
                    });
                }
                Ok(new) => {
                    self.scheduler.end_session_mutation();
                    (input.accept)(true);
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
                    (input.accept)(false);
                    tell(
                        &self.delivery,
                        &input.chat,
                        "创建确认保存失败，未执行；原确认已失效，请重新发送 /cd <路径>。",
                    )?;
                }
            },
            Done::DirectoryCreated { user, chat, result } => {
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
                            format!("目录已创建并切换：{}", path.display()),
                        )?;
                    }
                    Err(_) => tell(
                        &self.delivery,
                        &chat,
                        "目录创建或设置保存失败，当前目录未切换。路径可能已变化；若已创建部分目录会保留，不会自动删除或重试。请检查后重新发送 /cd <路径>。",
                    )?,
                }
            }
            Done::DirectoryChanged { user, chat, result } => {
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
                            format!(
                                "已切换目录：{}\n后续消息使用此目录的会话、模型和 Plan 设置。",
                                path.display()
                            ),
                        )?;
                    }
                    Err(_) => tell(
                        &self.delivery,
                        &chat,
                        "切换目录失败：路径可能已变化或状态保存失败。当前目录未切换；请检查后重新发送 /cd <路径>。",
                    )?,
                }
            }
            Done::DirectoryListed { chat, result } => tell(
                &self.delivery,
                &chat,
                result.map(|view| view.text()).unwrap_or_else(|_| {
                    "当前目录已失效或无法读取；请使用 /cd <工作区内绝对路径> 重新选择。".into()
                }),
            )?,
            Done::CompactClaim {
                input,
                session,
                result,
            } => match result {
                Ok(true) => {
                    (input.accept)(true);
                    let id = input.id;
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
                        super::flow::finish(
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
                        Done::CompactPrepared {
                            id,
                            result: async {
                                store
                                    .validate_directory(root, session.workspace.clone())
                                    .await?;
                                sessions::prepare_compaction(
                                    backend.as_ref(),
                                    store.as_ref(),
                                    session,
                                )
                                .await
                            }
                            .await,
                        }
                    });
                }
                Ok(false) => {
                    self.scheduler.end_session_mutation();
                    (input.accept)(true);
                }
                Err(()) => {
                    self.scheduler.end_session_mutation();
                    (input.accept)(false);
                    tell(
                        &self.delivery,
                        &input.chat,
                        "压缩请求保存失败，未执行；请重新发送。",
                    )?;
                }
            },
            Done::CompactPrepared { id, result } => {
                if !can_spawn(jobs, false)
                    && self
                        .active
                        .as_ref()
                        .is_some_and(|active| active.is_compact() && active.spec.id == id)
                {
                    super::flow::finish(
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
                        super::flow::finish(
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
                                Done::CompactSubmitted {
                                    id,
                                    result: backend.compact(thread).await,
                                }
                            });
                        }
                        Err(error) => super::flow::finish(
                            &mut self.active,
                            &mut self.scheduler,
                            &self.delivery,
                            format!("压缩准备失败：{error}；不会自动重试。"),
                        )?,
                    }
                }
            }
            Done::CompactSubmitted { id, result } => {
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
                                super::flow::finish(
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
                        Err(BackendError::Rejected(_)) if active.turn.is_none() => {
                            super::flow::finish(
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
                            return Err("压缩启动结果不确定".into());
                        }
                    }
                }
            }
            Done::PreferenceClaim {
                input,
                session,
                change,
                result,
            } => match result {
                Ok(true) => {
                    (input.accept)(true);
                    if !can_spawn(jobs, false) {
                        self.scheduler.end_session_mutation();
                        tell(&self.delivery, &input.chat, "系统繁忙，请稍后重新发送。")?;
                        return Ok(());
                    }
                    let backend = self.backend.clone();
                    let store = self.store.clone();
                    let root = self.settings.root.clone();
                    jobs.spawn(async move {
                        Done::PreferenceChanged {
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
                        }
                    });
                }
                Ok(false) => {
                    self.scheduler.end_session_mutation();
                    (input.accept)(true);
                }
                Err(()) => {
                    self.scheduler.end_session_mutation();
                    (input.accept)(false);
                    tell(
                        &self.delivery,
                        &input.chat,
                        "设置请求保存失败，请重新发送。",
                    )?;
                }
            },
            Done::PreferenceChanged { chat, result } => {
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
                )?;
            }
            Done::ArchiveSynced { result } => {
                result.map_err(|_| {
                    "归档通知同步失败，已停止运行；需核对本地绑定，不会自动重跑任务".to_owned()
                })?;
                self.scheduler.end_session_mutation();
            }
            Done::ThreadClaim {
                input,
                session,
                thread,
                action,
                result,
            } => match result {
                Ok(true) => {
                    (input.accept)(true);
                    if !can_spawn(jobs, false) {
                        self.scheduler.end_session_mutation();
                        tell(&self.delivery, &input.chat, "系统繁忙，请稍后重新发送。")?;
                        return Ok(());
                    }
                    let backend = self.backend.clone();
                    let store = self.store.clone();
                    let root = self.settings.root.clone();
                    jobs.spawn(async move {
                        Done::SessionChanged {
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
                        }
                    });
                }
                Ok(false) => {
                    self.scheduler.end_session_mutation();
                    (input.accept)(true);
                }
                Err(()) => {
                    self.scheduler.end_session_mutation();
                    (input.accept)(false);
                    tell(
                        &self.delivery,
                        &input.chat,
                        "会话操作请求保存失败，未执行；请重新发送。",
                    )?;
                }
            },
            Done::SessionChanged {
                chat,
                action,
                result,
            } => {
                self.scheduler.end_session_mutation();
                let recovery = matches!(&result, Err(sessions::StartError::Reconcile));
                let text = match result {
                    Ok(()) => action.success().into(),
                    Err(error) => format!(
                        "会话操作失败：{error}。目标必须属于当前目录且处于空闲状态；不会自动重试。"
                    ),
                };
                tell(&self.delivery, &chat, text)?;
                if recovery {
                    return Err("归档结果不确定，已停止运行，需核对本地绑定和远端状态".into());
                }
            }
            Done::Listed {
                refresh,
                chat,
                user,
                directory,
                generation,
                stop_snapshot,
                result,
            } => match result {
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
            },
            Done::Reset { input, result } => {
                self.scheduler.end_session_mutation();
                match result {
                    Ok(new) => {
                        (input.accept)(true);
                        if new {
                            tell(
                                &self.delivery,
                                &input.chat,
                                "已切换到新会话，下次提问时自动创建。",
                            )?;
                        }
                    }
                    Err(()) => {
                        (input.accept)(false);
                        tell(
                            &self.delivery,
                            &input.chat,
                            "新建会话失败，状态结果未确认；请重新发送一条 /new，不会自动重试。",
                        )?;
                    }
                }
            }
            Done::Admission {
                ticket,
                input,
                result,
            } => match result {
                Ok(new) => {
                    let queued = self.scheduler.commit_admission(ticket, new);
                    (input.accept)(true);
                    if queued {
                        tell(&self.delivery, &input.chat, "请求已接收。")?;
                    }
                }
                Err(_) => {
                    self.scheduler.abort_admission(ticket);
                    (input.accept)(false);
                    tell(
                        &self.delivery,
                        &input.chat,
                        "接收状态保存失败，未启动任务。",
                    )?;
                }
            },
            Done::Prepared { id, result } => {
                crate::diagnostics::emit(
                    crate::diagnostics::Event::TaskPrepared,
                    if result.is_ok() {
                        crate::diagnostics::Status::Ok
                    } else {
                        crate::diagnostics::Status::Failed
                    },
                    Some(&id),
                    0,
                );
                if !can_spawn(jobs, false)
                    && self
                        .active
                        .as_ref()
                        .is_some_and(|active| active.spec.id == id)
                {
                    super::flow::finish(
                        &mut self.active,
                        &mut self.scheduler,
                        &self.delivery,
                        "系统繁忙，任务未启动；请稍后重试。".into(),
                    )?;
                    return Ok(());
                }
                let preparing = self.active.as_mut().filter(|active| active.spec.id == id);
                if let Some(active) = preparing {
                    if active.stopping {
                        super::flow::finish(
                            &mut self.active,
                            &mut self.scheduler,
                            &self.delivery,
                            "准备阶段已停止，未启动任务".into(),
                        )?;
                        return Ok(());
                    }
                    match result {
                        Ok(input) => {
                            active.spec.mode = input.mode;
                            active.gate = Some(Execution::starting(
                                self.settings.epoch,
                                input.thread_id.clone(),
                                64,
                            ));
                            let backend = self.backend.clone();
                            let id = active.spec.id.clone();
                            jobs.spawn(async move {
                                Done::Started {
                                    id,
                                    result: backend.start_turn(input).await,
                                }
                            });
                        }
                        Err(error) => super::flow::finish(
                            &mut self.active,
                            &mut self.scheduler,
                            &self.delivery,
                            format!("准备失败：{error}"),
                        )?,
                    }
                }
            }
            Done::Started { id, result } => {
                crate::diagnostics::emit(
                    crate::diagnostics::Event::TaskStarted,
                    if result.is_ok() {
                        crate::diagnostics::Status::Ok
                    } else {
                        crate::diagnostics::Status::Failed
                    },
                    Some(&id),
                    0,
                );
                let starting = self.active.as_mut().filter(|active| active.spec.id == id);
                if let Some(active) = starting {
                    match result {
                        Ok(turn) => {
                            active.turn = Some(turn.clone());
                            let early = active
                                .gate
                                .as_mut()
                                .ok_or("缺少执行状态".to_owned())?
                                .bind(turn.clone())
                                .map_err(|_| "执行身份不匹配".to_owned())?;
                            if active.stopping {
                                spawn_interrupt(jobs, self.backend.clone(), turn.clone())?;
                            }
                            for item in early {
                                if let Some(offer) = super::flow::event(
                                    &mut self.active,
                                    &mut self.scheduler,
                                    &self.delivery,
                                    item,
                                )? {
                                    self.plan_offer = Some(offer);
                                }
                            }
                        }
                        Err(error) => {
                            super::flow::finish(
                                &mut self.active,
                                &mut self.scheduler,
                                &self.delivery,
                                format!("启动结果未知或失败：{error}；不会自动重试。"),
                            )?;
                            return Err("Codex 启动失败，已停止运行以避免重复执行".into());
                        }
                    }
                }
            }
            Done::Control { result } => {
                result.map_err(|_| "Codex 控制请求失败，需重启连接".to_owned())?;
            }
        }
        Ok(())
    }

    /// Deliver a freshly built list panel, refreshing the original card when
    /// the user asked for it.
    #[allow(clippy::too_many_arguments)]
    fn deliver_list(
        &mut self,
        jobs: &mut JoinSet<Done>,
        refresh: Option<String>,
        chat: String,
        user: String,
        directory: PathBuf,
        generation: u64,
        stop_snapshot: (u64, Option<String>),
        built: (bridge_core::view::Panel, Vec<(String, String)>),
    ) -> Result<(), String> {
        self.next_panel = self
            .next_panel
            .checked_add(1)
            .ok_or("卡片编号耗尽".to_owned())?;
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
            send_panel(None, jobs, self.messenger.clone(), owner, panel, commands);
        }
        Ok(())
    }
}
