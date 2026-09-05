# Feishu Codex Bridge

Termux 下通过飞书长连接使用当前目录的 Codex app-server。

## 安装

```sh
pkg install python
python -m pip install -r requirements.txt
cp .env.example .env
```

在飞书企业自建应用中开启机器人，订阅 `im.message.receive_v1` 和 `card.action.trigger`，订阅方式选择“使用长连接接收事件”，并授予机器人读取/发送消息、发送/更新消息卡片及上传文件的权限。

确认本机已登录 Codex，且 `codex app-server` 可启动。然后在目标项目目录启动：

```sh
set -a; . ./.env; set +a
python /path/to/feishu-codex-bridge/bridge.py
```

普通文本会进入当前目录的 Codex thread。会话 ID 自动保存到 `.feishu-codex-session`，重启后第一次提问会自动恢复。支持 `/new`、`/resume [thread_id]`、`/model [model]`、`/models`、`/status`、`/stop`、`/compact`、`/approve <id>`、`/deny <id>`。

机器人会使用交互式卡片展示状态、进度、最终回复、审批和常用命令；处理中卡片可直接点击“停止任务”，审批卡片可直接点击“允许/拒绝”，`/stop`、`/approve` 等文本命令仍然兼容。每轮完成后会回传最终 Markdown，并上传当前目录中本轮新增或修改的文件（最多 10 个，单文件大小由 `CODEX_MAX_ATTACHMENT_BYTES` 控制）。飞书图片/文件会下载到 `feishu-inbox` 后交给 Codex 读取；该目录不会被当成交付物再次上传。审批请求超过 `CODEX_APPROVAL_TIMEOUT_SECONDS` 秒会自动拒绝。

首次测试建议发送：

```text
/status
请只回复：连接测试成功
```
