# 配置与命令参考

本文是 `bridge.toml`、凭据环境变量、`bridge` 子命令与聊天命令的完整参考。配置模型定义在 `crates/bridge-cli/src/lib.rs`（全部段落 `deny_unknown_fields`，未知字段导致解析失败而不是被忽略）；校验在 `Config::validate` 与 `bridge config check`。

## bridge.toml 示例（带注释）

```toml
# Rust 迁移配置。当前生产服务仍使用 .env 的部署应显式迁移。

[workspace]
root      = "/absolute/workspace"                 # 工作区根（canonicalize 后必须存在且是目录）
cwd       = "/absolute/workspace/project"         # 初始工作目录，必须位于 root 之内
state_dir = "/absolute/private/bridge-state"      # 状态/健康/日志目录（运行期创建）

[access]
mode = "restricted"                              # restricted | open
allowed_open_ids = ["ou_replace_me"]             # 静态白名单，可多值
# pairing_code_env = "FEISHU_PAIRING_CODE"       # 无白名单时用配对码授权
# 旧的无限制部署必须显式选择 mode = "open"

[codex]
executable = "codex"
args       = ["app-server", "--enable", "collaboration_modes"]
sandbox    = "workspaceWrite"                    # workspaceWrite | dangerFullAccess

# `bridge run` 必需；初次验证请使用独立的测试机器人。
[feishu]
app_id_env     = "FEISHU_APP_ID"                 # 凭据只存环境变量名，绝不存值
app_secret_env = "FEISHU_APP_SECRET"
# proxy_env    = "FEISHU_PROXY_URL"              # 可选：HTTP/SOCKS 代理地址
```

## 字段参考

### [workspace]

| 字段 | 类型 | 约束 |
|---|---|---|
| `root` | 路径 | 必须绝对路径；`Workspace::new` canonicalize 后必须是存在的目录；运行期所有目录操作以它为边界 |
| `cwd` | 路径 | 必须绝对路径且解析后仍在 `root` 内（「初始工作目录无效或越出工作区」） |
| `state_dir` | 路径 | 必须绝对路径；存放状态、健康、日志与监督文件；更新部署时须保留 |

### [access]

| 字段 | 类型 | 约束 |
|---|---|---|
| `mode` | `restricted` \| `open` | restricted 需要白名单或 `pairing_code_env`（否则「restricted 模式需要白名单或 pairing_code_env」）；旧的无限制部署必须显式 `open` |
| `allowed_open_ids` | 字符串数组（默认空） | 运行期与状态文件里已配对的 `allowed_open_ids` 取并集 |
| `pairing_code_env` | 环境变量名 | 名字必须为 `[A-Za-z0-9_]+`；码长 16–256 字节且无空白；配对成功后用户 id 持久化，码本身永不落盘 |

### [codex]

| 字段 | 类型 | 约束 |
|---|---|---|
| `executable` | 路径 | 非空；不经 shell 直接 spawn |
| `args` | 字符串数组 | 非空；典型值 `["app-server", "--enable", "collaboration_modes"]` |
| `sandbox` | `workspaceWrite` \| `dangerFullAccess` | workspaceWrite：可写根 = 任务目录、禁网络；dangerFullAccess 关闭工作区包含（Codex 可写工作区之外），仅在接受风险时使用，且不要与不可信工作区组合 |

### [feishu]

| 字段 | 类型 | 约束 |
|---|---|---|
| `app_id_env` / `app_secret_env` | 环境变量名 | `bridge run` 必需；缺失或空 →「凭据环境变量不可用」 |
| `proxy_env` | 环境变量名（可选） | 值形如 `http(s)://` 或 `socks5(h)://user:pass@host:port`；同时作用于 WebSocket 与 REST |

代理也可不经配置、直接使用标准环境变量（`https_proxy/HTTPS_PROXY`、`wss_proxy/WSS_PROXY`、`http_proxy`、`all_proxy`、`no_proxy`）；显式 `proxy_env` 优先于环境变量。NO_PROXY 支持域后缀、CIDR 与端口限定。

## 凭据环境变量

| 变量 | 必填场景 |
|---|---|
| `FEISHU_APP_ID` / `FEISHU_APP_SECRET`（或配置声明的其他名字） | `bridge run` / `service start` / `guard` 前必须已在环境中 |
| `FEISHU_PAIRING_CODE`（或 `pairing_code_env` 声明的名字） | restricted 且无白名单、且尚无已配对用户时必填 |

`bridge credentials --file <cfg>` 校验这些变量；`--print` 把缺失项输出为 `export NAME='value'` 语句（供脚本 `eval`），配对码存在时追加 `BRIDGE_RUST_PAIRING_PRESENT=1`。交互终端里输入经 termios 隐藏；任何值都不会出现在错误信息、日志或配置里。

## 运行期环境变量（可调）

