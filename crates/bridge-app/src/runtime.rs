//! Minimal serial runtime: text, help, status and owner-scoped stop.
use crate::{
    AdmissionTicket, Scheduler,
    events::{AgentEvent, Incoming, TurnOutcome},
    execution::Execution,
    messaging::Messenger,
    ports::{AgentBackend, BackendError, Sandbox, TurnInput, TurnRef},
    requests::{AgentReply, RequestKind},
    sessions::{self, DurableJournal, SessionStore},
};
use bridge_core::{ExecutionMode, SessionKey, task::TaskSpec};
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
    pub id: String,
    pub user: String,
    pub chat: String,
    /// None denotes unsupported media/card input; it never becomes a task.
    pub text: Option<String>,
    pub accept: Box<dyn FnOnce(bool) + Send>,
}
pub trait Store: SessionStore + DurableJournal {}
impl<T: SessionStore + DurableJournal> Store for T {}

pub struct Settings {
    pub directory: PathBuf,
    pub allowed: BTreeSet<String>,
    pub open_access: bool,
    pub sandbox: Sandbox,
    pub epoch: u64,
}

struct Active {
    spec: TaskSpec,
    gate: Option<Execution>,
    turn: Option<TurnRef>,
    stopping: bool,
    output: String,
    truncated: bool,
    started: Instant,
}
enum Done {
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

fn tell(
    tx: &mpsc::Sender<(String, String)>,
    chat: &str,
    text: impl Into<String>,
) -> Result<(), String> {
    tx.try_send((chat.into(), text.into()))
        .map_err(|_| "回复队列已满或发送器已退出".into())
}
fn finish(
    active: &mut Option<Active>,
    scheduler: &mut Scheduler,
    delivery: &mpsc::Sender<(String, String)>,
    outcome: String,
) -> Result<(), String> {
    if let Some(active) = active.take() {
        scheduler.finish(&active.spec.id);
        let output = if active.output.is_empty() {
            "（无文本输出）"
        } else {
            &active.output
        };
        tell(
            delivery,
            &active.spec.chat,
            format!(
                "{outcome}\n\n{output}{}",
                if active.truncated {
                    "\n\n输出超过最小版 32 KiB 上限，已截断。"
                } else {
                    ""
                }
            ),
        )?;
    }
    Ok(())
}
fn event(
    active: &mut Option<Active>,
    scheduler: &mut Scheduler,
    delivery: &mpsc::Sender<(String, String)>,
    event: AgentEvent,
) -> Result<(), String> {
    match event {
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
    Ok(())
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
    let (delivery, mut deliveries) = mpsc::channel::<(String, String)>(128);
    let mut sender = tokio::spawn(async move {
        while let Some((chat, text)) = deliveries.recv().await {
            if !matches!(
                timeout(Duration::from_secs(45), messenger.send_text(chat, text)).await,
                Ok(Ok(()))
            ) {
                eprintln!("{{\"event\":\"delivery_failed\"}}");
            }
        }
    });
    let mut scheduler = Scheduler::new(64);
    let mut jobs = JoinSet::new();
    let mut active: Option<Active> = None;
    let mut next_task = 0_u64;
    let mut seen_commands = BTreeMap::<String, Instant>::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let result = async {
        loop {
            if jobs.len() > 128 {break Err("后台操作超过上限，停止运行".into());}
            if active.is_none() {
                if let Some(spec) = scheduler.start_next().cloned() {
                    tell(&delivery, &spec.chat, "已开始执行；可发送 /status 或 /stop。")?;
                    active = Some(Active {spec: spec.clone(), gate: None, turn: None, stopping: false, output: String::new(), truncated: false, started: Instant::now()});
                    let backend = backend.clone(); let store = store.clone(); let sandbox = settings.sandbox;
                    jobs.spawn(async move {
                        let result = sessions::prepare(backend.as_ref(), store.as_ref(), &spec, vec![], sandbox).await.map_err(|e| e.to_string());
                        Done::Prepared {id: spec.id, result}
                    });
                }
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break Ok(()),
                _ = &mut sender => break Err("发送器意外退出".into()),
                input = inputs.recv() => {
                    let Some(input) = input else {break Err("飞书连接已关闭".into());};
                    if !settings.open_access && !settings.allowed.contains(&input.user) {(input.accept)(true);continue;}
                    if input.id.is_empty() || input.user.is_empty() || input.chat.is_empty() {(input.accept)(false);continue;}
                    let text = input.text.as_deref().unwrap_or("").trim();
                    let session = SessionKey::new(&input.user, &settings.directory);
                    if input.text.is_none() || text.is_empty() || text.starts_with('/') {
                        let duplicate = seen_commands.contains_key(&input.id);
                        if !duplicate {
                            if seen_commands.len() >= 1000 {seen_commands.clear();}
                            seen_commands.insert(input.id.clone(), Instant::now());
                            match text {
                                "/help" => tell(&delivery, &input.chat, "Rust 最小运行版：发送文本开始任务。\n/status 查看状态\n/stop 停止自己的任务及排队请求\n当前暂不支持附件、卡片操作和其他命令。")?,
                                "/status" => {
                                    let state = active.as_ref().map(|a| format!("运行中，已用 {} 秒{}", a.started.elapsed().as_secs(), if a.stopping {"，正在停止"} else {""})).unwrap_or_else(|| "空闲".into());
                                    tell(&delivery, &input.chat, format!("Rust 最小运行版：{state}\n等待：{}，保存中：{}", scheduler.queued(), scheduler.pending_admissions()))?;
                                }
                                "/stop" => {
                                    let removed = scheduler.cancel_queued(&session);
                                    let mut stopping = false;
                                    if let Some(a) = active.as_mut().filter(|a| a.spec.session == session && a.spec.chat == input.chat) {
                                        stopping = true;
                                        if !a.stopping {
                                            a.stopping = true;
                                            if let Some(turn) = a.turn.clone() {let backend=backend.clone();jobs.spawn(async move {Done::Control {result:backend.interrupt(turn).await}});}
                                        }
                                    }
                                    tell(&delivery, &input.chat, format!("已取消 {removed} 项等待请求；{}", if stopping {"已请求停止当前任务"} else {"没有可停止的当前任务"}))?;
                                }
                                _ => tell(&delivery, &input.chat, "当前最小版仅支持文本任务、/help、/status、/stop。")?,
                            }
                        }
                        (input.accept)(true);
                        continue;
                    }
                    if text.len() > 32*1024 {tell(&delivery,&input.chat,"输入超过最小版 32 KiB 上限。")?;(input.accept)(true);continue;}
                    next_task = next_task.checked_add(1).ok_or("任务标识耗尽")?;
                    let spec=TaskSpec {id:format!("{}:{next_task}",settings.epoch),session,chat:input.chat.clone(),prompt:text.into(),model:None,mode:ExecutionMode::Execute};
                    match scheduler.reserve(input.id.clone(), spec) {
                        Ok(ticket) => {let store=store.clone();jobs.spawn(async move {let result=store.claim(input.id.clone()).await.map_err(|_|());Done::Admission {ticket,input,result}});}
                        Err(_) => {tell(&delivery,&input.chat,"任务队列繁忙，请稍后重新发送。")?;(input.accept)(false);}
                    }
                }
                done = jobs.join_next(), if !jobs.is_empty() => {
                    let done=done.ok_or("后台任务集合异常")?.map_err(|_|"后台任务异常退出")?;
                    match done {
                        Done::Admission {ticket,input,result} => match result {
                            Ok(new) => {let queued=scheduler.commit_admission(ticket,new);(input.accept)(true);if queued {tell(&delivery,&input.chat,"请求已接收。")?;}}
                            Err(_) => {scheduler.abort_admission(ticket);(input.accept)(false);tell(&delivery,&input.chat,"接收状态保存失败，未启动任务。")?;}
                        },
                        Done::Prepared {id,result} => {
                            if let Some(a)=active.as_mut().filter(|a| a.spec.id==id) {
                                if a.stopping {finish(&mut active,&mut scheduler,&delivery,"准备阶段已停止，未启动任务".into())?;continue;}
                                match result {
                                    Ok(input) => {a.gate=Some(Execution::starting(settings.epoch,input.thread_id.clone(),64));let backend=backend.clone();jobs.spawn(async move {Done::Started {id,result:backend.start_turn(input).await}});}
                                    Err(error) => finish(&mut active,&mut scheduler,&delivery,format!("准备失败：{error}"))?,
                                }
                            }
                        }
                        Done::Started {id,result} => {
                            if let Some(a)=active.as_mut().filter(|a|a.spec.id==id) {
                                match result {
                                    Ok(turn) => {
                                        a.turn=Some(turn.clone());
                                        let early=a.gate.as_mut().ok_or("缺少执行状态")?.bind(turn.clone()).map_err(|_|"执行身份不匹配")?;
                                        if a.stopping {let backend=backend.clone();jobs.spawn(async move {Done::Control {result:backend.interrupt(turn).await}});}
                                        for item in early {event(&mut active,&mut scheduler,&delivery,item)?;}
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
                            if let Some(gate)=active.as_mut().and_then(|a|a.gate.as_mut()) {
                                if let Some(notification)=gate.event(notification).map_err(|_|"Codex 提前事件过多")? {event(&mut active,&mut scheduler,&delivery,notification)?;}
                            }
                        }
                        Incoming::Request {request,reply} => {
                            if let Some(a)=&active {tell(&delivery,&a.spec.chat,"Codex 请求人工交互；最小版暂未开放审批/问答界面，已拒绝或返回空答案。")?;}
                            let response=match request.kind {RequestKind::Approval(_)=>AgentReply::Approve(false),RequestKind::Questions{..}=>AgentReply::Answers(BTreeMap::new())};
                            jobs.spawn(async move {Done::Control {result:reply.reply(response).await}});
                        }
                    }
                }
                _ = tick.tick() => {
                    if active.as_ref().is_some_and(|a|a.started.elapsed()>Duration::from_secs(3600)) {break Err("任务超过一小时上限，停止本次运行".into());}
                }
            }
        }
    }.await;
    if let Some(a) = active.take() {
        if let Some(turn) = a.turn {
            let _ = timeout(Duration::from_secs(3), backend.interrupt(turn)).await;
        }
        let _ = tell(
            &delivery,
            &a.spec.chat,
            "桥接已停止；未完成任务不会自动重跑。",
        );
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    drop(delivery);
    if !sender.is_finished() && timeout(Duration::from_secs(5), &mut sender).await.is_err() {
        sender.abort();
        let _ = sender.await;
    }
    result
}
