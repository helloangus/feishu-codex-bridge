//! Approval cards reach the real RPC reply adapter without network or a model.
use bridge_app::sessions::SessionStore;
use bridge_app::{
    events::{AgentEvent, Incoming, TurnOutcome},
    messaging::{DeliveryError, DeliveryFuture, MessageId, Messenger, ResourceKind},
    ports::{BackendError, TurnRef},
    runtime::{self, Input},
};
use bridge_core::view::{Button, Panel};
use bridge_local::{async_state::AsyncState, state::JsonStore};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    error::Error,
    fs::File,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;
struct Messages {
    text: mpsc::Sender<String>,
    cards: mpsc::Sender<(String, Panel)>,
    updates: mpsc::Sender<(String, Panel)>,
    fail: Arc<AtomicBool>,
    sequence: AtomicU64,
}
impl Messenger for Messages {
    fn send_text(&self, _: String, text: String) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            self.text
                .send(text)
                .await
                .map_err(|_| DeliveryError::Transport)
        })
    }
    fn send_panel(&self, _: String, panel: Panel) -> DeliveryFuture<'_, MessageId> {
        Box::pin(async move {
            if self.fail.load(Ordering::Relaxed) {
                return Err(DeliveryError::Transport);
            }
            let id = format!("card-{}", self.sequence.fetch_add(1, Ordering::Relaxed));
            self.cards
                .send((id.clone(), panel))
                .await
                .map_err(|_| DeliveryError::Transport)?;
            Ok(MessageId(id))
        })
    }
    fn update_panel(&self, id: MessageId, panel: Panel) -> DeliveryFuture<'_, ()> {
        Box::pin(async move {
            self.updates
                .send((id.0, panel))
                .await
                .map_err(|_| DeliveryError::Transport)
        })
    }
    fn upload(&self, _: String, _: String, _: File, _: ResourceKind) -> DeliveryFuture<'_, ()> {
        Box::pin(async { Err(DeliveryError::Transport) })
    }
}