| 变量 | 作用 |
|---|---|
| `CODEX_SERVICE_GLOBAL_STATE` | 覆盖 App ID 全局锁目录（默认 `$HOME/.feishu-codex-bridge`） |
| `CODEX_GENERATED_IMAGES` / `CODEX_HOME` | 生成图目录探测顺序（默认回退 `$HOME/.codex/generated_images`） |
| `BRIDGE_RUST_*`（setup.sh/start.sh） | 脚本级覆盖：ROOT/CWD/STATE_DIR/ALLOWED_USERS/SANDBOX/BIN/CONFIG/AUTO_INSTALL/PAIRING_PRESENT |

## bridge 子命令

```text
bridge guard       --config <路径>            外层守护（不建议手工使用；start.sh/前台运行用）
bridge supervise   --config <路径>            前台监督（同上）
bridge run         --config <路径>            前台运行桥接（需要 [feishu] 与凭据）
bridge service     --config <路径> start|stop|restart|status
bridge status      --config <路径>            只读：运行锁 + 快照 + LivePhase，不连接飞书
bridge config init --output <路径> --root --cwd --state-dir
                   [--allowed-user <id>]… [--danger-full-access]
                                              生成 0600 私有配置；拒绝覆盖；不写凭据；不启动
bridge config check --file <路径>             离线检查 TOML（不校验凭据、不连接外部）
bridge credentials --file <路径> [--print]    校验/补齐凭据；--print 输出 export 语句
bridge check-command '<文本>'                  验证聊天命令语法，不执行
```

`service start` 的启动链与锁文件见 [架构说明](architecture.md) 的「进程链与监督」；状态输出解读见 [运维指南](operations.md)。

## 聊天命令

文本命令由 `bridge-core::command::Command::parse` 解析（大小写不敏感、参数保留空格）；卡片按钮把同一批命令经不透明令牌回传。任务执行中，会话/目录/设置类命令会被变更门拒绝；查询类（`/status /help /stop /model /models /resume /archived /plan /cd`）始终可用。

| 命令 | 说明 |
|---|---|
| `/pair <配对码>` | 配对授权（restricted 且无白名单时）；60 秒窗口最多 10 次 |
| `/help` | 控制面板（按钮 + 完整命令说明） |
| `/status` | 运行状态、已用时长、当前目录、等待/保存中计数 |
| `文本任务` | 直接发送文本即开始任务；`/` 开头按命令解析 |
| `/stop [任务]` | 停止当前任务并取消本会话排队任务 |
| `/cd [路径]` | 无参浏览当前目录（最多 20 项）；带参切换（支持相对/绝对/空格/`..`，限工作区内） |
| `/cd-confirm <编号>` | 确认创建 `/cd` 提议的新目录（10 分钟内有效、须同用户同聊天同目录） |
| `/new` | 解绑当前会话（下次提问自动创建新会话） |
| `/resume [ID]` | 恢复会话；无参列出当前目录最近会话（最多 8 项） |
| `/archive <ID>` / `/archived` / `/unarchive <ID>` | 归档管理；归档会清除本地全部该线程绑定 |
| `/model [ID\|default]` | 设置模型；`default` 恢复桥接默认（`gpt-5.6-luna`） |
| `/models` | 列出可用模型（最多 20 项） |
| `/plan [on\|off\|开启\|关闭\|退出]` | Plan 模式开关 |
| `/compact` | 压缩当前会话上下文（全程持变更门） |
| `/approve <令牌>` / `/deny <令牌>` | 文本形态的审批（与卡片按钮等价） |

卡片交互产生的输入：审批/问答按钮、`/plan-action`（implement/fresh/stay）、`/choice`、`/answer`（自由文本答案，≤16 KiB）——这些都由运行时校验属主与令牌后执行，无法通过伪造文本绕过。

## state_dir 文件布局

| 路径（相对 state_dir） | 内容 |
|---|---|
| `state.json` / `state.previous.json` | 持久状态与写前备份（0600） |
| `seen-messages.json` | 消息去重日志（FIFO 1000 条） |
| `state.lock` | 状态存储单写者锁 |
| `runtime/health.json` | 健康快照（原子发布） |
| `runtime/events.jsonl`（+ `.1..3`） | 运行诊断日志（2 MiB 轮转 ×4） |
| `runtime/service.lock` | bridge 运行锁 |
| `runtime/supervisor.lock`、`control.sock`、`control.lock` | supervise 层锁与控制 socket |
| `runtime/supervisor/snapshot.json`、`events.jsonl` | supervise 层快照与日志 |
| `guard/runtime/guard.lock`、`guard.sock`、`guard/runtime/supervisor/…` | guard 层锁、socket、快照与日志 |

工作区内的 `feishu-inbox/` 存放本轮接收的附件（`{task}-{index}-{净化文件名}`）；`.bridge-state/`、`.runtime/` 等目录与 `.env`、`.feishu-codex*` 文件永远不进快照与 diff。
