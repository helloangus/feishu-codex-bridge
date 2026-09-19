# bridge-app 运行时

`crates/bridge-app/src/runtime/` 是组装后的应用核心：一个串行 select 循环拥有全部可变状态，后台作业通过类型化的 `Done` 事件回报结果。共 13 个源文件、约 4700 行。端口与用例函数见 [bridge-app](bridge-app.md)；端到端时序见 [运行时序图](../runtime-flows.md)。

| 文件 | 职责 |
|---|---|
| `mod.rs` | `run()` 入口、sender 投递任务、select 主循环、关机路径、`RuntimeError`、`Input` |
| `state.rs` | `Runtime` 结构与六个状态组件、`Done` 事件族、`maintain()` 簿记 |
| `input.rs` | 输入处理主分派（命令路由、卡片点击、准入、配对） |
| `ack.rs` | 一次性回执 `Ack`（settle 恰好一次、Drop 即 NAK） |
| `flow.rs` | 因果流共享助手：`tell`、`can_spawn`、`send_panel`、`spawn_reply`、`spawn_interrupt`、`event` 折叠、`finish` |
| `protocol.rs` | Codex 协议事件/请求 → 运行时效果 |
| `jobs/mod.rs` | `Done` → 处理函数路由 |
| `jobs/task.rs` | 任务域：plan_action / admission / prepared / started |
| `jobs/session.rs` | 会话域（807 行）：目录、压缩、偏好、线程、/new 全部 durable claim 链 |
| `jobs/card.rs` | 卡片域：审批卡发送/回传、面板刷新、列表投递 |
| `timers.rs` | 1 秒 tick：进度预览、确认过期、任务超时 |
| `tokens.rs` | 令牌铸造（唯一格式化点） |
| `limits.rs` | 全部运行时限额常量 |

## run() 与拓扑

```rust
pub async fn run(
    settings: Settings,                    // root/directory/allowed/open_access/sandbox/epoch
    diagnostics: Diagnostics,
    backend: Arc<dyn AgentBackend>,
    store: Arc<dyn Store>,                 // SessionStore + DurableJournal + DirectoryStore
    messenger: Arc<dyn Messenger>,
    task_files: Arc<dyn TaskFiles>,
    inputs: mpsc::Receiver<Input>,         // input (64)
    events: mpsc::Receiver<Result<Incoming, BackendError>>,  // event (256)
    cancel: CancellationToken,
) -> Result<(), RuntimeError>
```

`RuntimeError` 八个变体，文案可直接给用户/日志：`Connection` / `Capacity` / `Interaction` / `Maintenance` / `Backend` / `Storage`（Display 固定「状态保存或读取失败」）/ `Directory` / `Internal`。

开跑前：`store.validate_directory`（失败 →「初始目录无效或越出工作区」）→ `store.directory_preferences()` 灌入每用户目录。run 内部只 spawn 一个长任务——**sender 投递任务**（消费 delivery(128)）：`Text` 45 秒超时发文本；`Answer` 在 `rich_output()` 时走 `presentation.answer`（180 秒）；`Progress` 走 `presentation.progress` 后复位共享 busy 原子量。

select 主循环每轮先 `state.maintain(&mut jobs)`，再以 biased 优先级监听六源：

1. `cancel.cancelled()` → 进入关机路径
2. sender 退出 → `Connection("发送器意外退出")`
3. `inputs.recv()` → `state.handle_input`（None → `Connection("飞书连接已关闭")`）
4. `jobs.join_next()` → `state.handle_done`（panic → `Internal("后台任务异常退出")`）
5. `events.recv()` → `state.handle_protocol`（错误 → `Backend("Codex 协议或连接异常")`）
6. `tick.tick()`（1 秒）→ `state.handle_tick`

任何错误路径都停机返回——重启职责属于监督层，运行时绝不自动重试。

## 六个状态组件（state.rs）

