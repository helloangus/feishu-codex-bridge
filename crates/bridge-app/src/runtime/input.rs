//! User input handling: pairing, authorization, card clicks, commands,
//! answers and task admission. Every path acknowledges the input exactly once.
use super::flow::{can_spawn, send_panel, spawn_interrupt, spawn_reply, tell};
use super::state::{Done, FileDelivery, Runtime};
use crate::{interactions::{Choice, TextOutcome, Pending}, sessions};
use bridge_core::{command::Command, task::TaskSpec, ExecutionMode, SessionKey};
use std::{collections::BTreeMap, path::PathBuf};
use tokio::{
    task::JoinSet,
    time::{Duration, Instant},
};

const ANSWER_LIMIT_BYTES: usize = 16 * 1024;

/// Whether a pending interaction still belongs to the running task turn.
/// Stopping tasks and finished turns revoke their open interactions.
fn turn_is_live(active: Option<&super::state::Active>, pending: &Pending) -> bool {
    active.is_some_and(|active: &super::state::Active| {
        !active.stopping
            && active.spec.id == pending.task
            && active
                .gate
                .as_ref()
                .is_some_and(|gate| gate.accepts_request(&pending.request.turn))
    })
}

impl Runtime {
    pub(crate) async fn handle_input(
        &mut self,
        mut input: super::Input,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), String> {
        if input.id.is_empty() || input.user.is_empty() || input.chat.is_empty() {
            (input.accept)(false);
            return Ok(());
        }
        if input.card.is_none()
            && input
                .text
                .as_deref()
                .is_some_and(|text| text.split_whitespace().next() == Some("/pair"))
        {
            return self.handle_pairing(input).await;
        }
        if !self.settings.open_access && !self.allowed.contains(&input.user) {
            tell(
                &self.delivery,
                &input.chat,
                "当前飞书用户尚未授权。如管理员启用了配对，请发送 /pair <配对码>；否则请联系管理员加入白名单。",
            )?;
            (input.accept)(true);
            return Ok(());
        }
        let current = self
            .directories
            .get(&input.user)
            .unwrap_or(&self.settings.directory)
            .clone();
        if input.attachments.len() > 10 {
            tell(&self.delivery, &input.chat, "单条消息最多接收 10 个附件。")?;
            (input.accept)(true);
            return Ok(());
        }
        if !input.attachments.is_empty() {
            if input.card.is_some() {
                (input.accept)(false);
                return Ok(());
            }
            let text = input
                .text
                .get_or_insert_with(|| "请查看附件并处理用户请求。".into());
            if text.starts_with('/') {
                text.insert_str(0, "附件说明：");
            }
        }
        let mut refresh = None;
        let mut approval_source = None;
        if let Some(click) = input.card.take() {
            let snapshot = self.card_snapshot();
            let Some(command) = self.card_actions.take(
                &click,
                &input.user,
                &input.chat,
                &current,
                Instant::now(),
                &snapshot,
            ) else {
                tell(
                    &self.delivery,
                    &input.chat,
                    "卡片操作无效、已使用或已过期；请重新发送 /help 或 /cd <路径>。",
                )?;
                (input.accept)(true);
                return Ok(());
            };
            approval_source = Some(click.source.clone());
            if matches!(command.as_str(), "/resume" | "/archived" | "/models")
                && self.card_views.is_list(&click.source)
            {
                self.card_actions.invalidate_source(&click.source);
                self.card_views.remove(&click.source);
                refresh = Some(click.source.clone());
            }
            input.id = format!("card:{}", click.token);
            input.text = Some(command);
        }
        let text = input.text.as_deref().unwrap_or("").trim().to_owned();
        if text.starts_with("/plan-action ") {
            return self
                .handle_plan_action(input, text, approval_source, current, jobs)
                .await;
        }
        if matches!(self.files, FileDelivery::Delivering)
            && text.starts_with('/')
            && !matches!(
                text.as_str(),
                "/status" | "/help" | "/stop" | "/model" | "/models" | "/resume" | "/archived"
                    | "/plan" | "/cd"
            )
        {
            tell(
                &self.delivery,
                &input.chat,
                "正在整理本轮成果，请交付结束后再修改会话、目录或设置。",
            )?;
            (input.accept)(true);
            return Ok(());
        }
        if text.starts_with("/answer ") || text.starts_with("/answer-skip ") {
            return self
                .handle_answer(input, text, approval_source, current, jobs)
                .await;
        }
        if text.starts_with("/choice ") {
            return self
                .handle_choice(input, text, approval_source, current, jobs)
                .await;
        }
        let session = SessionKey::new(&input.user, &current);
        if text.starts_with('/')
            && !matches!(
                text.as_str(),
                "/help" | "/status" | "/models" | "/model" | "/plan" | "/cd" | "/resume"
                    | "/archived"
            )
        {
            self.plan_offer = None;
        }
        let command = Command::parse(&text);
        if let Ok(Command::Approve { token, allow }) = &command {
            return self
                .handle_approve(input, token, *allow, approval_source, current, jobs)
                .await;
        }
        // Command is matched by value inside the branch handlers.
        if text == "/help" {
            if seen_insert(&mut self.seen_commands, &input.id) {
                if !self.can_spawn(jobs, false) {
                    tell(&self.delivery, &input.chat, "系统繁忙，请稍后重试。")?;
                } else {
                    self.next_panel = self
                        .next_panel
                        .checked_add(1)
                        .ok_or("卡片编号耗尽".to_owned())?;
                    let (panel, commands) = crate::cards::help(&format!(
                        "panel-{}-{}",
                        self.settings.epoch, self.next_panel
                    ));
                    let owner = crate::cards::Owner {
                        user: input.user.clone(),
                        chat: input.chat.clone(),
                        directory: current.clone(),
                        generation: *self.card_generations.get(&input.user).unwrap_or(&0),
                        stop_snapshot: self.card_snapshot(),
                    };
                    send_panel(None, jobs, self.messenger.clone(), owner, panel, commands);
                }
            }
            (input.accept)(true);
            return Ok(());
        }
        if let Ok(Command::ConfirmDirectory(token)) = &command {
            let Some(creation) =
                self.confirmations
                    .get(token, &input.user, &input.chat, &current, Instant::now())
            else {
                tell(
                    &self.delivery,
                    &input.chat,
                    "创建确认无效、已过期或不属于当前用户/聊天/目录；请重新发送 /cd <路径>。",
                )?;
                (input.accept)(true);
                return Ok(());
            };
            if !can_spawn(jobs, false) {
                tell(&self.delivery, &input.chat, "系统繁忙，请稍后重试。")?;
                (input.accept)(true);
                return Ok(());
            }
            if !self.scheduler.begin_session_mutation() {
                tell(
                    &self.delivery,
                    &input.chat,
                    "有任务执行中、排队或目录正在更新；请空闲后在有效期内再次确认。",
                )?;
                (input.accept)(true);
                return Ok(());
            }
            self.confirmations.remove(token);
            let store = self.store.clone();
            jobs.spawn(async move {
                let result = store.claim(input.id.clone()).await.map_err(|_| ());
                Done::CreationClaim {
                    input,
                    creation,
                    result,
                }
            });
            return Ok(());
        }
        if let Ok(Command::ChangeDirectory(target)) = &command {
            if let Some(target) = target {
                if target.len() > 4096 || target.chars().any(char::is_control) {
                    tell(
                        &self.delivery,
                        &input.chat,
                        "目录参数无效：最多 4096 字节，不能包含换行或控制字符。",
                    )?;
                    (input.accept)(true);
                    return Ok(());
                }
                if !can_spawn(jobs, false) {
                    tell(&self.delivery, &input.chat, "系统繁忙，请稍后重试。")?;
                    (input.accept)(true);
                    return Ok(());
                }
                if !self.scheduler.begin_session_mutation() {
                    tell(
                        &self.delivery,
                        &input.chat,
                        "有任务执行中、排队或目录正在更新；请等待完成或先 /stop，再切换目录。",
                    )?;
                    (input.accept)(true);
                    return Ok(());
                }
                let target = target.clone();
                let store = self.store.clone();
                jobs.spawn(async move {
                    let result = store.claim(input.id.clone()).await.map_err(|_| ());
                    Done::DirectoryClaim {
                        input,
                        current,
                        target,
                        result,
                    }
                });
            } else {
                if seen_insert(&mut self.seen_commands, &input.id) {
                    if !can_spawn(jobs, false) {
                        tell(&self.delivery, &input.chat, "系统繁忙，请稍后重试。")?;
                    } else {
                        let store = self.store.clone();
                        let root = self.settings.root.clone();
                        let chat = input.chat.clone();
                        jobs.spawn(async move {
                            Done::DirectoryListed {
                                chat,
                                result: store.inspect_directory(root, current).await,
                            }
                        });
                    }
                }
                (input.accept)(true);
            }
            return Ok(());
        }
        let invalid_directory = !current.is_absolute()
            || !current.starts_with(&self.settings.root)
            || current
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir));
        if invalid_directory && !matches!(text.as_str(), "/help" | "/status" | "/stop") {
            tell(
                &self.delivery,
                &input.chat,
                "保存的目录已越出工作区；请使用 /cd <工作区内绝对路径> 重新选择。",
            )?;
            (input.accept)(true);
            return Ok(());
        }
        if text == "/compact" {
            if !can_spawn(jobs, false) {
                tell(&self.delivery, &input.chat, "系统繁忙，请稍后重试。")?;
                (input.accept)(true);
                return Ok(());
            }
            if !self.scheduler.begin_session_mutation() {
                tell(
                    &self.delivery,
                    &input.chat,
                    "有任务执行中、排队或会话正在更新；请等待完成或先 /stop，再压缩上下文。",
                )?;
                (input.accept)(true);
                return Ok(());
            }
            let store = self.store.clone();
            jobs.spawn(async move {
                let result = store.claim(input.id.clone()).await.map_err(|_| ());
                Done::CompactClaim {
                    input,
                    session,
                    result,
                }
            });
            return Ok(());
        }
        if matches!(
            &command,
            Ok(Command::Models | Command::Model(_) | Command::Plan(_))
        ) {
            return self
                .handle_preferences(input, command, refresh, session, current, jobs)
                .await;
        }
        if matches!(
            &command,
            Ok(Command::Resume(_) | Command::Archive(_) | Command::Unarchive(_) | Command::Archived)
        ) {
            return self
                .handle_threads(input, command, refresh, session, current, jobs)
                .await;
        }
        if text == "/new" {
            if !can_spawn(jobs, false) {
                tell(&self.delivery, &input.chat, "系统繁忙，请稍后重试。")?;
                (input.accept)(true);
                return Ok(());
            }
            if !self.scheduler.begin_session_mutation() {
                tell(
                    &self.delivery,
                    &input.chat,
                    "有任务执行中、排队或会话正在更新；请等待完成或先 /stop，再发送 /new。",
                )?;
                (input.accept)(true);
                return Ok(());
            }
            let store = self.store.clone();
            jobs.spawn(async move {
                // Claim before clearing. A repeated command must never
                // erase a newer binding, even after a process restart.
                let result = async {
                    let new = store.claim(input.id.clone()).await.map_err(|_| ())?;
                    if new {
                        store.clear(session).await.map_err(|_| ())?;
                    }
                    Ok(new)
                }
                .await;
                Done::Reset { input, result }
            });
            return Ok(());
        }
        if input.text.is_none() || text.is_empty() || text.starts_with('/') {
            self.handle_simple_command(input, text, session, current, jobs)
                .await?;
            return Ok(());
        }
        if text.len() > 32 * 1024 {
            tell(&self.delivery, &input.chat, "输入超过 32 KiB 上限。")?;
            (input.accept)(true);
            return Ok(());
        }
        self.admit_task(input, text, session, current, jobs).await
    }

    async fn handle_pairing(&mut self, input: super::Input) -> Result<(), String> {
        if self.pairing_window.elapsed() >= Duration::from_secs(60) {
            self.pairing_window = Instant::now();
            self.pairing_attempts = 0;
        }
        if self.pairing_attempts >= 10 {
            (input.accept)(true);
            return Ok(());
        }
        self.pairing_attempts += 1;
        let parts: Vec<_> = input
            .text
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .collect();
        let code = if parts.len() == 2 && parts[1].len() <= 256 && input.attachments.is_empty() {
            parts[1].to_owned()
        } else {
            String::new()
        };
        match self.store.pair(input.user.clone(), code).await {
            Ok(true) => {
                self.allowed.insert(input.user.clone());
                tell(
                    &self.delivery,
                    &input.chat,
                    "配对成功，后续消息可使用机器人。发送 /help 查看功能。",
                )?;
                (input.accept)(true);
            }
            Ok(false) => {
                tell(
                    &self.delivery,
                    &input.chat,
                    "配对未成功，请核对配对码或联系管理员。",
                )?;
                (input.accept)(true);
            }
            Err(_) => {
                tell(
                    &self.delivery,
                    &input.chat,
                    "配对保存失败，尚未授权，请稍后重试。",
                )?;
                (input.accept)(false);
            }
        }
        Ok(())
    }

    async fn handle_plan_action(
        &mut self,
        input: super::Input,
        text: String,
        approval_source: Option<String>,
        current: PathBuf,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), String> {
        let parts: Vec<_> = text.split_whitespace().collect();
        let valid = parts.len() == 3
            && matches!(parts[2], "implement" | "fresh" | "stay")
            && approval_source.is_some()
            && self.plan_offer.as_ref().is_some_and(|offer| {
                offer.token == parts[1]
                    && offer.task.session.user == input.user
                    && offer.task.chat == input.chat
                    && offer.task.session.workspace == current
                    && Instant::now() < offer.deadline
            });
        if !valid {
            tell(
                &self.delivery,
                &input.chat,
                "计划操作无效或已过期，请重新生成计划并使用原卡片。",
            )?;
            (input.accept)(true);
            return Ok(());
        }
        if matches!(self.files, FileDelivery::Delivering) || !self.scheduler.begin_session_mutation()
        {
            tell(
                &self.delivery,
                &input.chat,
                "有任务或成果交付进行中，请空闲后重新生成计划。",
            )?;
            (input.accept)(true);
            return Ok(());
        }
        if !can_spawn(jobs, false) {
            self.scheduler.end_session_mutation();
            tell(&self.delivery, &input.chat, "系统繁忙，请稍后重新生成计划。")?;
            (input.accept)(true);
            return Ok(());
        }
        let offer = self.plan_offer.take().ok_or("缺少计划".to_owned())?;
        let action = parts[2].to_owned();
        if let Some(source) = &approval_source {
            self.card_actions.invalidate_source(source);
            self.card_views
                .note(source, "已收到选择；实际结果请查看单独回复。");
        }
        self.next_task = self
            .next_task
            .checked_add(1)
            .ok_or("任务标识耗尽".to_owned())?;
        let task = if action == "stay" {
            None
        } else {
            Some(TaskSpec {
                id: format!("{}:{}", self.settings.epoch, self.next_task),
                session: offer.task.session.clone(),
                chat: input.chat.clone(),
                prompt: format!("请实施以下已确认的计划：\n\n{}", offer.text),
                model: None,
                mode: ExecutionMode::Execute,
            })
        };
        let store = self.store.clone();
        let backend = self.backend.clone();
        let root = self.settings.root.clone();
        jobs.spawn(async move {
            let result = async {
                let new = store
                    .claim(input.id.clone())
                    .await
                    .map_err(|e| e.to_string())?;
                if !new {
                    return Ok(false);
                }
                let session = offer.task.session;
                store
                    .validate_directory(root, session.workspace.clone())
                    .await
                    .map_err(|e| e.to_string())?;
                if store
                    .thread(session.clone())
                    .await
                    .map_err(|e| e.to_string())?
                    .as_deref()
                    != Some(&offer.thread)
                    || !store
                        .preferences(session.clone())
                        .await
                        .map_err(|e| e.to_string())?
                        .plan
                {
                    return Err("计划所属会话或模式已经变化".into());
                }
                sessions::prepare_compaction(backend.as_ref(), store.as_ref(), session.clone())
                    .await
                    .map_err(|e| e.to_string())?;
                store
                    .set_preference(
                        session.clone(),
                        sessions::PreferenceChange::Plan(action == "stay"),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                if action == "fresh" {
                    store.clear(session).await.map_err(|e| e.to_string())?;
                }
                Ok(true)
            }
            .await;
            Done::PlanAction { input, task, result }
        });
        Ok(())
    }

    async fn handle_answer(
        &mut self,
        input: super::Input,
        text: String,
        approval_source: Option<String>,
        current: PathBuf,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), String> {
        let skipping = text.starts_with("/answer-skip ");
        let mut parts = text.splitn(4, char::is_whitespace);
        let _ = parts.next();
        let token = parts.next().unwrap_or("").to_owned();
        let index = parts.next().and_then(|value| value.parse::<usize>().ok());
        let answer = parts.next().unwrap_or("").to_owned();
        let mut outcome = TextOutcome::Invalid;
        if approval_source.is_none()
            && input.attachments.is_empty()
            && ((skipping && answer.is_empty())
                || (!skipping
                    && !answer.trim().is_empty()
                    && answer.len() <= ANSWER_LIMIT_BYTES
                    && !answer
                        .chars()
                        .any(|c| c.is_control() && c != '\n' && c != '\t')))
        {
            if let Some(index) = index {
                let active = self.active.as_ref();
                outcome = self.approvals.answer_text(
                    &token,
                    &input.user,
                    &input.chat,
                    &current,
                    Instant::now(),
                    index,
                    &answer,
                    skipping,
                    |pending| turn_is_live(active, pending),
                );
            }
        }
        match outcome {
            TextOutcome::Recorded { complete, finished } => {
                tell(&self.delivery, &input.chat, "答案已记录。")?;
                if complete {
                    if let Some(pending) = finished {
                        spawn_reply(jobs, pending, false)?;
                    }
                }
            }
            TextOutcome::Invalid => {
                tell(
                    &self.delivery,
                    &input.chat,
                    "答案未接收：请先点击当前题的自行回答按钮，使用提示里的完整命令；答案须非空且不超过 16 KiB。",
                )?;
            }
        }
        (input.accept)(true);
        Ok(())
    }

    async fn handle_choice(
        &mut self,
        input: super::Input,
        text: String,
        approval_source: Option<String>,
        current: PathBuf,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), String> {
        let parts: Vec<_> = text.split_whitespace().collect();
        let mut outcome = Choice::Invalid;
        if parts.len() == 4 {
            if let Some(source) = &approval_source {
                let active = self.active.as_ref();
                outcome = self.approvals.answer_choice(
                    &parts[1],
                    &input.user,
                    &input.chat,
                    &current,
                    Instant::now(),
                    source,
                    parts[2].parse::<usize>().unwrap_or(usize::MAX),
                    parts[3],
                    |pending| turn_is_live(active, pending),
                );
            }
        }
        match outcome {
            Choice::Recorded { complete, finished } => {
                self.card_actions.invalidate_approval(&parts[1]);
                // The click source equals the entry source whenever the
                // recording was accepted, so the note uses the click.
                if let Some(source) = &approval_source {
                    self.card_views
                        .note(source, "本题已记录，其他按钮已失效。");
                }
                if complete {
                    if let Some(pending) = finished {
                        spawn_reply(jobs, pending, false)?;
                    }
                }
            }
            Choice::WaitingText => {
                self.card_actions.invalidate_approval(&parts[1]);
                if let Some(source) = &approval_source {
                    self.card_views
                        .note(source, "已进入自行回答，请按单独提示发送答案。");
                }
                let question = self
                    .approvals
                    .get(&parts[1])
                    .map(|pending| pending.question)
                    .unwrap_or_default();
                tell(
                    &self.delivery,
                    &input.chat,
                    format!(
                        "请发送 /answer {} {} <答案>\n仍可使用 /stop 停止任务；答案不会在确认回复或日志中回显，飞书聊天会保留你发送的内容。",
                        parts[1], question
                    ),
                )?;
                (input.accept)(true);
                return Ok(());
            }
            Choice::Invalid => {
                tell(
                    &self.delivery,
                    &input.chat,
                    "问答操作无效或已过期，请使用当前问题卡片。",
                )?;
            }
        }
        (input.accept)(true);
        Ok(())
    }

    async fn handle_approve(
        &mut self,
        input: super::Input,
        token: &str,
        allow: bool,
        approval_source: Option<String>,
        current: PathBuf,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), String> {
        let source = approval_source.clone();
        let active = self.active.as_ref();
        let pending = source.as_deref().and_then(|source| {
            self.approvals.approve(
                token,
                &input.user,
                &input.chat,
                &current,
                Instant::now(),
                source,
                |pending| turn_is_live(active, pending),
            )
        });
        match pending {
            Some(pending) => {
                self.card_actions.invalidate_approval(token);
                if let Some(source) = &pending.source {
                    self.card_views
                        .note(source, "审批选择已接收，回传结果请查看单独回复。");
                }
                spawn_reply(jobs, pending, allow)?;
            }
            None => {
                tell(
                    &self.delivery,
                    &input.chat,
                    "审批无效或已过期；请仅使用当前任务的审批卡片按钮。",
                )?;
            }
        }
        (input.accept)(true);
        Ok(())
    }

    async fn handle_preferences(
        &mut self,
        input: super::Input,
        command: Result<Command, bridge_core::command::ParseError>,
        refresh: Option<String>,
        session: SessionKey,
        _current: PathBuf,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), String> {
        let change = match &command {
            Ok(Command::Model(Some(model))) => Some(sessions::PreferenceChange::Model(
                if model == "default" { None } else { Some(model.clone()) },
            )),
            Ok(Command::Plan(Some(value))) => Some(sessions::PreferenceChange::Plan(*value)),
            _ => None,
        };
        if let Some(change) = change {
            if !can_spawn(jobs, false) {
                tell(&self.delivery, &input.chat, "系统繁忙，请稍后重试。")?;
                (input.accept)(true);
                return Ok(());
            }
            if !self.scheduler.begin_session_mutation() {
                tell(
                    &self.delivery,
                    &input.chat,
                    "有任务执行中、排队或设置正在更新；请等待完成或先 /stop，再修改设置。",
                )?;
                (input.accept)(true);
                return Ok(());
            }
            let store = self.store.clone();
            jobs.spawn(async move {
                let result = store.claim(input.id.clone()).await.map_err(|_| ());
                Done::PreferenceClaim {
                    input,
                    session,
                    change,
                    result,
                }
            });
        } else {
            if seen_insert(&mut self.seen_commands, &input.id) {
                if !can_spawn(jobs, false) {
                    tell(&self.delivery, &input.chat, "系统繁忙，请稍后重试。")?;
                } else {
                    let store = self.store.clone();
                    let backend = self.backend.clone();
                    let chat = input.chat.clone();
                    let user = input.user.clone();
                    let directory = session.workspace.clone();
                    let generation = *self.card_generations.get(&user).unwrap_or(&0);
                    let stop_snapshot = self.card_snapshot();
                    jobs.spawn(async move {
                        let result = async {
                            let preferences = store.preferences(session).await?;
                            match command {
                                Ok(Command::Models) => {
                                    let models = backend.models().await?;
                                    Ok(super::state::ListedContent::Models {
                                        entries: models,
                                        current: Some(
                                            preferences
                                                .model
                                                .unwrap_or_else(|| sessions::DEFAULT_MODEL.into()),
                                        ),
                                    })
                                }
                                Ok(Command::Model(None)) => Ok(
                                    super::state::ListedContent::Text(format!(
                                        "当前模型：{}",
                                        preferences
                                            .model
                                            .unwrap_or_else(|| sessions::DEFAULT_MODEL.into())
                                    )),
                                ),
                                _ => Ok(super::state::ListedContent::Text(format!(
                                    "Plan 模式：{}。使用 /plan on 或 /plan off 切换。",
                                    if preferences.plan { "已开启" } else { "已关闭" }
                                ))),
                            }
                        }
                        .await;
                        Done::Listed {
                            refresh,
                            chat,
                            user,
                            directory,
                            generation,
                            stop_snapshot,
                            result,
                        }
                    });
                }
            }
            (input.accept)(true);
        }
        Ok(())
    }

    async fn handle_threads(
        &mut self,
        input: super::Input,
        command: Result<Command, bridge_core::command::ParseError>,
        refresh: Option<String>,
        session: SessionKey,
        _current: PathBuf,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), String> {
        let (target, action, archived) = match command {
            Ok(Command::Resume(target)) => (target, sessions::ThreadAction::Resume, false),
            Ok(Command::Archive(id)) => (Some(id), sessions::ThreadAction::Archive, false),
            Ok(Command::Unarchive(id)) => (Some(id), sessions::ThreadAction::Unarchive, true),
            _ => (None, sessions::ThreadAction::Unarchive, true),
        };
        if let Some(thread) = target {
            if !sessions::valid_thread_id(&thread) {
                tell(
                    &self.delivery,
                    &input.chat,
                    "会话 ID 无效，请复制 /resume 或 /archived 列表中的完整命令。",
                )?;
                (input.accept)(true);
                return Ok(());
            }
            if !can_spawn(jobs, false) {
                tell(&self.delivery, &input.chat, "系统繁忙，请稍后重试。")?;
                (input.accept)(true);
                return Ok(());
            }
            if !self.scheduler.begin_session_mutation() {
                tell(
                    &self.delivery,
                    &input.chat,
                    "有任务执行中、排队或会话正在更新；请等待完成或先 /stop，再操作会话。",
                )?;
                (input.accept)(true);
                return Ok(());
            }
            let store = self.store.clone();
            jobs.spawn(async move {
                let result = store.claim(input.id.clone()).await.map_err(|_| ());
                Done::ThreadClaim {
                    input,
                    session,
                    thread,
                    action,
                    result,
                }
            });
        } else {
            if seen_insert(&mut self.seen_commands, &input.id) {
                if !can_spawn(jobs, false) {
                    tell(&self.delivery, &input.chat, "系统繁忙，请稍后重试。")?;
                } else {
                    let backend = self.backend.clone();
                    let chat = input.chat.clone();
                    let store = self.store.clone();
                    let root = self.settings.root.clone();
                    let user = input.user.clone();
                    let directory = session.workspace.clone();
                    let generation = *self.card_generations.get(&user).unwrap_or(&0);
                    let stop_snapshot = self.card_snapshot();
                    jobs.spawn(async move {
                        Done::Listed {
                            refresh,
                            chat,
                            user,
                            directory,
                            generation,
                            stop_snapshot,
                            result: async {
                                store
                                    .validate_directory(root, session.workspace.clone())
                                    .await?;
                                let entries =
                                    sessions::list_entries(backend.as_ref(), &session, archived)
                                        .await?;
                                Ok(super::state::ListedContent::Threads { entries, archived })
                            }
                            .await,
                        }
                    });
                }
            }
            (input.accept)(true);
        }
        Ok(())
    }

    async fn handle_simple_command(
        &mut self,
        input: super::Input,
        text: String,
        session: SessionKey,
        current: PathBuf,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), String> {
        if seen_insert(&mut self.seen_commands, &input.id) {
            match text.as_str() {
                "/status" => {
                    let state = self
                        .active
                        .as_ref()
                        .map(|active| {
                            format!(
                                "{}，已用 {} 秒{}",
                                if active.compact { "上下文压缩中" } else { "运行中" },
                                active.started.elapsed().as_secs(),
                                if active.stopping { "，正在停止" } else { "" }
                            )
                        })
                        .unwrap_or_else(|| "空闲".into());
                    tell(
                        &self.delivery,
                        &input.chat,
                        format!(
                            "Rust 桥接服务：{state}\n当前目录：{}\n等待：{}，保存中：{}",
                            current.display(),
                            self.scheduler.queued(),
                            self.scheduler.pending_admissions()
                        ),
                    )?;
                }
                "/stop" => {
                    let removed = self.scheduler.cancel_queued(&session);
                    let mut stopping = false;
                    if let Some(active) = self
                        .active
                        .as_mut()
                        .filter(|active| {
                            active.spec.session == session
                                && active.spec.chat == input.chat
                                && active.compact_outcome.is_none()
                        })
                    {
                        stopping = true;
                        if !active.stopping {
                            active.stopping = true;
                            if let Some(turn) = active.turn.clone() {
                                spawn_interrupt(jobs, self.backend.clone(), turn)?;
                            }
                        }
                    }
                    tell(
                        &self.delivery,
                        &input.chat,
                        format!(
                            "已取消 {removed} 项等待请求；{}",
                            if stopping {
                                "已请求停止当前任务"
                            } else {
                                "没有可停止的当前任务"
                            }
                        ),
                    )?;
                }
                _ => {
                    tell(
                        &self.delivery,
                        &input.chat,
                        "命令暂不支持或参数无效，请发送 /help 查看支持的命令。",
                    )?;
                }
            }
        }
        (input.accept)(true);
        Ok(())
    }

    async fn admit_task(
        &mut self,
        mut input: super::Input,
        text: String,
        session: SessionKey,
        _current: PathBuf,
        jobs: &mut JoinSet<Done>,
    ) -> Result<(), String> {
        self.next_task = self
            .next_task
            .checked_add(1)
            .ok_or("任务标识耗尽".to_owned())?;
        let spec = TaskSpec {
            id: format!("{}:{}", self.settings.epoch, self.next_task),
            session,
            chat: input.chat.clone(),
            prompt: text,
            model: None,
            mode: ExecutionMode::Execute,
        };
        if !input.attachments.is_empty() {
            self.resources
                .insert(spec.id.clone(), std::mem::take(&mut input.attachments));
        }
        match self.scheduler.reserve(input.id.clone(), spec) {
            Ok(ticket) => {
                if !can_spawn(jobs, false) {
                    self.scheduler.abort_admission(ticket);
                    self.resources.remove(&input.id);
                    tell(&self.delivery, &input.chat, "任务队列繁忙，请稍后重新发送。")?;
                    (input.accept)(false);
                    return Ok(());
                }
                let store = self.store.clone();
                jobs.spawn(async move {
                    let result = store.claim(input.id.clone()).await.map_err(|_| ());
                    Done::Admission {
                        ticket,
                        input,
                        result,
                    }
                });
            }
            Err(_) => {
                tell(&self.delivery, &input.chat, "任务队列繁忙，请稍后重新发送。")?;
                (input.accept)(false);
            }
        }
        Ok(())
    }
}

/// Record a command id once, clearing old entries when the bound is hit.
pub(crate) fn seen_insert(
    seen: &mut BTreeMap<String, Instant>,
    id: &str,
) -> bool {
    if seen.contains_key(id) {
        return false;
    }
    if seen.len() >= 1000 {
        seen.clear();
    }
    seen.insert(id.to_owned(), Instant::now());
    true
}
