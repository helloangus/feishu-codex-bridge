<proposed_plan>
# Feishu Codex Bridge Rust 重构计划

## 一、结论与实施基线

推荐采用 **Cargo workspace + 六个生产 crate + 显式状态机 + 飞书/Codex 独立适配层**，分阶段迁移现有功能。首版保持全局单任务执行，通过异步 I/O、有界队列、状态所有权和可独立测试的协议适配提升工程质量。

本方案采用你已经确认的约束：

- 支持 **Linux 与 Termux**，首版不扩展 Windows、macOS。
- 允许短期保留独立的 Python 飞书 SDK 适配进程，最终交付全 Rust 实现。
- 保持任务串行执行，预留多会话并发边界。
- 继续使用 **JSON 文件持久化**，不引入 SQLite。
- 新部署必须显式配置访问策略；迁移旧配置时保留原访问语义。

已完整阅读仓库内的源码、三份测试文件、README、PLAN、五份设计及运维文档、配置模板、依赖清单、启动/安装脚本与仓库规范。真实 `.env` 不参与读取，配置分析以模板、源码和脚本为准。

本次检查结果：

| 项目 | 结果 |
|---|---|
| Python 主体 | `bridge.py` 1,947 行，`service.py` 218 行 |
| 离线回归 | 47 项全部通过 |
| Python 语法检查 | 通过 |
| `git diff --check` | 通过 |
| 本机 Codex CLI | 0.153.4 |
| 本机飞书 SDK | lark-oapi 1.7.3 |
| 其他直接依赖 | httpx 0.28.1、websockets 15.0.1、requests 2.34.2、python-socks 3.0.0 |
| Rust 工具链 | 当前环境未安装，未执行 Rust 构建 |
| 工作区 | 存在未提交改动，以当前工作区而非仅 HEAD 为功能基线 |

现有 PLAN 中部分归档、差异展示及卡片布局仍标记为待真实飞书验收。47 项离线测试通过不能替代这些验收。本次仅制定计划，不修改文件或运行中的服务。

## 二、现有实现与具体问题

### 2.1 当前项目实际提供什么

项目定位是个人或小团队使用的“飞书上的本地 Codex 终端”，包括：

| 功能组 | 当前能力 |
|---|---|
| 访问与目录 | 白名单、配对码、按用户切换工作目录、创建目录确认、工作区边界检查 |
| 会话 | 新建、恢复、归档、取消归档、上下文压缩，绑定按用户和目录保存 |
| 执行 | 普通消息排队、模型选择、进度卡、停止当前及排队任务 |
| Plan | 持久化开关、计划详情、继续规划、原上下文实施、清空上下文后实施 |
| 人工交互 | 命令/文件审批、超时拒绝、选择题、其他文字回答、过期卡处理 |
| 输入输出 | 图片和文件输入、Markdown 分段、工程文本 diff、成果附件回传 |
| 运维 | 项目锁、App ID 全局锁、日志脱敏与轮转、健康状态、退避重启、跨重启去重 |

会话绑定按用户隔离，不等于底层 Codex 历史和文件系统构成多租户安全隔离。重构继续保留“同一设备上的受信任使用者”定位。

### 2.2 现有构建与启动链

当前没有编译打包、依赖锁文件和 GitHub Actions：

```text
setup.sh
  → 检查 Python / Codex
  → pip install -r requirements.txt
  → 交互生成权限 0600 的 .env
  → start.sh

start.sh
  → shell source .env
  → 校验工作目录与工作区根
  → service.py

service.py
  → 项目 flock + App ID 本机全局 flock
  → 启动 bridge.py 子进程
  → 读取、脱敏、轮转子进程日志
  → 根据日志更新 health.json
  → 异常退出后按 2、4、8……30 秒重启

bridge.py
  → 初始化 Feishu HTTP 客户端
  → 启动并初始化 Codex app-server
  → 加载状态、启动 worker 和定时线程
  → 应用飞书 SDK 兼容补丁
  → 注册消息与卡片回调，启动长连接
```

这里需要修正文档：`Bridge` 实际启动五个后台线程，包括 `plan_action_reaper`，架构文档只列出了四个。此外还存在 RPC reader、事件分发线程和动态创建的命令线程。

### 2.3 飞书机器人、SDK 与 HTTP API 的真实关系

[飞书适配实现](/home/angus/dev/feishu-codex-bridge/bridge.py:72)并非完全建立在 SDK 上：

- **机器人入口**：企业自建应用，订阅 `im.message.receive_v1` 和 `card.action.trigger`，使用长连接。
- **SDK**：负责连接地址获取、WebSocket、心跳、重连、帧处理与回调分发。
- **httpx**：项目自行调用 tenant access token、消息创建、卡片更新、文件/图片上传和资源下载接口。
- **卡片**：项目自行生成 Card JSON 2.0，按钮使用 callback behavior。
- **代理**：REST、SDK 地址获取、WebSocket 是三条路径，目前分别适配。
- **兼容补丁**：修改 SDK 私有 `_handle_data_frame`、`_ws_connect_kwargs` 和模块内 requests 引用。

