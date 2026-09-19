# test-support

离线测试设施（非发布 crate）：两个假 Codex 进程、共享测试装配与录制型假端口。设计约束写在 Cargo.toml 注释里：假进程放在**本包内**，因为 cargo 只对「拥有该二进制的包」的集成测试提供 `CARGO_BIN_EXE_*` 环境变量——测试因此绝不递归调用 cargo。生产 crate 对它只能 dev-dependency（由 `cargo xtask check-boundaries` 强制）。

## 两个假进程

### fake-codex-process：协议层最小假

供 `bridge-codex` 的进程/传输测试（`test-support/tests/process.rs`）验证 spawn/shutdown/类型化边界/进程组收割。

| argv[1] | 行为 |
|---|---|
| `simple` | 逐行读 JSON-RPC：`model/list` 回 `{"data":[{"id":"fake-model","isDefault":true}]}`，其余请求回 `{}` |
| `events` | 收到通知 `initialized` 时主动发服务端请求 `item/fileChange/requestApproval`（id `"approve-string"`）；应答 `decision != "decline"` 即报错；随后发 `turn/completed{interrupted}` |
| `parent <pid_file>` | 再 spawn 一个自身 `tool` 子进程并写 pid 文件，随后进入 simple 循环——验证 shutdown 收割整个进程组 |
| `tool` | sleep 30 秒（模拟长命工具进程） |

### fake-codex-runtime：行为层脚本化假

经 `Actor` 挂到**真实串行运行时**（`bridge_app::runtime::run`）上驱动完整行为。实际 argv 分发（`main()`）只有三个场景；信号文件写入进程 CWD（测试设为每例临时目录）：

| 场景 | 行为 | 信号文件 |
|---|---|---|
| `runtime` | models 返回 `gpt-5.6-luna/selected/chosen`；thread/start、resume 回 thread `thread`；thread/list 按 archived 标志匹配，故意混入 cwd=`/foreign` 的线程（**不得展示**）；archive/unarchive 翻转标志并追加 `archive-actions`；第一次 `turn/start` 主动发 `item/agentMessage/delta("fake answer")` + `item/completed`（plan 文本 `authoritative plan`）+ `turn/completed`——验证最终 Plan 以 item/completed 为准而非 delta；turn=2 时写 `started` 文件（测试轮询它作为「任务已启动」的确凿信号）；`turn/interrupt` 追加 `interrupts.jsonl` | `started`、`archive-actions`、`interrupts.jsonl` |
| `directory` | thread id = `th-` + sha256(cwd) 前 11 hex（resume 校验 threadId 与派生值一致）；`turn/start` 回发 delta，内容 `"{cwd}\|{model}\|{collaborationMode.mode}"` 摘要——测试据此断言目录/模型/Plan 路由；输入为 `hold` 时写 `holding` 且不回 turn/completed（挂起场景） | `holding` |
| `compact <mode>` | `thread/read`、`thread/resume` 要求 threadId == `thread`（每次把 method 追加到 `preparation`）；`thread/compact/start` 追加 `compactions`（`rejected` 回 −32000、`uncertain` 回 result null）；先发 `turn/started`（unrelated 线程 + thread 线程的 compact turn）+ stale `turn/completed`（`early` 再立即发 finished）；`prepare_stop` 把 read 应答扣押到下一次 `model/list` 才放行；`turn/interrupt` 追加 `compact-interrupts` | `preparation`、`compactions`、`compact-interrupts` |

`compact` 的 8 个模式：`wrong_resume`（resume 返回别的 thread id）、`foreign`（thread cwd 在工作区外）、`active`（thread 报告忙碌）、`prepare_stop`（stop 到达于 prepare 与 submit 之间）、`uncertain`（submit 返回不确定传输错误）、`rejected`（submit 被拒）、`early`（事件早于 turn 绑定）、`failed`（prepare 失败）。

> **维护提示**：本文件源码顶部还保留一份历史场景表（`thread/hang/slow/archive/flood/storage` 等），与当前 `main()` 的三场景分发**不同步**；信号文件名仍然准确，`storage` 场景（把 `seen-messages.json` 预替换成目录使持久化失败）确实由测试而非二进制实现。阅读时以 `main()` 与三个场景函数为准；新增场景必须同时更新该表与 `test-support/tests/runtime_flows.rs`。

