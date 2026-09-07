# Rust 状态迁移工具

Rust 当前仅提供离线迁移、命令语法及 TOML 配置检查，尚不能运行飞书机器人。
生产继续使用 `start.sh`。不要为测试这个工具停止当前服务或复制真实凭据到测试目录。

## 构建与验证

```sh
cargo test --workspace --locked
cargo build -p bridge-cli --locked
target/debug/bridge --help
target/debug/bridge check-command '/plan off'
target/debug/bridge config check --file bridge.toml
```

以 `bridge.example.toml` 创建自己的 TOML。路径须为绝对路径；restricted 必须显式提供
白名单或配对码环境变量名称。检查只验证结构与工作区，不检查凭据、网络或 Codex 登录态。
不会 source `.env`，也不会改进程 cwd。`dangerFullAccess` 必须显式选择。

## 迁移演练

先使用**合成数据或停止服务后的状态备份**演练。读取运行中的多份 JSON 不是事务快照。

```sh
target/debug/bridge migrate import-python \
  --source /absolute/python-state-backup \
  --destination /absolute/new-rust-state \
  --dry-run

target/debug/bridge migrate import-python \
  --source /absolute/python-state-backup \
  --destination /absolute/new-rust-state
```

目标目录必须不存在，父目录须存在。dry-run 不创建目录或锁。
默认读取 `.feishu-codex-session`、`.feishu-codex-settings`、`.feishu-codex-seen-messages`
和 `.feishu-codex-allowed-open-ids`。自定义旧路径使用 `--session-file`、`--settings-file`、
`--seen-file`、`--allowed-file` 指定。输出只包含计数，不输出内容或用户 ID。

旧单 thread ID 文件需要同时传 `--legacy-user` 与 `--legacy-cwd`；JSON 映射不需要。
未知设置字段、损坏 JSON、空消息 ID 都拒绝迁移。不会迁移活动审批或自动恢复执行任务。
若有 `migration-in-progress` 标记，表示导入中断；该目录不可运行，使用新的目标目录重试。
状态与去重两个文件不是跨文件事务，因此标记在两者持久化成功后才移除。

## 导出与回退

```sh
target/debug/bridge migrate export-python \
  --state-dir /absolute/rust-state \
  --output /absolute/new-python-export
```

导出目录必须不存在。全部四类状态使用 Python 原格式；状态目录写锁忙时拒绝导出。
未来生产切换前必须停止旧实例、确认 App ID 锁释放、备份状态，再导入和启动 Rust。
回退先停止 Rust、导出最新状态，再将输出切换到旧配置指定位置，最后启动 Python。
不要只恢复切换前的旧去重文件，否则可能重复执行已经处理的消息。
导出不能撤销 Codex 对工作目录造成的修改；旧操作卡片须重新打开。

Rust 工具不自动调整访问策略。原先开放部署在新 TOML 中显式使用 `mode = "open"`；
新安装使用 restricted。当前还未实现 `.env` 到 TOML 的自动转换或生产切换命令。
