# Rust 部署与交接

本批配置生成和包装脚本已开发，待统一验证。真实飞书及长期运行验收尚未完成，生产切换前先在目标环境验证。

## 从源码准备

Linux/proot Ubuntu 需要 Bash、Rust 工具链、C 编译/链接工具和已登录的 Codex CLI。在项目目录执行 `bash setup-rust.sh`。脚本构建本机 Rust 二进制，询问工作区根目录、初始目录、私有状态目录、白名单及沙箱模式，然后生成权限 0600 的 `bridge.toml`，不自动启动。已有配置只检查，不覆盖。默认采用 workspaceWrite；必须明确输入 dangerFullAccess 才关闭 Codex 沙箱。

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
