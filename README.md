# Feishu Codex Bridge

后台管理：`./start.sh start`（默认）、`./start.sh stop`、`./start.sh restart`、
`./start.sh status`、`./start.sh logs`。`status` 会区分桥接启动中、已初始化、已连上飞书及自动重启中；前台排查使用 `./start.sh foreground`。
请统一用此入口启动，进程锁防止同一项目重复启动。
日志在 `.runtime/bridge.log`，每份 2 MiB，保留 3 份备份；认证参数自动过滤。
运行目录不作为交付物上传。监督器会在桥接异常退出后自动退避重启；未配置设备开机自启。
同一机器人在不同项目目录中也只能运行一个实例，避免重复回复。

Termux 下通过飞书长连接使用当前目录的 Codex app-server。

## 在另一台 Termux 上部署

推荐使用引导脚本。将仓库复制或 `git clone` 到新设备后，在**希望 Codex 操作的项目目录**执行：

```sh
/path/to/feishu-codex-bridge/setup.sh
```

脚本会自动安装 Python（仅 Termux 缺少时）、安装 Python 依赖、检查 Codex CLI、交互式生成权限为 `600` 的 `.env`，然后启动服务。已有 `.env` 不会被覆盖。只检查环境而不修改内容可执行：

```sh
/path/to/feishu-codex-bridge/setup.sh --check
```

以下事项无法由程序代替，首次部署时按提示完成即可：

1. 在目标设备安装并登录 Codex CLI，确认 `codex --version` 可用。
2. 在飞书开放平台创建企业自建应用并启用机器人；复制 App ID 与 App Secret 给引导脚本。
3. 在“事件与回调”配置事件订阅 `im.message.receive_v1`，在“回调配置”配置卡片回传 `card.action.trigger`；两项均选择“使用长连接接收”。
4. 为机器人授予读取/发送消息、发送/更新消息卡片及上传文件权限，并发布应用版本。
5. 建议填写自己的飞书 `open_id` 作为白名单；不知道时可暂留空进行私聊测试，但这会允许任何能私聊机器人的人使用它。

运行结束后，在飞书私聊机器人发送 `/help`。迁移到新设备不会迁移旧设备的 Codex 登录态、飞书凭据或会话记录；如需延续旧会话，请安全地手工迁移工作目录内的 `.feishu-codex-session` 与 `.feishu-codex-settings`。

## 手动安装（排障用）

```sh
pkg install python
python -m pip install -r requirements.txt
cp .env.example .env
```

在飞书企业自建应用中开启机器人，在“事件与回调”中分别配置：事件订阅 `im.message.receive_v1`，以及“回调配置”中的卡片回传交互 `card.action.trigger`；两者都选择“使用长连接接收”。并授予机器人读取/发送消息、发送/更新消息卡片及上传文件的权限。

确认本机已登录 Codex，且 `codex app-server` 可启动。然后在目标项目目录启动：

```sh
set -a; . ./.env; set +a
python /path/to/feishu-codex-bridge/bridge.py
```

普通文本会进入当前目录的 Codex thread。会话 ID 自动保存到 `.feishu-codex-session`，模型偏好保存到 `.feishu-codex-settings`，并按飞书用户和工作目录隔离；重启后首次提问会自动恢复。`/models` 会发送模型选择卡片。`/status` 会显示任务状态、队列、会话、模型和桥接运行时长。支持 `/new`、`/resume [thread_id]`、`/model [model]`、`/models`、`/status`、`/stop`、`/compact`、`/approve <id>`、`/deny <id>`。

最近处理过的飞书消息 ID 会保存到 `.feishu-codex-seen-messages`（权限 `600`，最多 1,000 条），因此服务重启或飞书重投旧事件时不会重复执行或重复回复。日志仅记录消息 ID 的短哈希，便于排查，不记录聊天内容。

机器人会使用交互式卡片展示状态、进度、最终回复、审批和常用命令；处理中卡片可直接点击“停止任务”，审批卡片可直接点击“允许/拒绝”，`/stop`、`/approve` 等文本命令仍然兼容。每轮完成后会回传最终 Markdown，并上传当前目录中本轮新增或修改的文件（最多 10 个，单文件大小由 `CODEX_MAX_ATTACHMENT_BYTES` 控制）。飞书图片/文件会下载到 `feishu-inbox` 后交给 Codex 读取；该目录不会被当成交付物再次上传。审批请求超过 `CODEX_APPROVAL_TIMEOUT_SECONDS` 秒会自动拒绝。

首次测试建议发送：

```text
/status
请只回复：连接测试成功
```

## 回归测试

不需要飞书凭据或网络访问：

```sh
python -m unittest discover -s tests -v
```