struct Harness {
    store: Arc<AsyncState>,
    turns: mpsc::Receiver<Value>,
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
    input: mpsc::Sender<Input>,
    events: mpsc::Sender<Result<Incoming, BackendError>>,
    text: mpsc::Receiver<String>,
    cards: mpsc::Receiver<(String, Panel)>,
    updates: mpsc::Receiver<(String, Panel)>,
    replies: mpsc::Receiver<Value>,
    fail: Arc<AtomicBool>,
    cancel: CancellationToken,
    worker: tokio::task::JoinHandle<Result<(), String>>,
    remote: tokio::task::JoinHandle<Result<(), String>>,
    connection: bridge_codex::transport::Connection,
    seq: u64,
}
impl Harness {
    async fn new() -> TestResult<Self> {
        Self::with_start(None).await
    }
    async fn with_start(start: Option<oneshot::Receiver<()>>) -> TestResult<Self> {
        Self::configured(start, false).await
    }
    async fn configured(start: Option<oneshot::Receiver<()>>, plan: bool) -> TestResult<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_path_buf();
        let store = Arc::new(
            AsyncState::new(JsonStore::open(&root.join("state"))?)
                .with_pairing(Some("synthetic-pairing-code".into())),
        );
        store
            .set_preference(
                bridge_core::SessionKey::new("owner", &root),
                bridge_app::sessions::PreferenceChange::Plan(plan),
            )
            .await?;
        let (turn_tx, turns) = mpsc::channel(32);
        let cwd = root.clone();
        let (client, remote) = tokio::io::duplex(16384);
        let (read, write) = tokio::io::split(client);
        let connection = bridge_codex::transport::Connection::new(read, write, 71, 16384);
        let (reply_tx, replies) = mpsc::channel(32);
        let (started_tx, started_rx) = oneshot::channel();
        let remote = tokio::spawn(async move {
            let mut started_tx = Some(started_tx);
            let mut start = start;
            let mut threads = 0;
            let (read, mut write) = tokio::io::split(remote);
            let mut lines = BufReader::new(read).lines();
            while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
                let request: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
                let Some(method) = request["method"].as_str() else {
                    reply_tx.send(request).await.map_err(|e| e.to_string())?;
                    continue;
                };
                let result = match method {
                    "model/list" => json!({"data":[{"id":"model","isDefault":true}]}),
                    "thread/start" => {
                        threads += 1;
                        json!({"thread":{"id":if threads==1 {"thread"} else {"fresh"},"cwd":cwd}})
                    }
                    "thread/read" | "thread/resume" => {
                        json!({"thread":{"id":request["params"]["threadId"],"cwd":cwd,"status":{"type":"idle"}}})
                    }
                    "turn/start" => {
                        turn_tx
                            .send(request["params"].clone())
                            .await
                            .map_err(|e| e.to_string())?;
                        json!({"turn":{"id":"turn"}})
                    }
                    "turn/interrupt" => json!({}),
                    _ => return Err(format!("unexpected RPC: {method}")),
                };
                if method == "turn/start" {
                    if let Some(sender) = started_tx.take() {
                        let _ = sender.send(());
                    }
                    if let Some(wait) = start.take() {
                        wait.await.map_err(|_| "start barrier closed")?;
                    }
                }
                let response = json!({"id":request["id"],"result":result});
                write
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        });
        let (input, inputs) = mpsc::channel(32);
        let (events, event_rx) = mpsc::channel(32);
        let (text_tx, text) = mpsc::channel(64);
        let (cards_tx, cards) = mpsc::channel(32);
        let (updates_tx, updates) = mpsc::channel(32);
        let fail = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let worker = tokio::spawn(runtime::run(
            runtime::Settings {
                root: root.clone(),
                directory: root.clone(),
                allowed: BTreeSet::from(["owner".into(), "other".into()]),
                open_access: false,
                sandbox: bridge_app::ports::Sandbox::WorkspaceWrite,
                epoch: 71,
            },
            Arc::new(bridge_codex::backend::CodexBackend::new(
                connection.client.clone(),
            )),
            store.clone(),
            Arc::new(Messages {
                text: text_tx,
                cards: cards_tx,
                updates: updates_tx,
                fail: fail.clone(),
                sequence: AtomicU64::new(1),
            }),
            inputs,
            event_rx,
            cancel.clone(),
        ));
        let mut harness = Self {
            store,
            turns,
            _temp: temp,
            root,
            input,
            events,
            text,
            cards,
            updates,
            replies,
            fail,
            cancel,
            worker,
            remote,
            connection,
            seq: 0,
        };
        harness
            .send("owner", "chat", Some("test task".into()), None)
            .await?;
        harness.until("已开始执行").await?;
        started_rx.await?;
        Ok(harness)
    }
    async fn send(
        &mut self,
        user: &str,
        chat: &str,
        text: Option<String>,
        card: Option<bridge_app::cards::Click>,
    ) -> TestResult {
        self.seq += 1;
        let (ack, wait) = oneshot::channel();
        self.input
            .send(Input {
                attachments: vec![],
                id: format!("input-{}", self.seq),
                user: user.into(),
                chat: chat.into(),
                text,
                card,
                accept: Box::new(move |accepted| {
                    let _ = ack.send(accepted);
                }),
            })
            .await
            .map_err(|_| "input closed")?;
        assert!(wait.await?);
        Ok(())
    }
    async fn until(&mut self, pattern: &str) -> TestResult {
        loop {
            if tokio::time::timeout(Duration::from_secs(3), self.text.recv())
                .await
                .map_err(|_| format!("waiting for text: {pattern}"))?
                .ok_or("text closed")?
                .contains(pattern)
            {
                return Ok(());
            }
        }
    }
    async fn barrier(&mut self) -> TestResult {
        self.send("owner", "chat", Some("/model".into()), None)
            .await?;
        self.until("当前模型").await
    }
    async fn request(&mut self, id: &str, method: &str, extra: Value) -> TestResult {
        let mut params = json!({"threadId":"thread","turnId":"turn","itemId":id,"startedAtMs":1,"command":"echo approval-test","cwd":self.root});
        for (key, value) in extra.as_object().ok_or("extra must be object")? {
            params[key] = value.clone();
        }
        let (request, reply) = bridge_codex::requests::prepare(
            self.connection.client.clone(),
            71,
            bridge_codex::RpcId::String(id.into()),
            method,
            params,
        )
        .await?;
        self.events
            .send(Ok(Incoming::Request { request, reply }))
            .await
            .map_err(|_| "events closed")?;
        Ok(())
    }
    async fn card(&mut self) -> TestResult<(String, Panel)> {
        let card = tokio::time::timeout(Duration::from_secs(3), self.cards.recv())
            .await
            .map_err(|_| "waiting for card")?
            .ok_or("missing card")?;
        self.barrier().await?;
        Ok(card)
    }
    async fn click(&mut self, user: &str, chat: &str, source: &str, button: &Button) -> TestResult {
        let click = bridge_cli::bootstrap::decode_card_click(
            source.into(),
            &bridge_feishu::cards::action_value(&button.action),
        )
        .ok_or("invalid click")?;
        self.send(user, chat, None, Some(click)).await
    }
    async fn decision(&mut self, id: &str, expected: &str) -> TestResult {
        let value = tokio::time::timeout(Duration::from_secs(3), self.replies.recv())
            .await
            .map_err(|_| format!("waiting for decision {id}"))?
            .ok_or("missing decision")?;
        assert_eq!(value["id"], id);
        assert_eq!(value["result"]["decision"], expected);
        Ok(())
    }
    async fn close(mut self) -> TestResult {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), &mut self.worker)
            .await
            .map_err(|_| "closing worker")???;
        self.connection.shutdown().await?;
        tokio::time::timeout(Duration::from_secs(3), &mut self.remote)
            .await
            .map_err(|_| "closing remote")???;
        Ok(())
    }
}

