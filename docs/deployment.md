# 部署与迁移指南

## Rust 原生版

使用 [Rust 部署与交接指南](rust-deployment.md) 完成配置、后台启停、迁移及本机打包。Rust 不读取 Python `.env`。本批新增入口待验证，真实飞书验收另行进行。

## Python 保留版本流程

在希望 Codex 操作的目录执行：

```sh
/path/to/feishu-codex-bridge/setup.sh
```

脚本询问 App ID、App Secret、`open_id` 白名单、初始工作目录和允许切换的工作区根目录，并自动安装 Python 依赖、生成私有 `.env`、启动服务。工作区根目录必须包含初始目录；前提是已经安装并登录 Codex CLI。

## 飞书后台核对表

| 位置 | 配置 |
| --- | --- |
| 应用能力 | 已启用机器人。 |
| 事件订阅 | 添加 `im.message.receive_v1`，使用长连接。 |
| 回调配置 | 添加 `card.action.trigger`，使用长连接；它不在普通事件订阅列表。 |
| 权限 | 读取/发送消息、发送/更新卡片、上传图片/文件。 |
| 版本管理 | 每次权限或订阅改动后发布应用版本。 |

首次验收：运行 `./setup.sh --check` 和 `./start.sh status`，然后在飞书发送 `/status`、`/help`、`请只回复：连接测试成功`，并点击控制面板的状态和模型按钮。

Termux 若出现 Codex sandbox 命令统一以退出码 182 失败，重新运行 setup 并在提示中选择 `dangerFullAccess`，或在 `.env` 中显式设置 `CODEX_SANDBOX_MODE=dangerFullAccess` 后重启服务。该模式关闭 Codex sandbox，保留 Ask for approval，但不提供工作目录边界隔离。

## 迁移到新设备

新设备需重新安装/登录 Codex CLI、克隆 bridge、运行 `setup.sh`。确保旧设备先执行 `./start.sh stop`；本机全局锁不能跨设备阻止同一 App ID 的重复连接。

如需延续会话，安全迁移工作目录中的 `.feishu-codex-session` 与 `.feishu-codex-settings`。通常不必迁移 `.feishu-codex-seen-messages`；除非新设备接管时可能收到同一批历史事件。

## 常见问题

- **没有回复**：`./start.sh status` 长期等待连接时检查网络、长连接配置和代理。
- **卡片按钮无效**：检查“回调配置”的 `card.action.trigger`，并在日志中查 `card_action_received`。
- **重复回复**：确认没有其他设备、旧目录或手工 bridge 使用同一 App ID；本机 journal 只能去重本实例收到的消息。
- **附件无法读取**：检查工作目录的 `feishu-inbox/`，然后重新上传附件。
