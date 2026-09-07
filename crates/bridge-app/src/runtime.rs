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
    plan: Option<(String, bool)>,
    truncated: bool,
    started: Instant,
}
enum Done {
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
    ResumeClaim {
        input: Input,
        session: SessionKey,
        thread: String,
        result: Result<bool, ()>,
    },
    SessionChanged {
        chat: String,
        result: Result<(), sessions::StartError>,
    },
    Listed {
        chat: String,
        result: Result<String, sessions::StartError>,
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
        tell(
            delivery,
            &active.spec.chat,
            format!(
                "{outcome}\n\n{output}{}",
                if truncated {
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
                    active = Some(Active {spec: spec.clone(), gate: None, turn: None, stopping: false, output: String::new(), plan: None, truncated: false, started: Instant::now()});
                    let backend = backend.clone(); let store = store.clone(); let sandbox = settings.sandbox;
                    jobs.spawn(async move {
                        let result = sessions::prepare_configured(backend.as_ref(), store.as_ref(), &spec, sandbox).await.map_err(|e| e.to_string());
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
                    use bridge_core::command::Command;
                    let command = Command::parse(text);
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
                                jobs.spawn(async move {
                                    let result = async {
                                        let preferences=store.preferences(session).await?;
                                        match command {
                                            Ok(Command::Models) => {
                                                let models=backend.models().await?;
                                                let mut lines=vec!["可用模型（最多 20 项）：".to_owned()];
                                                for model in models.into_iter().take(20) {
                                                    if model.id.len()>256 || model.id.chars().any(char::is_control) {continue;}
                                                    lines.push(format!("{}{}\n/model {}",model.id,if preferences.model.as_ref()==Some(&model.id) {"（已选择）"} else if model.is_default {"（默认）"} else {""},model.id));
                                                }
                                                lines.push("/model default 恢复 Codex 默认模型".into());
                                                Ok(lines.join("\n\n"))
                                            }
                                            Ok(Command::Model(None)) => Ok(format!("当前模型：{}",preferences.model.unwrap_or_else(||"Codex 默认".into()))),
                                            _ => Ok(format!("Plan 模式：{}。使用 /plan on 或 /plan off 切换。",if preferences.plan {"已开启"} else {"已关闭"})),
                                        }
                                    }.await;
                                    Done::Listed {chat,result}
                                });
                            }
                            (input.accept)(true);
                        }
                        continue;
                    }
                    if let Ok(bridge_core::command::Command::Resume(target)) = bridge_core::command::Command::parse(text) {
                        if let Some(thread) = target {
                            if !sessions::valid_thread_id(&thread) {
                                tell(&delivery, &input.chat, "会话 ID 无效，请复制 /resume 列表中的完整命令。")?;
                                (input.accept)(true); continue;
                            }
                            if !scheduler.begin_session_mutation() {
                                tell(&delivery, &input.chat, "有任务执行中、排队或会话正在更新；请等待完成或先 /stop，再恢复会话。")?;
                                (input.accept)(true); continue;
                            }
                            let store = store.clone();
                            jobs.spawn(async move {
                                let result = store.claim(input.id.clone()).await.map_err(|_| ());
                                Done::ResumeClaim { input, session, thread, result }
                            });
                        } else {
                            if !seen_commands.contains_key(&input.id) {
                                if seen_commands.len() >= 1000 {seen_commands.clear();}
                                seen_commands.insert(input.id.clone(), Instant::now());
                                let backend = backend.clone(); let chat = input.chat.clone();
                                jobs.spawn(async move {Done::Listed {chat, result: sessions::list(backend.as_ref(), &session).await}});
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
                                "/help" => tell(&delivery, &input.chat, "Rust 最小运行版：发送文本开始任务。\n/status 查看状态\n/stop 停止自己的任务及排队请求\n/new 全局空闲时新建自己的会话\n/resume 查看当前目录会话\n/resume <ID> 全局空闲时恢复会话\n/models 列出模型\n/model [ID|default] 查看或设置模型\n/plan [on|off] 查看或设置 Plan 模式\n设置修改需全局空闲；当前暂不支持附件和卡片操作。")?,
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
                                _ => tell(&delivery, &input.chat, "命令暂不支持或参数无效，请发送 /help 查看支持的命令。")?,
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
                        Done::PreferenceClaim {input,session,change,result} => match result {
                            Ok(true) => {
                                (input.accept)(true);let backend=backend.clone();let store=store.clone();
                                jobs.spawn(async move {Done::PreferenceChanged {chat:input.chat,result:sessions::change_preference(backend.as_ref(),store.as_ref(),session,change).await}});
                            }
                            Ok(false) => {scheduler.end_session_mutation();(input.accept)(true);}
                            Err(()) => {scheduler.end_session_mutation();(input.accept)(false);tell(&delivery,&input.chat,"设置请求保存失败，请重新发送。")?;}
                        },
                        Done::PreferenceChanged {chat,result} => {
                            scheduler.end_session_mutation();
                            tell(&delivery,&chat,match result {Ok(())=>"设置已保存，后续任务生效。使用 /model 或 /plan 查看。".into(),Err(error)=>format!("设置失败：{error}；模型请从 /models 选择。不会自动重试，请检查后重新发送。")})?;
                        }
                        Done::ResumeClaim {input,session,thread,result} => {
                            match result {
                                Ok(true) => {
                                    (input.accept)(true);
                                    let backend = backend.clone(); let store = store.clone();
                                    jobs.spawn(async move {Done::SessionChanged {chat: input.chat, result: sessions::resume(backend.as_ref(), store.as_ref(), session, thread).await}});
                                }
                                Ok(false) => {scheduler.end_session_mutation();(input.accept)(true);}
                                Err(()) => {scheduler.end_session_mutation();(input.accept)(false);tell(&delivery,&input.chat,"恢复请求保存失败，未切换会话；请重新发送。")?;}
                            }
                        }
                        Done::SessionChanged {chat,result} => {
                            scheduler.end_session_mutation();
                            let text = match result {
                                Ok(()) => "会话已恢复，下次提问将继续该会话。".into(),
                                Err(error) => format!("恢复会话失败：{error}。目标必须属于当前目录且处于空闲状态；不会自动重试，请检查后重新发送。"),
                            };
                            tell(&delivery,&chat,text)?;
                        }
                        Done::Listed {chat,result} => tell(&delivery,&chat,result.unwrap_or_else(|error|format!("读取会话列表失败：{error}")))?,
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
