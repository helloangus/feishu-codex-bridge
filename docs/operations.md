# 运维指南

日常健康检查、状态解读、日志位置、故障处理原则与生产验收清单。状态与命令的字段级参考见 [配置与命令参考](configuration.md)；监督状态机与运行期文件布局见 [架构说明](architecture.md)。

## 日常健康检查

`./start.sh status`（或 `bridge status --config …`）是首要健康视图，输出三行：

```text
桥接状态：运行中
飞书连接：已连接，服务正常接收飞书消息。
自动恢复：监督器正在恢复服务。
```

解读规则：

- **监督器存活 + 最新快照**要合在一起看：单独一条旧的 `running` 快照不能证明进程活着；健康判定依赖心跳新鲜度（心跳每 10 秒写入，新鲜窗口 30 秒）。
- 飞书连接三态：`Connected`（正常接收）、`Reconnecting`（连接已中断，正在自动重连）、`Failed/Starting`（尚未连上）。
- 自动恢复一行直接渲染监督层的 `LivePhase` JSON（控制 socket 应答），没有自由文本需要解析；`Backoff` 时显示「将在 N 秒后重试」。
- 有意不暴露 PID 与时间戳，避免把内部标识当作运维依据。

## 运行期文件

全部位于配置的 `state_dir` 下：`runtime/health.json`（健康快照）、`runtime/events.jsonl` 与轮转备份 `.1..3`（运行诊断日志，2 MiB × 4）、两层监督各自的 `runtime/supervisor/snapshot.json` 与 events.jsonl、`state.json` / `seen-messages.json`（状态与去重）。诊断日志是元数据（事件名、状态、内部任务号、计数），不含消息文本、prompt 或远端错误内容。

## 自动恢复行为

监督状态机（Starting → Running → Backoff/Stopping → Stopped/Failed）与退避参数：

| 情形 | 行为 |
|---|---|
| run 异常退出 | supervise 按退避重启：2、4、8、16、30 秒封顶；失败超过 10 次放弃；连续运行 ≥60 秒才重置预算 |
| 心跳停滞 | 启动宽限 120 秒（从未收到心跳）/ 进度宽限 45 秒（心跳流动后）；停止并以固定 30 秒退避重启 |
| supervise 异常退出 | guard 以同样策略重启，并用 child-subreaper + pidfd 收割全部后代（含 Codex 子进程） |
| 飞书断连 | 桥内自动重连：首次抖动 ≤30 秒、之后 120 秒间隔；认证/代理类错误不重试（需要人工介入凭据或代理） |
| Codex 子进程退出/协议失败 | 运行时停机（「未自动重启任务」），交由 supervise 层重启整个 run |

## 故障处理原则

- 优先顺序：`status` → 针对性查看日志（events.jsonl、监督快照）→ 已验证的服务动作（`service stop/start`）。
- **不要**删除任何锁文件（guard.lock、supervisor.lock、service.lock、control.lock、state.lock），**不要**使用大范围 kill 命令——锁与 pidfd 机制已经保证互斥与清理，绕过它们会引入竞争。
- `stop` 与 `restart` 会校验归属并清理被监督的进程树（SIGTERM → 宽限 → SIGKILL → 后代收割）；等待其完成而不是并行干预。
- 状态保存失败（诊断中出现 Uncertain/状态保存失败）时：停止服务、检查磁盘与权限后重启——存储会拒绝在结果不确定后继续写入，这是防止状态错乱的防线，不要试图绕过。
- Codex 归档对账失败（「归档状态结果不确定…」）：按提示核对本地会话绑定与远端线程状态，不要直接重发任务。

## 生产验收清单

在当前 Linux 主机上签署生产切换之前，逐项验证并记录：

1. 配对/白名单行为与未授权拒绝（含受限模式、配对码过期与限流）。
2. 文本、卡片、会话命令、模型/Plan 设置、审批、逐题问答、附件收发、流式预览与停止的完整走查。
3. 强制飞书 WebSocket 重连与一次 Codex 子进程失败；确认恢复、有界退避、无重复执行。
4. start/status/restart/stop、锁互斥、健康新鲜度、日志轮转与后代进程清理。
5. 连续运行 72 小时：不丢连接、不重复消费、状态不串、无孤儿进程。

记录内容：时间戳、版本（`bridge --version` 与 BUILD-INFO）、脱敏日志与卡片截图。离线测试与本地打包不满足本清单。
