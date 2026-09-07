# Rust 重构实施方案与状态

以找回的原始完整计划为设计基线，见 [Rust 完整重构计划](plans/rust-refactor-original.md)，保留详细分析、架构、接口、迁移与验收；本文件继续维护实施状态。

## 已确认的约束

- 最终支持 Linux 与 Termux，全 Rust；迁移时允许 Python SDK 薄适配进程。
- 首版全局串行执行，不同时引入多租户或并行 Codex turn。
- 继续使用 JSON 状态，新部署必须显式配置访问策略。
- 不自动重跑可能已经产生文件副作用的任务。
- 不让新旧程序同时消费同一个生产飞书 App ID。

## 当前交付边界

当前 Rust 二进制提供离线迁移/配置检查，以及 [前台最小运行版](minimal-runtime.md)。最小版接通授权文本、串行执行、纯文本回复、状态和停止，仍不是全功能生产替换版。
`start.sh`、`setup.sh`、`bridge.py` 与 `service.py` 保持原运行入口。
不可将通过离线测试理解为已完成飞书或 Codex 端到端适配。

| 阶段 | 目标 | 当前状态与验收 |
| --- | --- | --- |
| P0 | 固定当前行为、版本、回归和测量基线 | 已固定 Rust 1.85.1、Python SDK 测试版本；既有 47 项回归保留。真实卡片验收及性能基线待补。 |
| P1 | workspace、核心规则、调度、JSON、迁移、快照、CLI、CI | 第一批已实现并由离线测试验证；统一异步 ports、所有既有回归的 Rust 对等覆盖仍需继续。 |
| P2 | 完整 Codex 进程/RPC/通知/审批适配 | 已实现有界双向 RPC、连接代次、子进程初始化/回收、异步 backend、0.153.4 schema 与 turn fixture；已补输出/Plan/完成/归档和基础审批/问答类型映射，完整执行循环与额外权限适配待完成。 |
| P3 | Rust 主业务 + Python SDK 长连接薄进程 | 最小主循环已接入持久化认领、会话准备、文本执行/回复/状态/停止和前台装配；其他命令、交互界面、附件交付及独立机器人验收待完成。 |
| P4 | Rust 飞书 WebSocket | 待实现 EVENT/CARD、分片、重连和代理矩阵；真实 Termux/手机验收后移除 Python。 |
| P5 | 运维、发布、回退与退役 | 待完成 Rust supervisor、健康控制通道、发布产物、72 小时持续运行和生产回退演练。 |

## 源码分析结论

Python 的 `Feishu` 自行用 httpx 调用 REST，lark-oapi 只负责长连接与回调；
因此 REST 与 WebSocket 都需要独立测试，不能仅替换 SDK import。
现有 `_handle_data_frame` 和 `_ws_connect_kwargs` 私有补丁必须收敛进适配层。

`CodexServer` 是唯一 stdout reader，响应由 ID 分发；这一原则必须保持。
目前 `Bridge` 中活动聊天、文本和进度是全局状态，不能通过增加 worker 获得并行。
每命令线程、无界队列、持锁网络 I/O、重启后 RPC ID 复用与固定临时文件写入竞争
分别由有界调度、明确状态所有者、独立 I/O、连接代次和 JSON 单写入者解决。

现有文档中的后台线程数需要后续修正：包含 Plan reaper 实际为五个，此外还有
RPC reader、事件分发和动态命令线程。健康检测不应继续解析 SDK connected 日志。

## 模块与依赖方向

```text
bridge-cli → bridge-app → bridge-core
           → bridge-feishu → bridge-core（后续实现 bridge-app ports）
           → bridge-codex  → bridge-core（后续实现 bridge-app ports）
           → bridge-local → bridge-app / bridge-core
```

- core：命令、任务状态、交互归属；不依赖 serde/网络/异步运行时。
- app：有界 FIFO、持久化认领边界；后续容纳主 actor 与异步 ports。
- local：JSON 存储、迁移、工作区解析、受控文件打开、快照和差异。
- feishu：wire 解码；后续 token、REST、卡片、代理和 WebSocket 全部在此。
- codex：wire envelope；后续 JSONL reader/writer、pending RPC、连接代次和事件映射。
- cli：配置、命令行与装配；后续 supervisor、日志和健康。
- xtask：通过 Cargo metadata 校验真实依赖名称，阻止 core/app 导入 vendor 依赖。