const COMMAND: &str = "item/commandExecution/requestApproval";

#[tokio::test]
async fn pairing_admits_new_user_without_sending_code_to_codex() -> TestResult {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut h = Harness::new().await?;
        let _initial = h.turns.recv().await.ok_or("initial turn missing")?;
        h.send("new-user", "chat", Some("/pair wrong".into()), None)
            .await?;
        h.until("配对未成功").await?;
        h.send(
            "new-user",
            "chat",
            Some("/pair synthetic-pairing-code".into()),
            None,
        )
        .await?;
        h.until("配对成功").await?;
        h.send("new-user", "chat", Some("/model".into()), None)
            .await?;
        h.until("当前模型").await?;
        assert!(h.turns.try_recv().is_err());
        let state = std::fs::read_to_string(h.root.join("state/state.json"))?;
        assert!(state.contains("new-user"));
        assert!(!state.contains("synthetic-pairing-code"));
        h.close().await
    })
    .await?
}

#[tokio::test]
async fn permission_and_network_cards_use_one_time_rpc_decisions() -> TestResult {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut h = Harness::new().await?;
        h.request("overlay", COMMAND, json!({
            "additionalPermissions":{"network":{"enabled":true},"fileSystem":{"read":["/input"],"write":["/output"]}},
            "networkApprovalContext":{"host":"example.test","protocol":"https"},
            "availableDecisions":["accept","acceptForSession","decline"]
        })).await?;
        let (source, card) = h.card().await?;
        for detail in ["/input", "/output", "不限定主机", "example.test", "HTTPS"] {assert!(card.body.contains(detail));}
        assert_eq!(card.buttons.len(), 2);
        h.click("other", "chat", &source, &card.buttons[0]).await?;
        h.until("卡片操作无效").await?;
        assert!(h.replies.try_recv().is_err());
        h.click("owner", "chat", &source, &card.buttons[0]).await?;
        h.decision("overlay", "accept").await?;
        h.until("回传本次同意").await?;
        h.click("owner", "chat", &source, &card.buttons[0]).await?;
        h.until("卡片操作无效").await?;
        assert!(h.replies.try_recv().is_err());

        h.request("network", COMMAND, json!({"command":null,"cwd":null,
            "networkApprovalContext":{"host":"example.test","protocol":"socks5Udp"}
        })).await?;
        let (source, card) = h.card().await?;
        assert_eq!(card.buttons.len(), 2);
        assert!(card.body.contains("SOCKS5 UDP"));
        h.click("owner", "chat", &source, &card.buttons[1]).await?;
        h.decision("network", "decline").await?;
        h.until("回传拒绝").await?;

        for (id, extra) in [
            ("session-only", json!({"networkApprovalContext":{"host":"example.test","protocol":"http"},"availableDecisions":["acceptForSession","decline"]})),
            ("oversize", json!({"additionalPermissions":{"fileSystem":{"read":["x".repeat(4096)]}}})),
            ("unknown", json!({"additionalPermissions":{"fileSystem":{"entries":[{"access":"write","path":{"type":"special","value":{"kind":"unknown","path":"future"}}}]}}}))
        ] {
            h.request(id, COMMAND, extra).await?;
            let (source, card) = h.card().await?;
            assert_eq!(card.buttons.len(), 1);
            h.click("owner", "chat", &source, &card.buttons[0]).await?;
            h.decision(id, "decline").await?;
            h.until("回传拒绝").await?;
        }
        h.close().await
    }).await?
}

impl Harness {
    async fn complete_plan(&mut self, text: String, outcome: TurnOutcome) -> TestResult {
        let turn = TurnRef {
            epoch: 71,
            thread_id: "thread".into(),
            turn_id: "turn".into(),
        };
        self.events
            .send(Ok(Incoming::Notification(AgentEvent::Plan {
                turn: turn.clone(),
                item: "plan".into(),
                text,
            })))
            .await
            .map_err(|_| "closed")?;
        self.events
            .send(Ok(Incoming::Notification(AgentEvent::Finished {
                turn,
                outcome,
            })))
            .await
            .map_err(|_| "closed")?;
        Ok(())
    }
}

