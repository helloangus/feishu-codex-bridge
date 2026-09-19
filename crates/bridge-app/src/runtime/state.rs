//! Runtime state: execution, interactions, cards and bounded background jobs.
use super::RuntimeError;
use super::flow::{can_spawn, spawn_reply, tell};
use super::limits;
use crate::{
    Scheduler,
    cards::CardToken,
    diagnostics::Diagnostics,
    directories::{Confirmations, DirectoryStore},
    execution::Execution,
    files::{Attachment, TaskFiles},
    interactions::{Interactions, Pending, ReplyOutcome},
    messaging::{DeliveryError, MessageId, Messenger},
    ports::{AgentBackend, TurnRef},
    presentation::Request as DeliveryRequest,
    requests::RequestKind,
    sessions::{self, DurableJournal, SessionStore},
};
use bridge_core::{
    task::{TaskId, TaskSpec},
    view::Panel,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
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

pub(crate) enum ActiveKind {
    Task,
    Compact {
        acknowledged: bool,
        terminal: Option<String>,
        thread: Option<String>,
    },
}

pub(crate) struct Active {
    pub(crate) kind: ActiveKind,
    pub(crate) spec: TaskSpec,
    pub(crate) gate: Option<Execution>,
    pub(crate) turn: Option<TurnRef>,
    pub(crate) stopping: bool,
    pub(crate) output: String,
    pub(crate) plan: Option<(String, bool)>,
    pub(crate) truncated: bool,
    pub(crate) started: Instant,
}

impl Active {
    pub(crate) fn is_compact(&self) -> bool {
        matches!(self.kind, ActiveKind::Compact { .. })
    }

    pub(crate) fn compact_is_terminal(&self) -> bool {
        matches!(
            self.kind,
            ActiveKind::Compact {
                terminal: Some(_),
                ..
            }
        )
    }
}

/// Task files move from the finished task to delivery in one bounded step.
pub(crate) enum FileDelivery {
    Idle,
    Holding(TaskSpec),
    Delivering,
}

pub(crate) struct PanelRefresh {
    pub(crate) source: String,
    pub(crate) owner: crate::cards::Owner,
    pub(crate) panel: Panel,
    pub(crate) commands: Vec<(CardToken, String)>,
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

/// One finished background job, grouped by the causal flow it belongs to so
/// [`super::jobs`] can route it to one handler per domain. Every completion
/// either advances its flow or reports a failure to the user; results unknown
/// to the protocol stop the run instead of retrying.
pub(crate) enum Done {
    /// Durable claim chains over session state: directory selection and
    /// creation, preferences, thread resume/archive, reset, compaction and
    /// archive sync. Each claim job owns the scheduler's session-mutation gate
    /// until its terminal event releases it.
    Session(SessionDone),
    /// Task lifecycle: plan implementation, durable admission, preparation,
    /// turn start and backend control replies (interrupts, denials).
    Task(TaskDone),
    /// Card and panel delivery receipts plus interaction reply receipts.
    Card(CardDone),
    /// Task-file delivery.
    Delivery(DeliveryDone),
}

pub(crate) enum SessionDone {
    ArchiveSynced {
        result: Result<(), sessions::SessionStoreError>,
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
    /// Compaction claim chain. Between `Prepared` and `Submitted` the active
    /// entry is a compaction; `acknowledged` records that the backend accepted
    /// the request, and a terminal event that arrives unacknowledged must park
    /// its label in `terminal` while keeping the mutation gate closed.
    CompactClaim {
        input: super::Input,
        session: bridge_core::SessionKey,
        result: Result<bool, ()>,
    },
    CompactPrepared {
        id: TaskId,
        result: Result<String, sessions::StartError>,
    },
    CompactSubmitted {
        id: TaskId,
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
    Reset {
        input: super::Input,
        result: Result<bool, ()>,
    },
}

pub(crate) enum TaskDone {
    PlanAction {
        input: super::Input,
        task: Option<TaskSpec>,
        result: Result<bool, String>,
    },
    Admission {
        ticket: crate::AdmissionTicket,
        input: super::Input,
        result: Result<bool, ()>,
    },
    Prepared {
        id: TaskId,
        result: Result<crate::ports::TurnInput, String>,
    },
    Started {
        id: TaskId,
        result: Result<TurnRef, crate::ports::BackendError>,
    },
    Control {
        result: Result<(), crate::ports::BackendError>,
    },
}

pub(crate) enum CardDone {
    ApprovalSent {
        token: CardToken,
        panel: Panel,
        commands: Vec<(CardToken, String)>,
        result: Result<MessageId, DeliveryError>,
    },
    ApprovalReplied {
        outcome: ReplyOutcome,
    },
    PanelUpdated,
    PanelSent {
        refreshed: bool,
        panel: Panel,
        entries: Vec<(CardToken, crate::cards::Action)>,
        result: Result<MessageId, DeliveryError>,
    },
    Listed {
        refresh: Option<String>,
        chat: String,
        user: String,
        directory: PathBuf,
        generation: u64,
        stop_snapshot: (u64, Option<TaskId>),
        result: Result<ListedContent, sessions::StartError>,
    },
}

pub(crate) enum DeliveryDone {
    FilesDelivered,
}

/// Authorization and duplicate suppression for inbound inputs: the effective
/// allowlist, the pairing throttle and one-time command ids.
pub(crate) struct Admission {
    pub(crate) allowed: BTreeSet<String>,
    pub(crate) pairing_window: Instant,
    pub(crate) pairing_attempts: u32,
    pub(crate) seen_commands: BTreeMap<String, Instant>,
}

/// The single active execution plus everything that feeds it: the scheduler
/// admitting tasks, the identity counter and staged per-task attachments.
///
/// Invariant: whichever flow acquires the scheduler's session-mutation gate
/// releases it at that chain's terminal completion (see [`super::jobs`]).
pub(crate) struct TaskTrack {
    pub(crate) scheduler: Scheduler,
    pub(crate) active: Option<Active>,
    pub(crate) next_task: u64,
    pub(crate) resources: BTreeMap<TaskId, Vec<Attachment>>,
    pub(crate) files: FileDelivery,
}

/// Cards: minted actions, delivered views, per-user invalidation generations,
/// the panel refresh queue and the plan-offer card.
pub(crate) struct CardBook {
    pub(crate) actions: crate::cards::Actions,
    pub(crate) views: crate::cards::Views,
    pub(crate) generations: BTreeMap<String, u64>,
    pub(crate) refreshes: VecDeque<PanelRefresh>,
    pub(crate) updating_panel: bool,
    pub(crate) next_panel: u64,
    pub(crate) plan_offer: Option<crate::plans::Offer>,
}

/// Approval and question interactions plus their buffered protocol context.
pub(crate) struct Approvals {
    pub(crate) interactions: Interactions,
    pub(crate) file_changes:
        BTreeMap<(u64, String, String, String), Vec<crate::requests::FileChange>>,
    pub(crate) next_token: u64,
}

/// Per-user session bookkeeping: selected directories, creation confirmations
/// and the archive-sync backlog.
pub(crate) struct SessionBook {
    pub(crate) directories: BTreeMap<String, PathBuf>,
    pub(crate) confirmations: Confirmations,
    pub(crate) next_confirmation: u64,
    pub(crate) archived_threads: BTreeSet<String>,
}

/// Progress preview throttling shared with the delivery sender task.
pub(crate) struct Progress {
    pub(crate) busy: Arc<AtomicBool>,
    pub(crate) last: Instant,
}

/// Owner of the entire run state. Methods live beside their concern:
/// [`super::input`], [`super::protocol`], [`super::jobs`] and [`super::timers`];
/// the state components own their fields and invariants.
pub(crate) struct Runtime {
    pub(crate) settings: Settings,
    pub(crate) diagnostics: Diagnostics,
    pub(crate) backend: Arc<dyn AgentBackend>,
    pub(crate) store: Arc<dyn Store>,
    pub(crate) messenger: Arc<dyn Messenger>,
    pub(crate) task_files: Arc<dyn TaskFiles>,
    pub(crate) delivery: mpsc::Sender<DeliveryRequest>,
    pub(crate) admission: Admission,
    pub(crate) tasks: TaskTrack,
    pub(crate) cards: CardBook,
    pub(crate) approvals: Approvals,
    pub(crate) session_book: SessionBook,
    pub(crate) progress: Progress,
}

impl Runtime {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        settings: Settings,
        diagnostics: Diagnostics,
        backend: Arc<dyn AgentBackend>,
        store: Arc<dyn Store>,
        messenger: Arc<dyn Messenger>,
        task_files: Arc<dyn TaskFiles>,
        delivery: mpsc::Sender<DeliveryRequest>,
        directories: BTreeMap<String, PathBuf>,
        progress_busy: Arc<AtomicBool>,
    ) -> Self {
        let allowed = settings.allowed.clone();
        Self {
            settings,
            diagnostics,
            backend,
            store,
            messenger,
            task_files,
            delivery,
            admission: Admission {
                allowed,
                pairing_window: Instant::now(),
                pairing_attempts: 0,
                seen_commands: BTreeMap::new(),
            },
            tasks: TaskTrack {
                scheduler: Scheduler::new(limits::SCHEDULED_TASKS),
                active: None,
                next_task: 0,
                resources: BTreeMap::new(),
                files: FileDelivery::Idle,
            },
            cards: CardBook {
                actions: crate::cards::Actions::default(),
                views: crate::cards::Views::default(),
                generations: BTreeMap::new(),
                refreshes: VecDeque::new(),
                updating_panel: false,
                next_panel: 0,
                plan_offer: None,
            },
            approvals: Approvals {
                interactions: Interactions::default(),
                file_changes: BTreeMap::new(),
                next_token: 0,
            },
            session_book: SessionBook {
                directories,
                confirmations: Confirmations::default(),
                next_confirmation: 0,
                archived_threads: BTreeSet::new(),
            },
            progress: Progress {
                busy: progress_busy,
                last: Instant::now(),
            },
        }
    }

    /// Whether another background job fits. User-triggered work keeps the
    /// control reserve free; control work may use it.
    pub(crate) fn can_spawn(&self, jobs: &JoinSet<Done>, control: bool) -> bool {
        can_spawn(jobs, control)
    }

    /// Whether the active task still owns this turn and accepts requests.
    pub(crate) fn turn_is_live(&self, turn: &TurnRef) -> bool {
        self.tasks.active.as_ref().is_some_and(|active| {
            !active.stopping
                && active
                    .gate
                    .as_ref()
                    .is_some_and(|gate| gate.accepts_request(turn))
        })
    }

    /// The snapshot card actions compare themselves against.
    pub(crate) fn card_snapshot(&self) -> (u64, Option<TaskId>) {
        (
            self.tasks.next_task,
            self.tasks
                .active
                .as_ref()
                .map(|active| active.spec.id.clone()),
        )
    }

    /// Bookkeeping that runs between every event: archive sync, expiries,
    /// approval card delivery, panel refreshes, file delivery and admission.
    /// Returning `Err` stops the run.
    pub(crate) async fn maintain(&mut self, jobs: &mut JoinSet<Done>) -> Result<(), RuntimeError> {
        if !self.session_book.archived_threads.is_empty()
            && self.can_spawn(jobs, false)
            && self.tasks.scheduler.begin_invalidation()
        {
            self.cards.plan_offer = None;
            let thread = self
                .session_book
                .archived_threads
                .pop_first()
                .ok_or(RuntimeError::Internal("缺少归档同步目标"))?;
            let store = self.store.clone();
            jobs.spawn(async move {
                Done::Session(SessionDone::ArchiveSynced {
                    result: store.clear_thread(thread).await,
                })
            });
        }
        if self
            .cards
            .plan_offer
            .as_ref()
            .is_some_and(|offer| Instant::now() >= offer.deadline)
        {
            self.cards.plan_offer = None;
        }
        self.cards
            .actions
            .retain_plan(self.cards.plan_offer.as_ref().map(|offer| &offer.token));
        self.tasks
            .resources
            .retain(|id: &TaskId, _| self.tasks.scheduler.has_task(id));
        let live_keys: std::collections::BTreeSet<(u64, String, String, String)> = self
            .approvals
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
        self.approvals
            .file_changes
            .retain(|key, _| live_keys.contains(key));
        self.expire_stale_approvals(jobs)?;
        self.deliver_approval_cards(jobs);
        self.refresh_panels(jobs);
        self.deliver_files(jobs);
        self.deliver_plan_offer(jobs);
        self.start_next_task(jobs);
        Ok(())
    }

    fn expire_stale_approvals(&mut self, jobs: &mut JoinSet<Done>) -> Result<(), RuntimeError> {
        let alive = |pending: &Pending| {
            self.tasks.active.as_ref().is_some_and(|active| {
                active.spec.id == pending.task
                    && !active.stopping
                    && active
                        .gate
                        .as_ref()
                        .is_some_and(|gate| gate.accepts_request(&pending.request.turn))
            })
        };
        for token in self
            .approvals
            .interactions
            .stale_tokens(Instant::now(), alive)
        {
            let Some(pending) = self.approvals.interactions.remove(&token) else {
                continue;
            };
            self.cards.actions.invalidate_approval(&token);
            if pending.is_questions() {
                tell(
                    &self.delivery,
                    &pending.owner.chat,
                    "问答未完成且已超时或任务已结束/停止，正在停止桥接；未提交空答案，不会自动重跑。",
                )?;
                return Err(RuntimeError::Interaction("问答未完成，停止本次运行"));
            }
            let message = "审批已超时或任务已结束/停止，正在回传拒绝。";
            if let Some(source) = &pending.source {
                self.cards.views.note(source, message);
            }
            tell(&self.delivery, &pending.owner.chat, message)?;
            spawn_reply(&self.diagnostics, jobs, pending, false)?;
        }
        Ok(())
    }

    fn deliver_approval_cards(&mut self, jobs: &mut JoinSet<Done>) {
        let live = |pending: &Pending| {
            self.tasks
                .active
                .as_ref()
                .is_some_and(|active| active.turn.as_ref() == Some(&pending.request.turn))
        };
        for token in self.approvals.interactions.unsent_tokens(live) {
            if !self.can_spawn(jobs, false) {
                // Retry on a later loop iteration; nothing was marked sent.
                break;
            }
            let Some(pending) = self.approvals.interactions.get_mut(&token) else {
                continue;
            };
            pending.card_dispatched = true;
            self.cards.next_panel = match self.cards.next_panel.checked_add(1) {
                Some(value) => value,
                None => return,
            };
            let prefix = format!("panel-{}-{}", self.settings.epoch, self.cards.next_panel);
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
                    limits::MESSAGE_TIMEOUT,
                    messenger.send_panel(chat, panel.clone()),
                )
                .await
                .unwrap_or(Err(DeliveryError::Transport));
                Done::Card(CardDone::ApprovalSent {
                    token,
                    panel,
                    commands,
                    result,
                })
            });
        }
    }

    fn refresh_panels(&mut self, jobs: &mut JoinSet<Done>) {
        if self.cards.updating_panel || !self.can_spawn(jobs, false) {
            return;
        }
        if let Some(refresh) = self.cards.refreshes.pop_front() {
            self.cards.updating_panel = super::flow::send_panel(
                &self.diagnostics,
                Some(refresh.source),
                jobs,
                self.messenger.clone(),
                refresh.owner,
                refresh.panel,
                refresh.commands,
            );
            return;
        }
        let snapshot = self.card_snapshot();
        if let Some((source, panel)) =
            self.cards
                .views
                .next_update(&self.cards.actions, Instant::now(), &snapshot)
        {
            self.cards.updating_panel = true;
            let messenger = self.messenger.clone();
            let diagnostics = self.diagnostics.clone();
            jobs.spawn(async move {
                if !matches!(
                    timeout(
                        limits::MESSAGE_TIMEOUT,
                        messenger.update_panel(MessageId(source), panel)
                    )
                    .await,
                    Ok(Ok(()))
                ) {
                    diagnostics.emit(
                        crate::diagnostics::Event::CardFailed,
                        crate::diagnostics::Status::Failed,
                        None,
                        0,
                    );
                }
                Done::Card(CardDone::PanelUpdated)
            });
        }
    }

    fn deliver_files(&mut self, jobs: &mut JoinSet<Done>) {
        if self.tasks.active.is_some() {
            return;
        }
        let FileDelivery::Holding(spec) = &self.tasks.files else {
            return;
        };
        let spec = spec.clone();
        self.tasks.files = FileDelivery::Delivering;
        if !self.can_spawn(jobs, false) {
            self.tasks.files = FileDelivery::Holding(spec);
            return;
        }
        let task_files = self.task_files.clone();
        let messenger = self.messenger.clone();
        let diagnostics = self.diagnostics.clone();
        jobs.spawn(async move {
            let result = timeout(
                limits::FILE_FINISH_TIMEOUT,
                task_files.finish_files(
                    spec.id.as_str().to_owned(),
                    spec.chat.clone(),
                    spec.session.workspace,
                ),
            )
            .await;
            diagnostics.emit(
                crate::diagnostics::Event::FilesFinished,
                if matches!(&result, Ok(Ok(()))) {
                    crate::diagnostics::Status::Ok
                } else {
                    crate::diagnostics::Status::Failed
                },
                Some(spec.id.as_str()),
                0,
            );
            if !matches!(result, Ok(Ok(()))) {
                let _ = timeout(
                    limits::BACKEND_REPLY_TIMEOUT,
                    messenger.send_text(
                        spec.chat.clone(),
                        "成果物处理失败或超时，未自动重试；请检查工作目录。".into(),
                    ),
                )
                .await;
            }
            Done::Delivery(DeliveryDone::FilesDelivered)
        });
    }

    fn deliver_plan_offer(&mut self, jobs: &mut JoinSet<Done>) {
        if self.tasks.active.is_some() || !matches!(self.tasks.files, FileDelivery::Idle) {
            return;
        }
        if self.tasks.scheduler.queued() > 0 || self.tasks.scheduler.pending_admissions() > 0 {
            return;
        }
        let unsent = self
            .cards
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
        if let Some(offer) = self.cards.plan_offer.as_mut() {
            offer.sent = true;
        }
        let Some(offer) = self.cards.plan_offer.as_ref() else {
            return;
        };
        self.cards.next_panel = match self.cards.next_panel.checked_add(1) {
            Some(value) => value,
            None => return,
        };
        let (panel, commands) = crate::plans::panel(
            offer,
            &format!("panel-{}-{}", self.settings.epoch, self.cards.next_panel),
        );
        let owner = crate::cards::Owner {
            user: offer.task.session.user.clone(),
            chat: offer.task.chat.clone(),
            directory: offer.task.session.workspace.clone(),
            generation: *self
                .cards
                .generations
                .get(&offer.task.session.user)
                .unwrap_or(&0),
            stop_snapshot: (self.tasks.next_task, None),
        };
        super::flow::send_panel(
            &self.diagnostics,
            None,
            jobs,
            self.messenger.clone(),
            owner,
            panel,
            commands,
        );
    }

    fn start_next_task(&mut self, jobs: &mut JoinSet<Done>) {
        if self.tasks.active.is_some() || !matches!(self.tasks.files, FileDelivery::Idle) {
            return;
        }
        if !self.can_spawn(jobs, false) {
            return;
        }
        let Some(spec) = self.tasks.scheduler.start_next().cloned() else {
            return;
        };
        self.cards.plan_offer = None;
        self.tasks.files = FileDelivery::Holding(spec.clone());
        let attachments: Vec<Attachment> =
            self.tasks.resources.remove(&spec.id).unwrap_or_default();
        let delivery = self.delivery.clone();
        let _ = tell(
            &delivery,
            &spec.chat,
            "已开始执行；可发送 /status 或 /stop。",
        );
        self.tasks.active = Some(Active {
            kind: ActiveKind::Task,
            spec: spec.clone(),
            gate: None,
            turn: None,
            stopping: false,
            output: String::new(),
            plan: None,
            truncated: false,
            started: Instant::now(),
        });
        let backend = self.backend.clone();
        let store = self.store.clone();
        let sandbox = self.settings.sandbox;
        let root = self.settings.root.clone();
        let task_files = self.task_files.clone();
        let diagnostics = self.diagnostics.clone();
        jobs.spawn(async move {
            let result = async {
                store
                    .validate_directory(root, spec.session.workspace.clone())
                    .await
                    .map_err(|_| {
                        "当前目录已失效或越出工作区，请使用 /cd <绝对路径> 重新选择目录。"
                            .to_owned()
                    })?;
                let count = attachments.len();
                let prepared = timeout(
                    limits::FILE_PREPARE_TIMEOUT,
                    task_files.prepare_files(
                        spec.id.as_str().to_owned(),
                        spec.session.workspace.clone(),
                        attachments,
                    ),
                )
                .await;
                diagnostics.emit(
                    crate::diagnostics::Event::FilesPrepared,
                    if matches!(&prepared, Ok(Ok(_))) {
                        crate::diagnostics::Status::Ok
                    } else {
                        crate::diagnostics::Status::Failed
                    },
                    Some(spec.id.as_str()),
                    count,
                );
                let files = prepared
                    .map_err(|_| "附件准备超时".to_owned())?
                    .map_err(|e| format!("附件或快照准备失败：{e}"))?;
                let mut configured = spec.clone();
                configured.prompt.push_str(&files.prompt);
                let mut turn = sessions::prepare_configured(
                    backend.as_ref(),
                    store.as_ref(),
                    &configured,
                    sandbox,
                )
                .await
                .map_err(|e| e.to_string())?;
                turn.images = files.images;
                timeout(
                    limits::FILE_BIND_TIMEOUT,
                    task_files.bind_files(spec.id.as_str().to_owned(), turn.thread_id.clone()),
                )
                .await
                .map_err(|_| "生成图片快照超时".to_owned())?
                .map_err(|e| e.to_string())?;
                Ok(turn)
            }
            .await;
            Done::Task(TaskDone::Prepared {
                id: spec.id,
                result,
            })
        });
    }
}