参考 [ripgrep workspace](https://github.com/BurntSushi/ripgrep/blob/master/Cargo.toml)、
[Vector sources/transforms/sinks](https://github.com/vectordotdev/vector/tree/master/src) 和
[Tokio 关闭流程](https://tokio.rs/tokio/topics/shutdown)，不复制大型工程全部抽象。

## 关键行为约束

任务提交固定 user、chat、cwd、model、mode；排队期间不读取后来更改的设置。
全局最多一个活动任务，队列有容量；满载或会话变更时不认领消息。
认领持久化失败不执行；当前去重窗口仍是 1,000 条，不能宣称永久 exactly-once。
终态与交付结果分别表示；迟到进度或上传失败不能重写执行结果。

交互 owner 包括 session、chat、card、task 和连接代次。错误归属校验不能消费 token；
到期与用户操作由同一状态所有者处理。审批最终使用本地随机 token，不能直接用可复用 RPC ID。
审批超时拒绝，问题超时提交已有回答与空缺项，Plan 操作到期失效。

P2 已按 0.153.4 CLI 生成部分 schema，并实现整数/字符串 RPC ID、唯一 stdout reader、
响应与服务端请求分类、有限帧大小、EOF 唤醒、超时清理和旧代次拒绝。
每轮显式发送协作模式及 sandbox/approval；未知审批不可当作允许。
依据：[Codex app-server 官方文档](https://learn.chatgpt.com/docs/app-server)。

P3/P4 的 token 刷新要 single-flight；REST、地址发现和 WS 共用代理决策，
CONNECT/SOCKS 不能靠 reqwest 配置隐式覆盖 WS。所有 HTTP 操作检查业务错误码。
结果未知的消息创建或任务启动不盲目重试；进度可以合并，审批/停止/完成不能丢弃。

## JSON 与文件策略

存储格式带 schema_version。state.json 将会话、模型、目录、Plan 和配对用户一起提交；
seen-messages.json 单独保存有界 journal。持锁单写入者通过私有临时文件、文件 fsync、
rename、父目录 fsync 持久化后才更新内存；不确定失败后拒绝后续写入，要求重新打开。
备份为 state.previous.json；损坏或未来格式拒绝启动，不能自动回退丢失去重记录。

迁移严格不读取或执行 `.env`。旧 Bash `%q` 配置不能按普通 dotenv 随意解析；
目前配置由用户显式写 TOML，自动配置转换尚未实现。会话键暂保留旧 user:cwd 编码
以便完整导出，core 的 SessionKey 仍是独立字段，不从字符串提取用户身份。

扫描使用限制：200 文件、256 KiB 文本、10,000 遍历项、20,000 diff 字符、最多十个成果。
快照明确完整性，扫描未完成不得把未观察文件当成删除。实际打开逐级 no-follow，
最后检查普通文件并使用非阻塞打开以避免 FIFO 替换挂起。快照应在有界 blocking worker 执行。
接收附件、生成图片和上传生命周期尚未接入；P3 必须统一使用相同文件访问策略。

## 测试与工程化路线

当前 CI 跑 fmt、Clippy、Rust 测试/doctest、依赖边界、rustdoc、Python 回归和空白检查。
新增测试覆盖持久化失败、重启去重、双写入者、损坏/未来 schema、交互竞态、队列满载、
错误完成 ID、迁移 dry-run/导出、符号链接、扫描上限和工程文本分类。
Python 交叉回归用真实原代码读取 Rust 导出的状态，全部数据只在 TemporaryDirectory。

后续增加 nextest、cargo-llvm-cov、cargo-deny、actionlint、ShellCheck 及协议 fixture；
对固定 CLI 与候选 CLI 做兼容 job。升级依赖与协议必须独立 PR，保留 Cargo.lock。
测试不得带生产凭据；真实 Feishu/Codex 验收独立执行并记录版本与脱敏截图。

发布阶段构建 Linux x86_64/aarch64 与独立 Android aarch64 产物，固定 NDK，
真实 Termux 验证，不能将 Linux ARM 包视作 Termux 包。
发布附校验和、兼容矩阵、迁移说明、构建来源证明；先 draft 后发布，不自动部署。
当前未新增 LICENSE、不发布 crates.io；仓库许可需由作者另行确定。

## 后续性能验收

记录桥接进程自身 CPU/RSS（排除 Codex）、入口和停止延迟、卡片请求数、
快照耗时、文件描述符和交互清理后的资源回落。固定增量输出、20 MiB 附件、
200 文件快照与重连压力样本。优化目标以 Python 实测为基线，不承诺模型生成加速。

## 2026-09-07 实施检查点

- Rust 39 项、Python 54 项离线测试通过；schema 校验使用 `jsonschema==4.17.3` 与 `pyrsistent==0.20.0`，可在当前 Android Python 环境使用纯 Python 安装。
- fmt、Clippy（禁止警告）、crate 依赖边界、rustdoc（禁止警告）和 diff 空白检查通过。
- 卡片命令往返测试覆盖指定会话恢复、归档、取消归档、模型、目录、Plan 和停止，交互仍须经过业务 owner/token 校验。
- SDK IPC 校验版本、进程代次、连续序号、2 MiB 帧上限及事件来源；任何错误使当前解码器永久失效，要求监督器重新建立连接。读取器仍须在分配整帧前施加同样大小限制。
- SDK 薄进程当前只保证有界内存排队，SDK ACK 早于 Rust 持久化；不能据此承诺可靠接收。生产接入前必须实现父进程持久化确认和超时失败 ACK，并补齐首次连接状态通知。
- 增加 reqwest/rustls 后，Android 全 workspace 检查在 ring C/汇编编译阶段失败：当前 `aarch64-linux-android-gcc` 的 proot 包装报告 `unknown option '-cc1'`。需要独立、可用的 Android NDK 编译器后重新检查和链接，并在真实 Termux 验证；此前不含此依赖的检查不能代表当前版本。
- 下一步先补 Codex 事件/审批契约和飞书 REST 无网络模拟测试，再实现业务主循环；生产替换仍待 P3–P5 验收。

完整基线保存后继续补齐 REST 响应边界回归，Rust 累计 42 项通过；HTTP 业务码、响应大小与 URL 参数隔离均采用内存响应测试，无网络调用。该覆盖不代表已完成 token 刷新、附件或完整 HTTP 请求契约验收。

P2 新增输出增量、完成和归档的类型化事件映射；缺失执行身份与未知终态拒绝处理。应用消费、失败详情展示、Plan、审批与问题映射仍待完成。异步 port 与原方案实现方式的差异见 [ADR 0001](adr/0001-async-port-futures.md)。

## 后续实施：交互与 IPC 确认

已实现 `ReplyHandle` 单次消费、未知请求显式错误、审批/问答 schema fixture、交互容量与归属/期限检查、部分答案超时保留、执行事件身份过滤及提前完成暂存。`AppServer.next_event` 输出应用类型，假子进程验证审批往返后完成；完整 actor 尚未装配。

SDK `emit_confirmed` 现在最多等待 2 秒；Rust IPC pump 对应用确认最多等 1 秒，再限时写回。失联、超时、接收队列满或丢弃确认句柄均不会产生成功 ACK。该接受确认只定义接口，应用仍必须在耐久认领或明确非任务处置后确认，尚未完成生产接收集成。先前“SDK ACK 早于 Rust 确认”和“缺首次连接通知”的检查点描述已由本批实现推进。

当前仅支持能完整呈现的命令/文件审批；额外权限 overlay 和网络上下文请求明确报不兼容，后续补类型化展示与测试。未知服务端请求不能当作可忽略通知。

## 会话准备与耐久认领接入

`bridge-app::sessions::prepare` 选择已配置/默认模型，创建或恢复 thread，并通过 SessionStore 保存绑定后才返回 TurnInput。主循环可在发送 turn/start 前建立事件身份校验；便利 start 用例执行相同准备顺序。恢复返回不同 ID 或目录时拒绝启动，执行失败不清除已提交绑定，不自动重试。

`bridge-local::async_state::AsyncState` 将已有 JsonStore 移交给共享单写入者，在最多两个 spawn_blocking 任务内完成读写；互斥锁仅覆盖文件操作。调用者取消后，已开始的阻塞写入继续完成，许可随写入结束释放。生产监督器仍需限时排空这些操作，当前并未因此完成服务生命周期。

Scheduler 新增 reserve/commit_admission/abort_admission，业务所有者可在等待持久化期间继续处理停止等控制操作。待保存项计入容量，提交按预留顺序进入 FIFO，保存失败和已取消项不会执行。异步 DurableJournal 返回新认领或重复，跨 crate 测试用真实 JSON 文件及内存 SDK IPC 验证 ACK 与调度结果：保存失败拒绝应答，重复成功应答但不重复执行。