#[tokio::test]
async fn plan_choices_execute_once_in_default_mode_or_stay_in_plan() -> TestResult {
    for choice in 0..3 {
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut h = Harness::configured(None, true).await?;
            let initial = h.turns.recv().await.ok_or("initial turn")?;
            assert_eq!(initial["collaborationMode"]["mode"], "plan");
            h.complete_plan(
                "1. Implement exact feature\n2. Verify".into(),
                TurnOutcome::Completed,
            )
            .await?;
            let (source, card) = h.card().await?;
            assert_eq!(card.title, "Plan 已完成，请确认下一步");
            assert_eq!(card.buttons.len(), 3);
            for (user, chat, src) in [
                ("other", "chat", source.as_str()),
                ("owner", "foreign", source.as_str()),
                ("owner", "chat", "wrong"),
            ] {
                h.click(user, chat, src, &card.buttons[choice]).await?;
                h.until("卡片操作无效").await?;
            }
            h.send(
                "owner",
                "chat",
                Some("/plan-action plan-71:1 implement".into()),
                None,
            )
            .await?;
            h.until("计划操作无效").await?;
            h.click("owner", "chat", &source, &card.buttons[choice])
                .await?;
            let key = bridge_core::SessionKey::new("owner", &h.root);
            if choice == 2 {
                h.until("保持 Plan 模式").await?;
                assert!(h.store.preferences(key).await?.plan);
                assert!(h.turns.try_recv().is_err());
            } else {
                h.until("已加入实施队列").await?;
                let turn = h.turns.recv().await.ok_or("implementation turn")?;
                assert_eq!(turn["collaborationMode"]["mode"], "default");
                assert_eq!(
                    turn["threadId"],
                    if choice == 0 { "thread" } else { "fresh" }
                );
                assert!(turn.to_string().contains("Implement exact feature"));
                assert!(!h.store.preferences(key).await?.plan);
            }
            h.click("owner", "chat", &source, &card.buttons[(choice + 1) % 3])
                .await?;
            h.until("卡片操作无效").await?;
            assert!(h.turns.try_recv().is_err());
            h.close().await
        })
        .await??;
    }
    Ok(())
}

#[tokio::test]
async fn failed_oversized_or_non_plan_turns_never_offer_implementation() -> TestResult {
    for (plan, text, outcome) in [
        (true, "plan".into(), TurnOutcome::Interrupted),
        (true, "x".repeat(16001), TurnOutcome::Completed),
        (false, "plan".into(), TurnOutcome::Completed),
    ] {
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut h = Harness::configured(None, plan).await?;
            h.complete_plan(text, outcome).await?;
            h.barrier().await?;
            h.send(
                "owner",
                "chat",
                Some("/plan-action plan-71:1 implement".into()),
                None,
            )
            .await?;
            h.until("计划操作无效").await?;
            assert!(h.cards.try_recv().is_err());
            h.close().await
        })
        .await??;
    }
    Ok(())
}

#[tokio::test]
async fn expired_plan_and_changed_thread_are_rejected_without_execution() -> TestResult {
    for expired in [true, false] {
        tokio::time::timeout(Duration::from_secs(620), async {
            let mut h = Harness::configured(None, true).await?;
            let _ = h.turns.recv().await;
            h.complete_plan("exact plan".into(), TurnOutcome::Completed)
                .await?;
            let (source, card) = h.card().await?;
            if expired {
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(601)).await;
                tokio::time::resume();
            } else {
                h.store
                    .bind(
                        bridge_core::SessionKey::new("owner", &h.root),
                        "foreign".into(),
                    )
                    .await?;
            }
            h.click("owner", "chat", &source, &card.buttons[0]).await?;
            h.until(if expired {
                "卡片操作无效"
            } else {
                "计划选择处理失败"
            })
            .await?;
            assert!(h.turns.try_recv().is_err());
            h.close().await
        })
        .await??;
    }
    Ok(())
}

#[tokio::test]
async fn failed_plan_card_delivery_only_falls_back_to_non_executable_text() -> TestResult {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut h = Harness::configured(None, true).await?;
        let _ = h.turns.recv().await;
        h.fail.store(true, Ordering::Relaxed);
        h.complete_plan("exact plan".into(), TurnOutcome::Completed)
            .await?;
        h.until("计划确认卡片发送失败").await?;
        assert!(h.cards.try_recv().is_err());
        assert!(h.turns.try_recv().is_err());
        h.close().await
    })
    .await?
}

#[tokio::test]
async fn plan_claim_failure_never_changes_mode_or_starts_implementation() -> TestResult {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut h = Harness::configured(None, true).await?;
        let _ = h.turns.recv().await;
        h.complete_plan("exact plan".into(), TurnOutcome::Completed)
            .await?;
        let (source, card) = h.card().await?;
        let journal = h.root.join("state/seen-messages.json");
        std::fs::remove_file(&journal)?;
        std::fs::create_dir(&journal)?;
        h.click("owner", "chat", &source, &card.buttons[1]).await?;
        h.until("计划选择处理失败").await?;
        let key = bridge_core::SessionKey::new("owner", &h.root);
        assert!(h.store.preferences(key.clone()).await?.plan);
        assert_eq!(h.store.thread(key).await?.as_deref(), Some("thread"));
        assert!(h.turns.try_recv().is_err());
        h.close().await
    })
    .await?
}

