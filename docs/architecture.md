# 架构与数据流

本文描述当前实现。主入口为 `bridge.py`，后台生命周期由 `service.py` 管理。

## 目标与边界

目标是把本地 Codex app-server 安全、可观察地暴露为飞书机器人的私聊入口：保留 Codex thread，支持审批和附件，并把可读的结果及交付物回传飞书。

当前不做多设备分布式调度、多 worker 并发执行、任意用户隔离沙箱或设备开机自启。一个飞书 App ID 应只运行一个实例。

## 组件图

```text
                      ┌──────────────────────────────────────┐
                      │              service.py              │
                      │ flock + 全局 App 锁 + 日志 + 重启策略 │
                      └──────────────────┬───────────────────┘
                                         │ 子进程 stdout
                                         ▼
┌──────────────┐   长连接事件    ┌──────────────────────────────────────┐   JSON-RPC stdio   ┌──────────────────┐
│ 飞书机器人    │◀───────────────▶│              bridge.py               │◀──────────────────▶│ codex app-server │
│ 消息/卡片/文件│   REST 发送      │ Feishu / Bridge / CodexServer 三层   │                    │ 本机 Codex CLI    │
└──────────────┘                  └──────────┬───────────────┬───────────┘                    └──────────────────┘
                                               │               │
                                               ▼               ▼
                                   工作目录状态文件          feishu-inbox/
                                   session/settings/dedup    附件暂存
```

### `Feishu`

负责 tenant access token 缓存、文本/卡片发送、卡片更新、图片或文件上传及附件下载。HTTP 客户端默认使用 `trust_env=True`，SDK WebSocket 恢复环境代理发现，地址获取保留 requests 原生环境代理行为。`FEISHU_PROXY_URL` 可显式覆盖三条连接路径。SDK 适配仅修改飞书模块引用，不修改共享 requests 模块。

### `CodexServer`

启动 `CODEX_APP_SERVER`（默认 `codex app-server --enable collaboration_modes`），通过 stdin/stdout 使用 JSON-RPC，维护 `key → thread_id`、活动 turn、审批请求和等待响应表。每个 turn 显式使用 Codex `on-request`（Ask for approval）审批策略。默认 sandbox 为 `workspaceWrite`，可写根目录仅为该 turn 已固定的工作目录且网络关闭；Termux 无法运行 Linux sandbox 时，用户必须在 `.env` 显式选择 `dangerFullAccess`，此时目录边界由本机用户权限负责。

**一个 reader 原则**：stdout 只能由 `_receive_rpc` 读取。该线程将响应按 JSON-RPC `id` 分发给 `pending` 队列，将通知转交给 `notifications`；`_dispatch_events` 再处理通知，并把 `turn/completed` 放入 `completions`。新增 RPC 方法必须调用 `request()` 或 `send()`，绝不能直接读取 `process.stdout`。

### `Bridge`

`Bridge` 把飞书事件转化为命令或任务，并维护用户状态、进度卡、审批卡、选择题、Plan 模式、交付物和去重日志。它创建四个后台线程：

| 线程 | 职责 |
| --- | --- |
| `worker` | 从 FIFO `jobs` 队列取一个普通消息，执行完整 Codex turn。 |
| `approval_reaper` | 每 5 秒扫描审批，超时后自动拒绝。 |
| `progress_worker` | 每 2 秒更新当前任务的飞书进度卡。 |
| `question_reaper` | 处理超时未回答的 Codex 选择题。 |

飞书回调会快速返回；耗时命令（如 `/resume`、`/models`）会放到独立线程，避免阻塞 WebSocket 心跳。

## 入站与任务流程

```text
im.message.receive_v1
        │
        ▼
Bridge.receive
        │  1. 检查并持久化 message_id 去重
        │  2. 检查 open_id 白名单
        │  3. 解析文本或附件元数据
        ├── /command ──▶ 后台 command 线程 ──▶ 飞书卡片/JSON-RPC 请求
        └── 普通消息 ──▶ jobs FIFO ──▶ 单 worker ──▶ CodexServer.turn
```

附件下载到任务目录的 `<cwd>/feishu-inbox/`；图片以 `localImage` 输入传入 app-server，其他文件以本地路径附加到提示词。普通任务会创建一次性 `task_id` 和进度卡，记录工作目录快照，按需恢复会话，等待匹配的 `turn/completed`，保存 thread ID，更新最终卡并上传交付物。

## 卡片、审批与交付物

默认使用飞书 Card JSON 2.0。有说明的模型/会话条目采用“说明在上、满宽按钮在下”，避免手机截断；控制面板采用短标签两列。按钮回调最终都会转为 `Bridge.command()` 的文本命令，确保业务逻辑只有一套。

运行中的任务更新同一张卡。最终内容超过实用卡片长度时，`split_card_content()` 约按 5,600 字符拆分，并在跨卡时闭合、重开 Markdown 代码围栏。

审批通知会展示操作类型、原因、命令、目录、文件变动和有限长度 diff，并显示 10 分钟处理时限；超时后原卡会更新为自动拒绝。审批 ID 与聊天、原卡片绑定；旧卡、重复操作和超时审批都会安全拒绝。Codex 选择题卡同样显示剩余时限，超时更新原卡并回传空答案。计划完成卡的三项后续操作也有时限，超时后原卡更新为失效状态。

