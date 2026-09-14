//! Runtime state: execution, interactions, cards and bounded background jobs.
use super::flow::{can_spawn, spawn_reply, tell};
use crate::{
    Scheduler,
    directories::{Confirmations, DirectoryStore},
    execution::Execution,
    interactions::{Interactions, Pending, ReplyOutcome},
    messaging::{Attachment, DeliveryError, MessageId, Messenger},
    ports::{AgentBackend, TurnRef},
    presentation::Request as DeliveryRequest,
    requests::RequestKind,
    sessions::{self, DurableJournal, SessionStore},
};
use bridge_core::{task::TaskSpec, view::Panel};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tokio::{
    sync::mpsc,
    task::JoinSet,
    time::{Instant, timeout},
};

/// User-facing entry point configuration for one run.
pub struct Settings {
    pub root: std::path::PathBuf,
    pub directory: std::path::PathBuf,
    pub allowed: BTreeSet<String>,
    pub open_access: bool,
    pub sandbox: crate::ports::Sandbox,
    pub epoch: u64,
}

/// The durable state ports the runtime depends on.
pub trait Store: SessionStore + DurableJournal + DirectoryStore {}
impl<T: SessionStore + DurableJournal + DirectoryStore> Store for T {}

/// Total background jobs; beyond this the run stops rather than losing work.
pub(crate) const BACKGROUND_LIMIT: usize = 128;
/// Capacity kept aside for stop, denial and shutdown replies.
pub(crate) const CONTROL_RESERVE: usize = 16;

pub(crate) struct Active {
    pub(crate) compact: bool,
    pub(crate) compact_ack: bool,
    pub(crate) compact_outcome: Option<String>,
    pub(crate) compact_thread: Option<String>,
    pub(crate) spec: TaskSpec,
    pub(crate) gate: Option<Execution>,
    pub(crate) turn: Option<TurnRef>,
    pub(crate) stopping: bool,
    pub(crate) output: String,
    pub(crate) plan: Option<(String, bool)>,
    pub(crate) truncated: bool,
    pub(crate) started: Instant,
}

/// Task files move from the finished task to delivery in one bounded step.
pub(crate) enum FileDelivery {
    Idle,
    Holding(TaskSpec),
    Delivering,
}

