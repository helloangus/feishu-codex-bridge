# 实现参考：crate 详解

本目录按 crate 拆分实现细节，是 [架构说明](../architecture.md) 的展开：每一篇给出该 crate 的模块清单、关键类型、控制流与常量表，符号名与代码一一对应，路径均相对仓库根目录。系统层面的依赖方向、进程链与任务拓扑不再重复，请先读架构说明。

## 阅读顺序建议

1. [bridge-core](bridge-core.md) — 全部共享类型从这里出发：命令、会话身份、任务状态机、视图。
2. [bridge-app](bridge-app.md) — 端口定义与应用行为（调度、会话、审批、卡片、文件、呈现）。
3. [bridge-app 运行时](bridge-app-runtime.md) — select 循环、六个状态组件、后台作业与全部运行时限额。
4. [bridge-local](bridge-local.md) — JSON 状态存储、AsyncState、safeio 文件安全原语、工作区与附件。
5. [bridge-feishu](bridge-feishu.md) — 飞书 WebSocket 入口、REST 出口、代理、卡片渲染。
6. [bridge-codex](bridge-codex.md) — Codex JSON-RPC 传输、进程所有权、协议快照。
7. [bridge-cli](bridge-cli.md) — 配置模型、进程链装配、两层监督、健康与日志、CLI。
8. [test-support](test-support.md) — 假 Codex 进程的场景契约与共享测试装配。
9. [工具链与脚本](tooling.md) — xtask 校验门、仓库脚本、CI 与打包。

## 各篇通用的三条约定

- **fail-closed**：畸形输入、容量超限、身份不匹配一律显式失败（拒绝、丢弃并计数或停机），绝不静默降级或无限缓存。
- **先持久化后副作用**：消息认领、会话绑定、目录变更都是先落盘再生效；结果不确定时拒绝后续操作而不是重试。
- **边界即审计面**：厂商类型不越过端口；文件安全只看 `bridge-local/src/safeio.rs`；结果文案只看 `bridge-app/src/presentation.rs`。