| 组件 | 字段 | 不变量 |
|---|---|---|
| `Admission` | `allowed`、`pairing_window`、`pairing_attempts`、`seen_commands`（≤1000，满即整体 clear） | 每个输入恰好结算一次 Ack；丢弃即拒绝 |
| `TaskTrack` | `scheduler`（64）、`active: Option<Active>`、`next_task`、`resources: BTreeMap<TaskId, Vec<Attachment>>`、`files: FileDelivery` | 谁取变更门，谁在终态事件释放 |
| `CardBook` | `actions`、`views`、`generations`、`refreshes`（≤128）、`updating_panel`（单飞）、`next_panel`、`plan_offer` | 所有点击走 `resolve_click` 唯一入口 |
| `Approvals` | `interactions`（≤32）、`file_changes`（key=(epoch,thread,turn,item)，≤32）、`next_token` | 未完成问答不提交；回传不确定即停机 |
| `SessionBook` | `directories`、`confirmations`（≤100）、`next_confirmation`、`archived_threads`（≤128） | 偏好在内存选择变更前持久化 |
| `Progress` | `busy: Arc<AtomicBool>`（与 sender 共享）、`last` | 进度失败不重试、不吞最终回复 |

`Active`（唯一活动执行）：`kind: ActiveKind::{Task, Compact{acknowledged, terminal, thread}}`、`spec`、`gate: Option<Execution>`、`turn`、`stopping`、`output`（32 KiB 截断）、`plan: Option<(String, bool)>`（16 KB）、`truncated`、`started`。`FileDelivery` 状态机：`Idle → Holding(TaskSpec) → Delivering → Idle`，保证同一时刻只有一轮成果在整理。

`maintain()`（每个事件之间的簿记，顺序执行）：归档积压同步（`begin_invalidation` 门 → `clear_thread`，同时清 plan offer）→ plan offer 过期清理 → 任务附件资源仅保留队列中仍存在的 → FileChange 缓冲仅保留 live turn → `expire_stale_approvals`（过期审批拒绝、未完成问答 → 停机）→ `deliver_approval_cards`（补发未投递的审批/问答卡）→ `refresh_panels`（刷新队列优先，否则 `views.next_update` 裁剪更新）→ `deliver_files`（空闲且 Holding 时投递成果，180 秒）→ `deliver_plan_offer` → `start_next_task`。

## 输入处理（input.rs + ack.rs）

`Input{attachments, card: Option<Click>, id, user, chat, text: Option<String>, ack}`。`Ack` 包装 oneshot：`settle` 首次胜出、后续 no-op；**Drop 未 settle 即向 transport 发 false（NAK）**——后台作业被 abort 绝不让飞书无限等待。

`handle_input` 的分派顺序即优先级：

| # | 分支 | 要点 |
|---|---|---|
| 1 | 空字段校验 | 失败 `ack(false)` |
| 2 | `/pair`（无卡） | `handle_pairing`：60 秒窗口 10 次（超限静默丢弃）；码 ≤256 字节；成功加入 `allowed` |
| 3 | 白名单检查 | open_access 跳过；拒绝文案「当前飞书用户尚未授权…」+ `ack(true)` |
| 4–6 | 目录解析、附件数（>10 拒绝）、附件+卡片冲突 | 附件文本默认「请查看附件并处理用户请求。」 |
| 7 | 卡片点击 | `resolve_click` 失败 →「卡片操作无效、已使用或已过期」；List 卡点击失效整卡记刷新；`input.id = "card:{token}"`、`input.text = 命令串` 后走普通路由 |
| 8 | `/plan-action` 前缀 | `handle_plan_action`（Plan 三选一） |
| 9 | 成果整理中（Delivering） | 会话/目录/设置类命令被拒「正在整理本轮成果…」（白名单 `/status /help /stop /model /models /resume /archived /plan /cd` 可用） |
| 10 | `/answer` `/answer-skip` `/choice` 前缀 | `handle_answer` / `handle_choice` |
| 11–12 | 非查询命令清 plan offer；`Command::Approve` | `handle_approve` |
| 13–17 | `/help`（去重后发面板）、`/cd-confirm`、`/cd`、目录越界守卫、`/compact` | 会话/目录/压缩命令全部先 `begin_session_mutation` |
| 18–19 | `/models /model /plan`、`/resume /archive /unarchive /archived` | 查询型发 `Listed` 卡片作业；设置/操作型走 `PreferenceClaim`/`ThreadClaim` |
| 20 | `/new` | `begin_session_mutation` → **先 claim 再 clear**（防重复命令抹掉新绑定） |
| 21 | `/status`、`/stop`、其余 | `handle_simple_command`；未知命令兜底文案 |
| 22–23 | 文本 >32 KiB 拒绝；`admit_task` | `next_task += 1` → `TaskSpec{id: {epoch}:{n}}` → `scheduler.reserve` → 成功后 spawn `store.claim` |

