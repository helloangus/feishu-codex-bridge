//! Minimal serial runtime: text, help, status and owner-scoped stop.
use crate::diagnostics::{Event as LogEvent, Status as LogStatus, emit};
use crate::presentation::{Presentation, Request as DeliveryRequest};
use crate::{
    AdmissionTicket, Scheduler,
    directories::{Confirmations, Creation, DirectoryStore},
    events::{AgentEvent, Incoming, TurnOutcome},
    execution::Execution,
    messaging::Messenger,
    ports::{AgentBackend, BackendError, Sandbox, TurnInput, TurnRef},
    requests::{AgentReply, AgentRequest, ReplyHandle, RequestKind},
    sessions::{self, DurableJournal, SessionStore},
};
use bridge_core::{ExecutionMode, SessionKey, task::TaskSpec};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::mpsc,
    task::JoinSet,
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;

pub struct Input {
    pub attachments: Vec<crate::messaging::Attachment>,
    pub card: Option<crate::cards::Click>,
    pub id: String,
    pub user: String,
    pub chat: String,
    /// None denotes media or a separately validated card; never an ordinary task.
    pub text: Option<String>,
    pub accept: Box<dyn FnOnce(bool) + Send>,
}
pub trait Store: SessionStore + DurableJournal + DirectoryStore {}
impl<T: SessionStore + DurableJournal + DirectoryStore> Store for T {}

pub struct Settings {
    pub root: PathBuf,
    pub directory: PathBuf,
    pub allowed: BTreeSet<String>,
    pub open_access: bool,
    pub sandbox: Sandbox,
    pub epoch: u64,
}

struct Active {
    compact: bool,
    compact_ack: bool,
    compact_outcome: Option<String>,
    compact_thread: Option<String>,
    spec: TaskSpec,
    gate: Option<Execution>,
    turn: Option<TurnRef>,
    stopping: bool,
    output: String,
    plan: Option<(String, bool)>,
    truncated: bool,
    started: Instant,
}

struct PendingApproval {
    waiting_text: bool,
    answers: BTreeMap<String, Vec<String>>,
    question: usize,
    request: AgentRequest,
    reply: Box<dyn ReplyHandle>,
    task: String,
    owner: crate::cards::Owner,
    deadline: Instant,
    sending: bool,
    source: Option<String>,
}

fn reply_approval(jobs: &mut JoinSet<Done>, pending: PendingApproval, allow: bool) {
    if let RequestKind::Questions { questions, .. } = &pending.request.kind {
        if questions.is_empty()
            || questions.iter().any(|q| {
                !pending.answers.get(&q.id).is_some_and(|answers| {
                    !answers.is_empty() && answers.iter().all(|a| !a.trim().is_empty())
                })
            })
        {
            // Dropping the handle never writes an empty answer, including shutdown.
            return;
        }
    }
    jobs.spawn(async move {
        let questions = matches!(&pending.request.kind, RequestKind::Questions { .. });
        let response = if questions {
            AgentReply::Answers(pending.answers)
        } else {
            AgentReply::Approve(allow)
        };
        let result = timeout(Duration::from_secs(10), pending.reply.reply(response))
            .await
            .unwrap_or(Err(BackendError::Uncertain));
        if questions {
            emit(
                LogEvent::AnswerReturned,
                if result.is_ok() {
                    LogStatus::Ok
                } else {
                    LogStatus::Failed
                },
                Some(&pending.task),
                0,
            );
        }
        Done::ApprovalReplied {
            questions,
            chat: pending.owner.chat,
            allow,
            result,
        }
    });
}

enum Done {
    ArchiveSynced {
        result: Result<(), sessions::SessionStoreError>,
    },
    PlanAction {
        input: Input,
        task: Option<TaskSpec>,
        result: Result<bool, String>,
    },
    FilesDelivered,
    ApprovalSent {
        token: String,
        panel: bridge_core::view::Panel,
        commands: Vec<(String, String)>,
        result: Result<crate::messaging::MessageId, crate::messaging::DeliveryError>,
    },
    ApprovalReplied {
        questions: bool,
        chat: String,
        allow: bool,
        result: Result<(), BackendError>,
    },
    PanelUpdated,
    PanelSent {
        refreshed: bool,
        panel: bridge_core::view::Panel,
        entries: Vec<(String, crate::cards::Action)>,
        result: Result<crate::messaging::MessageId, crate::messaging::DeliveryError>,
    },
    DirectoryProposed {
        user: String,
        chat: String,
        current: PathBuf,
        target: String,
        result: Result<Option<PathBuf>, sessions::SessionStoreError>,
    },
    CreationClaim {
        input: Input,
        creation: Creation,
        result: Result<bool, ()>,
    },
    DirectoryCreated {
        user: String,
        chat: String,
        result: Result<PathBuf, sessions::SessionStoreError>,
    },
    DirectoryClaim {
        input: Input,
        current: PathBuf,
        target: String,
        result: Result<bool, ()>,
    },
    DirectoryChanged {
        user: String,
        chat: String,
        result: Result<PathBuf, sessions::SessionStoreError>,
    },
    DirectoryListed {
        chat: String,
        result: Result<crate::directories::DirectoryView, sessions::SessionStoreError>,
    },
    CompactClaim {
        input: Input,
        session: SessionKey,
        result: Result<bool, ()>,
    },
    CompactPrepared {
        id: String,
        result: Result<String, sessions::StartError>,
    },
    CompactSubmitted {
        id: String,
        result: Result<(), BackendError>,
    },
    PreferenceClaim {
        input: Input,
        session: SessionKey,
        change: sessions::PreferenceChange,
        result: Result<bool, ()>,
    },
    PreferenceChanged {
        chat: String,
        result: Result<(), sessions::StartError>,
    },
    ThreadClaim {
        input: Input,
        session: SessionKey,
        thread: String,
        action: sessions::ThreadAction,
        result: Result<bool, ()>,
    },
    SessionChanged {
        chat: String,
        action: sessions::ThreadAction,
        result: Result<(), sessions::StartError>,
    },
    Listed {
        refresh: Option<String>,
        chat: String,
        user: String,
        directory: PathBuf,
        generation: u64,
        stop_snapshot: (u64, Option<String>),
        result: Result<ListedContent, sessions::StartError>,
    },
    Reset {
        input: Input,
        result: Result<bool, ()>,
    },
    Admission {
        ticket: AdmissionTicket,
        input: Input,
        result: Result<bool, ()>,
    },
    Prepared {
        id: String,
        result: Result<TurnInput, String>,
    },
    Started {
        id: String,
        result: Result<TurnRef, BackendError>,
    },
    Control {
        result: Result<(), BackendError>,
    },
}

enum ListedContent {
    Text(String),
    Threads {
        entries: Vec<sessions::ListedThread>,
        archived: bool,
    },
    Models {
        entries: Vec<crate::ports::Model>,
        current: Option<String>,
    },
}

fn tell(
    tx: &mpsc::Sender<DeliveryRequest>,
    chat: &str,
    text: impl Into<String>,
) -> Result<(), String> {
    tx.try_send(DeliveryRequest::Text(chat.into(), text.into()))
        .map_err(|_| "回复队列已满或发送器已退出".into())
}

