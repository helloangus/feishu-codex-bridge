# 工作进度检查点(2026-09-15)

本文件记录 PLAN.md 整改计划 A–F 工作包的执行进度,作为多设备协作的检查点。
按 PLAN.md 约定:各分支独立开发,目标为汇总分支 `integration/rust-remediation`;
本文件不替代 PR 描述,也不修改 PLAN.md 的勾选状态。

## 基线与分支

- 共同基线 B0:`67aef93`(docs: replace remediation plan and clarify distributed development)
- 汇总分支:`integration/rust-remediation`(本地已建立,待推送)
- 依赖前置 PR:`deps/rustix-termios`(root Cargo.toml 启用 rustix `termios` feature,
  供 CLI 隐藏凭据输入使用;按计划"依赖 PR 先合并"约定先行合入汇总分支)
- 执行环境:aarch64 Linux,Rust 1.85.1(rustup 已装),构建限 `-j 2`(2 GiB 内存防 OOM)

## 各工作包状态

### A `fix/cli-lifecycle` — 已完成并合并

- 分支 tip:`adfe542`;合并 commit:`d363708`(在汇总分支上)
- 交付内容:
  - 修复独立 `status` 命令缺 Tokio 计时器导致服务存活时 panic 的问题;
    `status` 与 `service status` 统一为共享查询实现(enable_all runtime)。
  - 控制接口动作/响应枚举化(`ControlCommand`/`ControlResponse`),单字节线编码集中维护。
  - 配置错误类型化 `ConfigError`(保留安全用户提示与内部原因);`config check`
    静态校验补全:必须含 `[feishu]` 段、凭据/代理环境变量名合法性。
  - 新增 `bridge credentials` 命令:按配置声明的环境变量读取凭据、隐藏输入
    (rustix termios)、统一配对码 16–256 字节 + Unicode 空白规则、已配对用户
    (配置白名单或 state.json)可留空、`--print` 输出安全转义的 export 行。
  - `setup.sh`/`start.sh` 删除 codex PATH 检查与重复的凭据/配对码校验,委托 Rust CLI;
    `BRIDGE_RUST_PAIRING_CODE_PROMPTED` 机制由 `BRIDGE_RUST_PAIRING_PRESENT` 取代。
  - 服务控制完成提示改为 `bridge status --config <路径>`,二进制直接使用成立。
  - sandbox 配置→应用端口转换集中为单一 `From` 实现;清除 CLI 过渡期注释。
- 新增测试:`crates/bridge-cli/tests/cli_status.rs`(真实子进程 + 临时锁 + 本地
  control socket,覆盖"服务存在/控制接口超时/服务不存在"与 guard 变体、
  超时不 panic 回归)、`tests/cli_credentials.rs`(环境变量名、离线校验、
  管道输入、配对码拒绝、已配对状态、自定义变量名)。
- 验证:fmt、clippy -D warnings、226 个 workspace 测试(串行)、产品 debug 构建、
  rustdoc、依赖边界、shell 语法、git diff --check 全部通过。

### B `refactor/runtime-flow` — 进行中(检查点提交)

- 已完成:
  - `bridge-app/src/interactions.rs` 重写为唯一交互管理实现:吸收原 runtime 内部
    `PendingApproval` 流程与 `reply_approval`;提供 insert(容量 32 + 同 turn/item
    去重)、stale/unsent 审查、逐题文本/按钮答案、审批消费、过期、drain 与
    `deliver_reply`(不完整答案禁止提交、结果未知返回 Uncertain)。
    旧 `interactions::Registry` 已删除;30 个单测覆盖归属校验、旧连接 epoch、
    重复点击、逐题答案、过期、不完整答案、结果未知等约束。
  - 1515 行 `runtime.rs` 拆分为 `runtime/` 目录模块:`mod.rs`(公共 API、run 主循环、
    关停序列)、`state.rs`(Runtime 状态结构、Done、FileDelivery 枚举、maintain)、
    `flow.rs`(tell/send_panel/finish/event/can_spawn/spawn_reply/spawn_interrupt)、
    `input.rs`(输入处理:配对/授权/卡片/命令/答案/任务准入)、`jobs.rs`(后台完成)、
    `protocol.rs`(通知与请求)、`timers.rs`(tick)。`runtime::run` 外部签名不变。
  - 编译零警告,bridge-app 30 个单测全部通过。
- 待完成(下次继续):
  - 跑通 `bridge-cli` 全部行为测试套件(approval/card/directory/session/files/
    minimal 等,约 3400 行)并修复偏差——这是本次重构的安全网,尚未执行;
  - 资源上限改造收尾:后台任务容量预检已铺开(`can_spawn` + `CONTROL_RESERVE=16`),
    需复查所有 spawn 点分类(用户路径返回繁忙、控制路径用保留容量、结果未知仍停止);
  - 将原 runtime.rs 中 compact 终态测试迁入 `flow.rs` 或新 `runtime_` 前缀测试;
  - 过渡期文案清理("最小运行版/最小版"等)已在拆分中顺带完成,需复查;
  - 全量验证后提交、合并到汇总分支。
- 注意:本检查点提交为 WIP,CLI 行为测试未跑,不得据此认为 B 包已交付。

### C `test/protocol-contracts` — 未开始
### D `build/tooling` — 未开始
### E/F — 按 PLAN.md 串行化约定,待 A–D 合并后开工

## 汇总分支合并记录

1. `4283bb3` merge: rustix termios feature prerequisite for work package A
2. `d363708` merge: work package A fix/cli-lifecycle

## 待人工确认/授权事项(统一留到最后)

- 推送各分支到 origin(本次检查点执行)、创建 PR 并审批合并到远程汇总分支;
- A–D 全部合入后完整验证并记录 B1;E、F 依次开工;
- 最终验收后由维护者决定合入主分支;不自动部署;
- 真实飞书交互、目标主机生命周期、长时间稳定性属于部署验收,本轮不覆盖。