pub(crate) enum ListedContent {
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

pub(crate) enum Done {
    ArchiveSynced {
        result: Result<(), sessions::SessionStoreError>,
    },
    PlanAction {
        input: super::Input,
        task: Option<TaskSpec>,
        result: Result<bool, String>,
    },
    FilesDelivered,
    ApprovalSent {
        token: String,
        panel: Panel,
        commands: Vec<(String, String)>,
        result: Result<MessageId, DeliveryError>,
    },
    ApprovalReplied {
        outcome: ReplyOutcome,
    },
    PanelUpdated,
    PanelSent {
        refreshed: bool,
        panel: Panel,
        entries: Vec<(String, crate::cards::Action)>,
        result: Result<MessageId, DeliveryError>,
    },
    DirectoryProposed {
        user: String,
        chat: String,
        current: PathBuf,
        target: String,
        result: Result<Option<PathBuf>, sessions::SessionStoreError>,
    },
    CreationClaim {
        input: super::Input,
        creation: crate::directories::Creation,
        result: Result<bool, ()>,
    },
    DirectoryCreated {
        user: String,
        chat: String,
        result: Result<PathBuf, sessions::SessionStoreError>,
    },
    DirectoryClaim {
        input: super::Input,
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
        input: super::Input,
        session: bridge_core::SessionKey,
        result: Result<bool, ()>,
    },
    CompactPrepared {
        id: String,
        result: Result<String, sessions::StartError>,
    },
    CompactSubmitted {
        id: String,
        result: Result<(), crate::ports::BackendError>,
    },
    PreferenceClaim {
        input: super::Input,
        session: bridge_core::SessionKey,
        change: sessions::PreferenceChange,
        result: Result<bool, ()>,
    },
    PreferenceChanged {
        chat: String,
        result: Result<(), sessions::StartError>,
    },
    ThreadClaim {
        input: super::Input,
        session: bridge_core::SessionKey,
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
        input: super::Input,
        result: Result<bool, ()>,
    },
    Admission {
        ticket: crate::AdmissionTicket,
        input: super::Input,
        result: Result<bool, ()>,
    },
    Prepared {
        id: String,
        result: Result<crate::ports::TurnInput, String>,
    },
    Started {
        id: String,
        result: Result<TurnRef, crate::ports::BackendError>,
    },
    Control {
        result: Result<(), crate::ports::BackendError>,
    },
}

/// Owner of the entire run state. Methods live beside their concern:
/// [`super::input`], [`super::protocol`], [`super::jobs`] and [`super::timers`].
pub(crate) struct Runtime {
    pub(crate) settings: Settings,
    pub(crate) backend: Arc<dyn AgentBackend>,
    pub(crate) store: Arc<dyn Store>,
    pub(crate) messenger: Arc<dyn Messenger>,
    pub(crate) delivery: mpsc::Sender<DeliveryRequest>,
    pub(crate) directories: BTreeMap<String, PathBuf>,
    pub(crate) scheduler: Scheduler,
    pub(crate) active: Option<Active>,
    pub(crate) next_task: u64,
    pub(crate) resources: BTreeMap<String, Vec<Attachment>>,
    pub(crate) files: FileDelivery,
    pub(crate) plan_offer: Option<crate::plans::Offer>,
    pub(crate) seen_commands: BTreeMap<String, Instant>,
    pub(crate) confirmations: Confirmations,
    pub(crate) next_confirmation: u64,
    pub(crate) card_actions: crate::cards::Actions,
    pub(crate) card_views: crate::cards::Views,
    pub(crate) updating_panel: bool,
    pub(crate) refreshes: VecDeque<(String, crate::cards::Owner, Panel, Vec<(String, String)>)>,
    pub(crate) card_generations: BTreeMap<String, u64>,
    pub(crate) next_panel: u64,
    pub(crate) approvals: Interactions,
    pub(crate) file_changes:
        BTreeMap<(u64, String, String, String), Vec<crate::requests::FileChange>>,
    pub(crate) next_approval: u64,
    pub(crate) progress_busy: Arc<AtomicBool>,
    pub(crate) last_progress: Instant,
    pub(crate) archived_threads: BTreeSet<String>,
    pub(crate) allowed: BTreeSet<String>,
    pub(crate) pairing_window: Instant,
    pub(crate) pairing_attempts: u32,
}

impl Runtime {
    pub(crate) fn new(
        settings: Settings,
        backend: Arc<dyn AgentBackend>,
        store: Arc<dyn Store>,
        messenger: Arc<dyn Messenger>,
        delivery: mpsc::Sender<DeliveryRequest>,
        directories: BTreeMap<String, PathBuf>,
        progress_busy: Arc<AtomicBool>,
    ) -> Self {
        let allowed = settings.allowed.clone();
        Self {
            settings,
            backend,
            store,
            messenger,
            delivery,
            directories,
            scheduler: Scheduler::new(64),
            active: None,
            next_task: 0,
            resources: BTreeMap::new(),
            files: FileDelivery::Idle,
            plan_offer: None,
            seen_commands: BTreeMap::new(),
            confirmations: Confirmations::default(),
            next_confirmation: 0,
            card_actions: crate::cards::Actions::default(),
            card_views: crate::cards::Views::default(),
            updating_panel: false,
            refreshes: VecDeque::new(),
            card_generations: BTreeMap::new(),
            next_panel: 0,
            approvals: Interactions::default(),
            file_changes: BTreeMap::new(),
            next_approval: 0,
            progress_busy,
            last_progress: Instant::now(),
            archived_threads: BTreeSet::new(),
            allowed,
            pairing_window: Instant::now(),
            pairing_attempts: 0,
        }
    }