fn send_panel(
    source: Option<String>,
    jobs: &mut JoinSet<Done>,
    messenger: Arc<dyn Messenger>,
    owner: crate::cards::Owner,
    panel: bridge_core::view::Panel,
    commands: Vec<(String, String)>,
) {
    let deadline = Instant::now() + Duration::from_secs(600);
    jobs.spawn(async move {
        let fallback = if commands
            .iter()
            .any(|(_, command)| command.starts_with("/plan-action "))
        {
            format!(
                "{}\n{}\n\n计划确认卡片发送失败，未开放实施；请继续讨论并重新生成计划。",
                panel.title, panel.body
            )
        } else {
            format!(
                "{}\n{}\n{}",
                panel.title,
                panel.body,
                commands
                    .iter()
                    .map(|(_, command)| command.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        let result = match timeout(Duration::from_secs(45), async {
            if let Some(source) = &source {
                let id = crate::messaging::MessageId(source.clone());
                messenger.update_panel(id.clone(), panel.clone()).await?;
                Ok(id)
            } else {
                messenger
                    .send_panel(owner.chat.clone(), panel.clone())
                    .await
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(crate::messaging::DeliveryError::Transport),
        };
        if result.is_err()
            && !matches!(
                timeout(
                    Duration::from_secs(45),
                    messenger.send_text(owner.chat.clone(), fallback)
                )
                .await,
                Ok(Ok(()))
            )
        {
            eprintln!("{{\"event\":\"delivery_failed\"}}");
        }
        let entries = commands
            .into_iter()
            .map(|(token, command)| {
                let stop_snapshot = if command == "/stop" || command.starts_with("/plan-action ") {
                    Some(owner.stop_snapshot.clone())
                } else {
                    None
                };
                (
                    token,
                    crate::cards::Action {
                        generation: owner.generation,
                        user: owner.user.clone(),
                        chat: owner.chat.clone(),
                        directory: owner.directory.clone(),
                        source: String::new(),
                        deadline,
                        command,
                        stop_snapshot,
                    },
                )
            })
            .collect();
        Done::PanelSent {
            refreshed: source.is_some(),
            panel,
            entries,
            result,
        }
    });
}
fn finish(
    active: &mut Option<Active>,
    scheduler: &mut Scheduler,
    delivery: &mpsc::Sender<DeliveryRequest>,
    outcome: String,
) -> Result<(), String> {
    if let Some(active) = active.take() {
        emit(
            LogEvent::TaskFinished,
            if outcome.starts_with("执行完成") {
                LogStatus::Ok
            } else {
                LogStatus::Failed
            },
            Some(&active.spec.id),
            active.output.len(),
        );
        if active.compact {
            scheduler.end_session_mutation();
            return tell(delivery, &active.spec.chat, outcome);
        }
        scheduler.finish(&active.spec.id);
        let (text, truncated) = active
            .plan
            .as_ref()
            .map(|(text, truncated)| (text.as_str(), *truncated))
            .unwrap_or((&active.output, active.truncated));
        let output = if text.is_empty() {
            "（无文本输出）"
        } else {
            text
        };
        delivery
            .try_send(DeliveryRequest::Answer {
                task: active.spec.id,
                chat: active.spec.chat,
                text: format!(
                    "{outcome}\n\n{output}{}",
                    if truncated {
                        "\n\n输出超过最小版 32 KiB 上限，已截断。"
                    } else {
                        ""
                    }
                ),
            })
            .map_err(|_| "回复队列已满".to_owned())?;
    }
    Ok(())
}
fn event(
    active: &mut Option<Active>,
    scheduler: &mut Scheduler,
    delivery: &mpsc::Sender<DeliveryRequest>,
    event: AgentEvent,
) -> Result<Option<crate::plans::Offer>, String> {
    let offer = if matches!(
        &event,
        AgentEvent::Finished {
            outcome: TurnOutcome::Completed,
            ..
        }
    ) {
        active
            .as_ref()
            .filter(|a| !a.compact && !a.stopping && a.spec.mode == ExecutionMode::Plan)
            .and_then(|a| {
                let (text, truncated) = a.plan.as_ref()?;
                if *truncated || text.trim().is_empty() || text.len() > 16000 {
                    return None;
                }
                Some(crate::plans::Offer {
                    task: a.spec.clone(),
                    thread: a.turn.as_ref()?.thread_id.clone(),
                    text: text.clone(),
                    token: format!("plan-{}", a.spec.id),
                    sent: false,
                    deadline: Instant::now() + Duration::from_secs(600),
                })
            })
    } else {
        None
    };
    match event {
        AgentEvent::Plan { text, .. } => {
            if let Some(active) = active {
                let mut end = text.len().min(32 * 1024);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                active.plan = Some((text[..end].to_owned(), end < text.len()));
            }
        }
        AgentEvent::Output { delta, .. } => {
            if let Some(active) = active {
                let available = (32 * 1024_usize).saturating_sub(active.output.len());
                let mut end = delta.len().min(available);
                while !delta.is_char_boundary(end) {
                    end -= 1;
                }
                active.output.push_str(&delta[..end]);
                active.truncated |= end < delta.len();
            }
        }
        AgentEvent::Finished { outcome, .. } => {
            if let Some(a) = active.as_mut().filter(|a| a.compact) {
                let label = match outcome {
                    TurnOutcome::Completed => "上下文压缩完成。".into(),
                    TurnOutcome::Interrupted => "上下文压缩已停止。".into(),
                    TurnOutcome::Failed { message, .. } => format!(
                        "上下文压缩失败：{}",
                        message
                            .unwrap_or_else(|| "Codex 未返回原因".into())
                            .chars()
                            .take(1000)
                            .collect::<String>()
                    ),
                };
                if !a.compact_ack {
                    a.compact_outcome = Some(label);
                    return Ok(None);
                }
                return finish(active, scheduler, delivery, label).map(|_| None);
            }
            let label = match outcome {
                TurnOutcome::Completed => "执行完成".into(),
                TurnOutcome::Interrupted => "任务已停止".into(),
                TurnOutcome::Failed { message, .. } => format!(
                    "执行失败：{}",
                    message
                        .unwrap_or_else(|| "Codex 未返回原因".into())
                        .chars()
                        .take(1000)
                        .collect::<String>()
                ),
            };
            finish(active, scheduler, delivery, label)?;
        }
        _ => {}
    }
    Ok(offer)
}

/// All network and persistence work is spawned; the owner remains responsive
/// to stop/status. Fatal backend failures terminate this run, never replay work.
pub async fn run(
    settings: Settings,
    backend: Arc<dyn AgentBackend>,
    store: Arc<dyn Store>,
    messenger: Arc<dyn Messenger>,
    mut inputs: mpsc::Receiver<Input>,
    mut events: mpsc::Receiver<Result<Incoming, BackendError>>,
    cancel: CancellationToken,
) -> Result<(), String> {
    store
        .validate_directory(settings.root.clone(), settings.directory.clone())
        .await
        .map_err(|_| "初始目录无效或越出工作区")?;
    let mut directories = store
        .directory_preferences()
        .await
        .map_err(|_| "无法读取用户目录设置")?;
    let (delivery, mut deliveries) = mpsc::channel::<DeliveryRequest>(128);
    let text_messenger = messenger.clone();
    let progress_busy = Arc::new(AtomicBool::new(false));
    let busy = progress_busy.clone();
    let mut sender = tokio::spawn(async move {
        let mut presentation = Presentation::default();
        while let Some(request) = deliveries.recv().await {
            let result = match request {
                DeliveryRequest::Text(chat, text) => {
                    timeout(
                        Duration::from_secs(45),
                        text_messenger.send_text(chat, text),
                    )
                    .await
                }
                DeliveryRequest::Answer { task, chat, text } => {
                    let id = task.clone();
                    let result = if text_messenger.rich_output() {
                        timeout(
                            Duration::from_secs(180),
                            presentation.answer(text_messenger.as_ref(), task, chat, text),
                        )
                        .await
                    } else {
                        timeout(
                            Duration::from_secs(45),
                            text_messenger.send_text(chat, text),
                        )
                        .await
                    };
                    emit(
                        LogEvent::AnswerDelivered,
                        if matches!(&result, Ok(Ok(()))) {
                            LogStatus::Ok
                        } else {
                            LogStatus::Failed
                        },
                        Some(&id),
                        0,
                    );
                    result
                }
                DeliveryRequest::Progress { task, chat, text } => {
                    if text_messenger.rich_output() {
                        presentation
                            .progress(text_messenger.as_ref(), task, chat, text)
                            .await;
                    }
                    busy.store(false, Ordering::Release);
                    continue;
                }
            };
            if !matches!(result, Ok(Ok(()))) {
                eprintln!("{{\"event\":\"delivery_failed\"}}");
            }
        }
    });
    let mut scheduler = Scheduler::new(64);
    let mut jobs = JoinSet::new();
    let mut active: Option<Active> = None;
    let mut next_task = 0_u64;
    let mut resources = BTreeMap::new();
    let mut tracked_files: Option<TaskSpec> = None;
    let mut delivering_files = false;
    let mut plan_offer: Option<crate::plans::Offer> = None;
    let mut seen_commands = BTreeMap::<String, Instant>::new();
    let mut confirmations = Confirmations::default();
    let mut next_confirmation = 0_u64;
    let mut card_actions = crate::cards::Actions::default();
    let mut card_views = crate::cards::Views::default();
    let mut updating_panel = false;
    let mut refreshes = std::collections::VecDeque::new();
    let mut card_generations = BTreeMap::<String, u64>::new();
    let mut next_panel = 0_u64;
    let mut approvals = BTreeMap::<String, PendingApproval>::new();
    let mut file_changes =
        BTreeMap::<(u64, String, String, String), Vec<crate::requests::FileChange>>::new();
    let mut next_approval = 0_u64;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut last_progress = Instant::now();
    let mut archived_threads = BTreeSet::<String>::new();
    let mut allowed = settings.allowed.clone();
    let mut pairing_window = Instant::now();
    let mut pairing_attempts = 0_u32;
    let result = async {
        'runtime: loop {
            if !archived_threads.is_empty() && scheduler.begin_invalidation() {
                plan_offer = None;
                let thread = archived_threads.pop_first().ok_or("缺少归档同步目标")?;
                let store = store.clone();
                jobs.spawn(async move {
                    Done::ArchiveSynced { result: store.clear_thread(thread).await }
                });
            }
            if plan_offer.as_ref().is_some_and(|p|Instant::now()>=p.deadline) {plan_offer=None;}
            card_actions.retain_plan(plan_offer.as_ref().map(|p|p.token.as_str()));
            resources.retain(|id: &String,_|scheduler.has_task(id));
            if jobs.len() > 128 {break Err("后台操作超过上限，停止运行".into());}
            file_changes.retain(|(epoch,thread,turn,_),_| active.as_ref().is_some_and(|a| !a.stopping && a.gate.as_ref().is_some_and(|g| g.accepts_request(&TurnRef {epoch:*epoch,thread_id:thread.clone(),turn_id:turn.clone()}))));
            let invalid: Vec<_> = approvals.iter().filter(|(_, pending)| {
                Instant::now() >= pending.deadline || !active.as_ref().is_some_and(|a| {
                    a.spec.id == pending.task && !a.stopping && a.gate.as_ref().is_some_and(|g| g.accepts_request(&pending.request.turn))
                })
            }).map(|(token, _)| token.clone()).collect();
            for token in invalid {
                if let Some(pending) = approvals.remove(&token) {
                    card_actions.invalidate_approval(&token);
                    if matches!(&pending.request.kind,RequestKind::Questions {..}) {
                        tell(&delivery,&pending.owner.chat,"问答未完成且已超时或任务已结束/停止，正在停止桥接；未提交空答案，不会自动重跑。")?;
                        break 'runtime Err("问答未完成，停止本次运行".into());
                    }
                    let message = "审批已超时或任务已结束/停止，正在回传拒绝。";
                    if let Some(source) = &pending.source {card_views.note(source,message);}
                    tell(&delivery,&pending.owner.chat,message)?;
                    reply_approval(&mut jobs,pending,false);
                }
            }
            for (token, pending) in &mut approvals {
                if !pending.sending && active.as_ref().is_some_and(|a| a.turn.as_ref() == Some(&pending.request.turn)) {
                    pending.sending = true;
                    next_panel=next_panel.checked_add(1).ok_or("卡片编号耗尽")?;
                    let prefix=format!("panel-{}-{next_panel}",settings.epoch);
                    let (panel,commands)=match &pending.request.kind {
                        RequestKind::Approval(request)=>crate::cards::approval(request,token,&prefix),
                        RequestKind::Questions {questions,..}=>crate::cards::question(&questions[pending.question],pending.question,questions.len(),token,&prefix),
                    };
                    let token=token.clone(); let chat=pending.owner.chat.clone(); let messenger=messenger.clone();
                    jobs.spawn(async move {
                        let result=timeout(Duration::from_secs(45),messenger.send_panel(chat,panel.clone())).await.unwrap_or(Err(crate::messaging::DeliveryError::Transport));
                        Done::ApprovalSent {token,panel,commands,result}
                    });
                }
            }
            if !updating_panel {
                if let Some((source, owner, panel, commands)) = refreshes.pop_front() {
                    updating_panel = true;
                    send_panel(Some(source), &mut jobs, messenger.clone(), owner, panel, commands);
                }
            }
            if !updating_panel {
                let snapshot = (next_task, active.as_ref().map(|a| a.spec.id.clone()));
                if let Some((source, panel)) = card_views.next_update(&card_actions, Instant::now(), &snapshot) {
                    updating_panel = true;
                    let messenger = messenger.clone();
                    jobs.spawn(async move {
                        if !matches!(timeout(Duration::from_secs(45), messenger.update_panel(crate::messaging::MessageId(source), panel)).await, Ok(Ok(()))) {
                            eprintln!("{{\"event\":\"card_update_failed\"}}");
                        }
                        Done::PanelUpdated
                    });
                }
            }
            if active.is_none() && !delivering_files {
                if let Some(spec)=tracked_files.take() {
                    delivering_files=true;
                    let messenger=messenger.clone();
                    jobs.spawn(async move {
                        let result=timeout(Duration::from_secs(180),messenger.finish_files(spec.id.clone(),spec.chat.clone(),spec.session.workspace)).await;
                        emit(LogEvent::FilesFinished, if matches!(&result,Ok(Ok(()))) {LogStatus::Ok} else {LogStatus::Failed}, Some(&spec.id), 0);
                        if !matches!(result,Ok(Ok(()))) {let _=timeout(Duration::from_secs(10),messenger.send_text(spec.chat,"成果物处理失败或超时，未自动重试；请检查工作目录。".into())).await;}
                        Done::FilesDelivered
                    });
                }
            }
            if active.is_none() && !delivering_files {
                if tracked_files.is_none() && scheduler.queued()==0 && scheduler.pending_admissions()==0 {
                    if let Some(offer)=plan_offer.as_mut().filter(|p|!p.sent) {
                        offer.sent=true;
                        next_panel=next_panel.checked_add(1).ok_or("卡片编号耗尽")?;
                        let (panel,commands)=crate::plans::panel(offer,&format!("panel-{}-{next_panel}",settings.epoch));
                        let owner=crate::cards::Owner {user:offer.task.session.user.clone(),chat:offer.task.chat.clone(),directory:offer.task.session.workspace.clone(),generation:*card_generations.get(&offer.task.session.user).unwrap_or(&0),stop_snapshot:(next_task,None)};
                        send_panel(None,&mut jobs,messenger.clone(),owner,panel,commands);
                    }
                }
                if let Some(spec) = scheduler.start_next().cloned() {
                    plan_offer=None;
                    tracked_files=Some(spec.clone());
                    let attachments: Vec<crate::messaging::Attachment>=resources.remove(&spec.id).unwrap_or_default();
                    tell(&delivery, &spec.chat, "已开始执行；可发送 /status 或 /stop。")?;
                    active = Some(Active {compact: false, compact_ack: false, compact_outcome: None, compact_thread: None, spec: spec.clone(), gate: None, turn: None, stopping: false, output: String::new(), plan: None, truncated: false, started: Instant::now()});
                    let backend = backend.clone(); let store = store.clone(); let sandbox = settings.sandbox;let root=settings.root.clone();let messenger=messenger.clone();
                    jobs.spawn(async move {
                        let result = async {
                            store.validate_directory(root,spec.session.workspace.clone()).await.map_err(|_|"当前目录已失效或越出工作区，请使用 /cd <绝对路径> 重新选择目录。".to_owned())?;
                            let count=attachments.len();
                            let prepared=timeout(Duration::from_secs(120),messenger.prepare_files(spec.id.clone(),spec.session.workspace.clone(),attachments)).await;
                            emit(LogEvent::FilesPrepared, if matches!(&prepared,Ok(Ok(_))) {LogStatus::Ok} else {LogStatus::Failed}, Some(&spec.id), count);
                            let files=prepared.map_err(|_|"附件准备超时".to_owned())?.map_err(|e|format!("附件或快照准备失败：{e}"))?;
                            let mut configured=spec.clone();configured.prompt.push_str(&files.prompt);
                            let mut turn=sessions::prepare_configured(backend.as_ref(), store.as_ref(), &configured, sandbox).await.map_err(|e| e.to_string())?;
                            turn.images=files.images;
                            timeout(Duration::from_secs(60),messenger.bind_files(spec.id.clone(),turn.thread_id.clone())).await.map_err(|_|"生成图片快照超时".to_owned())?.map_err(|e|e.to_string())?;
                            Ok(turn)
                        }.await;
                        Done::Prepared {id: spec.id, result}
                    });
                }
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break Ok(()),
                _ = &mut sender => break Err("发送器意外退出".into()),
                input = inputs.recv() => {
                    let Some(mut input) = input else {break Err("飞书连接已关闭".into());};
                    if input.id.is_empty() || input.user.is_empty() || input.chat.is_empty() {(input.accept)(false);continue;}
                    if input.card.is_none() && input.text.as_deref().is_some_and(|t| t.split_whitespace().next() == Some("/pair")) {
                        if pairing_window.elapsed() >= Duration::from_secs(60) {pairing_window=Instant::now();pairing_attempts=0;}
                        if pairing_attempts >= 10 {(input.accept)(true);continue;}
                        pairing_attempts += 1;
                        let parts:Vec<_> = input.text.as_deref().unwrap_or_default().split_whitespace().collect();
                        let code = if parts.len()==2 && parts[1].len()<=256 && input.attachments.is_empty() {parts[1].to_owned()} else {String::new()};
                        match store.pair(input.user.clone(), code).await {
                            Ok(true) => {allowed.insert(input.user.clone());tell(&delivery,&input.chat,"配对成功，后续消息可使用机器人。发送 /help 查看功能。")?;(input.accept)(true);}
                            Ok(false) => {tell(&delivery,&input.chat,"配对未成功，请核对配对码或联系管理员。")?;(input.accept)(true);}
                            Err(_) => {tell(&delivery,&input.chat,"配对保存失败，尚未授权，请稍后重试。")?;(input.accept)(false);}
                        }
                        continue;
                    }
                    if !settings.open_access && !allowed.contains(&input.user) {(input.accept)(true);continue;}
                    let current=directories.get(&input.user).unwrap_or(&settings.directory).clone();
                    if input.attachments.len()>10 {tell(&delivery,&input.chat,"单条消息最多接收 10 个附件。")?;(input.accept)(true);continue;}
                    if !input.attachments.is_empty() {
                        if input.card.is_some() { (input.accept)(false);continue; }
                        let text=input.text.get_or_insert_with(||"请查看附件并处理用户请求。".into());
                        if text.starts_with('/') {text.insert_str(0,"附件说明：");}
                    }
                    let mut refresh = None;
                    let mut approval_source = None;
                    if let Some(click)=input.card.take() {
                        let snapshot=(next_task,active.as_ref().map(|a|a.spec.id.clone()));
                        let Some(command)=card_actions.take(&click,&input.user,&input.chat,&current,Instant::now(),&snapshot) else {
                            tell(&delivery,&input.chat,"卡片操作无效、已使用或已过期；请重新发送 /help 或 /cd <路径>。")?;(input.accept)(true);continue;
                        };
                        approval_source = Some(click.source.clone());
                        if matches!(command.as_str(), "/resume" | "/archived" | "/models") && card_views.is_list(&click.source) {
                            card_actions.invalidate_source(&click.source);
                            card_views.remove(&click.source);
                            refresh = Some(click.source.clone());
                        }
                        input.id=format!("card:{}",click.token);input.text=Some(command);
                    }
                    let text = input.text.as_deref().unwrap_or("").trim();
                    if text.starts_with("/plan-action ") {
                        let parts:Vec<_>=text.split_whitespace().collect();
                        let valid=parts.len()==3 && matches!(parts[2],"implement"|"fresh"|"stay") && approval_source.is_some() && plan_offer.as_ref().is_some_and(|p|p.token==parts[1] && p.task.session.user==input.user && p.task.chat==input.chat && p.task.session.workspace==current && Instant::now()<p.deadline);
                        if !valid {tell(&delivery,&input.chat,"计划操作无效或已过期，请重新生成计划并使用原卡片。")?;(input.accept)(true);continue;}
                        if delivering_files || !scheduler.begin_session_mutation() {tell(&delivery,&input.chat,"有任务或成果交付进行中，请空闲后重新生成计划。")?;(input.accept)(true);continue;}
                        let offer=plan_offer.take().ok_or("缺少计划")?;
                        let action=parts[2].to_owned();
                        if let Some(source)=&approval_source {card_actions.invalidate_source(source);card_views.note(source,"已收到选择；实际结果请查看单独回复。");}
                        next_task=next_task.checked_add(1).ok_or("任务标识耗尽")?;
                        let task=if action=="stay" {None} else {Some(TaskSpec {id:format!("{}:{next_task}",settings.epoch),session:offer.task.session.clone(),chat:input.chat.clone(),prompt:format!("请实施以下已确认的计划：\n\n{}",offer.text),model:None,mode:ExecutionMode::Execute})};
                        let store=store.clone();let backend=backend.clone();let root=settings.root.clone();
                        jobs.spawn(async move {
                            let result=async {
                                let new=store.claim(input.id.clone()).await.map_err(|e|e.to_string())?;
                                if !new {return Ok(false);}
                                let session=offer.task.session;
                                store.validate_directory(root,session.workspace.clone()).await.map_err(|e|e.to_string())?;
                                if store.thread(session.clone()).await.map_err(|e|e.to_string())?.as_deref()!=Some(&offer.thread) || !store.preferences(session.clone()).await.map_err(|e|e.to_string())?.plan {return Err("计划所属会话或模式已经变化".into());}
                                sessions::prepare_compaction(backend.as_ref(),store.as_ref(),session.clone()).await.map_err(|e|e.to_string())?;
                                store.set_preference(session.clone(),sessions::PreferenceChange::Plan(action=="stay")).await.map_err(|e|e.to_string())?;
                                if action=="fresh" {store.clear(session).await.map_err(|e|e.to_string())?;}
                                Ok(true)
                            }.await;
                            Done::PlanAction {input,task,result}
                        });
                        continue;
                    }
                    if delivering_files && text.starts_with('/') && !matches!(text,"/status"|"/help"|"/stop"|"/model"|"/models"|"/resume"|"/archived"|"/plan"|"/cd") {
                        tell(&delivery,&input.chat,"正在整理本轮成果，请交付结束后再修改会话、目录或设置。")?;(input.accept)(true);continue;
                    }
                    if text.starts_with("/answer ") || text.starts_with("/answer-skip ") {
                        let skipping=text.starts_with("/answer-skip ");
                        let mut parts=text.splitn(4,char::is_whitespace);
                        let _=parts.next();let token=parts.next().unwrap_or("");let index=parts.next().and_then(|v|v.parse::<usize>().ok());let answer=parts.next().unwrap_or("");
                        let mut complete=None;
                        if approval_source.is_none() && input.attachments.is_empty() && ((skipping && answer.is_empty()) || (!skipping && !answer.trim().is_empty() && answer.len()<=16*1024 && !answer.chars().any(|c|c.is_control() && c!='\n' && c!='\t'))) {
                            if let Some(pending)=approvals.get_mut(token) {
                        if pending.waiting_text && !skipping && index==Some(pending.question) && pending.owner.user==input.user && pending.owner.chat==input.chat && pending.owner.directory==current && Instant::now()<pending.deadline
                                    && active.as_ref().is_some_and(|a|!a.stopping && a.spec.id==pending.task && a.turn.as_ref()==Some(&pending.request.turn)) {
                                    if let RequestKind::Questions {questions,..}=&pending.request.kind {
                                        pending.answers.insert(questions[pending.question].id.clone(),if skipping {vec![]} else {vec![answer.into()]});
                                        pending.waiting_text=false;pending.question+=1;pending.sending=false;pending.source=None;
                                        complete=Some(pending.question==questions.len());
                                    }
                                }
                            }
                        }
                        if let Some(done)=complete {
                            tell(&delivery,&input.chat,"答案已记录。")?;
                            if done {if let Some(pending)=approvals.remove(token) {reply_approval(&mut jobs,pending,false);}}
                        } else {tell(&delivery,&input.chat,"答案未接收：请先点击当前题的自行回答按钮，使用提示里的完整命令；答案须非空且不超过 16 KiB。")?;}
                        (input.accept)(true);continue;
                    }
                    if text.starts_with("/choice ") {
                        let parts:Vec<_>=text.split_whitespace().collect();
                        let mut finished=None;
                        if parts.len()==4 && approval_source.is_some() {
                            if let Some(pending)=approvals.get_mut(parts[1]) {
                                let valid=pending.source==approval_source && pending.owner.user==input.user && pending.owner.chat==input.chat && pending.owner.directory==current
                                    && Instant::now()<pending.deadline && parts[2].parse::<usize>().ok()==Some(pending.question)
                                    && active.as_ref().is_some_and(|a|!a.stopping && a.spec.id==pending.task && a.turn.as_ref()==Some(&pending.request.turn));
                                if valid {
                                    if let RequestKind::Questions {questions,..}=&pending.request.kind {
                                        let question=&questions[pending.question];
                                        if parts[3]=="other" && (question.other || question.options.is_empty() || question.secret) {
                                            pending.waiting_text=true;
                                            card_actions.invalidate_approval(parts[1]);
                                            if let Some(source)=&pending.source {card_views.note(source,"已进入自行回答，请按单独提示发送答案。");}
                                            tell(&delivery, &input.chat, format!("请发送 /answer {} {} <答案>\n仍可使用 /stop 停止任务；答案不会在确认回复或日志中回显，飞书聊天会保留你发送的内容。", parts[1], pending.question))?;
                                            (input.accept)(true);continue;
                                        }
                                        let answer=parts[3].parse::<usize>().ok().and_then(|i|question.options.get(i)).map(|o|vec![o.label.clone()]);
                                        if let Some(answer)=answer {
                                            pending.answers.insert(question.id.clone(),answer);
                                            card_actions.invalidate_approval(parts[1]);
                                            if let Some(source)=&pending.source {card_views.note(source,"本题已记录，其他按钮已失效。");}
                                            pending.question+=1;
                                            pending.source=None;pending.sending=false;
                                            finished=Some((parts[1].to_owned(),pending.question==questions.len()));
                                        }
                                    }
                                }
                            }
                        }
                        if let Some((token,complete))=finished {
                            if complete {if let Some(pending)=approvals.remove(&token) {reply_approval(&mut jobs,pending,false);}}
                        } else {tell(&delivery,&input.chat,"问答操作无效或已过期，请使用当前问题卡片。")?;}
                        (input.accept)(true);continue;
                    }
                    let session = SessionKey::new(&input.user, &current);
                    if text.starts_with('/') && !matches!(text,"/help"|"/status"|"/models"|"/model"|"/plan"|"/cd"|"/resume"|"/archived") {plan_offer=None;}
                    use bridge_core::command::Command;
                    let command = Command::parse(text);
                    if let Ok(Command::Approve {token,allow}) = &command {
                            let valid = approvals.get(token).is_some_and(|pending| {
                                matches!(&pending.request.kind,RequestKind::Approval(_)) &&
                            approval_source.is_some() && pending.source == approval_source && Instant::now() < pending.deadline
                                && pending.owner.user == input.user && pending.owner.chat == input.chat && pending.owner.directory == current
                                && active.as_ref().is_some_and(|a| !a.stopping && a.spec.id == pending.task && a.turn.as_ref() == Some(&pending.request.turn))
                        });
                        if valid {
                            if let Some(pending)=approvals.remove(token) {
                                card_actions.invalidate_approval(token);
                                if let Some(source)=&pending.source {card_views.note(source,"审批选择已接收，回传结果请查看单独回复。");}
                                reply_approval(&mut jobs,pending,*allow);
                            }
                        } else {tell(&delivery,&input.chat,"审批无效或已过期；请仅使用当前任务的审批卡片按钮。")?;}
                        (input.accept)(true);continue;
                    }
                    if text=="/help" {
                        if !seen_commands.contains_key(&input.id) {
                            if seen_commands.len()>=1000 {seen_commands.clear();}
                            seen_commands.insert(input.id.clone(),Instant::now());
                            next_panel=next_panel.checked_add(1).ok_or("卡片编号耗尽")?;
                            let (panel,commands)=crate::cards::help(&format!("panel-{}-{next_panel}",settings.epoch));
                            let owner=crate::cards::Owner {user:input.user.clone(),chat:input.chat.clone(),directory:current.clone(),generation:*card_generations.get(&input.user).unwrap_or(&0),stop_snapshot:(next_task,active.as_ref().map(|a|a.spec.id.clone()))};
                            send_panel(None,&mut jobs,messenger.clone(),owner,panel,commands);
                        }
                        (input.accept)(true);continue;
                    }
                    if let Ok(Command::ConfirmDirectory(token))=&command {
                        let Some(creation)=confirmations.get(token,&input.user,&input.chat,&current,Instant::now()) else {
                            tell(&delivery,&input.chat,"创建确认无效、已过期或不属于当前用户/聊天/目录；请重新发送 /cd <路径>。")?;
                            (input.accept)(true);continue;
                        };
                        if !scheduler.begin_session_mutation() {
                            tell(&delivery,&input.chat,"有任务执行中、排队或目录正在更新；请空闲后在有效期内再次确认。")?;
                            (input.accept)(true);continue;
                        }
                        confirmations.remove(token);
                        let store=store.clone();
                        jobs.spawn(async move {let result=store.claim(input.id.clone()).await.map_err(|_|());Done::CreationClaim {input,creation,result}});
                        continue;
                    }
                    if let Ok(Command::ChangeDirectory(target)) = &command {
                        if let Some(target)=target {
                            if target.len()>4096 || target.chars().any(char::is_control) {
                                tell(&delivery,&input.chat,"目录参数无效：最多 4096 字节，不能包含换行或控制字符。")?;
                                (input.accept)(true);continue;
                            }
                            if !scheduler.begin_session_mutation() {
                                tell(&delivery,&input.chat,"有任务执行中、排队或目录正在更新；请等待完成或先 /stop，再切换目录。")?;
                                (input.accept)(true);continue;
                            }
                            let target=target.clone();let store=store.clone();
                            jobs.spawn(async move {let result=store.claim(input.id.clone()).await.map_err(|_|());Done::DirectoryClaim {input,current,target,result}});
                        } else {
                            if !seen_commands.contains_key(&input.id) {
                                if seen_commands.len()>=1000 {seen_commands.clear();}
                                seen_commands.insert(input.id.clone(),Instant::now());
                                let store=store.clone();let root=settings.root.clone();let chat=input.chat.clone();
                                jobs.spawn(async move {Done::DirectoryListed {chat,result:store.inspect_directory(root,current).await}});
                            }
                            (input.accept)(true);
                        }
                        continue;
                    }
                    let invalid_directory = !current.is_absolute() || !current.starts_with(&settings.root) || current.components().any(|c|matches!(c,std::path::Component::ParentDir));
                    if invalid_directory && !matches!(text,"/help"|"/status"|"/stop") {
                        tell(&delivery,&input.chat,"保存的目录已越出工作区；请使用 /cd <工作区内绝对路径> 重新选择。")?;
                        (input.accept)(true);continue;
                    }
                    if text == "/compact" {
                        if !scheduler.begin_session_mutation() {
                            tell(&delivery,&input.chat,"有任务执行中、排队或会话正在更新；请等待完成或先 /stop，再压缩上下文。")?;
                            (input.accept)(true);continue;
                        }
                        let store=store.clone();
                        jobs.spawn(async move {let result=store.claim(input.id.clone()).await.map_err(|_|());Done::CompactClaim {input,session,result}});
                        continue;
                    }
                    if matches!(&command, Ok(Command::Models | Command::Model(_) | Command::Plan(_))) {
                        let change = match &command {
                            Ok(Command::Model(Some(model))) => Some(sessions::PreferenceChange::Model(if model == "default" {None} else {Some(model.clone())})),
                            Ok(Command::Plan(Some(value))) => Some(sessions::PreferenceChange::Plan(*value)),
                            _ => None,
                        };
                        if let Some(change) = change {
                            if !scheduler.begin_session_mutation() {
                                tell(&delivery,&input.chat,"有任务执行中、排队或设置正在更新；请等待完成或先 /stop，再修改设置。")?;
                                (input.accept)(true);continue;
                            }
                            let store = store.clone();
                            jobs.spawn(async move {let result=store.claim(input.id.clone()).await.map_err(|_|());Done::PreferenceClaim {input,session,change,result}});
                        } else {
                            if !seen_commands.contains_key(&input.id) {
                                if seen_commands.len() >= 1000 {seen_commands.clear();}
                                seen_commands.insert(input.id.clone(),Instant::now());
                                let store=store.clone();let backend=backend.clone();let chat=input.chat.clone();
                                let user=input.user.clone(); let directory=session.workspace.clone();
                                let generation=*card_generations.get(&user).unwrap_or(&0);
                                let stop_snapshot=(next_task,active.as_ref().map(|a|a.spec.id.clone()));
                                jobs.spawn(async move {
                                    let result = async {
                                        let preferences=store.preferences(session).await?;
                                        match command {
                                            Ok(Command::Models) => {
                                                let models=backend.models().await?;
                                                Ok(ListedContent::Models {entries:models,current:preferences.model})
                                            }
                                            Ok(Command::Model(None)) => Ok(ListedContent::Text(format!("当前模型：{}",preferences.model.unwrap_or_else(||"Codex 默认".into())))),
                                            _ => Ok(ListedContent::Text(format!("Plan 模式：{}。使用 /plan on 或 /plan off 切换。",if preferences.plan {"已开启"} else {"已关闭"}))),
                                        }
                                    }.await;
                                    Done::Listed {refresh,chat,user,directory,generation,stop_snapshot,result}
                                });
                            }
                            (input.accept)(true);
                        }
                        continue;
                    }
                    if matches!(&command, Ok(Command::Resume(_) | Command::Archive(_) | Command::Unarchive(_) | Command::Archived)) {
                        let (target,action,archived) = match command {
                            Ok(Command::Resume(target)) => (target,sessions::ThreadAction::Resume,false),
                            Ok(Command::Archive(id)) => (Some(id),sessions::ThreadAction::Archive,false),
                            Ok(Command::Unarchive(id)) => (Some(id),sessions::ThreadAction::Unarchive,true),
                            _ => (None,sessions::ThreadAction::Unarchive,true),
                        };
                        if let Some(thread) = target {
                            if !sessions::valid_thread_id(&thread) {
                                tell(&delivery, &input.chat, "会话 ID 无效，请复制 /resume 或 /archived 列表中的完整命令。")?;
                                (input.accept)(true); continue;
                            }
                            if !scheduler.begin_session_mutation() {
                                tell(&delivery, &input.chat, "有任务执行中、排队或会话正在更新；请等待完成或先 /stop，再操作会话。")?;
                                (input.accept)(true); continue;
                            }
                            let store = store.clone();
                            jobs.spawn(async move {
                                let result = store.claim(input.id.clone()).await.map_err(|_| ());
                                Done::ThreadClaim { input, session, thread, action, result }
                            });
                        } else {
                            if !seen_commands.contains_key(&input.id) {
                                if seen_commands.len() >= 1000 {seen_commands.clear();}
                                seen_commands.insert(input.id.clone(), Instant::now());
                                let backend = backend.clone(); let chat = input.chat.clone();let store=store.clone();let root=settings.root.clone();
                                let user=input.user.clone(); let directory=session.workspace.clone();
                                let generation=*card_generations.get(&user).unwrap_or(&0);
                                let stop_snapshot=(next_task,active.as_ref().map(|a|a.spec.id.clone()));
                                jobs.spawn(async move {Done::Listed {refresh,chat,user,directory,generation,stop_snapshot,result: async {store.validate_directory(root,session.workspace.clone()).await?;let entries=sessions::list_entries(backend.as_ref(), &session,archived).await?;Ok(ListedContent::Threads {entries,archived})}.await}});
                            }
                            (input.accept)(true);
                        }
                        continue;
                    }
                    if text == "/new" {
                        if !scheduler.begin_session_mutation() {
                            tell(&delivery, &input.chat, "有任务执行中、排队或会话正在更新；请等待完成或先 /stop，再发送 /new。")?;
                            (input.accept)(true);
                            continue;
                        }
                        let store = store.clone();
                        jobs.spawn(async move {
                            // Claim before clearing. A repeated command must never
                            // erase a newer binding, even after a process restart.
                            let result = async {
                                let new = store.claim(input.id.clone()).await.map_err(|_| ())?;
                                if new {store.clear(session).await.map_err(|_| ())?;}
                                Ok(new)
                            }.await;
                            Done::Reset {input, result}
                        });
                        continue;
                    }
                    if input.text.is_none() || text.is_empty() || text.starts_with('/') {
                        let duplicate = seen_commands.contains_key(&input.id);
                        if !duplicate {
                            if seen_commands.len() >= 1000 {seen_commands.clear();}
                            seen_commands.insert(input.id.clone(), Instant::now());
                            match text {
                                "/status" => {
                                    let state = active.as_ref().map(|a| format!("{}，已用 {} 秒{}", if a.compact {"上下文压缩中"} else {"运行中"}, a.started.elapsed().as_secs(), if a.stopping {"，正在停止"} else {""})).unwrap_or_else(|| "空闲".into());
                                    tell(&delivery, &input.chat, format!("Rust 最小运行版：{state}\n当前目录：{}\n等待：{}，保存中：{}",current.display(), scheduler.queued(), scheduler.pending_admissions()))?;
                                }
                                "/stop" => {
                                    let removed = scheduler.cancel_queued(&session);
                                    let mut stopping = false;
                                    if let Some(a) = active.as_mut().filter(|a| a.spec.session == session && a.spec.chat == input.chat && a.compact_outcome.is_none()) {
                                        stopping = true;
                                        if !a.stopping {
                                            a.stopping = true;
                                            if let Some(turn) = a.turn.clone() {let backend=backend.clone();jobs.spawn(async move {Done::Control {result:backend.interrupt(turn).await}});}
                                        }
                                    }
                                    tell(&delivery, &input.chat, format!("已取消 {removed} 项等待请求；{}", if stopping {"已请求停止当前任务"} else {"没有可停止的当前任务"}))?;
                                }
                                _ => tell(&delivery, &input.chat, "命令暂不支持或参数无效，请发送 /help 查看支持的命令。")?,
                            }
                        }
                        (input.accept)(true);
                        continue;
                    }
                    if text.len() > 32*1024 {tell(&delivery,&input.chat,"输入超过最小版 32 KiB 上限。")?;(input.accept)(true);continue;}
                    next_task = next_task.checked_add(1).ok_or("任务标识耗尽")?;
                    let spec=TaskSpec {id:format!("{}:{next_task}",settings.epoch),session,chat:input.chat.clone(),prompt:text.into(),model:None,mode:ExecutionMode::Execute};
                    if !input.attachments.is_empty() {resources.insert(spec.id.clone(),std::mem::take(&mut input.attachments));}
                    match scheduler.reserve(input.id.clone(), spec) {
                        Ok(ticket) => {let store=store.clone();jobs.spawn(async move {let result=store.claim(input.id.clone()).await.map_err(|_|());Done::Admission {ticket,input,result}});}
                        Err(_) => {tell(&delivery,&input.chat,"任务队列繁忙，请稍后重新发送。")?;(input.accept)(false);}
                    }
                }
                done = jobs.join_next(), if !jobs.is_empty() => {
                    let done=done.ok_or("后台任务集合异常")?.map_err(|_|"后台任务异常退出")?;
                    match done {
                        Done::PlanAction {input,task,result} => {
                            scheduler.end_session_mutation();(input.accept)(true);
                            match result {
                                Ok(true)=>if let Some(task)=task {
                                    let ticket=scheduler.reserve(input.id,task).map_err(|_|"计划实施入队失败")?;
                                    scheduler.commit_admission(ticket,true);
                                    tell(&delivery,&input.chat,"已关闭 Plan 模式，确认的计划已加入实施队列。")?;
                                } else {tell(&delivery,&input.chat,"保持 Plan 模式，可继续发送消息讨论或修改计划。")?;},
                                Ok(false)=>tell(&delivery,&input.chat,"此计划选择已处理，不会重复执行。")?,
                                Err(error)=>tell(&delivery,&input.chat,format!("计划选择处理失败：{error}。未启动实施，设置可能已部分保存；请检查 /plan 和当前会话后重新生成计划，不会自动重试。"))?,
                            }
                        }
                        Done::FilesDelivered => {delivering_files=false;}
                        Done::ApprovalReplied {chat,allow,questions,result} => {
                            match result {
                                Ok(()) => tell(&delivery,&chat,if questions {"已向 Codex 回传答案。"} else if allow {"已向 Codex 回传本次同意；执行结果请等待任务回复。"} else {"已向 Codex 回传拒绝。"})?,
                                Err(_) => {tell(&delivery,&chat,if questions {"问答回传结果不确定，已停止连接，不会自动重试。"} else {"审批回传结果不确定，已停止连接，不会自动重试。"})?;break Err("交互回传失败".into());}
                            }
                        }
                        Done::ApprovalSent {token,panel,commands,result} => {
                            if panel.title=="Codex 问答" {emit(LogEvent::QuestionSent, if result.is_ok() {LogStatus::Ok} else {LogStatus::Failed}, approvals.get(&token).map(|p|p.task.as_str()), panel.buttons.len());}
                            if let Ok(id)=&result {card_views.insert(id.0.clone(),panel);}
                            if let Some(pending)=approvals.get_mut(&token) {
                                let valid=Instant::now()<pending.deadline && active.as_ref().is_some_and(|a| !a.stopping && a.spec.id==pending.task && a.turn.as_ref()==Some(&pending.request.turn));
                                let registered=if let Ok(id)=result {
                                    pending.source=Some(id.0.clone());
                                    valid && card_actions.insert(commands.into_iter().map(|(token,command)| (token,crate::cards::Action {
                                        user:pending.owner.user.clone(),chat:pending.owner.chat.clone(),directory:pending.owner.directory.clone(),generation:pending.owner.generation,
                                        deadline:pending.deadline,source:id.0.clone(),command,stop_snapshot:None,
                                    })).collect(),Instant::now())
                                } else {false};
                                if !registered {
                                    if let Some(pending)=approvals.remove(&token) {
                                        if matches!(&pending.request.kind,RequestKind::Questions {..}) {
                                            tell(&delivery,&pending.owner.chat,"问答卡片发送失败或失效，停止本次运行；未提交空答案。")?;
                                            break 'runtime Err("问答无法交付".into());
                                        }
                                        tell(&delivery,&pending.owner.chat,"审批卡片发送失败、已失效或登记已满，正在拒绝请求。")?;
                                        reply_approval(&mut jobs,pending,false);
                                    }
                                }
                            }
                        }
                        Done::PanelUpdated => {updating_panel=false;}
                        Done::PanelSent {refreshed,panel,mut entries,result} => {
                            if refreshed {updating_panel=false;}
                            if let Ok(id)=result {
                                card_views.insert(id.0.clone(),panel);
                                entries.retain(|(_,entry)|entry.generation==*card_generations.get(&entry.user).unwrap_or(&0));
                                for (_,entry) in &mut entries {entry.source=id.0.clone();}
                                if !card_actions.insert(entries,Instant::now()) {eprintln!("{{\"event\":\"card_actions_full\"}}");}
                            }
                        }
                        Done::DirectoryClaim {input,current,target,result} => match result {
                            Ok(true) => {
                                (input.accept)(true);let store=store.clone();let root=settings.root.clone();
                                jobs.spawn(async move {Done::DirectoryProposed {result:store.propose_directory(root,current.clone(),target.clone()).await,user:input.user,chat:input.chat,current,target}});
                            }
                            Ok(false) => {scheduler.end_session_mutation();(input.accept)(true);}
                            Err(()) => {scheduler.end_session_mutation();(input.accept)(false);tell(&delivery,&input.chat,"目录请求保存失败，未切换；请重新发送。")?;}
                        },
                        Done::DirectoryProposed {user,chat,current,target,result} => match result {
                            Ok(None) => {
                                let store=store.clone();let root=settings.root.clone();
                                jobs.spawn(async move {Done::DirectoryChanged {result:store.change_directory(user.clone(),root,current,target).await,user,chat}});
                            }
                            Ok(Some(path)) => {
                                scheduler.end_session_mutation();
                                next_confirmation=next_confirmation.checked_add(1).ok_or("确认编号耗尽")?;
                                let token=format!("cd-{}-{next_confirmation}",settings.epoch);
                                let prompt=format!("目录不存在：{}\n确认后将创建该目录及缺失的父目录，并切换到此目录。\n\n/cd-confirm {token}\n\n仅限当前用户在此聊天确认，10 分钟内有效；超时或切换目录后失效，不会自动创建。",path.display());
                                let entry=Creation {user:user.clone(),chat:chat.clone(),current:current.clone(),input:target,target:path,deadline:Instant::now()+Duration::from_secs(600)};
                                if confirmations.insert(token.clone(),entry) {
                                    next_panel=next_panel.checked_add(1).ok_or("卡片编号耗尽")?;
                                    let (panel,commands)=crate::cards::panel("创建目录确认",prompt,vec![("确认创建并切换".into(),format!("/cd-confirm {token}"))],&format!("panel-{}-{next_panel}",settings.epoch));
                                    let owner=crate::cards::Owner {generation:*card_generations.get(&user).unwrap_or(&0),user,chat:chat.clone(),directory:current,stop_snapshot:(next_task,None)};
                                    send_panel(None,&mut jobs,messenger.clone(),owner,panel,commands);
                                }
                                else {tell(&delivery,&chat,"待确认请求已满，未创建目录；请稍后重新发送 /cd <路径>。")?;}
                            }
                            Err(_) => {scheduler.end_session_mutation();tell(&delivery,&chat,"目录路径无效或无法读取；目标须位于工作区内，不能是文件。未创建或切换目录。")?;}
                        },
                        Done::CreationClaim {input,creation,result} => match result {
                            Ok(true) if Instant::now()<creation.deadline => {
                                (input.accept)(true);let store=store.clone();let root=settings.root.clone();
                                jobs.spawn(async move {Done::DirectoryCreated {result:store.create_directory(creation.user.clone(),root,creation.current,creation.input,creation.target).await,user:creation.user,chat:creation.chat}});
                            }
                            Ok(new) => {scheduler.end_session_mutation();(input.accept)(true);if new {tell(&delivery,&input.chat,"创建确认已过期，未执行；请重新发送 /cd <路径>。")?;}}
                            Err(()) => {scheduler.end_session_mutation();(input.accept)(false);tell(&delivery,&input.chat,"创建确认保存失败，未执行；原确认已失效，请重新发送 /cd <路径>。")?;}
                        },
                        Done::DirectoryCreated {user,chat,result} => {
                            scheduler.end_session_mutation();
                            match result {
                                Ok(path) => {confirmations.invalidate(&user);card_actions.invalidate(&user);*card_generations.entry(user.clone()).or_default()+=1;directories.insert(user,path.clone());tell(&delivery,&chat,format!("目录已创建并切换：{}",path.display()))?;}
                                Err(_) => tell(&delivery,&chat,"目录创建或设置保存失败，当前目录未切换。路径可能已变化；若已创建部分目录会保留，不会自动删除或重试。请检查后重新发送 /cd <路径>。")?,
                            }
                        }
                        Done::DirectoryChanged {user,chat,result} => {
                            scheduler.end_session_mutation();
                            match result {
                                Ok(path) => {confirmations.invalidate(&user);card_actions.invalidate(&user);*card_generations.entry(user.clone()).or_default()+=1;directories.insert(user,path.clone());tell(&delivery,&chat,format!("已切换目录：{}\n后续消息使用此目录的会话、模型和 Plan 设置。",path.display()))?;}
                                Err(_) => tell(&delivery,&chat,"切换目录失败：路径可能已变化或状态保存失败。当前目录未切换；请检查后重新发送 /cd <路径>。")?,
                            }
                        }
                        Done::DirectoryListed {chat,result} => tell(&delivery,&chat,result.map(|view|view.text()).unwrap_or_else(|_|"当前目录已失效或无法读取；请使用 /cd <工作区内绝对路径> 重新选择。".into()))?,
                        Done::CompactClaim {input,session,result} => match result {
                            Ok(true) => {
                                (input.accept)(true);
                                let id=input.id;
                                active=Some(Active {compact:true,compact_ack:false,compact_outcome:None,compact_thread:None,spec:TaskSpec {id:id.clone(),session:session.clone(),chat:input.chat.clone(),prompt:String::new(),model:None,mode:ExecutionMode::Execute},gate:None,turn:None,stopping:false,output:String::new(),plan:None,truncated:false,started:Instant::now()});
                                tell(&delivery,&input.chat,"正在准备压缩上下文；可发送 /status 或 /stop。")?;
                                let backend=backend.clone();let store=store.clone();
                                let root=settings.root.clone();
                                jobs.spawn(async move {Done::CompactPrepared {id,result:async {store.validate_directory(root,session.workspace.clone()).await?;sessions::prepare_compaction(backend.as_ref(),store.as_ref(),session).await}.await}});
                            }
                            Ok(false) => {scheduler.end_session_mutation();(input.accept)(true);}
                            Err(()) => {scheduler.end_session_mutation();(input.accept)(false);tell(&delivery,&input.chat,"压缩请求保存失败，未执行；请重新发送。")?;}
                        },
                        Done::CompactPrepared {id,result} => {
                            if let Some(a)=active.as_mut().filter(|a|a.compact && a.spec.id==id) {
                                if a.stopping {finish(&mut active,&mut scheduler,&delivery,"准备阶段已停止，未启动压缩。".into())?;continue;}
                                match result {
                                    Ok(thread) => {
                                        a.gate=Some(Execution::starting(settings.epoch,thread.clone(),64));
                                        // Record the expected thread before the request can emit notifications.
                                        a.compact_thread=Some(thread.clone());
                                        let backend=backend.clone();
                                        jobs.spawn(async move {Done::CompactSubmitted {id,result:backend.compact(thread).await}});
                                    }
                                    Err(error) => finish(&mut active,&mut scheduler,&delivery,format!("压缩准备失败：{error}；不会自动重试。"))?,
                                }
                            }
                        }
                        Done::CompactSubmitted {id,result} => {
                            if let Some(a)=active.as_mut().filter(|a|a.compact && a.spec.id==id) {
                                match result {
                                    Ok(()) => {
                                        a.compact_ack=true;
                                        if let Some(label)=a.compact_outcome.take() {finish(&mut active,&mut scheduler,&delivery,label)?;}
                                        else {tell(&delivery,&a.spec.chat,"已请求压缩上下文，正在等待完成；可发送 /status 或 /stop。")?;}
                                    }
                                    Err(BackendError::Rejected(_)) if a.turn.is_none() => finish(&mut active,&mut scheduler,&delivery,"压缩请求被拒绝；不会自动重试。".into())?,
                                    Err(error) => {
                                        tell(&delivery,&a.spec.chat,format!("压缩启动结果不确定：{error}；桥接将停止，不会自动重试。"))?;
                                        break Err("压缩启动结果不确定".into());
                                    }
                                }
                            }
                        }
                        Done::PreferenceClaim {input,session,change,result} => match result {
                            Ok(true) => {
                                (input.accept)(true);let backend=backend.clone();let store=store.clone();
                                let root=settings.root.clone();
                                jobs.spawn(async move {Done::PreferenceChanged {chat:input.chat,result:async {store.validate_directory(root,session.workspace.clone()).await?;sessions::change_preference(backend.as_ref(),store.as_ref(),session,change).await}.await}});
                            }
                            Ok(false) => {scheduler.end_session_mutation();(input.accept)(true);}
                            Err(()) => {scheduler.end_session_mutation();(input.accept)(false);tell(&delivery,&input.chat,"设置请求保存失败，请重新发送。")?;}
                        },
                        Done::PreferenceChanged {chat,result} => {
                            scheduler.end_session_mutation();
                            tell(&delivery,&chat,match result {Ok(())=>"设置已保存，后续任务生效。使用 /model 或 /plan 查看。".into(),Err(error)=>format!("设置失败：{error}；模型请从 /models 选择。不会自动重试，请检查后重新发送。")})?;
                        }
                        Done::ArchiveSynced {result} => {
                            result.map_err(|_| "归档通知同步失败，已停止运行；需核对本地绑定，不会自动重跑任务")?;
                            scheduler.end_session_mutation();
                        }
                        Done::ThreadClaim {input,session,thread,action,result} => {
                            match result {
                                Ok(true) => {
                                    (input.accept)(true);
                                    let backend = backend.clone(); let store = store.clone();
                                    let root=settings.root.clone();
                                    jobs.spawn(async move {Done::SessionChanged {chat: input.chat, action, result: async {store.validate_directory(root,session.workspace.clone()).await?;sessions::change_thread(backend.as_ref(), store.as_ref(), session, thread,action).await}.await}});
                                }
                                Ok(false) => {scheduler.end_session_mutation();(input.accept)(true);}
                                Err(()) => {scheduler.end_session_mutation();(input.accept)(false);tell(&delivery,&input.chat,"会话操作请求保存失败，未执行；请重新发送。")?;}
                            }
                        }
                        Done::SessionChanged {chat,action,result} => {
                            scheduler.end_session_mutation();
                            let recovery = matches!(&result,Err(sessions::StartError::Reconcile));
                            let text = match result {
                                Ok(()) => action.success().into(),
                                Err(error) => format!("会话操作失败：{error}。目标必须属于当前目录且处于空闲状态；不会自动重试。"),
                            };
                            tell(&delivery,&chat,text)?;
                            if recovery {break Err("归档结果不确定，已停止运行，需核对本地绑定和远端状态".into());}
                        }
                        Done::Listed {refresh,chat,user,directory,generation,stop_snapshot,result} => match result {
                            Ok(ListedContent::Text(text)) => tell(&delivery,&chat,text)?,
                            Ok(ListedContent::Threads {entries,archived}) => {
                                next_panel=next_panel.checked_add(1).ok_or("卡片编号耗尽")?;
                                let (panel,commands)=crate::cards::threads(&entries,archived,&format!("panel-{}-{next_panel}",settings.epoch));
                                let owner=crate::cards::Owner {user,chat:chat.clone(),directory,generation,stop_snapshot};
                                if let Some(source)=refresh {
                                    if refreshes.len() >= 128 {tell(&delivery,&chat,"卡片刷新队列已满，请重新发送列表命令。")?;}
                                    else {refreshes.push_back((source,owner,panel,commands));}
                                } else {send_panel(None,&mut jobs,messenger.clone(),owner,panel,commands);}
                            }
                            Ok(ListedContent::Models {entries,current}) => {
                                next_panel=next_panel.checked_add(1).ok_or("卡片编号耗尽")?;
                                let (panel,commands)=crate::cards::models(&entries,current.as_deref(),&format!("panel-{}-{next_panel}",settings.epoch));
                                let owner=crate::cards::Owner {user,chat:chat.clone(),directory,generation,stop_snapshot};
                                if let Some(source)=refresh {
                                    if refreshes.len() >= 128 {tell(&delivery,&chat,"卡片刷新队列已满，请重新发送列表命令。")?;}
                                    else {refreshes.push_back((source,owner,panel,commands));}
                                } else {send_panel(None,&mut jobs,messenger.clone(),owner,panel,commands);}
                            }
                            Err(error) => tell(&delivery,&chat,format!("读取会话列表失败：{error}"))?,
                        },
                        Done::Reset {input,result} => {
                            scheduler.end_session_mutation();
                            match result {
                                Ok(new) => {
                                    (input.accept)(true);
                                    if new {tell(&delivery, &input.chat, "已切换到新会话，下次提问时自动创建。")?;}
                                }
                                Err(()) => {
                                    (input.accept)(false);
                                    tell(&delivery, &input.chat, "新建会话失败，状态结果未确认；请重新发送一条 /new，不会自动重试。")?;
                                }
                            }
                        }
                        Done::Admission {ticket,input,result} => match result {
                            Ok(new) => {let queued=scheduler.commit_admission(ticket,new);(input.accept)(true);if queued {tell(&delivery,&input.chat,"请求已接收。")?;}}
                            Err(_) => {scheduler.abort_admission(ticket);(input.accept)(false);tell(&delivery,&input.chat,"接收状态保存失败，未启动任务。")?;}
                        },
                        Done::Prepared {id,result} => {
                            emit(LogEvent::TaskPrepared, if result.is_ok() {LogStatus::Ok} else {LogStatus::Failed}, Some(&id), 0);
                            if let Some(a)=active.as_mut().filter(|a| a.spec.id==id) {
                                if a.stopping {finish(&mut active,&mut scheduler,&delivery,"准备阶段已停止，未启动任务".into())?;continue;}
                                match result {
                                    Ok(input) => {a.spec.mode=input.mode;a.gate=Some(Execution::starting(settings.epoch,input.thread_id.clone(),64));let backend=backend.clone();jobs.spawn(async move {Done::Started {id,result:backend.start_turn(input).await}});}
                                    Err(error) => finish(&mut active,&mut scheduler,&delivery,format!("准备失败：{error}"))?,
                                }
                            }
                        }
                        Done::Started {id,result} => {
                            emit(LogEvent::TaskStarted, if result.is_ok() {LogStatus::Ok} else {LogStatus::Failed}, Some(&id), 0);
                            if let Some(a)=active.as_mut().filter(|a|a.spec.id==id) {
                                match result {
                                    Ok(turn) => {
                                        a.turn=Some(turn.clone());
                                        let early=a.gate.as_mut().ok_or("缺少执行状态")?.bind(turn.clone()).map_err(|_|"执行身份不匹配")?;
                                        if a.stopping {let backend=backend.clone();jobs.spawn(async move {Done::Control {result:backend.interrupt(turn).await}});}
                                        for item in early {if let Some(offer)=event(&mut active,&mut scheduler,&delivery,item)? {plan_offer=Some(offer);}}
                                    }
                                    Err(error) => {finish(&mut active,&mut scheduler,&delivery,format!("启动结果未知或失败：{error}；不会自动重试。"))?;break Err("Codex 启动失败，已停止运行以避免重复执行".into());}
                                }
                            }
                        }
                        Done::Control {result} => {result.map_err(|_|"Codex 控制请求失败，需重启连接")?;}
                    }
                }
                incoming = events.recv() => {
                    let incoming=incoming.ok_or("Codex 事件连接已关闭")?.map_err(|_|"Codex 协议或连接异常")?;
                    match incoming {
                        Incoming::Notification(notification) => {
                            if let AgentEvent::Archived {epoch,thread} = &notification {
                                if *epoch != settings.epoch {continue;}
                                if !sessions::valid_thread_id(thread) {break Err("归档通知会话 ID 无效".into());}
                                if !archived_threads.contains(thread) && archived_threads.len() >= 128 {
                                    break Err("待同步归档通知超过上限，停止运行".into());
                                }
                                archived_threads.insert(thread.clone());
                                plan_offer = None;
                                continue;
                            }
                            if let AgentEvent::FileChanges {turn,item,changes} = &notification {
                                if active.as_ref().is_some_and(|a| !a.stopping && a.gate.as_ref().is_some_and(|g|g.accepts_request(turn))) {
                                    let key=(turn.epoch,turn.thread_id.clone(),turn.turn_id.clone(),item.clone());
                                    // Duplicate item snapshots are ambiguous: revoke pending approvals.
                                    if file_changes.contains_key(&key) || approvals.values().any(|p|p.request.turn==*turn && p.request.item==*item) {
                                        for pending in approvals.values_mut().filter(|p|p.request.turn==*turn && p.request.item==*item) {pending.deadline=Instant::now();}
                                        if let Some(changes)=file_changes.get_mut(&key) {changes.clear();}
                                    } else if file_changes.len()<32 {file_changes.insert(key,changes.clone());}
                                }
                                continue;
                            }
                            if let AgentEvent::Started {turn} = &notification {
                                if let Some(a)=active.as_mut().filter(|a|a.compact && a.gate.is_some() && turn.epoch==settings.epoch && a.compact_thread.as_ref()==Some(&turn.thread_id)) {
                                    if a.turn.as_ref().is_some_and(|current|current!=turn) {break Err("压缩期间出现其他执行，停止运行".into());}
                                    if a.turn.is_none() {
                                        a.turn=Some(turn.clone());
                                        let early=a.gate.as_mut().ok_or("缺少压缩状态")?.bind(turn.clone()).map_err(|_|"压缩执行身份不匹配")?;
                                        if a.stopping {let backend=backend.clone();let turn=turn.clone();jobs.spawn(async move {Done::Control {result:backend.interrupt(turn).await}});}
                                        for item in early {event(&mut active,&mut scheduler,&delivery,item)?;}
                                    }
                                }
                                continue;
                            }
                            if let Some(gate)=active.as_mut().and_then(|a|a.gate.as_mut()) {
                                if let Some(notification)=gate.event(notification).map_err(|_|"Codex 提前事件过多")? {if let Some(offer)=event(&mut active,&mut scheduler,&delivery,notification)? {plan_offer=Some(offer);}}
                            }
                        }
                        Incoming::Request {mut request,reply} => {
                            if let RequestKind::Questions {questions,..}=&request.kind {emit(LogEvent::QuestionReceived, LogStatus::Ok, active.as_ref().map(|a|a.spec.id.as_str()), questions.len());}
                            if let RequestKind::Approval(approval)=&mut request.kind {
                                if approval.kind==crate::requests::ApprovalKind::FileChange {
                                    approval.changes=file_changes.get(&(request.turn.epoch,request.turn.thread_id.clone(),request.turn.turn_id.clone(),request.item.clone())).cloned();
                                }
                            }
                            if matches!(&request.kind, RequestKind::Questions { .. })
                                && !active.as_ref().is_some_and(|a| {
                                    a.spec.mode == ExecutionMode::Plan
                                        && !a.stopping
                                        && a.gate.as_ref().is_some_and(|g| g.accepts_request(&request.turn))
                                })
                            {
                                if let Some(a) = &active {
                                    tell(&delivery, &a.spec.chat, "普通执行模式不支持 Codex 问答请求；请在 Plan 模式下重新发起。")?;
                                }
                                break Err("问答模式或执行身份无效，未提交空答案".into());
                            }
                            if let RequestKind::Questions {questions,..}=&request.kind {
                                if questions.iter().any(|q| !crate::cards::question_supported(q)) {
                                    if let Some(a)=&active {tell(&delivery,&a.spec.chat,"问答详情超过展示上限，停止本次运行；未提交空答案。")?;}
                                    break Err("问答无法完整展示".into());
                                }
                            }
                            if !matches!(&request.kind,RequestKind::Questions {questions,..} if questions.is_empty() || questions.len()>32) {
                                if let Some(a)=active.as_ref().filter(|a| !a.stopping && a.gate.as_ref().is_some_and(|g| g.accepts_request(&request.turn))) {
                                    if approvals.len()<32 && !approvals.values().any(|p| p.request.turn==request.turn && p.request.item==request.item) {
                                        next_approval=next_approval.checked_add(1).ok_or("审批编号耗尽")?;
                                        let token=format!("approval-{}-{next_approval}",settings.epoch);
                                        let owner=crate::cards::Owner {user:a.spec.session.user.clone(),chat:a.spec.chat.clone(),directory:a.spec.session.workspace.clone(),generation:*card_generations.get(&a.spec.session.user).unwrap_or(&0),stop_snapshot:(next_task,Some(a.spec.id.clone()))};
                                        approvals.insert(token,PendingApproval {waiting_text:false,answers:BTreeMap::new(),question:0,request,reply,task:a.spec.id.clone(),owner,deadline:Instant::now()+Duration::from_secs(600),sending:false,source:None});
                                        continue;
                                    }
                                    tell(&delivery,&a.spec.chat,"审批请求重复或待处理数量已满，已拒绝该请求。")?;
                                }
                            } else if let Some(a)=&active {tell(&delivery,&a.spec.chat,"问答请求无效，停止本次运行；未提交空答案。")?;}
                            let response=match request.kind {RequestKind::Approval(_)=>AgentReply::Approve(false),RequestKind::Questions{..}=>break Err("问答请求无效或数量超过上限，未提交空答案".into())};
                            jobs.spawn(async move {Done::Control {result:timeout(Duration::from_secs(10),reply.reply(response)).await.unwrap_or(Err(BackendError::Uncertain))}});
                        }
                    }
                }
                _ = tick.tick() => {
                    if messenger.rich_output() && last_progress.elapsed()>=Duration::from_secs(3) {
                        last_progress=Instant::now();
                        if let Some(a)=active.as_ref().filter(|a|!a.compact) {
                            if delivery.capacity()>8 && !progress_busy.swap(true,Ordering::AcqRel) {
                                let output=a.plan.as_ref().map(|(text,_)|text.as_str()).unwrap_or(&a.output);
                                let preview:String=output.chars().take(1000).collect();
                                let text=format!("{} · 已用 {} 秒\n\n{}{}",if a.stopping {"正在停止"} else {"正在执行"},a.started.elapsed().as_secs(),if preview.is_empty() {"等待 Codex 输出…"} else {&preview},if preview.len()<output.len() {"\n\n（进度预览，完整内容将在结束后发送）"} else {""});
                                if delivery.try_send(DeliveryRequest::Progress {task:a.spec.id.clone(),chat:a.spec.chat.clone(),text}).is_err() {progress_busy.store(false,Ordering::Release);}
                            }
                        }
                    }
                    for creation in confirmations.expire(Instant::now()) {tell(&delivery,&creation.chat,format!("目录创建确认已过期：{}\n未自动创建；如需继续，请重新发送 /cd <路径>。",creation.target.display()))?;}
                    if active.as_ref().is_some_and(|a|a.started.elapsed()>Duration::from_secs(3600)) {break Err("任务超过一小时上限，停止本次运行".into());}
                }
            }
        }
    }.await;
    if let Some(a) = active.take() {
        if let Some(turn) = a.turn {
            let _ = timeout(Duration::from_secs(3), backend.interrupt(turn)).await;
        }
        let _ = delivery.try_send(DeliveryRequest::Answer {
            task: a.spec.id,
            chat: a.spec.chat,
            text: "桥接已停止；未完成任务不会自动重跑。".into(),
        });
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    for (_, pending) in approvals {
        reply_approval(&mut jobs, pending, false);
    }
    let _ = timeout(Duration::from_secs(3), async {
        while jobs.join_next().await.is_some() {}
    })
    .await;
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    drop(delivery);
    if !sender.is_finished() && timeout(Duration::from_secs(5), &mut sender).await.is_err() {
        sender.abort();
        let _ = sender.await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_terminal_before_ack_keeps_mutation_gate_and_sends_no_success() -> Result<(), String>
    {
        let mut scheduler = Scheduler::new(1);
        assert!(scheduler.begin_session_mutation());
        let (delivery, mut messages) = mpsc::channel(4);
        let turn = TurnRef {
            epoch: 1,
            thread_id: "thread".into(),
            turn_id: "compact".into(),
        };
        let mut active = Some(Active {
            compact: true,
            compact_ack: false,
            compact_outcome: None,
            compact_thread: Some("thread".into()),
            spec: TaskSpec {
                id: "request".into(),
                session: SessionKey::new("user", "/project"),
                chat: "chat".into(),
                prompt: String::new(),
                model: None,
                mode: ExecutionMode::Execute,
            },
            gate: None,
            turn: Some(turn.clone()),
            stopping: false,
            output: String::new(),
            plan: None,
            truncated: false,
            started: Instant::now(),
        });
        event(
            &mut active,
            &mut scheduler,
            &delivery,
            AgentEvent::Finished {
                turn,
                outcome: TurnOutcome::Completed,
            },
        )?;
        assert!(messages.try_recv().is_err());
        assert!(!scheduler.begin_session_mutation());
        let label = active
            .as_mut()
            .and_then(|a| a.compact_outcome.take())
            .ok_or("missing terminal")?;
        finish(&mut active, &mut scheduler, &delivery, label)?;
        assert!(active.is_none());
        assert!(scheduler.begin_session_mutation());
        assert!(
            matches!(messages.try_recv().map_err(|e|e.to_string())?,DeliveryRequest::Text(_,text) if text=="上下文压缩完成。")
        );
        Ok(())
    }
}