**忙时拒绝文案**（全部真实存在于代码）：变更门被占 →「有任务执行中、排队或目录正在更新；请等待完成或先 /stop，再切换目录。」等分场景变体；通用「系统繁忙，请稍后重试。」；任务准入「任务队列繁忙，请稍后重新发送。」；列表「系统繁忙，请稍后重新发送列表命令。」；计划「系统繁忙，请稍后重新生成计划。」。

## 因果流（flow.rs + Done 族）

`Done` 四大域（state.rs）：

- `SessionDone`（15 变体）：`ArchiveSynced`、`DirectoryProposed`、`CreationClaim`、`DirectoryCreated`、`DirectoryClaim`、`DirectoryChanged`、`DirectoryListed`、`CompactClaim`、`CompactPrepared`、`CompactSubmitted`、`PreferenceClaim`、`PreferenceChanged`、`ThreadClaim`、`SessionChanged`、`Reset`
- `TaskDone`（5）：`PlanAction`、`Admission`、`Prepared`、`Started`、`Control`
- `CardDone`（5）：`ApprovalSent`、`ApprovalReplied`、`PanelUpdated`、`PanelSent`、`Listed`
- `DeliveryDone`（1）：`FilesDelivered`（内联复位 FileDelivery::Idle）

共享助手：`tell`（文本入 delivery，满 → `Capacity`）；`can_spawn(control)`（用户作业要求 `jobs.len() + 16 < 128`，控制作业可用满）；`spawn_reply`（控制容量内回传审批/答案）；`send_panel`（deadline = 600 秒；plan-action 卡失败有专门兜底文案；`/stop` 与 `/plan-action` 按钮绑定 stop_snapshot）；`spawn_interrupt`（`backend.interrupt` → `TaskDone::Control`）；`flow::event`（协议事件折叠：Plan 16 KB、Output 32 KiB、Finished 映射 Outcome 或 park 到 Compact.terminal；Plan 模式完成且未截断时构造 plan offer）；`flow::finish`（任务域：compact 则 `end_session_mutation` + 文本；普通任务 `scheduler.finish` + `Answer` 投递；失败原因取预览 1000 字符）。

## jobs/task.rs：任务域

- `plan_action`：**入口先 `end_session_mutation()`**；implement → 关闭 Plan 模式并把计划文本作为新任务 `reserve + commit_admission` 直接入实施队列；stay → 保持 Plan 模式；重复处理 →「此计划选择已处理，不会重复执行。」
- `admission`：claim 成功 → `commit_admission` + `ack(true)` +「请求已接收。」；失败 → `abort_admission` + `ack(false)` +「接收状态保存失败，未启动任务。」
- `prepared`：忙 → `NotStarted("系统繁忙，任务未启动…")`；stopping → `NotStarted("准备阶段已停止…")`；成功 → 建 `Execution::starting(epoch, thread, 64)` 门 + spawn `backend.start_turn`；失败 → `PrepareFailed`。
- `started`：`gate.bind(turn)` 失败 → `Interaction("执行身份不匹配")`；stopping 补 `spawn_interrupt`；回放 early 事件逐个过 `flow::event`；失败 → `StartUnknown` + **`Err(Backend("Codex 启动失败，已停止运行以避免重复执行"))`**（turn 可能在运行，必须停机）。

## jobs/session.rs：会话域的 durable claim 链

文件头不变量：每条链在接受命令时取 `begin_session_mutation`，其终态事件在此**恰好一次** `end_session_mutation`；durable store 不认识的结果拒绝输入且绝不重放副作用。