#[tokio::test]
async fn new_task_or_reset_revokes_all_plan_buttons() -> TestResult {
    for reset in [true, false] {
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut h = Harness::configured(None, true).await?;
            let _ = h.turns.recv().await;
            h.complete_plan("exact plan".into(), TurnOutcome::Completed)
                .await?;
            let (source, card) = h.card().await?;
            h.send(
                "owner",
                "chat",
                Some(
                    if reset {
                        "/new"
                    } else {
                        "discuss another plan"
                    }
                    .into(),
                ),
                None,
            )
            .await?;
            if reset {
                h.until("已切换到新会话").await?;
            } else {
                let _ = h.turns.recv().await.ok_or("next task")?;
            }
            h.click("owner", "chat", &source, &card.buttons[0]).await?;
            h.until("卡片操作无效").await?;
            let (id, updated) = h.updates.recv().await.ok_or("plan update")?;
            assert_eq!(id, source);
            assert!(updated.buttons.is_empty());
            assert!(h.turns.try_recv().is_err());
            h.close().await
        })
        .await??;
    }
    Ok(())
}

#[tokio::test]
async fn early_plan_completion_waits_for_rpc_identity_before_offering_buttons() -> TestResult {
    tokio::time::timeout(Duration::from_secs(20), async {
        let (release, wait) = oneshot::channel();
        let mut h = Harness::configured(Some(wait), true).await?;
        h.complete_plan("early exact plan".into(), TurnOutcome::Completed)
            .await?;
        assert!(h.cards.try_recv().is_err());
        release.send(()).map_err(|_| "release")?;
        let (_, card) = h.card().await?;
        assert_eq!(card.buttons.len(), 3);
        assert!(card.body.contains("early exact plan"));
        h.close().await
    })
    .await?
}

#[tokio::test]
async fn free_answers_bind_owner_question_and_preserve_multiline_without_echo() -> TestResult {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut h = Harness::configured(None, true).await?;
        h.request("free", "item/tool/requestUserInput", json!({"isBlocking":true,"questions":[
            {"id":"one","header":"Other","question":"Describe","isOther":true,"options":[{"label":"default","description":"default"}]},
            {"id":"two","header":"Private","question":"Enter value","isSecret":true},
            {"id":"three","header":"Optional","question":"Anything else?"}
        ]})).await?;
        for index in 0..2 {
            let (source, card) = h.card().await?;
            let button = card.buttons.iter().find(|b| b.label.contains("自行回答")).ok_or("missing free answer")?;
            h.click("owner", "chat", &source, button).await?;
            let prompt = loop {
                let text = h.text.recv().await.ok_or("missing prompt")?;
                if text.contains("/answer ") { break text; }
            };
            let command = prompt.split("/answer ").nth(1).ok_or("missing command")?.split(" <答案>").next().ok_or("missing token")?;
            assert!(command.ends_with(&index.to_string()));
            let answer = format!("/answer {command} /stop 保留原文\n第二行");
            h.send("other", "chat", Some(answer.clone()), None).await?;
            h.until("答案未接收").await?;
            h.send("owner", "chat", Some(format!("/answer {command} {}", "x".repeat(16*1024+1))), None).await?;
            h.until("答案未接收").await?;
            h.send("owner", "other-chat", Some(answer.clone()), None).await?;
            h.until("答案未接收").await?;
            h.send("owner", "chat", Some(answer.clone()), None).await?;
            h.until("答案已记录").await?;
            h.send("owner", "chat", Some(answer), None).await?;
            h.until("答案未接收").await?;
        }
        let (source,card)=h.card().await?;
        let button = card.buttons.iter().find(|b| b.label.contains("自行回答")).ok_or("missing free answer")?;
        h.click("owner","chat",&source,button).await?;
        let prompt = loop {
            let text=h.text.recv().await.ok_or("missing prompt")?;
            if text.contains("/answer ") {break text;}
        };
        let command = prompt.split("/answer ").nth(1).ok_or("missing command")?.split(" <答案>").next().ok_or("missing token")?;
        h.send("owner","chat",Some(format!("/answer {command} no additional information")),None).await?;
        let reply = h.replies.recv().await.ok_or("missing answers")?;
        assert_eq!(reply["result"], json!({"answers":{"one":{"answers":["/stop 保留原文\n第二行"]},"two":{"answers":["/stop 保留原文\n第二行"]},"three":{"answers":["no additional information"]}}}));
        assert!(h.replies.try_recv().is_err());
        h.close().await
    }).await?
}

