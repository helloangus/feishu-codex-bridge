# Feishu Codex Bridge

使用原生 Rust 服务把飞书消息、卡片和附件连接到本机已登录的 Codex app-server。服务包含飞书长连接、Codex JSON-RPC、持久状态、任务调度以及带健康检查的两层监督器，不需要额外语言运行时。

## 快速开始

需要 Linux、Bash、C 编译器、Rustup 以及已安装并登录的 Codex CLI。首次配置和启动：

```sh
./setup.sh
./start.sh status
```

脚本生成权限为 `0600` 的 `bridge.toml`。飞书 App ID、App Secret 和可选配对码只从环境或交互输入取得，不写入配置。已有配置不会被覆盖。

常用操作：

```sh
./start.sh start
./start.sh foreground
./start.sh status
./start.sh restart
./start.sh stop
```

配置示例见 `bridge.example.toml`，完整步骤见 [部署指南](docs/deployment.md)。

## 功能

- 私聊与群聊消息、富文本、图片和文件附件。
- `/help`、`/status`、`/new`、`/resume`、`/archive`、`/compact`、`/cd`、`/model`、`/plan` 和 `/stop`。
- 命令/文件审批、逐题问答、Plan 确认、流式预览和最终 Markdown 卡片。
- 用户授权、配对码、消息去重、会话/目录/模型设置持久化。
- 原生飞书 WebSocket、HTTP/SOCKS 代理、心跳、重连和有界缓冲。
- 服务锁、App ID 全局锁、日志轮转、健康状态、异常退避和后代进程清理。

## 开发验证

```sh
cargo xtask check
```

统一入口依次执行 fmt、Clippy（`-D warnings`）、串行 workspace 测试、产品构建、rustdoc（warnings 视为错误）、依赖边界、仓库卫生、Shell 语法、diff 检查和打包结构校验；`cargo xtask fetch` 预取依赖后可用 `cargo xtask check --offline` 完全离线执行。发布打包使用 `cargo xtask package [输出目录]`，根目录 `package.sh` 是它的转发入口。

测试不读取真实配置、不访问外部服务、不启动长期服务；网络协议测试使用本地 socket。协议假进程和 schema 校验由 Rust 测试工具完成，协议 fixture、生产序列化与真实解码均对照 `schemas/` 下版本化 schema 校验，协议基线版本集中在 `bridge_codex::protocol::CODEX_SCHEMA_BASELINE`。开发约定见 [开发指南](docs/development.md)，当前系统边界见 [架构说明](docs/architecture.md)。

## 整改交接

[PLAN.md](PLAN.md) 是当前工程整改清单，记录已发现的问题、修改方向与验收条件。未勾选的项目尚未实现；接手修改前先阅读交接状态，不沿用已经删除的迁移计划或过渡期设计说明。

## 发布状态

离线回归不等于生产验收。真实飞书交互、当前 Linux 部署环境的服务生命周期和 72 小时稳定性仍须按 [运维指南](docs/operations.md) 验证并记录，未完成前不要宣称生产切换完成。
