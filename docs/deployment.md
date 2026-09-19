# 部署指南

本文同时以 `DEPLOYMENT.md` 的形式随二进制包发布（与 `bridge`、`bridge.example.toml`、`BUILD-INFO.txt`、`SHA256SUMS` 同目录），因此不引用仓库内其他文档。两种部署方式：源码部署（需要 Rust/C 工具链）与二进制包部署（只需要兼容的 Linux 主机）。

## 前置条件

- 兼容的 Linux 主机（x86_64/aarch64 均可，按宿主机构建）。
- 已安装并登录的 Codex CLI（服务运行用户可执行 `codex`）。
- 飞书应用凭据：App ID 与 App Secret；受限部署若无静态白名单且尚无已配对用户，还需要配对码（16–256 字节、不含空白）。
- Bash（源码部署）；Rustup 与 C 编译器（仅源码部署需要；依赖链包含原生加密库与 Linux 系统接口）。

## 方式一：源码部署

在目标主机检出源码后：

```sh
./setup.sh
```

脚本依次：检测或安装 Rust/C 工具链（Ubuntu/Debian，需要 apt-get 与 sudo，非交互需 `BRIDGE_RUST_AUTO_INSTALL=1`）→ 只构建 `bridge` 二进制 → 生成权限 `0600` 的 `bridge.toml` → 交互输入凭据 → 启动受监督服务。已有配置不会被覆盖。可选行为：

- `./setup.sh --no-start`：只构建和配置，不启动，也不要求凭据。
- `./setup.sh --check`：只校验现有二进制与配置。
- 环境变量可预设答案：`BRIDGE_RUST_ROOT`、`BRIDGE_RUST_CWD`、`BRIDGE_RUST_STATE_DIR`、`BRIDGE_RUST_ALLOWED_USERS`、`BRIDGE_RUST_SANDBOX`。

非交互 setup 且要启动服务时，必须提供 `FEISHU_APP_ID` 与 `FEISHU_APP_SECRET`；无静态白名单且无已配对用户的受限部署还需要 `FEISHU_PAIRING_CODE`。

生命周期管理（不要绕过 guard/supervisor 直接操作；同一主机上同一个 App ID 只允许一个实例）：

```sh
./start.sh start        # 后台启动（guard → supervise → run）
./start.sh status       # 健康与飞书连接状态
./start.sh restart
./start.sh stop
./start.sh foreground   # 前台运行，便于观察日志
```

首次使用：在飞书聊天里向机器人发送 `/pair <配对码>`（仅当部署使用配对码模式）。

## 方式二：二进制包部署

包内含且仅含五个文件：`bridge` 可执行文件、`bridge.example.toml`、本指南（`DEPLOYMENT.md`）、`BUILD-INFO.txt`、`SHA256SUMS`。不包含凭据、状态、日志、附件或任何兼容运行时；不需要 Rust、Cargo 或源码。使用前先校验：

```sh
sha256sum -c SHA256SUMS
./bridge --version
```

生成配置（把示例路径替换为你的工作区与状态目录；初始目录必须位于工作区根之内）。配置生成拒绝覆盖已有文件，也不启动服务：

```sh
./bridge config init --output ./bridge.toml \
  --root /absolute/workspace --cwd /absolute/workspace \
  --state-dir /absolute/private/bridge-state
./bridge config check --file ./bridge.toml
```

静态白名单在生成时指定（可重复该选项）：`--allowed-user ou_your_open_id`。否则配置使用配对码模式。凭据通过服务运行用户的环境变量提供（`FEISHU_APP_ID`、`FEISHU_APP_SECRET`，需要时加 `FEISHU_PAIRING_CODE`）——二进制不提供交互输入；不要把秘密写进 shell 历史或 TOML。配置里声明了自定义环境变量名的，提供对应名字的变量。`config check` 是离线检查，不校验凭据或连通性。

两个离线辅助：

- `./bridge credentials --file ./bridge.toml`：校验配置声明的凭据环境变量（退出码反映缺失/非法）；`--print` 输出缺失项的 `export` 语句，供脚本 `eval` 后启动。
- `./bridge check-command '/status'`：校验一条聊天文本命令的语法，不执行。
- `./bridge config init --danger-full-access`：生成 Codex 沙箱完全开放的配置。这会关闭所有任务的工作区包含，仅在接受 Codex 写出工作区之外时使用，且不要与不可信工作区组合。

服务生命周期（包目录内执行；`service start` 成功只代表 guard 已启动，用 `status` 确认飞书连接）：

```sh
./bridge service --config ./bridge.toml start
./bridge service --config ./bridge.toml status
./bridge service --config ./bridge.toml restart
./bridge service --config ./bridge.toml stop
```

受监督的前台运行用 `./bridge guard --config ./bridge.toml`；等价的只读状态查询是 `./bridge status --config ./bridge.toml`。

## 更新与注意事项

- 配置生成权限为 `0600`。运行期状态、健康文件与轮转日志都位于配置的 state_dir 下；**更新既有原生安装时保留该目录**（会话绑定、去重日志与配对用户都在其中）。
- 切换新包前先停止旧服务；替换文件后再启动。
- 每次启动会先做归档对账（对照后端归档线程清理本地绑定），对账失败不会接收业务消息——这是有意设计，防止已归档会话被误用。

## 从源码打包

```sh
bash package.sh [输出目录]        # 等价于 cargo xtask package --release [输出目录]
```

打包器以宿主机构建 release 二进制（显式拒绝交叉目标），可执行路径只取自本次构建的 Cargo 消息（绝不回退到陈旧的 `target/release/bridge`），支持自定义 `CARGO_TARGET_DIR`。脏工作树需要显式 `--allow-dirty` 并记录进 `BUILD-INFO.txt`。包在暂存目录组装并完整校验（文件集合、逐文件 SHA-256、文档内相对链接），然后原子发布到输出目录（默认 `dist/`，替换旧包）；任何失败都不会留下半成品包。

打包成功不等于生产验收通过：真实平台的验收清单见仓库文档的运维指南（配对、消息/卡片、审批/问答、附件、重连、服务生命周期与 72 小时稳定性记录）。
