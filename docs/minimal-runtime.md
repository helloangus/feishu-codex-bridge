# Rust 最小运行版

本入口运行 Rust 业务、Codex 适配与 REST；Python 仅负责飞书 SDK 长连接。它支持授权文本任务、串行排队、会话连续、文本回复，以及 `/help`、`/status`、`/stop`、`/new`、`/resume`、`/models`、`/model`、`/plan`。这是前台最小版，不是原始计划中全部功能完成的过渡版。

## 准备

使用独立测试飞书自建应用，按 README 配置消息权限、长连接和消息事件订阅。已有生产机器人继续使用 start.sh。最小版复用 Python service.py 的 App ID 全局锁，同一用户、同一全局状态目录下，不允许两者连接同一 App ID。

```sh
cargo build --locked -p bridge-cli
python3 -m venv .venv
.venv/bin/python -m pip install -r compat/feishu-sdk/requirements.txt
cp bridge.example.toml bridge.toml
```

修改 bridge.toml 中的绝对工作区、初始目录、私有状态目录、允许的 open_id、Python 路径和 adapter 路径。`codex` 必须已登录；保持 executable 与 args 分开，桥接不执行 shell 命令字符串。配置检查不会登录或发消息。

```sh
target/debug/bridge config check --file bridge.toml
export FEISHU_APP_ID='your-test-app-id'
read -rs -p 'Feishu app secret: ' FEISHU_APP_SECRET
export FEISHU_APP_SECRET
target/debug/bridge run --config bridge.toml
```

正常启动不读取或执行旧 `.env`。不要将凭据写进 TOML、提交或日志。若配置 proxy_env，使用对应环境变量作为飞书 REST 和 SDK 专用代理；未指定时采用各客户端的环境代理发现。现有代理/Termux 实机矩阵仍待验收。

日志 `rust_runtime_started` 表示主循环启动；`feishu_connection` 的 `Connected` 表示 SDK 已建立连接。首次连通与重连通过显式事件上报，不解析 SDK 日志正文。

## 最小验收

1. 未授权账号发送消息，不应执行或收到业务回复。
2. 授权账号发送 `/help` 和 `/status`，获得文本回复。
3. 发送“只回复 hello，不修改文件”，收到接收/启动提示和最终文本。
4. 连续发送两个任务，验证串行；再次提问验证复用会话。
5. 任务运行时发送 `/status` 和 `/stop`；自己的运行任务被请求中断，尚未执行的请求取消。其他用户不能中断当前任务。
6. Ctrl-C 或 TERM 关闭；确认 SDK、Codex 及所属工具进程退出。重新启动不自动回放未完成任务。
7. 全局空闲时发送 `/new`，确认新会话提示；下一条消息应创建新会话，其他用户和目录的绑定保持不变。运行、排队或正在保存接收状态时发送 `/new` 应被拒绝。此命令只清除本地绑定，不删除或归档 Codex 历史会话。
8. 发送 `/resume` 获取当前目录最近会话的文本列表，复制 `/resume <ID>` 恢复旧会话，再提问验证上下文。目标必须在当前目录且空闲；存在运行/排队任务时拒绝切换。当前列表最多 8 项，卡片操作和分页尚未迁移。与 Python 一致，同目录会话由授权用户共享可见，不声称提供用户级历史会话隔离。
9. `/models` 显示最多 20 项，复制 `/model <ID>` 设置后用 `/model` 查询；不存在的模型应失败且不覆盖旧设置。`/model default` 恢复 Codex 默认模型。
10. `/plan on` 后查询 `/plan` 应为开启，再发送“只分析并给出计划，不修改文件”；最终回复应包含 Codex 的计划文本。`/plan off` 返回普通执行模式。设置按用户和目录持久化，重启和 `/new` 不重置模型与 Plan 设置。

普通测试已使用内存 IPC、临时 JSON、假 app-server 与 fake Messenger 验证链路，不使用真实凭据、不调用模型。真实飞书/CLI 验收应独立记录，不能把离线通过视作手机端验证通过。

## 明确范围与限制

- 未设置时使用普通执行模式和 Codex 默认模型；已导入或保存的模型/Plan 设置会在任务准备时加载。固定配置工作目录，目录偏好仍不应用。
- 最小版没有配对界面。restricted 模式必须有配置白名单或导入的已配对用户；只有 pairing_code_env 不足以启动。
- 附件、卡片动作、Plan 确认/实施按钮、归档和压缩等命令尚未迁移；Plan 最终文本可回复，但人工问答仍返回空答案。
- 修改模型/Plan 设置要求全局空闲；有运行、排队、待保存任务时拒绝，避免影响已经接收的任务。与 Python 原行为的阶段性差异见 ADR 0002；查询仍可在运行时使用。
- 收到审批会拒绝，问题返回空答案，并告知用户；不会静默自动批准。尚不能完整显示的权限请求会导致协议失败并停止本次运行。
- 输入和累计输出各限 32 KiB，输出过长会标记截断；没有文件成果回传、Markdown 卡片或流式进度，最终输出采用分段纯文本。
- 队列最多 64 项（含认领保存中），文本任务、会话变更与模型/Plan 设置共用最近 1,000 条持久化消息去重；查询命令只有运行期去重，重投可能再次显示提示，停止本身幂等。变更先认领再执行，认领后崩溃、网络或保存失败不自动重试；收到失败提示时需重新发送一条命令。SDK 收到认领确认不等于变更已完成，应等待业务回复。去重窗口外的历史命令不保证被识别。
- 执行最长一小时。网络/协议异常终止本次运行，不自动重跑。发送失败记录 delivery_failed，不改变执行结局，也不盲目重发。
- 仅前台运行，日志输出到 stderr；完整健康文件、日志轮转、退避监督和服务命令仍属后续范围。
- 当前 Linux 环境完成构建与离线验证；独立 Android 构建及真实 Termux 运行尚未通过，不将 Linux ARM 产物当作 Termux 产物。

## 验证命令

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo xtask check-boundaries
python -m unittest discover -s tests -v
git diff --check
```

原始范围继续以 [完整计划](plans/rust-refactor-original.md) 为准，阶段进度见 [PLAN.md](../PLAN.md)。
