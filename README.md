# Feishu Codex Bridge

把运行在 Termux（或兼容 Linux 环境）中的 [Codex app-server](https://developers.openai.com/) 接入飞书机器人。你可以在飞书私聊机器人，让 Codex 在指定工作目录中完成任务、审批操作、接收图片/文件，并把文本、卡片和交付物发回飞书。

这个项目偏向个人或小团队的“手机上的远程 Codex 终端”：服务驻留在你的设备上，代码、会话状态和 Codex 登录态都留在本机。

> 当前实现为单进程、单任务 worker。会话和模型设置按用户隔离，但多个任务会排队执行；它不是多租户的远程代码执行平台。

## 功能

- 飞书私聊文本 → 当前目录的 Codex thread；最终 Markdown 回复以飞书卡片展示。
- `/new`、`/resume`、`/archive`、`/archived`、`/compact` 管理 Codex 会话；服务重启后自动恢复已保存会话。
- `/cd` 可在受限工作区内按用户切换目录，支持移动端快捷目录卡。
- `/models` 卡片选择模型，模型偏好按飞书用户与工作目录保存。
- `/help` 提供移动端友好的控制面板；支持文本命令和卡片按钮。
- Codex 进度卡片、任务耗时、长回复安全分段、单任务停止按钮。
- 将 Codex 的审批请求渲染为说明卡片，可直接允许或拒绝；原审批卡片会更新为最终结果。
- 飞书图片/文件下载后作为本地输入交给 Codex；本轮修改的文件和生成图片会回传飞书。
- 服务管理、日志轮转、异常退避重启、飞书连接健康状态和跨重启消息去重。

## 快速开始

### 1. 准备飞书应用

在飞书开放平台创建企业自建应用，并完成以下配置：

1. 启用机器人。
2. 在“事件与回调 → 事件订阅”添加 `im.message.receive_v1`，选择“使用长连接接收”。
3. 在“事件与回调 → 回调配置”添加卡片回传 `card.action.trigger`，同样选择“使用长连接接收”。
4. 授予机器人读取/发送消息、发送/更新消息卡片及上传文件所需权限，并发布应用版本。
5. 记录 App ID、App Secret，建议同时记录自己的 `open_id` 用于白名单。

详细排障与迁移说明见 [部署指南](docs/deployment.md)。

### 2. 准备 Codex CLI

确保本机已经安装并登录 Codex CLI，且以下命令可用：

```sh
codex --version
```

本桥接通过 `codex app-server` 与本地 CLI 通信；它不会替你安装或登录 Codex，也不会额外实现项目文件上传通道。Codex CLI 本身的数据处理遵循你所使用的 Codex 产品与账户配置。

### 3. 运行引导脚本

将仓库克隆到设备后，切换到**希望 Codex 操作的工作目录**，运行：

```sh
/path/to/feishu-codex-bridge/setup.sh
```

脚本会自动安装缺失的 Termux Python、安装 Python 依赖、检查 Codex CLI、以交互方式创建权限为 `600` 的 `.env`，并启动服务。已有 `.env` 不会被覆盖。

只诊断、不修改环境：

```sh
/path/to/feishu-codex-bridge/setup.sh --check
```

配置成功后，在飞书私聊机器人发送 `/help`。

## 使用方式

普通文本会加入任务队列并交给 Codex。例如：

```text
检查当前目录的测试失败原因，修复后运行测试，并把修改的文件发给我。
```

常用命令如下。`/help` 会发送等价的可点击控制面板。

| 命令 | 作用 |
| --- | --- |
| `/help` | 显示控制面板。 |
| `/status` | 显示任务状态、队列、目录、会话、模型和桥接运行时长。 |
| `/cd` / `/cd <路径>` | 显示当前目录的子目录，或切换到工作区内的绝对/相对路径；不存在的目录会先请求确认创建。 |
| `/new` | 清除当前用户在当前目录的会话绑定；下次提问创建新会话。 |
| `/resume` / `/resume <thread_id>` | 按标题和完整 ID 列出最近 8 个对话，卡片支持恢复、归档；也可恢复指定 ID。 |
| `/archive <thread_id>` | 归档当前目录的对话，从普通列表隐藏并保留历史记录。 |
| `/archived` | 查看当前目录最近 8 个已归档对话，卡片支持取消归档。 |
| `/unarchive <thread_id>` | 取消归档；不自动切换当前对话，可再用 `/resume` 恢复。 |
| `/model` / `/models` / `/model <model>` | 查看、选择或设置模型。 |
| `/plan` / `/plan on` / `/plan off` | 查看、开启或关闭 Plan 模式；控制面板会显示状态型“开启 Plan”或“关闭 Plan”按钮，并原地更新。开启后普通消息只产出计划。 |
| `/compact` | 请求 Codex 压缩当前 thread 上下文。 |
| `/stop` | 中断当前任务，或取消等待队列中的任务。 |
| `/approve <id>` / `/deny <id>` | 用文本处理审批；通常直接点审批卡片即可。 |
| `/pair <配对码>` | 将发送者加入本机白名单；仅在配置配对码时可由未授权用户使用。 |

对话卡片底部的“列表导航”区域提供“查看已归档对话”或“返回普通对话列表”，与每条对话的操作分开。

归档当前对话后会清除对应的会话绑定，下次提问自动创建新对话。归档会使用 Codex 自带接口，也可能归档派生子对话；不删除项目文件。卡片操作绑定用户、聊天和目录，10 分钟后或重启后失效，请重新打开列表。桥接有执行中或排队任务时不能归档、取消归档或恢复对话；对话操作期间到达的新提问会提示稍后重发。

### 附件与交付物

- 图片下载到当前 `<cwd>/feishu-inbox/` 并作为 app-server `localImage` 输入交给 Codex。
- 文件下载后，以本地路径附加到提示词。
- 代码、Markdown、JSON、配置等工程文本只发本轮 unified diff 卡片，不上传原文件。卡片显示相对路径、新增/修改/删除、增删行数及上下文；长差异分段并明确标注截断，仅时间戳变化不发卡。
- 图片（含 SVG）、PDF、Office、音视频、压缩包等成果继续作为附件发送，不另发二进制差异卡。每轮合计最多 10 个，默认单文件上限 20 MiB；生成图片参与去重和总数限制。
- 过大、无法读取或无法解码的工程文本只显示原因，不回退上传源码。未知二进制只提示变化，不自动上传。快照默认最多 200 个文件、每文件最多读取 256 KiB 文本，不保证覆盖超出扫描上限的文件。
- `feishu-inbox/`、`.runtime/`、`.git/`、`.feishu-codex*`、真实 `.env`、符号链接，以及 `__pycache__`、`.pyc`/`.pyo`、虚拟环境、`node_modules`、常见测试缓存、`target`/`build`/`dist` 和编译产物均不读取、不比较、不上传。

## 运维

统一通过 `start.sh` 管理服务：

```sh
./start.sh start       # 后台启动（默认）
./start.sh stop        # 安全停止监督器和桥接子进程
./start.sh restart     # 重启
./start.sh status      # 服务与飞书连接健康状态
./start.sh logs        # 最近 60 行脱敏日志
./start.sh foreground  # 前台运行，适合排障
```

监督器在桥接异常退出时以 2、4、8…30 秒的退避间隔重启。日志保存于 `.runtime/bridge.log`，单文件最多 2 MiB，保留 3 个历史副本。详见[运行手册](docs/operations.md)。

## 配置

复制 `.env.example` 为 `.env`，或使用 `setup.sh` 生成。`.env` 不会提交到 Git。

| 变量 | 必填 | 说明 |
| --- | --- | --- |
| `FEISHU_APP_ID` / `FEISHU_APP_SECRET` | 是 | 飞书企业自建应用凭据。 |
| `FEISHU_ALLOWED_OPEN_IDS` | 建议 | 逗号分隔的允许用户 `open_id`；未设置配对码时留空表示不做白名单限制。 |
| `FEISHU_PAIRING_CODE` | 否 | 设置后可用 `/pair <配对码>` 安全地自助加入白名单；配对完成后删除该项并重启。 |
| `CODEX_BRIDGE_CWD` | 是 | 初始 Codex 工作目录，以及 bridge 会话、设置、去重等状态文件的位置。必须在工作区根内。 |
| `CODEX_WORKSPACE_ROOT` | 是 | `/cd` 可访问的工作区根目录；目录及其符号链接解析后的目标都不得越出该范围。 |
| `CODEX_MODEL` | 否 | 默认模型；也可以用 `/models` 按用户设置。 |
| `CODEX_APP_SERVER` | 否 | app-server 启动命令，默认启用 `collaboration_modes` 实验特性。 |
| `CODEX_SANDBOX_MODE` | 否 | 默认 `workspaceWrite`；Termux 无法运行 Linux sandbox 时，必须在 `.env` 中显式设置 `dangerFullAccess`，setup 首次配置会询问。每个 turn 仍使用 Codex `Ask for approval`（`on-request`）。 |
| `CODEX_MAX_ATTACHMENT_BYTES` | 否 | 回传文件上限，默认 `20971520`。 |
| `CODEX_APPROVAL_TIMEOUT_SECONDS` | 否 | 审批超时自动拒绝时间，默认 `600` 秒。 |
| `CODEX_QUESTION_TIMEOUT_SECONDS` | 否 | Codex 选择题等待回答时间，默认 `600` 秒。 |
| `CODEX_SESSION_FILE` / `CODEX_SETTINGS_FILE` / `CODEX_SEEN_MESSAGES_FILE` / `CODEX_ALLOWED_OPEN_IDS_FILE` | 否 | 覆盖状态文件默认位置。 |

## 安全边界

- **务必设置 `FEISHU_ALLOWED_OPEN_IDS`。** 留空时，任何能私聊机器人的用户都可向本机 Codex 发起任务。
- 不便取得 `open_id` 时，可暂时设置一个长随机 `FEISHU_PAIRING_CODE`，然后让本人发送 `/pair <配对码>`。每位成功配对的用户都会保存到权限为 `600` 的 `.feishu-codex-allowed-open-ids`；完成后删除配对码并重启，避免继续发放访问权限。
- `.env` 和状态文件以 `600` 权限写入；不要将它们提交、截图或发到聊天中。
- 日志不记录聊天全文、审批命令内容、文件内容、token、WebSocket 认证参数或明文用户 ID；事件以短哈希关联。
- bridge 会在你设置的工作目录下载附件、运行 Codex 并扫描交付物；请使用明确且受信任的项目目录。
- 飞书审批卡片只是 Codex 请求授权的入口，不能替代对命令、路径和文件改动本身的审查。

## 架构与后续开发

- [架构与数据流](docs/architecture.md)：组件边界、线程模型、状态、协议和故障恢复。
- [设计决策](docs/design.md)：为什么使用单 worker、卡片 JSON 2.0、持久化去重和 SDK 兼容补丁。
- [开发指南](docs/development.md)：调试、测试、增加命令/卡片、修改 app-server 适配层。
- [部署指南](docs/deployment.md)：新设备部署、飞书后台配置、迁移和排障。
- [运行手册](docs/operations.md)：日志、状态机、故障定位和安全操作。

## 测试

测试不需要飞书凭据或网络：

```sh
python -m unittest discover -s tests -v
```

当前回归测试覆盖会话/模型持久化、跨重启消息去重、移动端卡片布局、长代码块分段、状态卡和服务健康状态。

## 项目结构

```text
.
├── bridge.py              # 飞书 ↔ Codex app-server 主桥接
├── service.py             # 锁、日志轮转、健康状态、异常重启监督器
├── start.sh               # 统一服务入口，加载 .env
├── setup.sh               # 新设备交互式部署引导
├── .env.example           # 配置模板
├── docs/                  # 架构、设计、开发、部署与运维文档
└── tests/                 # 无网络单元/回归测试
```

## 许可与贡献

当前仓库尚未声明开源许可证；在公开复用或接受外部贡献前，请先补充 `LICENSE` 并明确贡献规则。

提交改动前请运行测试，并同步更新相关 `docs/` 与 `PLAN.md`。涉及飞书卡片、WebSocket 回调或 Codex app-server 协议的改动，除单测外还应完成真实飞书端验收。

### 飞书专用代理

默认使用启动服务时继承的 Linux 代理环境变量。HTTPS 接口和安全 WebSocket 可通过 `https_proxy` / `HTTPS_PROXY` 配置，并遵循 `no_proxy` / `NO_PROXY`；没有代理变量时直连。WebSocket 还支持 `wss_proxy`。需要 `websockets>=15` 及带 `_ws_connect_kwargs` 的 lark-oapi。

更换代理环境后，在新环境的终端执行 `./start.sh restart` 即可，无需修改项目配置。运行中的进程不会自动获得终端后来修改的环境变量；桌面设置中的代理也需要由启动环境导出。若 `.env` 自己设置了代理变量，应删除固定值，以免覆盖终端环境。

`FEISHU_PROXY_URL` 仅用于显式覆盖，例如 `http://127.0.0.1:7890`；通常无需配置。依赖清单已包含 HTTP 与 SOCKS 代理支持；升级时执行 `python -m pip install -r requirements.txt`。

## Rust 重构（进行中）

已建立 Cargo workspace、核心状态机、有界调度、JSON 存储与迁移工具，并提供 `bridge run --config bridge.toml` 前台最小版：Rust 处理授权文本、Codex 执行、回复、状态、停止、会话新建/恢复，以及模型和 Plan 设置，Python 仅运行 SDK 长连接。使用方式和限制见 [最小运行版](docs/minimal-runtime.md)。Python 服务入口仍为 `start.sh`，Rust 前台入口单独运行，两者使用同一 App ID 锁互斥。

详见 [Rust 实施方案与进度](docs/rust-refactor.md) 和 [迁移工具使用说明](docs/migration.md)。开发验证：`cargo test --workspace --locked`，然后运行既有 Python 回归。

完整 Rust 重构设计基线见 [完整重构计划](docs/plans/rust-refactor-original.md)，实施进度见 [方案与状态](docs/rust-refactor.md)。