本机 SDK 源码确实存在 CARD 分支直接返回、WebSocket 显式关闭环境代理发现的逻辑，说明这些补丁有实际来源；但通过源码字符串判断并替换私有方法十分脆弱。[飞书 SDK 对应源码](https://github.com/larksuite/oapi-sdk-python/blob/v1.7.3/lark_oapi/ws/client.py)

### 2.4 Codex 接入与主要调用链

[CodexServer](/home/angus/dev/feishu-codex-bridge/bridge.py:243)启动本机已登录的 Codex CLI，通过 stdio JSONL 双向通信，不直接调用 OpenAI Responses API。

```text
飞书消息
  → receive()
  → message_id 持久化去重
  → 授权检查、内容解析
  ├─ 文本命令 → command()
  ├─ 当前问题的文字回答 → answer_user_input()
  └─ 普通任务 → jobs → worker()
                         → 固定任务目录、创建 task_id
                         → 任务前快照
                         → 下载附件
                         → thread/resume 或 thread/start
                         → turn/start
                         → 等待匹配 thread_id + turn_id 的完成事件
                         → 保存会话绑定
                         → 最终回复 / Plan 详情与操作卡
                         → 任务后快照、diff、附件上传
```

反向事件链：

```text
app-server stdout
  → 唯一 RPC reader
  ├─ 响应 → pending[id]
  └─ 通知及服务端请求 → 事件分发
                        → Bridge.codex_event()
                        ├─ 输出增量 → 进度状态
                        ├─ approval → 审批卡 → 原 RPC ID 回复
                        ├─ requestUserInput → 逐题交互 → 原 RPC ID 回复
                        └─ archived → 清理会话绑定及旧操作
```

已使用的接口包括初始化、模型列表、thread 创建/读取/恢复/列表/归档/取消归档/压缩、turn 启动/停止，以及服务端发起的审批和问题请求。

官方文档确认 app-server 使用双向 JSON-RPC，且 CLI 能生成与其版本对应的 JSON Schema。重构应保持这一接入方式，并锁定协议契约。[Codex App Server 官方文档](https://learn.chatgpt.com/docs/app-server)

### 2.5 应优先解决的问题

以下区分已存在的结构问题与需要回归验证的失败风险，不将代码推断视作已发生的生产事故。

| 问题 | 当前依据与影响 | 重构处理 |
|---|---|---|
| 业务与协议混合 | `Bridge` 同时操作 SDK 数据、RPC 字典、卡片、文件与状态 | 业务只接收内部命令和事件 |
| 无界并发入口 | 每个命令和多类回调创建 daemon thread；任务队列无容量限制 | 有界消息队列与受控异步任务 |
| 锁覆盖网络 I/O | 进度更新和选择题路径持锁调用 HTTP | 状态所有者只计算转换，I/O 独立执行 |
| RPC 事件分发受飞书阻塞 | 审批、问题事件处理直接发送卡片 | RPC 读取、业务处理、飞书发送分离 |
| 执行状态全局化 | `current_chat`、`turn_text`、进度等依赖唯一活动任务 | 显式 `TaskContext`，事件携带执行身份 |
| 审批身份不够稳定 | 审批使用原始整数 RPC ID，进程重启后 ID 可重复 | 本地一次性 token + 连接代次 + 原始 RPC ID |
| 事件关联不足 | completion 校验 thread/turn，但其他输出多依赖“当前任务” | 对所有可关联事件校验连接和执行身份 |
| JSON 写入竞争 | 设置等路径使用固定 `.tmp`，多命令线程可同时写 | 单一存储所有者、独占写入与耐久提交 |
| 去重落盘失败仍执行 | 保存异常仅记录日志 | 持久化失败时不启动副作用任务 |
| 会话保存偏晚 | 新 thread 在 turn 结束后才保存绑定 | 创建/恢复成功即保存，避免失败后丢绑定 |
| 模式转换不够显式 | `/plan off` 后并非所有路径都发送 default 协作模式 | 每轮明确指定有效模式并做连续回归 |
| 回调异常边界不统一 | `on_message()` 无统一保护，部分命令分支异常可能逃逸 | 单一入口错误映射和任务终态处理 |
| 操作 token 提前消费 | 部分 Plan/目录操作先 `pop` 再校验 | 校验成功后原子认领，避免无效请求耗尽操作 |
| 下载与文件处理边界不一致 | 下载完整读入内存；生成图片扫描未复用普通快照全部过滤 | 流式限额下载、统一文件访问策略 |
| 健康状态依赖日志 | 匹配 `bridge_ready`、SDK connected 字符串，缺少可靠断线状态 | 显式健康事件、心跳与状态新鲜度 |
| 可复现性不足 | Python 依赖仅下限，无锁；SDK 却依赖私有实现 | 固定过渡依赖、Cargo.lock、协议版本矩阵 |
| 测试与实现绑得太紧 | 大量 `__new__` 和全局变量替换，监督器仅一项测试 | 可构造服务、fake ports、进程及故障测试 |

另外应补齐配置文档、忽略规则和示例中遗漏的状态文件及配置项；目录确认和文件打开时重新验证路径，避免只在生成卡片时检查一次。

## 三、推荐架构与接口边界

### 3.1 借鉴哪些 Rust 项目

| 参考项目 | 采用的设计 | 本项目具体应用 |
|---|---|---|
| ripgrep | 工作区管理、能力库与 CLI 分离 | 核心能力独立 crate，CLI 仅装配和管理运行 |
| Vector | sources、transforms、sinks 与 topology 分工 | 飞书入口、业务转换、交付出口分离，统一任务生命周期 |
| Tokio 官方实践 | 明确任务所有者、通知取消并等待结束 | 管理 RPC、发送队列、定时任务和关闭流程 |

参考的是边界与生命周期设计，不照搬大型项目的模块数量。依据：[ripgrep workspace](https://github.com/BurntSushi/ripgrep/blob/master/Cargo.toml)、[Vector 工程结构](https://github.com/vectordotdev/vector/tree/master/src)、[Tokio graceful shutdown](https://tokio.rs/tokio/topics/shutdown)。mini-redis 可作为学习样例，但其官方定位是教学项目，不作为生产成熟度依据。

### 3.2 六个生产 crate

| crate | 职责 | 不允许依赖的内容 |
|---|---|---|
| `bridge-core` | 身份类型、命令、任务/交互状态机、权限和交付分类规则 | Tokio、HTTP、SDK、RPC wire 类型、实际文件 I/O |
| `bridge-app` | 用例编排、ports、调度、事件路由、交互注册表、交付流程 | 飞书/Codex 具体客户端 |
| `bridge-feishu` | 飞书入站解码、REST、认证、卡片渲染、长连接、代理 | Codex 适配器 |
| `bridge-codex` | app-server 子进程、JSONL、RPC、协议 DTO 与兼容映射 | 飞书卡片和用户界面 |
| `bridge-local` | JSON 存储、目录操作、附件文件、快照/diff、生成图片发现 | 飞书/Codex wire 类型 |
| `bridge-cli` | 配置、依赖装配、命令行、监督器、健康、日志、信号 | 具体业务决策分支 |

另外设置 `bridge-test-support` 与 `xtask`，分别服务测试和维护工具，不进入生产依赖链。

依赖方向：

```text
bridge-cli
  ├─ bridge-app ──────────────→ bridge-core
  ├─ bridge-feishu ───────────→ bridge-app / bridge-core
  ├─ bridge-codex ────────────→ bridge-app / bridge-core
  └─ bridge-local ────────────→ bridge-app / bridge-core
```

适配器之间不互相调用。避免新增无明确边界的 `common`、`utils`、`manager` crate。

### 3.3 内部模型与 ports

核心类型采用 newtype 和 enum：

- `UserId`、`ConversationId`、`SessionKey { user, workspace }`。
- `TaskId`、`ThreadId`、`TurnId`、`ConnectionEpoch`、`InteractionToken`。
- `Command`：文本和按钮统一解码后的强类型操作。
- `TaskSpec`：提交时固定的用户、目录、输入、模型和执行模式。
- `AgentEvent`：输出、计划、审批、问题、完成、归档、断连。
- `View`：进度、会话列表、审批、问题、计划、diff 等内部展示模型。
- `DeliveryReport`：成功、失败、跳过与原因。

关键 ports 放在 `bridge-app`，使用对象安全的异步接口；采用 `async-trait`，便于注入生产实现和 fake。调用频率由网络和模型交互主导，这里的动态分发不是主要性能问题。

| port | 最小能力 |
|---|---|
| `AgentBackend` | 能力查询、模型/会话管理、启动/停止 turn、回复审批与问题 |
| `Messenger` | 发送/更新 `View`、发送文本、上传受控文件、返回投递回执 |
| `ResourceFetcher` | 将附件引用流式写入受控目的文件 |
| `StateStore` | 读取视图、持久化业务变更、认领消息、导入/导出 |
| `WorkspaceAccess` | 解析目录、确认创建、受控打开文件 |
| `ArtifactService` | 快照、差异、成果发现、交付候选列表 |
| `Clock` / `IdGenerator` | 可测试时间与标识生成 |

接口约束：

1. SDK 类型、`serde_json::Value`、HTTP response、RPC method 字符串不穿过业务边界。
2. 未知协议字段只保留在适配层，不通过 `extra: Value` 继续扩散。
3. ports 返回分类明确的错误；业务不解析第三方错误字符串。
4. 飞书 `message_id` 等外部标识包装成内部句柄，业务不依赖格式。
5. 核心定义“展示什么”，飞书适配器定义“如何生成 Card JSON 2.0”。

### 3.4 调度与运行流程

使用一个业务状态所有者处理有界 mailbox，保留一个活动 Codex turn：

```text
飞书接收 → 解码与快速协议应答 → 应用入口
                                  ↓
                         授权 / 去重持久化
                                  ↓
                      Command / TaskSpec
                                  ↓
                   应用状态机与全局 FIFO
                      ↓             ↓
                  Codex port     本地文件 port
                      ↓             ↓
                       AgentEvent / 结果
                                  ↓
                            View / 交付项
                                  ↓
                          飞书发送调度器
```

关键原则：

- 状态所有者不能等待整个 turn；启动 I/O 后接收完成消息，期间持续处理停止、审批和状态查询。
- 工作队列默认 64 项；满时明确提示重发，不先记为已接收任务。
- 管理类外部操作并发上限 8；HTTP 并发上限 4；文件扫描/diff 阻塞任务上限 2。
- 停止、审批和协议响应使用独立控制通道，避免排在普通输出后面。
- `watch` 保存最新进度，每两秒合并更新；最终状态优先，禁止迟到进度覆盖完成卡。
- 同一卡片更新按序执行；上传不能独占审批卡发送能力。
- `spawn_blocking` 处理文件扫描、diff 和耐久写入，必须同时有并发上限。
- 用 `CancellationToken` 和受监督任务集合统一关闭；后台任务退出必须可观察。

任务状态明确为：

```text
Queued → Preparing → Running ↔ WaitingApproval / WaitingInput
                         ↓
                      Stopping
                         ↓
              Completed / Failed / Interrupted
                         ↓
                  Delivering → Finished
```

任务执行结果与交付结果分别记录。Codex 成功但上传失败时，不能将整项任务描述为“执行失败”。

首版对会话切换继续实施现有全局空闲约束。未来增加并发时，再将任务执行器按 session 拆分，并新增目录冲突锁；不以增加 worker 数作为并发实现。

## 四、第三方适配与持久化方案

### 4.1 Codex：保留本地 app-server，隔离协议变化

适配器内部划分为：

```text
process → jsonl transport → rpc multiplexer → wire DTO → domain mapper
```

具体实施：

- 唯一 stdout reader、唯一受控 writer。
- RPC 响应用 `oneshot` 分发；服务端请求与普通通知分别解码。
- RPC ID 支持整数与字符串，并保留原始类型。
- 连接重启增加 `ConnectionEpoch`；旧代次响应、审批和输出不能影响新任务。
- reader 不执行飞书 HTTP，也不直接更新业务状态。
- 对通知按 thread/turn/item 路由；缺少身份时仅使用已经建立的明确映射，无法归属则拒绝影响活动任务。
- JSONL 设置可配置帧上限，默认 8 MiB；畸形帧、EOF、超时均产生明确适配错误。
- 非关键进度可合并；关键事件队列超限时显式失败并关闭连接，不静默丢失审批或完成事件。
- stderr 单独处理，默认不输出原始诊断正文，不能混入协议流。

协议维护策略：

1. 以本机 0.153.4 作为首个兼容基线，保存 schema、CLI 版本和校验摘要。
2. 手写本项目用到的 DTO，通过 schema 和 fixture 验证；不绑定整个 Codex 内部 workspace。
3. `generate-json-schema` 只由维护命令执行，正常构建不联网或启动 Codex。
4. 更新 schema、DTO 和映射通过独立 PR 审核。
5. 实验能力集中为 capabilities；不支持 Plan 时明确报告不可用，不悄悄改成执行模式。
6. 每轮明确发送已选模式、模型及 sandbox/approval 策略，消除对历史协作模式的隐式依赖。
7. 未知审批不能默认转换为允许；应返回协议支持的拒绝/不支持结果，必要时中止当前 turn。

连接异常不自动重跑任务。新 thread 创建及恢复成功后立即保存绑定；RPC 结果不确定时保留“结果未知”状态，避免重发 `turn/start`。

### 4.2 飞书：过渡适配进程与最终 Rust 实现

**过渡阶段**

保留一个仅负责 SDK 长连接的 Python 子进程：

- SDK 的消息、卡片回调转换为版本化内部 JSONL envelope。
- envelope 含协议版本、事件序号、连接代次与事件内容。
- stdout 仅用于协议，stderr 用于受控日志。
- 有界缓冲与本地接受确认，不能无限创建线程或无限缓存。
- SDK 帧应答不等待 Codex 执行或飞书 REST 请求。
- 白名单、配对、会话、审批、Plan 和去重全部由 Rust 管理。
- Rust 接管 REST、卡片、附件；兼容补丁集中在这个进程内，固定 SDK 依赖版本及测试。

**最终阶段**

在 `bridge-feishu` 内实现本项目所需的最小长连接客户端：

- `reqwest`：token、消息、卡片、资源与连接地址获取。
- `tokio-tungstenite`：WebSocket。
- `prost`：协议帧。
- 显式实现连接配置、心跳、断线重连、分片合并、EVENT/CARD 应答。
- 分片缓存有数量、字节和超时上限。
- 协议定义记录来源和版本；生成代码只通过维护命令更新。

默认不引入覆盖全部飞书 API 的社区 SDK，以免把当前依赖私有实现的问题转移到另一套大型依赖。若后续改用 SDK，只能替换此 crate 内部实现，必须通过相同契约测试。

**认证与代理**

- token 刷新采用 single-flight，避免并发刷新。
- 统一检查 HTTP 状态和飞书业务错误码。
- 显式 `FEISHU_PROXY_URL` 优先，否则保持环境代理及 `NO_PROXY` 行为。
- REST、地址发现、WebSocket 使用同一代理决策模块。
- WebSocket 的 CONNECT/SOCKS 建连单独实现和测试，不假设 HTTP 客户端代理会自动生效。
- TLS 默认验证证书，允许显式配置额外 CA 文件。

WebSocket 库与 HTTP 代理是独立能力，应分别适配。[tokio-tungstenite 文档](https://docs.rs/tokio-tungstenite/latest/tokio_tungstenite/)、[reqwest Proxy 文档](https://docs.rs/reqwest/latest/reqwest/struct.Proxy.html)

**重试规则**

- 查询、token 获取和明确可安全重试的请求允许有限退避重试。
- 创建消息和上传后的发送若结果不确定，不盲目重发；只有接口明确提供去重能力时才携带稳定幂等标识重试。
- 卡片更新采用版本顺序和最新状态覆盖。
- 任何重试都不能再次触发 Codex 任务。

### 4.3 统一人工交互注册表

审批、选择题、Plan 后续操作和目录创建共用 `InteractionRegistry`。

每条交互记录绑定：

- 用户、聊天、session、任务。
- 来源卡片、创建时间、到期时间。
- 可执行操作集合。
- 必要时绑定 Codex 连接代次和原始 RPC ID。

操作采用 `Pending → Resolving → Resolved/Expired/Invalidated` 状态转换。先验证身份和有效期，再认领操作；RPC 写入结果不确定时不重新开放“允许”。

保留现有默认 600 秒与超时后果：

- 审批：拒绝。
- 选择题：提交已有答案，未答项为空。
- Plan 后续操作：失效，但不删除计划内容。
- 服务重启：旧交互全部失效。

文本 `/approve <id>` 改用桥接生成的本地审批编号，不暴露可在重启后复用的 RPC ID；命令形式保持不变。

### 4.4 JSON 状态：单写入者、版本化与可回退

保留 JSON，推荐集中为两个文件：

| 文件 | 内容 |
|---|---|
| `state.json` | schema 版本、会话、用户目录、模型、Plan 模式、配对用户、有限任务状态 |
| `seen-messages.json` | 最近 1,000 个已认领消息 ID |

相关业务字段放在同一个 `state.json`，使归档清绑定、模式切换等本地变更可通过一次原子替换提交。

持久化要求：

- 单一 `StateStore` 所有者串行写入。
- 同目录唯一临时文件，创建即 `0600`。
- 写入、flush、文件 `sync_all`、rename、父目录同步。
- 内存状态在提交成功后更新。
- 保留上一份完整版本供显式恢复；损坏不能静默当空配置启动。
- 未识别的未来 schema 版本拒绝写入。
- JSON 易于人工阅读，但仅支持停止服务后编辑。
- 认领消息保存失败时不执行任务，健康状态进入 degraded。

继续采用当前偏向“至多一次”的语义：消息认领成功后、执行前崩溃，可能需要用户重发；不承诺 exactly-once，也不自动恢复执行未完成任务。1,000 条是有限去重窗口，不描述为永久去重。

提供两个显式工具：

```text
bridge migrate import-python --dry-run
bridge migrate import-python
bridge migrate export-python --output <directory>
```

迁移读取四类旧状态文件和旧单 thread ID 格式。单 ID 无法确定用户归属时要求显式传入用户，不能自动绑定给所有人。导出写入指定目录，不覆盖原文件。

### 4.5 附件、快照与差异

保留已经形成的产品规则：

- 工程文本只发 diff。
- 已知成果格式自动上传。
- 未知二进制、不可读或超大文本只显示原因。
- 默认最多 200 个快照文件、每文件读取 256 KiB 文本、diff 20,000 字符。
- 每轮附件总计最多 10 个、单附件 20 MiB。
- 继续排除收件箱、缓存、构建输出、真实 `.env`、状态文件与符号链接。

改进实现：

- 下载使用流式写入和实际字节计数，默认输入上限同为 20 MiB，另设可配置输入限额。
- 临时下载完成后原子提交；取消或超限清理临时文件。
- 目录确认、文件读取和上传前重新校验；受控打开时禁止跟随符号链接。
- 对生成图片路径应用相同检查，生成目录作为显式受信任根单独管理。
- 快照携带 `complete` 和跳过原因，未完整扫描不能推导不存在文件已删除。
- 文本仍比较内容，不退化为仅比较 mtime/size。
- 开始和结束快照各一次，结束结果同时供 diff 与上传使用。
- Markdown 分段按 Unicode 边界处理，并计入补齐围栏后的长度；覆盖无语言围栏、超长单行和中文。
- 最终答复过长时溢写私有临时文件并分段读取，避免无限累积字符串。
- diff 在有界阻塞任务中计算，使用 `similar`；目录遍历使用 `walkdir` 和显式项目过滤规则，避免因自动引入 `.gitignore` 改变现有交付语义。

## 五、工程化、运维与发布

### 5.1 依赖与代码规范

| 领域 | 推荐选择 |
|---|---|
| 异步 | `tokio`、`tokio-util` |
| 序列化与配置 | `serde`、`serde_json`、`toml`、`clap` |
| HTTP / WS / 帧 | `reqwest` + rustls、`tokio-tungstenite`、`prost` |
| 错误 | 库层 `thiserror`，CLI 边界 `anyhow` |
| 日志 | `tracing`、`tracing-subscriber` |
| 文件与差异 | `tempfile`、`walkdir`、`similar` |
| 测试 | `proptest`、`insta`、`assert_cmd`、`criterion` |

工程约束：

- 使用 Rust 2024 edition、workspace resolver 3。
- 统一 `[workspace.dependencies]`、`[workspace.package]`、`[workspace.lints]`，提交 Cargo.lock。
- 固定 `rust-toolchain.toml` 工具链；P0 根据 Linux/Termux 构建验证结果记录明确版本及 MSRV，后续升级通过独立 PR。
- 依赖默认关闭不需要的 feature，不使用漂移的 Git 分支依赖。
- 库公开类型和 ports 写 rustdoc；生产路径禁止 `unwrap/expect`。
- 默认禁止自有代码 `unsafe`；如 Unix 平台适配确实需要，局限到最小模块并附安全说明和专项测试。
- 不设计泛化插件系统、动态加载或数据库抽象集合。

Cargo workspace 支持统一包元数据、依赖和 lint 配置，可直接用于约束 crate 边界。[Cargo 官方文档](https://doc.rust-lang.org/cargo/reference/workspaces.html)

### 5.2 错误处理

业务错误至少区分：

- 未授权、非法目录、无效/过期交互。
- 队列已满、功能不支持。
- 协议不兼容、连接断开、操作超时。
- 状态保存失败。
- 执行失败、交付失败、结果不确定。

每个第三方错误映射为内部类别、是否可重试、脱敏诊断信息和用户提示。完整 HTTP body、认证 URL、聊天正文和审批命令不直接进入日志或通用错误展示。

关键后台任务 panic 或异常退出由监督器处理，不能让服务继续显示健康但实际已无 worker。

### 5.3 配置与服务生命周期

新增：

```text
bridge run
bridge service start|stop|restart|status|logs
bridge config check
bridge doctor
bridge migrate ...
```

保留 `start.sh` 作为薄兼容入口；最终 `setup.sh` 只完成安装、配置与诊断，不再安装 Python。

配置采用明确加载次序：

```text
内置默认值 < bridge.toml < 显式导入的旧配置 < 进程环境 < CLI 参数
```

正常 Rust 启动不执行 shell 配置。旧 `.env` 的 Bash `%q` 转义与普通 dotenv 并不等价，因此迁移工具仅支持明确的字面量语法；遇到命令替换、变量展开等可执行表达式时拒绝自动解析并指出行号，不运行它们。

其他要求：

- app-server 配置拆成 executable 和 args 数组。
- 不热更新凭据、代理和 sandbox，统一重启生效。
- 新安装默认拒绝未授权访问；旧开放配置迁移为显式 `access.mode = "open"`。
- `workspaceWrite`、关闭网络、`on-request` 保持默认；`dangerFullAccess` 必须显式设置。
- 状态目录与工作目录分离；旧路径可由迁移配置指定。

监督器继续支持 Termux，不依赖 systemd：

- 同一二进制提供 supervisor 与内部 worker 模式。
- supervisor 持有项目锁和 App ID 全局锁；过渡期兼容现有锁命名和位置。
- PID 校验结合锁、可执行文件和进程启动身份，不能只信 PID 文件。
- 健康由独立结构化控制通道上报，包含连接状态、worker 状态、心跳时间与 Codex 代次。
- 停止顺序：停止接收 → 取消排队任务 → 处理待交互 → 中断 turn → 限时排空交付 → 持久化 → 关闭子进程。
- 超时后对所属进程组 TERM/KILL 并回收，避免孤儿 Codex。
- 保留 2–30 秒退避；稳定运行后重置退避。
- Linux 可附 systemd 模板，但必须关闭内部重复监督，避免两层相互重启。

### 5.4 日志与可观测性

保留 JSON 日志，增加 task、session 哈希、连接代次、交付标识和耗时字段。禁止以完整用户、路径、thread ID 作为高基数指标标签。

至少记录：

- 入站、重复、未授权、队列拒绝数量。
- 队列长度、等待时间、活动任务状态。
- RPC 超时、断连、未知通知及协议错误。
- 审批超时、过期点击、卡片更新与投递失败。
- 文件扫描时间、读入字节、截断与跳过原因。
- 存储失败、进程重启与关停耗时。

默认提供日志和 `status --json`，不强制部署 Prometheus/OTel 服务。日志继续按 2 MiB、三个历史副本轮转；轮转作为单独 writer 实现并测试，不能假设 tracing 自动提供按大小轮转。

### 5.5 GitHub Actions 与发布流程

| workflow | 触发 | 内容 |
|---|---|---|
| `ci.yml` | PR、主分支 push | 格式、Clippy、测试、文档、边界检查、脚本和工作流检查 |
| `coverage.yml` | PR、主分支 | LLVM 覆盖率与报告 artifact |
| `dependencies.yml` | 定时、依赖变更 | cargo-deny、许可证与来源检查、依赖更新 |
| `compatibility.yml` | 手动、定时 | 固定 Codex 与候选版本协议检查、SDK fixture 回放 |
| `release.yml` | 版本 tag | 构建、打包、校验、来源证明、GitHub Release 草稿 |

推荐使用：

- `actions/checkout`。
- [`dtolnay/rust-toolchain`](https://github.com/dtolnay/rust-toolchain)。
- [`Swatinem/rust-cache`](https://github.com/Swatinem/rust-cache)。
- [`taiki-e/install-action`](https://github.com/taiki-e/install-action) 安装固定版本检查工具。
- `actions/upload-artifact`。
- `actions/attest-build-provenance`。
- Dependabot 更新 Cargo 与 GitHub Actions。

常规检查命令：

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo nextest run --workspace --locked
cargo test --workspace --doc --locked
cargo doc --workspace --no-deps --locked
cargo deny check
cargo xtask check-boundaries
cargo xtask check-fixtures
actionlint
shellcheck start.sh setup.sh
git diff --check
```

迁移期继续运行全部 Python 回归。`nextest` 与 doctest 分开；覆盖率使用 `cargo-llvm-cov`；依赖审计使用 cargo-deny，避免堆叠重复工具。[nextest](https://github.com/nextest-rs/nextest)、[cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov)、[cargo-deny](https://github.com/EmbarkStudios/cargo-deny)、[actionlint](https://github.com/rhysd/actionlint)

安全与发布约束：

- Actions 固定完整 commit SHA，默认 `contents: read`；只在发布和证明 job 增权。
- PR 测试不使用真实飞书凭据、不登录真实 Codex，不调用付费模型。
- 不在含生产凭据的自托管 runner 上运行外部 PR。
- Linux 发布 `x86_64-unknown-linux-gnu`、`aarch64-unknown-linux-gnu`；Termux 发布独立 `aarch64-linux-android` 产物。
- Android 使用固定 NDK 构建，并在真实 Termux 验证。不能把 Linux ARM64 包当作 Termux 包。[Rust Android 支持说明](https://doc.rust-lang.org/rustc/platform-support/android.html)
- release 包含二进制、配置样例、迁移说明、兼容矩阵、SHA-256 与构建来源证明。
- tag 先生成 draft release；真实飞书、Termux 和回退验收通过后发布。
- 不自动部署到个人设备，不默认发布 crates.io 包；仓库尚无 LICENSE，不能由重构擅自选定许可证。

Actions 权限及来源证明按 GitHub 官方机制配置。[安全使用说明](https://docs.github.com/en/actions/reference/security/secure-use)、[构建来源证明](https://docs.github.com/en/actions/how-tos/secure-your-work/use-artifact-attestations/use-artifact-attestations)

## 六、分阶段迁移与验收标准

| 阶段 | 实施任务 | 完成标准 |
|---|---|---|
| **P0：冻结行为与建立基线** | 保存当前工作区基线；建立功能矩阵、协议 fixture、配置映射、性能回放；补齐已知缺口测试；验证工具链与目标平台 | 47 项测试全部映射到验收项；记录明确 CLI/SDK/工具链版本；手机端待验收项有独立记录；不以当前潜在缺陷作为兼容要求 |
| **P1：workspace、核心与本地能力** | 建立六 crate；实现命令/状态机、ports、JSON 单写入者、迁移工具、文件分类与 diff；搭建 CI | 无飞书/Codex 依赖即可测试业务；旧状态 dry-run、导入、导出通过；并发写入、损坏文件、路径边界与快照上限通过 |
| **P2：Rust Codex 适配** | 实现子进程、RPC、事件关联、模式、审批/问题映射与停止 | fake server 覆盖乱序、断连、重复 ID、未知事件；本机隔离环境完成初始化与无副作用协议检查；旧代次事件不能影响新任务 |
| **P3：Rust 主业务 + Python 连接过渡** | SDK 薄进程、Rust REST/卡片/附件、所有命令迁移、服务生命周期和健康上报 | 测试机器人端到端功能通过；业务不再导入 Python；同 App ID 不出现双实例消费；所有原命令和卡片路径可用 |
| **P4：全 Rust 飞书长连接** | 帧协议、心跳、分片、CARD、重连、HTTP/SOCKS 代理；替换 SDK 子进程 | 同一组 fixture 下新旧适配结果一致；直连/代理/断线恢复通过；真实手机按钮通过；运行和安装不需要 Python |
| **P5：稳定性、发布与退役** | 性能对比、持续运行、故障注入、跨平台发布、状态回退演练、同步文档 | Linux/Termux 构建与运行通过；连续运行至少 72 小时无失联或状态泄漏；停止无孤儿进程；成功完成 Rust→Python 状态回退演练 |

每阶段交付可独立审核的 PR，不将协议、状态格式、卡片和监督器混成一次大提交。P4 是最大外部兼容风险，应在 P0 就开始收集长连接契约样本。

**切换方式**

1. 开发验证使用独立测试机器人，禁止 Python 和 Rust 同时连接生产 App ID。
2. 生产切换前停止接收并排空任务，备份 JSON。
3. 停止旧实例，确认释放全局锁，再导入并启动 Rust。
4. 先验证 `/status`、`/help`、模型和会话列表，再做无副作用任务及审批测试。
5. 回退时停止 Rust，将最新状态导出为 Python 格式，再启动旧版。
6. 不回放迁移前未完成任务；旧卡片失效后由用户重新打开控制面板。

回退只恢复桥接版本和状态，不承诺撤销 Codex 已对工作目录造成的修改。

## 七、测试体系与性能验收

### 7.1 必须覆盖的行为

| 层次 | 重点案例 |
|---|---|
| 纯业务测试 | 授权、会话隔离、FIFO、排队取消、显式 Plan 切换、归档清绑定 |
| 交互状态测试 | 允许与超时同时到达、重复点击、错误用户/聊天/目录/来源卡、重启后旧 token |
| RPC 契约测试 | 响应乱序、完成先于启动响应、字符串 ID、晚到响应、断连、未知审批 |
| 飞书协议测试 | EVENT/CARD、分片乱序/重复/超限、心跳、重连、业务错误码、token 并发刷新 |
| 投递测试 | HTTP 429/5xx、请求结果不确定、更新乱序、进度不能覆盖终态、附件不阻塞审批 |
| 文件测试 | 超长行、中文、无语言围栏、非 UTF-8、空文件、末尾换行、相同 metadata 内容变化 |
| 路径测试 | 越界目录、符号链接、确认后路径变化、生成图片链接、下载超限与取消 |
| 存储测试 | 单写入者、临时文件失败、rename 前后崩溃、损坏状态、未知 schema、旧格式 round-trip |
| 进程测试 | 双启动、App 锁、旧 PID、TERM、强制退出、退避重置、无孤儿子进程 |
| 真实验收 | 所有命令、Plan 三分支、问题其他回答、审批、停止、归档、图片/PDF 与代理恢复 |

默认测试使用临时目录和 fake ports；跨进程测试使用有明确退出期限的 fake app-server。真实飞书与 Codex 联调独立标记，不进入普通离线测试。

覆盖率目标：核心状态机与纯规则行覆盖率至少 90%，workspace 至少 80%；更关键的是上述竞态和失败场景不能缺项。

### 7.2 性能如何衡量

Rust 主要改善桥接自身的资源与等待开销，不能据此承诺模型生成速度提升。

建立固定回放：

- 连续文本增量与进度刷新。
- 命令突发和队列满载。
- 200 文件、每文件最多 256 KiB 的快照。
- 20 MiB 附件下载。
- 反复连接重启和 1,000 次交互创建/清理。

比较 Python 与 Rust 的：

- 桥接自身 CPU、空闲/峰值 RSS，排除 Codex 子进程。
- 入口接收、调度和停止信号传递的 P50/P95。
- 卡片更新请求数量。
- 快照/diff 耗时。
- 长时间运行后的内存、待处理请求和文件描述符数量。

发布门槛：协议读取不因模拟慢 HTTP 停顿；队列、缓冲和临时文件受限；压力结束后资源回到稳定范围；性能不出现无法解释的明显回退。降低空闲 RSS 和 CPU 作为量化优化目标，在 P0 实测后记录具体预算，不预先宣称倍数收益。

## 八、最终推荐目录结构

```text
feishu-codex-bridge/
├── Cargo.toml
├── Cargo.lock
├── rust-toolchain.toml
├── rustfmt.toml
├── clippy.toml
├── deny.toml
├── .gitignore
├── .env.example                    # 旧变量兼容及迁移说明
├── bridge.example.toml
├── start.sh                        # 薄兼容入口
├── setup.sh                        # Rust 安装与配置引导
├── README.md
├── PLAN.md                         # 阶段、证据、待验收项
├── CHANGELOG.md
├── CONTRIBUTING.md
├── SECURITY.md
├── AGENTS.md
│
├── crates/
│   ├── bridge-core/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── ids.rs
│   │       ├── command.rs
│   │       ├── task.rs
│   │       ├── interaction.rs
│   │       ├── policy.rs
│   │       ├── view.rs
│   │       └── error.rs
│   ├── bridge-app/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── ports/
│   │       ├── admission.rs
│   │       ├── scheduler.rs
│   │       ├── sessions.rs
│   │       ├── interactions.rs
│   │       ├── execution.rs
│   │       └── delivery.rs
│   ├── bridge-feishu/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── auth.rs
│   │       ├── rest.rs
│   │       ├── inbound.rs
│   │       ├── cards/
│   │       ├── websocket/
│   │       ├── proxy.rs
│   │       └── wire/
│   ├── bridge-codex/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── process.rs
│   │       ├── transport.rs
│   │       ├── rpc.rs
│   │       ├── mapping.rs
│   │       ├── capabilities.rs
│   │       └── wire/
│   ├── bridge-local/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── state/
│   │       ├── migration.rs
│   │       ├── workspace.rs
│   │       ├── attachments.rs
│   │       ├── snapshot.rs
│   │       ├── diff.rs
│   │       └── generated_images.rs
│   ├── bridge-cli/
│   │   ├── Cargo.toml
│   │   ├── src/
│   │   │   ├── main.rs
│   │   │   ├── lib.rs
│   │   │   ├── config.rs
│   │   │   ├── bootstrap.rs
│   │   │   ├── supervisor.rs
│   │   │   ├── health.rs
│   │   │   └── telemetry.rs
│   │   └── tests/                 # 跨 crate 与进程集成测试入口
│   └── bridge-test-support/
│       ├── Cargo.toml
│       └── src/                   # fake ports、时钟、受限 fake server
│
├── xtask/
├── fixtures/
│   ├── codex/0.153.4/
│   ├── feishu/
│   ├── cards/
│   └── legacy-state/
├── schemas/
│   ├── codex/0.153.4/
│   ├── feishu/
│   └── state/
├── benches/
├── docs/
│   ├── architecture.md
│   ├── design.md
│   ├── development.md
│   ├── deployment.md
│   ├── operations.md
│   ├── migration.md
│   ├── compatibility.md
│   ├── testing.md
│   └── adr/
├── packaging/
│   ├── linux/
│   └── termux/
└── .github/
    ├── workflows/
    │   ├── ci.yml
    │   ├── coverage.yml
    │   ├── dependencies.yml
    │   ├── compatibility.yml
    │   └── release.yml
    ├── dependabot.yml
    └── pull_request_template.md
```

迁移期间保留原 Python 文件，并增加临时 `compat/feishu-sdk/`；P5 完成后从默认交付中移除。各 crate 的单元测试与模块相邻，集成测试置于实际 Cargo package 内，避免虚拟 workspace 根目录下的测试未被执行。

最终验收的核心标准是：**飞书升级主要修改 `bridge-feishu`，Codex 升级主要修改 `bridge-codex` 与协议样本；会话、调度、审批归属、Plan 和交付规则能够在不启动这两类第三方依赖的情况下独立验证。**
</proposed_plan>