## src/ 支撑设施

| 文件 | 提供 |
|---|---|
| `actor.rs` | `Actor` harness：「把假 app-server 进程绑定到真实 serial runtime」。`ActorConfig{executable（必须绝对路径，不经 PATH）, args, server_cwd, epoch, root, directory, allowed, open_access, sandbox}`；`Actor::start` = `AppServer::spawn` → `backend()` → 两条 mpsc(16) → forwarder 任务（退出时 `server.shutdown()`）→ `runtime::run` worker（配 `IdleFiles`）；`stop()` 取消并 join 返回双结果 |
| `messenger.rs` | `RecordingMessenger`：录制型 Messenger 假，通道容量 64；`send_panel` 生成顺序 MessageId；`update_panel` **先记录再失败**（失败的可视更新不得回放或撤销命令）；`upload` 默认 `Transport` 失败；失败开关可运行时翻转；断言辅助 `expect_text/expect_chat_text/expect_text_within/expect_card/expect_update` |
| `fakes.rs` | 宏 `fake_codex_process!` / `fake_codex_runtime!`：展开 `env!("CARGO_BIN_EXE_fake-codex-*")`（只能在本包测试内展开） |
| `files.rs` | `IdleFiles`：`TaskFiles` 的 no-op 假（bind/prepare/finish 全部 Ok） |
| `input.rs` | runtime `Input` 的统一构造：`make_input`（oneshot ack → `Ack::from_receipt`）、`submit`（发送并等待准入 bool）、`submit_text`（发送纯文本并断言被准入） |
| `diagnostics.rs` | 录制型 `Sink`：`recorder()` 返回 `(Diagnostics, Arc<Mutex<Vec<String>>>)`，实例独立无串扰 |

## 集成测试

| 文件 | 覆盖 |
|---|---|
| `tests/process.rs`（3 个） | spawn + models + shutdown 收割；服务端请求经类型化边界往返（approve(false) → Interrupted）；shutdown 收割进程组内的 tool 子进程 |
| `tests/runtime_flows.rs`（13 个） | 真实 store + RecordingMessenger + 假 runtime 的完整命令面：未授权拒绝、模型/Plan 设置、busy 保护（任务中 /new /plan /resume /archive 全被拒）、/status、/stop、命令重放防护（重放旧 /resume /plan /model 不得覆盖新状态）、归档往返与重放幂等、`interrupts.jsonl` 契约；9 个 compact 参数化用例（complete/early/stop/failed/rejected/uncertain/empty/foreign/active/wrong_resume/storage/prepare_stop，含「claim 失败不准入」「uncertain 必须停机且 compactions 已写」「运行中守卫」）；后台投递失败经注入 diagnostics 可观察（`CardFailed` + `DeliveryFailed`）；两个 runtime 实例的诊断互相隔离 |
| `tests/directory_routing.rs`（2 个） | 真实 store + directory 假：/cd 浏览、创建确认一次性且须同用户同 chat、每目录独立模型/Plan（`child|chosen|plan` vs `root|gpt-5.6-luna|default`）、任务中 /cd 被拒、队列消息、重启后旧确认失效且设置按目录恢复、目录被换成越界符号链接后报「当前目录已失效或越出工作区」并可 /cd 回根、保存失败缓存不变 |
| `tests/resource_limits.rs`（3 个，不用假进程） | 进程内 `HangingBackend`（start_turn 永远 pending）+ `SlowMessenger`：慢投递下控制命令仍被回答；投递洪峰最终以 `RuntimeError::Capacity` 有界停止而非内存增长；200 条命令洪峰出现「任务队列繁忙」且控制路径存活 |

## 测试总体约束（全仓库）

- 不读取真实 `bridge.toml`、凭据或用户状态；不访问外部服务；不启动长期服务；不改用户工作目录。
- 网络协议测试使用本地 socket 与内存 duplex 流；文件测试使用临时目录。
- `cargo xtask check` 串行跑全部测试（`--test-threads=1`），假进程由 cargo 构建供给。