#[tokio::test]
async fn questions_collect_options_require_answer_without_disclosure() -> TestResult {
    tokio::time::timeout(Duration::from_secs(20),async {
        let mut h=Harness::configured(None, true).await?;
        h.request("questions","item/tool/requestUserInput",json!({"isBlocking":true,"questions":[
            {"id":"one","header":"Language","question":"Choose language","options":[{"label":"Rust","description":"compiled"},{"label":"Python","description":"interpreted"}]},
            {"id":"secret","header":"Private","question":"hidden-sensitive-question","isSecret":true,"options":[{"label":"hidden-sensitive-answer","description":"private"}]}
        ]})).await?;
        let (source,first)=h.card().await?;
        h.click("other","chat",&source,&first.buttons[0]).await?;
        h.until("卡片操作无效").await?;
        assert!(h.replies.try_recv().is_err());
        h.click("owner","chat",&source,&first.buttons[0]).await?;
        let (second_source,second)=h.card().await?;
        assert!(!second.body.contains("hidden-sensitive-answer"));
        assert_eq!(second.buttons.len(),1);
        h.click("owner","chat",&source,&first.buttons[1]).await?;
        h.until("卡片操作无效").await?;
        assert!(h.replies.try_recv().is_err());
        h.click("owner","chat",&second_source,&second.buttons[0]).await?;
        let prompt = loop {
            let text = h.text.recv().await.ok_or("missing answer prompt")?;
            if text.contains("/answer ") { break text; }
        };
        let command = prompt.split("/answer ").nth(1).ok_or("missing answer command")?.split(" <答案>").next().ok_or("missing answer token")?;
        h.send("owner","chat",Some(format!("/answer {command} keep private")),None).await?;
        let reply=h.replies.recv().await.ok_or("missing answer")?;
        assert_eq!(reply["id"],"questions");
        assert_eq!(reply["result"],json!({"answers":{"one":{"answers":["Rust"]},"secret":{"answers":["keep private"]}}}));
        h.until("回传答案").await?;
        h.close().await
    }).await?
}

#[tokio::test]
async fn question_expiry_stops_without_returning_partial_answers() -> TestResult {
    tokio::time::timeout(Duration::from_secs(620),async {
        let mut h=Harness::configured(None, true).await?;
        h.request("partial","item/tool/requestUserInput",json!({"isBlocking":true,"questions":[
            {"id":"one","header":"First","question":"Choose","options":[{"label":"yes","description":"continue"}]},
            {"id":"two","header":"Second","question":"Choose again","options":[{"label":"no","description":"stop"}]}
        ]})).await?;
        let (source,first)=h.card().await?;
        h.click("owner","chat",&source,&first.buttons[0]).await?;
        let _=h.card().await?;
        tokio::time::pause();tokio::time::advance(Duration::from_secs(601)).await;tokio::time::resume();
        assert!(tokio::time::timeout(Duration::from_secs(10), &mut h.worker).await??.is_err());
        assert!(h.replies.try_recv().is_err());
        h.worker=tokio::spawn(async {Ok(())});
        h.close().await
    }).await?
}

#[tokio::test]
async fn question_failure_paths_never_submit_empty_answers() -> TestResult {
    for scenario in ["execute", "delivery", "oversized", "shutdown"] {
        tokio::time::timeout(Duration::from_secs(20), async {
            let mut h=Harness::configured(None, scenario!="execute").await?;
            h.fail.store(scenario=="delivery", Ordering::Relaxed);
            h.request("required", "item/tool/requestUserInput", json!({"isBlocking":true,"questions":[
                {"id":"q","header":"Choice","question":if scenario=="oversized" {"x".repeat(16001)} else {"Choose".into()},"options":[{"label":"yes","description":"confirm"}]}
            ]})).await?;
            if scenario=="shutdown" {
                let _=h.card().await?;
                h.cancel.cancel();
            }
            let result=tokio::time::timeout(Duration::from_secs(10), &mut h.worker).await??;
            assert_eq!(result.is_ok(),scenario=="shutdown");
            assert!(h.replies.try_recv().is_err());
            if scenario!="shutdown" {assert!(h.cards.try_recv().is_err());}
            h.worker=tokio::spawn(async {Ok(())});
            h.close().await
        }).await??;
    }
    Ok(())
}

#[tokio::test]
async fn option_text_in_plan_output_never_opens_question_card() -> TestResult {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut h = Harness::configured(None, true).await?;
        let turn = TurnRef {
            epoch: 71,
            thread_id: "thread".into(),
            turn_id: "turn".into(),
        };
        h.events
            .send(Ok(Incoming::Notification(AgentEvent::Output {
                turn: turn.clone(),
                item: "text".into(),
                delta: "请选择：A. 咖啡 B. 茶 C. 果汁".into(),
            })))
            .await?;
        h.events
            .send(Ok(Incoming::Notification(AgentEvent::Finished {
                turn,
                outcome: TurnOutcome::Completed,
            })))
            .await?;
        h.until("咖啡").await?;
        assert!(h.cards.try_recv().is_err());
        assert!(h.replies.try_recv().is_err());
        h.close().await
    })
    .await?
}