- **目录链**：`/cd`（DirectoryClaim → propose → 已存在 `change_directory`；不存在铸造 `cd-{epoch}-{n}` 确认令牌 + 卡片）→ `/cd-confirm`（CreationClaim → `create_directory` 重算提案须一致 → safeio 建目录）→ `directory_finished` 共享尾部：`end_session_mutation` + `confirmations.invalidate` + `actions.invalidate` + **世代 +1**（全部旧按钮失效）+ 更新内存目录。保存失败保留旧选择。
- **Compact 链**：`compact_claim`（建 `Active{Compact{acknowledged:false}}`）→ `compact_prepared`（**先记录期望 thread 再建门**——必须在请求可能发出通知前记录）→ `compact_submitted`（acknowledged = true；若终态已 park 立即 finish；`Rejected` 且无 turn → `NotStarted("压缩请求被拒绝…")`；其他错误 → 「压缩启动结果不确定…」+ `Err(Maintenance)`）→ 终态经 `flow::event`：未 ACK 时 park 到 `kind.terminal`（门保持关闭、无消息），已 ACK 则 `finish(Outcome::Compact(..))` 并释放门。
- **偏好链**：`/model`、`/plan on|off` → `PreferenceClaim` → `change_preference`（模型必须在 models 列表）→ `preference_changed`。
- **线程链**：`/resume /archive /unarchive` → `ThreadClaim` → `change_thread` → `session_changed`；`StartError::Reconcile` → 文本 + `Err(Maintenance("归档结果不确定，已停止运行…"))`。
- **/new**：`Reset`（claim 先于 clear）；失败 →「新建会话失败，状态结果未确认；请重新发送一条 /new，不会自动重试。」

## jobs/card.rs：卡片域

- `approval_sent`：问答卡发 `QuestionSent` 诊断；成功 → `views.insert`；对 pending 复核（未过期、active 未停、turn 匹配）→ 记 `source` 并登记按钮命令；**注册失败的安全默认**：问答卡失败 → 停机不提交空答案；审批卡失败 → 回传拒绝。
- `approval_replied`：成功文案三分（答案 / 同意 / 拒绝）；`Uncertain` →「回传结果不确定，已停止连接，不会自动重试。」+ `Err(Interaction)`。
- `panel_sent`：刷新路径复位 `updating_panel`；成功回填按钮 source 并按当前世代过滤。
- `listed` + `deliver_list`：**面板号只在 `deliver_list` 铸造**（`++next_panel`）；List 卡重发入 `refreshes` 队列（≤128，满 →「卡片刷新队列已满」）；全新发送走 `can_spawn` + `send_panel`。

## 协议层（protocol.rs）

`handle_protocol` 对 `Incoming::Request`（审批/问答）：FileChange 审批先回填缓存的 diff（key=(epoch,thread,turn,item)）；问答三道门（Plan 模式存活 turn / `question_supported` 可展示 / 1–32 题）；可注册时 `interactions.insert`（token = `tokens::approval(epoch, ++next_token)`、deadline 600 秒）后返回——**卡片投递延迟到 maintain 统一补发**；不可注册 →「审批请求重复或待处理数量已满，已拒绝该请求。」并立即回传拒绝；问答无效 → `Err(Interaction)`（不提交空答案）。拒绝回传受 `BACKEND_REPLY_TIMEOUT(10s)` 约束，超时按 Uncertain 处理。

`handle_notification`：`Archived` → 积压（≤128，超出停机）待 maintain 同步；`FileChanges` → 仅 live turn，重复 item 快照视为歧义（撤销待审批并清缓存）；`Started` → 仅 Compact 且 thread 匹配时绑定 turn、重放 early、stopping 补 interrupt；其余交给 `active.gate.event()`（early 溢出 → `Backend("Codex 提前事件过多")`）。

## timers.rs（每秒 tick）

1. **进度预览**：`rich_output()` 且距上次 ≥3 秒；delivery 超额容量 >8 且 busy CAS 成功；预览取 plan 或 output 前 1000 字符，格式「（正在停止|正在执行）· 已用 N 秒」；空预览显示「等待 Codex 输出…」。
2. **目录确认过期**：逐个通知「目录创建确认已过期：{target}；未自动创建」。
3. **任务超时**：超过 1 小时 → `Err(Maintenance("任务超过一小时上限，停止本次运行"))`。

审批/问答 600 秒过期不在 tick 里，而在每个事件间的 `maintain::expire_stale_approvals`。

## tokens.rs：令牌铸造（唯一格式化点）

