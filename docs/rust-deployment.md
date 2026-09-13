# Rust 部署与交接

本批配置生成和包装脚本已开发，待统一验证。真实飞书及长期运行验收尚未完成，生产切换前先在目标环境验证。

## 从源码准备

默认部署环境是 Ubuntu/Debian，需要 Bash 和已登录的 Codex CLI。在项目目录直接执行 `./setup-rust.sh`。若缺少 Rust、rustup 或 C 编译器，脚本会询问是否立即安装：先通过 `apt-get` 安装 `build-essential` 和 `curl`，再使用 [Rust 官网 rustup 脚本](https://rustup.rs/) 安装 Rust，不使用 Ubuntu 系统包中的 Rust；无交互自动化可设置 `BRIDGE_RUST_AUTO_INSTALL=1`。脚本构建本机 Rust 二进制，并默认以当前用户的 `$HOME` 为工作区根目录和初始目录、`$HOME/.local/state/feishu-codex-bridge` 为私有状态目录，生成权限 0600 的 `bridge.toml` 后启动服务。已有配置只检查，不覆盖。默认采用 workspaceWrite。

在交互终端中，脚本会显示上述三个路径，直接回车接受默认值，或输入路径覆盖。这样首次运行无需事先手动设置工作目录，但仍可在生成配置前调整。

首次启动时会询问飞书 App ID、隐藏输入的 App Secret，以及配对码，并提示可在另一个终端运行 `openssl rand -hex 16` 生成安全随机值。配对码必须为 16–256 字节且不能包含空白；首次启动或无白名单时必填，已有白名单或已有成功配对用户时可以留空。它们仅导出给本次启动的守护进程及其子进程，不写入 `bridge.toml`、状态目录或日志。启动后，尚未授权的用户需在飞书私聊机器人发送 `/pair <配对码>`；普通消息会返回授权指引，不再静默丢弃。若只需构建和生成配置，请使用 `./setup-rust.sh --no-start`；非交互启动需要预先设置 `FEISHU_APP_ID` 和 `FEISHU_APP_SECRET`，无白名单时还需设置 `FEISHU_PAIRING_CODE`。

桥接在未设置用户模型偏好时默认使用 `gpt-5.6-luna`，启动时会通过 Codex app-server 的模型列表确认该模型可用。飞书中的 `/model <ID>` 仍可按用户和工作目录覆盖它，`/model default` 恢复此默认值。

无需交互的默认值可通过环境变量覆盖：`BRIDGE_RUST_ROOT`、`BRIDGE_RUST_CWD`、`BRIDGE_RUST_STATE_DIR`、`BRIDGE_RUST_ALLOWED_USERS`（逗号分隔）及 `BRIDGE_RUST_SANDBOX`（`workspaceWrite` 或明确指定的 `dangerFullAccess`）。未指定白名单时启用配对。

无白名单时使用配对：启动时提供至少 16 字节、最多 256 字节且无空白的 `FEISHU_PAIRING_CODE`，用户私聊 `/pair <配对码>`。配对成功写入授权列表。配对码会存在飞书聊天中，不写入桥接状态或日志。

自动化配置可用原生命令，无需 Python 或交互终端：

```sh
target/debug/bridge config init --output bridge.toml \
  --root /absolute/workspace --cwd /absolute/workspace/project \
  --state-dir /absolute/private/bridge-state --allowed-user ou_example
target/debug/bridge config check --file bridge.toml
```

`--allowed-user` 可重复；省略时生成配对配置。路径由 TOML 序列化，不拼接 shell；先验证后原子发布，已有文件或符号链接不会被覆盖。命令不创建状态目录、不读取 `.env`、不写凭据、不连接飞书或 Codex。

## 运行

```sh
bash start-rust.sh start
bash start-rust.sh status
bash start-rust.sh stop
bash start-rust.sh restart
# 前台双层监督：
bash start-rust.sh foreground
```

启动、重启、前台运行在交互终端中可输入 App ID、隐藏的 App Secret 和配对码；白名单模式下配对码可留空。也可以预先设置 `FEISHU_APP_ID`、`FEISHU_APP_SECRET`、`FEISHU_PAIRING_CODE` 环境变量。不保存凭据，所以在新终端启动/重启时需再次提供；已运行的后台守护及其自动重启子进程会继承启动环境。非交互环境必须提前设置所需变量。status/stop 不要求凭据。

默认读取仓库 `bridge.toml` 和 `target/debug/bridge`。可通过 `BRIDGE_RUST_CONFIG`、`BRIDGE_RUST_BIN` 指定其他路径；自定义配置若改了凭据变量名，应预先导出对应变量，直接调用原生命令或在非交互环境使用脚本。脚本不解析或执行 `.env`。

从发布目录运行时先执行 `sha256sum -c SHA256SUMS`，使用 `./bridge config init` 生成配置，再以已设置凭据的环境运行 `./bridge service --config bridge.toml start`。发布目录包含本机二进制、样例、部署与迁移文档和构建信息；无需 Python。管理命令为 `./bridge service --config bridge.toml` 后接 `status`、`stop` 或 `restart`。

## 旧版本迁移与回退

1. 停止旧 Python 服务：`bash start.sh stop`，核对旧实例和工具子进程已退出。同一 App ID 不应跨设备同时运行。
2. 按 [迁移指南](migration.md) 把旧状态导入一个新的 Rust 状态目录；保留原文件和旧程序。只导入授权用户、会话、设置与去重记录，不回放旧任务。
3. 配置 Rust 指向新状态目录，在受控工作目录开始验收。升级二进制前先停止外层守护；确认干净停止后再替换二进制、启动，避免同一监督链混用版本。
4. 回退时先停止 Rust，确认后代已退出，再启动旧 Python 服务。若要保留 Rust 期间新增的会话/偏好，先用迁移工具导出到新的目录，按迁移指南人工检查后应用，不能直接覆盖旧文件。

## 验收与打包

先统一运行离线测试、构建、Clippy、格式和边界检查，再在飞书验收收发、配对、审批、Plan 问答、附件成果物、归档、停止与重启。Termux/proot 还需验证内层异常退出后的接管、心跳停滞恢复及长时间运行。外层守护自身强杀或设备重启后的自启动不由本项目保证。

本机打包命令为 `bash package-rust.sh`，每次创建独立发布目录，记录源码提交、已跟踪文件修改状态、编译器平台与校验和，不包含凭据、状态、日志或用户附件。未跟踪开发文件可能参与编译，因此发布前必须确认源码已完整纳入版本控制；打包不等于测试通过或可跨平台运行。脚本不会上传、发布或切换服务。