#[tokio::test]
async fn file_approval_uses_exact_item_details_and_rejects_session_grants() -> TestResult {
    tokio::time::timeout(Duration::from_secs(20),async {
        let mut h=Harness::new().await?;
        for (id,grant,expected) in [("edit",false,"accept"),("grant",true,"decline"),("missing",false,"decline")] {
            let event=bridge_codex::events::notification(71,"item/started",json!({"threadId":"thread","turnId":"turn","item":{"id":if id=="missing" {"different-item"} else {id},"type":"fileChange","changes":[{"path":"/workspace/test.txt","kind":{"type":"update","movePath":"/workspace/renamed.txt"},"diff":"@@ -1 +1 @@\n-old\n+new"}]}}))?.ok_or("file event missing")?;
            h.events.send(Ok(Incoming::Notification(event))).await.map_err(|_|"events closed")?;
            h.request(id,"item/fileChange/requestApproval",if grant {json!({"grantRoot":"/workspace"})} else {json!({})}).await?;
            let (source,card)=h.card().await?;
            if id!="missing" {
                assert!(card.body.contains("/workspace/renamed.txt"));
                assert!(card.body.contains("-old\n+new"));
            }
            assert_eq!(card.buttons.len(),if expected=="accept" {2} else {1});
            if grant {assert!(card.body.contains("整个会话"));assert!(!card.body.contains("仅本次请求有效"));}
            h.click("owner","chat",&source,&card.buttons[0]).await?;
            h.decision(id,expected).await?;
        }
        h.close().await
    }).await?
}

#[tokio::test]
async fn changed_file_snapshot_revokes_already_visible_approval() -> TestResult {
    tokio::time::timeout(Duration::from_secs(20),async {
        let mut h=Harness::new().await?;
        let event=bridge_codex::events::notification(71,"item/started",json!({"threadId":"thread","turnId":"turn","item":{"id":"edit","type":"fileChange","changes":[{"path":"/test","kind":{"type":"add"},"diff":"+safe"}]}}))?.ok_or("missing event")?;
        h.events.send(Ok(Incoming::Notification(event.clone()))).await.map_err(|_|"events closed")?;
        h.request("edit","item/fileChange/requestApproval",json!({})).await?;
        let (source,card)=h.card().await?;
        assert_eq!(card.buttons.len(),2);
        h.events.send(Ok(Incoming::Notification(event))).await.map_err(|_|"events closed")?;
        h.decision("edit","decline").await?;
        h.click("owner","chat",&source,&card.buttons[0]).await?;
        h.until("卡片操作无效").await?;
        assert!(h.replies.try_recv().is_err());
        h.close().await
    }).await?
}

#[tokio::test]
async fn request_before_start_response_waits_for_matching_identity() -> TestResult {
    tokio::time::timeout(Duration::from_secs(20), async {
        let (release, wait) = oneshot::channel();
        let mut h = Harness::with_start(Some(wait)).await?;
        h.request("early", COMMAND, json!({})).await?;
        // Give the actor an event after the request, then observe its output.
        let (ack, received) = oneshot::channel();
        let request = bridge_codex::requests::decode(
            71,
            COMMAND,
            json!({"threadId":"unrelated","turnId":"turn","itemId":"barrier","startedAtMs":1}),
        )?;
        h.events
            .send(Ok(Incoming::Request {
                request,
                reply: Box::new(BarrierReply(ack)),
            }))
            .await
            .map_err(|_| "events closed")?;
        received.await?;
        assert!(h.cards.try_recv().is_err());
        release.send(()).map_err(|_| "release closed")?;
        let (source, card) = h.card().await?;
        h.click("owner", "chat", &source, &card.buttons[0]).await?;
        h.decision("early", "accept").await?;
        h.close().await
    })
    .await?
}

struct FailedReply;
struct BarrierReply(oneshot::Sender<()>);
impl bridge_app::requests::ReplyHandle for BarrierReply {
    fn reply(
        self: Box<Self>,
        response: bridge_app::requests::AgentReply,
    ) -> bridge_app::ports::BackendFuture<'static, ()> {
        Box::pin(async move {
            assert!(matches!(
                response,
                bridge_app::requests::AgentReply::Approve(false)
            ));
            let _ = self.0.send(());
            Ok(())
        })
    }
}
impl bridge_app::requests::ReplyHandle for FailedReply {
    fn reply(
        self: Box<Self>,
        _: bridge_app::requests::AgentReply,
    ) -> bridge_app::ports::BackendFuture<'static, ()> {
        Box::pin(async { Err(BackendError::Uncertain) })
    }
}