    /// Whether another background job fits. User-triggered work keeps the
    /// control reserve free; control work may use it.
    pub(crate) fn can_spawn(&self, jobs: &JoinSet<Done>, control: bool) -> bool {
        can_spawn(jobs, control)
    }

    /// Whether the active task still owns this turn and accepts requests.
    pub(crate) fn turn_is_live(&self, turn: &TurnRef) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| !active.stopping && active.gate.as_ref().is_some_and(|gate| gate.accepts_request(turn)))
    }

    /// The snapshot card actions compare themselves against.
    pub(crate) fn card_snapshot(&self) -> (u64, Option<String>) {
        (
            self.next_task,
            self.active.as_ref().map(|active| active.spec.id.clone()),
        )
    }

    /// Bookkeeping that runs between every event: archive sync, expiries,
    /// approval card delivery, panel refreshes, file delivery and admission.
    /// Returning `Err` stops the run.
    pub(crate) async fn maintain(&mut self, jobs: &mut JoinSet<Done>) -> Result<(), String> {
        if !self.archived_threads.is_empty() && self.scheduler.begin_invalidation() {
            self.plan_offer = None;
            let thread = self
                .archived_threads
                .pop_first()
                .ok_or("缺少归档同步目标".to_owned())?;
            let store = self.store.clone();
            jobs.spawn(async move {
                Done::ArchiveSynced {
                    result: store.clear_thread(thread).await,
                }
            });
        }
        if self
            .plan_offer
            .as_ref()
            .is_some_and(|offer| Instant::now() >= offer.deadline)
        {
            self.plan_offer = None;
        }
        self.card_actions
            .retain_plan(self.plan_offer.as_ref().map(|offer| offer.token.as_str()));
        self.resources
            .retain(|id: &String, _| self.scheduler.has_task(id));
        let live_keys: std::collections::BTreeSet<(u64, String, String, String)> = self
            .file_changes
            .keys()
            .filter(|(epoch, thread, turn, _)| {
                self.turn_is_live(&TurnRef {
                    epoch: *epoch,
                    thread_id: thread.clone(),
                    turn_id: turn.clone(),
                })
            })
            .cloned()
            .collect();
        self.file_changes.retain(|key, _| live_keys.contains(key));
        self.expire_stale_approvals(jobs)?;
        self.deliver_approval_cards(jobs);
        self.refresh_panels(jobs);
        self.deliver_files(jobs);
        self.deliver_plan_offer(jobs);
        self.start_next_task(jobs);
        Ok(())
    }

    fn expire_stale_approvals(&mut self, jobs: &mut JoinSet<Done>) -> Result<(), String> {
        let alive = |pending: &Pending| {
            self.active.as_ref().is_some_and(|active| {
                active.spec.id == pending.task
                    && !active.stopping
                    && active
                        .gate
                        .as_ref()
                        .is_some_and(|gate| gate.accepts_request(&pending.request.turn))
            })
        };
        for token in self.approvals.stale_tokens(Instant::now(), alive) {
            let Some(pending) = self.approvals.remove(&token) else {
                continue;
            };
            self.card_actions.invalidate_approval(&token);
            if pending.is_questions() {
                tell(
                    &self.delivery,
                    &pending.owner.chat,
                    "问答未完成且已超时或任务已结束/停止，正在停止桥接；未提交空答案，不会自动重跑。",
                )?;
                return Err("问答未完成，停止本次运行".into());
            }
            let message = "审批已超时或任务已结束/停止，正在回传拒绝。";
            if let Some(source) = &pending.source {
                self.card_views.note(source, message);
            }
            tell(&self.delivery, &pending.owner.chat, message)?;
            spawn_reply(jobs, pending, false)?;
        }
        Ok(())
    }

    fn deliver_approval_cards(&mut self, jobs: &mut JoinSet<Done>) {
        let live = |pending: &Pending| {
            self.active.as_ref().is_some_and(|active| {
                active.turn.as_ref() == Some(&pending.request.turn)
            })
        };
        for token in self.approvals.unsent_tokens(live) {
            if !self.can_spawn(jobs, true) {
                // Retry on a later loop iteration; nothing was marked sent.
                break;
            }
            let Some(pending) = self.approvals.get_mut(&token) else {
                continue;
            };
            pending.sending = true;
            self.next_panel = match self.next_panel.checked_add(1) {
                Some(value) => value,
                None => return,
            };
            let prefix = format!("panel-{}-{}", self.settings.epoch, self.next_panel);
            let (panel, commands) = match &pending.request.kind {
                RequestKind::Approval(request) => crate::cards::approval(request, &token, &prefix),
                RequestKind::Questions { questions, .. } => crate::cards::question(
                    &questions[pending.question],
                    pending.question,
                    questions.len(),
                    &token,
                    &prefix,
                ),
            };
            let chat = pending.owner.chat.clone();
            let messenger = self.messenger.clone();
            jobs.spawn(async move {
                let result = timeout(
                    Duration::from_secs(45),
                    messenger.send_panel(chat, panel.clone()),
                )
                .await
                .unwrap_or(Err(DeliveryError::Transport));
                Done::ApprovalSent {
                    token,
                    panel,
                    commands,
                    result,
                }
            });
        }
    }

    fn refresh_panels(&mut self, jobs: &mut JoinSet<Done>) {
        if self.updating_panel {
            return;
        }
        if let Some((source, owner, panel, commands)) = self.refreshes.pop_front() {
            self.updating_panel = true;
            super::flow::send_panel(
                Some(source),
                jobs,
                self.messenger.clone(),
                owner,
                panel,
                commands,
            );
            return;
        }
        let snapshot = self.card_snapshot();
        if let Some((source, panel)) =
            self.card_views
                .next_update(&self.card_actions, Instant::now(), &snapshot)
        {
            self.updating_panel = true;
            let messenger = self.messenger.clone();
            jobs.spawn(async move {
                if !matches!(
                    timeout(
                        Duration::from_secs(45),
                        messenger.update_panel(MessageId(source), panel)
                    )
                    .await,
                    Ok(Ok(()))
                ) {
                    eprintln!("{{\"event\":\"card_update_failed\"}}");
                }
                Done::PanelUpdated
            });
        }
    }

    fn deliver_files(&mut self, jobs: &mut JoinSet<Done>) {
        if self.active.is_some() {
            return;
        }
        if let FileDelivery::Holding(spec) =
            std::mem::replace(&mut self.files, FileDelivery::Delivering)
        {
            if !self.can_spawn(jobs, true) {
                self.files = FileDelivery::Holding(spec);
                return;
            }
            let messenger = self.messenger.clone();
            jobs.spawn(async move {
                let result = timeout(
                    Duration::from_secs(180),
                    messenger.finish_files(spec.id.clone(), spec.chat.clone(), spec.session.workspace),
                )
                .await;
                crate::diagnostics::emit(
                    crate::diagnostics::Event::FilesFinished,
                    if matches!(&result, Ok(Ok(()))) {
                        crate::diagnostics::Status::Ok
                    } else {
                        crate::diagnostics::Status::Failed
                    },
                    Some(&spec.id),
                    0,
                );
                if !matches!(result, Ok(Ok(()))) {
                    let _ = timeout(
                        Duration::from_secs(10),
                        messenger.send_text(
                            spec.chat.clone(),
                            "成果物处理失败或超时，未自动重试；请检查工作目录。".into(),
                        ),
                    )
                    .await;
                }
                Done::FilesDelivered
            });
        }
    }

    fn deliver_plan_offer(&mut self, jobs: &mut JoinSet<Done>) {
        if self.active.is_some() || !matches!(self.files, FileDelivery::Idle) {
            return;
        }
        if self.scheduler.queued() > 0 || self.scheduler.pending_admissions() > 0 {
            return;
        }
        let unsent = self
            .plan_offer
            .as_ref()
            .filter(|offer| !offer.sent)
            .is_some();
        if !unsent {
            return;
        }
        if !self.can_spawn(jobs, false) {
            return;
        }
        if let Some(offer) = self.plan_offer.as_mut() {
            offer.sent = true;
        }
        let Some(offer) = self.plan_offer.as_ref() else {
            return;
        };
        self.next_panel = match self.next_panel.checked_add(1) {
            Some(value) => value,
            None => return,
        };
        let (panel, commands) =
            crate::plans::panel(offer, &format!("panel-{}-{}", self.settings.epoch, self.next_panel));
        let owner = crate::cards::Owner {
            user: offer.task.session.user.clone(),
            chat: offer.task.chat.clone(),
            directory: offer.task.session.workspace.clone(),
            generation: *self
                .card_generations
                .get(&offer.task.session.user)
                .unwrap_or(&0),
            stop_snapshot: (self.next_task, None),
        };
        super::flow::send_panel(None, jobs, self.messenger.clone(), owner, panel, commands);
    }

    fn start_next_task(&mut self, jobs: &mut JoinSet<Done>) {
        if self.active.is_some() || !matches!(self.files, FileDelivery::Idle) {
            return;
        }
        let Some(spec) = self.scheduler.start_next().cloned() else {
            return;
        };
        self.plan_offer = None;
        self.files = FileDelivery::Holding(spec.clone());
        let attachments: Vec<Attachment> = self.resources.remove(&spec.id).unwrap_or_default();
        let delivery = self.delivery.clone();
        let _ = tell(
            &delivery,
            &spec.chat,
            "已开始执行；可发送 /status 或 /stop。",
        );
        self.active = Some(Active {
            compact: false,
            compact_ack: false,
            compact_outcome: None,
            compact_thread: None,
            spec: spec.clone(),
            gate: None,
            turn: None,
            stopping: false,
            output: String::new(),
            plan: None,
            truncated: false,
            started: Instant::now(),
        });
        if !self.can_spawn(jobs, true) {
            // The task stays active; preparation retries through the normal
            // completion flow once capacity frees up.
            return;
        }
        let backend = self.backend.clone();
        let store = self.store.clone();
        let sandbox = self.settings.sandbox;
        let root = self.settings.root.clone();
        let messenger = self.messenger.clone();
        jobs.spawn(async move {
            let result = async {
                store
                    .validate_directory(root, spec.session.workspace.clone())
                    .await
                    .map_err(|_| "当前目录已失效或越出工作区，请使用 /cd <绝对路径> 重新选择目录。".to_owned())?;
                let count = attachments.len();
                let prepared = timeout(
                    Duration::from_secs(120),
                    messenger.prepare_files(spec.id.clone(), spec.session.workspace.clone(), attachments),
                )
                .await;
                crate::diagnostics::emit(
                    crate::diagnostics::Event::FilesPrepared,
                    if matches!(&prepared, Ok(Ok(_))) {
                        crate::diagnostics::Status::Ok
                    } else {
                        crate::diagnostics::Status::Failed
                    },
                    Some(&spec.id),
                    count,
                );
                let files = prepared
                    .map_err(|_| "附件准备超时".to_owned())?
                    .map_err(|e| format!("附件或快照准备失败：{e}"))?;
                let mut configured = spec.clone();
                configured.prompt.push_str(&files.prompt);
                let mut turn = sessions::prepare_configured(backend.as_ref(), store.as_ref(), &configured, sandbox)
                    .await
                    .map_err(|e| e.to_string())?;
                turn.images = files.images;
                timeout(
                    Duration::from_secs(60),
                    messenger.bind_files(spec.id.clone(), turn.thread_id.clone()),
                )
                .await
                .map_err(|_| "生成图片快照超时".to_owned())?
                .map_err(|e| e.to_string())?;
                Ok(turn)
            }
            .await;
            Done::Prepared { id: spec.id, result }
        });
    }


}
