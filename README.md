# Feishu Codex Bridge

后台管理：`./start.sh start`（默认）、`./start.sh stop`、`./start.sh restart`、
`./start.sh status`、`./start.sh logs`。`status` 会区分桥接启动中、已初始化、已连上飞书及自动重启中；前台排查使用 `./start.sh foreground`。
请统一用此入口启动，进程锁防止同一项目重复启动。
日志在 `.runtime/bridge.log`，每份 2 MiB，保留 3 份备份；认证参数自动过滤。
运行目录不作为交付物上传。监督器会在桥接异常退出后自动退避重启；未配置设备开机自启。
同一机器人在不同项目目录中也只能运行一个实例，避免重复回复。

Termux 下通过飞书长连接使用当前目录的 Codex app-server。

## 安装

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

机器人会使用交互式卡片展示状态、进度、最终回复、审批和常用命令；处理中卡片可直接点击“停止任务”，审批卡片可直接点击“允许/拒绝”，`/stop`、`/approve` 等文本命令仍然兼容。每轮完成后会回传最终 Markdown，并上传当前目录中本轮新增或修改的文件（最多 10 个，单文件大小由 `CODEX_MAX_ATTACHMENT_BYTES` 控制）。飞书图片/文件会下载到 `feishu-inbox` 后交给 Codex 读取；该目录不会被当成交付物再次上传。审批请求超过 `CODEX_APPROVAL_TIMEOUT_SECONDS` 秒会自动拒绝。

首次测试建议发送：

```text
/status
请只回复：连接测试成功
```