#[tokio::test]
async fn uncertain_reply_stops_without_reenabling_approval() -> TestResult {
    tokio::time::timeout(Duration::from_secs(20),async {
        let mut h=Harness::new().await?;
        let request=bridge_codex::requests::decode(71,COMMAND,json!({"threadId":"thread","turnId":"turn","itemId":"uncertain","startedAtMs":1,"command":"echo test","cwd":h.root}))?;
        h.events.send(Ok(Incoming::Request {request,reply:Box::new(FailedReply)})).await.map_err(|_|"events closed")?;
        let (source,card)=h.card().await?;
        h.click("owner","chat",&source,&card.buttons[0]).await?;
        h.until("审批回传结果不确定").await?;
        assert!((&mut h.worker).await?.is_err());
        assert!(h.replies.try_recv().is_err());
        h.connection.shutdown().await?;
        (&mut h.remote).await??;
        Ok(())
    }).await?
}

#[tokio::test]
async fn approval_is_owner_source_bound_one_use_and_replies_over_rpc() -> TestResult {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut h = Harness::new().await?;
        h.request("allow", COMMAND, json!({})).await?;
        let (source, card) = h.card().await?;
        assert_eq!(card.buttons.len(), 2);
        for (user, chat, source) in [
            ("other", "chat", source.as_str()),
            ("owner", "other-chat", source.as_str()),
            ("owner", "chat", "wrong-source"),
        ] {
            h.click(user, chat, source, &card.buttons[0]).await?;
            h.until("卡片操作无效").await?;
        }
        h.send("owner", "chat", Some("/approve approval-71-1".into()), None)
            .await?;
        h.until("审批无效").await?;
        assert!(h.replies.try_recv().is_err());
        h.click("owner", "chat", &source, &card.buttons[0]).await?;
        h.decision("allow", "accept").await?;
        h.until("回传本次同意").await?;
        h.click("owner", "chat", &source, &card.buttons[1]).await?;
        h.until("卡片操作无效").await?;
        assert!(h.replies.try_recv().is_err());
        let (id, updated) = h.updates.recv().await.ok_or("missing terminal card")?;
        assert_eq!(id, source);
        assert!(updated.buttons.is_empty());
        h.request("deny", COMMAND, json!({})).await?;
        let (source, card) = h.card().await?;
        h.click("owner", "chat", &source, &card.buttons[1]).await?;
        h.decision("deny", "decline").await?;
        h.close().await
    })
    .await?
}

#[tokio::test]
async fn failed_delivery_and_unrelated_request_are_declined() -> TestResult {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut h = Harness::new().await?;
        h.fail.store(true, Ordering::Relaxed);
        h.request("failure", COMMAND, json!({})).await?;
        h.decision("failure", "decline").await?;
        assert!(h.cards.try_recv().is_err());
        h.request("foreign", COMMAND, json!({"threadId":"foreign"}))
            .await?;
        h.decision("foreign", "decline").await?;
        h.fail.store(false, Ordering::Relaxed);
        h.request("file", "item/fileChange/requestApproval", json!({}))
            .await?;
        let (source, card) = h.card().await?;
        assert_eq!(card.buttons.len(), 1);
        assert_eq!(card.buttons[0].label, "拒绝");
        h.click("owner", "chat", &source, &card.buttons[0]).await?;
        h.decision("file", "decline").await?;
        h.close().await
    })
    .await?
}

#[tokio::test]
async fn expiry_stop_completion_and_shutdown_deny_pending_approval() -> TestResult {
    for mode in ["expiry", "stop", "complete", "shutdown"] {
        tokio::time::timeout(
            Duration::from_secs(if mode == "expiry" { 620 } else { 20 }),
            async {
                let mut h = Harness::new().await?;
                h.request(mode, COMMAND, json!({})).await?;
                let (source, card) = h.card().await?;
                match mode {
                    "expiry" => {
                        tokio::time::pause();
                        tokio::time::advance(Duration::from_secs(601)).await;
                        tokio::time::resume();
                    }
                    "stop" => {
                        h.send("owner", "chat", Some("/stop".into()), None).await?;
                    }
                    "complete" => {
                        h.events
                            .send(Ok(Incoming::Notification(AgentEvent::Finished {
                                turn: TurnRef {
                                    epoch: 71,
                                    thread_id: "thread".into(),
                                    turn_id: "turn".into(),
                                },
                                outcome: TurnOutcome::Completed,
                            })))
                            .await
                            .map_err(|_| "events closed")?;
                    }
                    _ => h.cancel.cancel(),
                }
                h.decision(mode, "decline").await?;
                if mode != "shutdown" {
                    h.click("owner", "chat", &source, &card.buttons[0]).await?;
                    h.until("卡片操作无效").await?;
                    assert!(h.replies.try_recv().is_err());
                }
                h.close().await
            },
        )
        .await??;
    }
    Ok(())
}