`/plan on` 为当前用户和目录开启 Codex Plan 模式。`/help` 的 Plan 按钮是状态型开关，使用显式开启/关闭动作并在原控制面板卡片更新，旧卡重复点击也是幂等的。计划完成时先更新原进度卡，再发送完整 Markdown“计划详情”卡（优先结构化 plan item，缺失时明确标记为最终文本降级展示），最后发送置底的“计划下一步”卡，其中包含“实现此计划”“清空上下文后实现”“留在 Plan 模式”三项操作。两种实施路径都会保留计划文本，前者保留当前 thread。Codex 发出 `item/tool/requestUserInput` 时，bridge 逐题渲染选项卡并以 JSON-RPC 原请求 ID 回写选择；“其他”回答由用户下一条普通文本提供，超时返回空答案。

bridge 启动时读取 app-server 的模型列表并缓存标记为默认的模型。进入或退出协作模式时优先使用用户已选模型，否则使用这个实际可用的默认值；不会猜测模型 ID。失败的 turn 会把 app-server 提供的错误摘要回传到最终卡片。

交付使用本轮开始和结束各一次的受限快照。扫描先剪枝缓存、依赖、编译输出和桥接状态目录，跳过符号链接、非普通文件和真实 `.env`。工程文本比较内容，按文件发送带操作类型与增删行数的 unified diff，不上传源码；已知图片/文档/媒体/压缩包仅进入附件队列，未知二进制仅提示。结束快照供差异和上传共用，生成图片去重后纳入每轮 10 个附件上限；超限快照不把仍存在的遗漏文件误报为删除。

## 状态与去重

默认状态位于 `CODEX_BRIDGE_CWD`，通过“临时文件 → `os.replace`”原子保存，且权限为 `0600`：

| 文件 | 内容 | 隔离键 |
| --- | --- | --- |
| `.feishu-codex-session` | `{user:cwd: thread_id}` | 用户 + 目录 |
| `.feishu-codex-settings` | `{models, directories, plan_modes: {user:cwd: true}}` | 用户 + 目录 / 用户 |
| `.feishu-codex-seen-messages` | 最近最多 1,000 个消息 ID | 全桥接 |
| `.runtime/health.json` | 服务阶段、PID、更新时间 | 服务实例 |

消息 journal 使飞书重投、断线重连和服务重启后不会重复执行同一消息。它偏向“至多一次”：若在记录 ID 后异常退出，该消息可能不会重试；这比重复执行可能改文件或运行命令的任务更安全。

`/cd` 只接受 `CODEX_WORKSPACE_ROOT` 内的目录；解析符号链接后仍须在该根目录内。每位用户的目录会持久保存，且入队任务会固定提交时的目录，避免后续切换影响运行中的工作。

## 服务监督与 SDK 兼容

`start.sh` 加载 `.env` 并调用 `service.py`。监督器有项目内锁和按 `FEISHU_APP_ID` 派生的本机全局锁；bridge 异常退出后按 2、4、8…30 秒退避重启。日志会过滤 token、WebSocket 认证参数、App ID 和 App Secret。健康状态由 `bridge_ready` 和 SDK `connected` 日志推进：`starting → ready → connected`。

部分 `lark-oapi` 1.7.x 会在长连接中忽略 CARD 帧。`patch_lark_card_callback()` 运行时修复该分发路径。升级 SDK 后必须真实点击 `/help`、模型、恢复、审批和停止按钮确认补丁仍正确。

## 并发边界与扩展

当前任务队列全局 FIFO、只有一个 worker，因此 `turn_text`、`current_chat`、审批和进度卡不会串线。代价是多个用户的普通任务会排队。未来如需并发，应为每个 `user_id + cwd` 建立独立 `CodexServer`、任务队列、进度/审批状态和工作目录锁；不能只增加 worker 数量。

### 对话归档

`CodexServer` 封装 `thread/read`、`thread/archive`、`thread/unarchive` 与 `thread/list(archived=...)`。对话标题使用 `name → title → preview → 未命名`；所有操作以完整 ID 定位。卡片令牌保存用户、聊天、目录、来源消息、可操作 ID 和 10 分钟有效期，重启或重复点击会失效。

`thread_lock` 串行化列表、归档、恢复和 Plan 后续操作。`task_lock` 下设置 `thread_mutation`，有任务时拒绝切换；RPC 期间释放短锁，普通消息看到标记后提示重发，不阻塞 WebSocket 心跳等待 RPC。成功归档清除所有匹配的内存/持久化绑定及旧 Plan 操作；`thread/archived` 通知经既有分发器清理派生对话绑定。失败不提前清除绑定。

## Rust 迁移边界

新增 workspace 与当前 Python 运行路径并存。Rust 核心、存储和迁移工具可离线使用，尚无生产事件循环或 SDK 替代。工程边界和后续调用链见 [Rust 实施方案](rust-refactor.md)。本文前述 Python 运行流程仍是现行生产实现。