| 函数 | 格式 | 用途 |
|---|---|---|
| `panel_prefix(epoch, seq)` | `panel-{epoch}-{seq}` | 每张卡的前缀；按钮为 `{prefix}-{index}` |
| `confirmation(epoch, seq)` | `cd-{epoch}-{seq}` | 目录创建确认 |
| `approval(epoch, seq)` | `approval-{epoch}-{seq}` | 审批/问答条目 |
| `plan(task)` | `plan-{task}`（如 `plan-1:1`） | plan offer，每任务一个 |
| `click_receipt(token)` | `card:{token}` | 卡片点击的持久化输入 id |

模块注释：所有令牌绑定**进程 epoch + 单调序号（或 task id）**，旧进程/旧任务的令牌永远不可重放；本模块之外不得格式化令牌。

## limits.rs 全表

| 常量 | 值 | 常量 | 值 |
|---|---|---|---|
| `BACKGROUND_JOBS` | 128 | `TICK` | 1 秒 |
| `CONTROL_JOB_RESERVE` | 16 | `PROGRESS_INTERVAL` | 3 秒 |
| `DELIVERY_QUEUE` | 128 | `TASK_TIMEOUT` | 3600 秒 |
| `DELIVERY_PROGRESS_RESERVE` | 8 | `INTERACTION_TIMEOUT` | 600 秒 |
| `INTERACTIONS` | 32 | `PAIRING_WINDOW` | 60 秒 |
| `QUESTIONS_PER_REQUEST` | 32 | `BACKEND_REPLY_TIMEOUT` | 10 秒 |
| `FILE_CHANGE_ITEMS` | 32 | `MESSAGE_TIMEOUT` | 45 秒 |
| `ARCHIVED_THREADS` | 128 | `ANSWER_DELIVERY_TIMEOUT` | 180 秒 |
| `PANEL_REFRESHES` | 128 | `FILE_PREPARE_TIMEOUT` | 120 秒 |
| `EARLY_PROTOCOL_EVENTS` | 64 | `FILE_BIND_TIMEOUT` | 60 秒 |
| `SCHEDULED_TASKS` | 64 | `FILE_FINISH_TIMEOUT` | 180 秒 |
| `SEEN_COMMANDS` | 1000 | `ATTACHMENT_DOWNLOAD_TIMEOUT` | 45 秒 |
| `ATTACHMENTS_PER_MESSAGE` | 10 | `DIFF_PANEL_TIMEOUT` | 15 秒 |
| `INPUT_BYTES` | 32 KiB | `UPLOAD_TIMEOUT` | 30 秒 |
| `ANSWER_BYTES` | 16 KiB | `SHUTDOWN_INTERRUPT_TIMEOUT` | 3 秒 |
| `PATH_BYTES` | 4096 | `SHUTDOWN_REPLY_TIMEOUT` | 3 秒 |
| `PAIRING_CODE_BYTES` / `PAIRING_ATTEMPTS` | 256 / 10 | `SHUTDOWN_SENDER_TIMEOUT` | 5 秒 |
| `PLAN_BYTES` / `OUTPUT_BYTES` / `PREVIEW_CHARS` | 16 000 / 32 KiB / 1000 | | |

模块注释：「固定运行时边界。保持内部，使配置与持久状态兼容的同时每个生产者应用同一组限制。」

## 关机路径（cancel 触发后）

1. 退出 select 循环（保留结果）。
2. **中断活动执行**：`active.take()`，有 turn 则 3 秒内 `interrupt`。
3. **BridgeStopped 通知**：`Answer{outcome: BridgeStopped, body: None}` 入 delivery——「任务将不会恢复」。
4. `jobs.abort_all()` + 排空。
5. **必需拒绝**：`interactions.drain()` 逐个 `spawn_reply(pending, false)`——每个未决审批/问答都回传拒绝（仅控制容量满会中断）。
6. 3 秒内排空这些回复作业，再 abort 全部。
7. 关闭 delivery → 5 秒内等 sender 退出（超时 abort）。
8. bootstrap 收尾：join 五任务 → `health.finish` 发布最终相位 → AppServer shutdown 收割 Codex 进程组。

## 运行时单元测试

runtime 模块本身以集成测试覆盖（见 [test-support](test-support.md) 与 bridge-cli/tests），模块内仅有 state/flow 的少量不变量断言；上文引用的文案与行为均可在 `crates/bridge-cli/tests/`（approval_runtime、card_runtime、directory_switch、files_runtime、session_*）与 `crates/test-support/tests/runtime_flows.rs`、`resource_limits.rs` 中找到对应断言。
