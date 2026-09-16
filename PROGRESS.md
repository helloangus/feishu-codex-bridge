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

### B `refactor/runtime-flow` — 已完成并合入汇总分支

- 交付提交:`d7fcf8e`（基于检查点 `6f5a8e2`）。
- `bridge-app::interactions` 是审批和逐题问答的唯一状态机；删除已无调用方的
  `bridge-core::interaction` 及旧 Registry。覆盖归属、epoch、重复点击、逐题答案、
  过期、未完成答案不提交、结果未知与 32 项审批风暴上限。
- 运行时维持 `runtime::run` 的外部签名，并按输入、协议、后台完成、计时器和关闭
  拆分。`ActiveKind` 明确区分普通任务与压缩任务，避免原 compact 布尔值和多个
  `Option` 组合形成非法阶段。
- 新增 `runtime::limits` 集中运行时容量和超时。普通后台工作最多使用 112 个槽位，
  为停止、审批回传和关闭保留 16 个；用户侧饱和返回繁忙，协议结果未知和无法安全
  回传的控制失败仍停止。卡片、刷新、文件交付、归档同步和任务准备均在创建前检查。
- 删除测试说明中的过时 minimal 文案；公共端口、持久化格式和 `runtime::run` 外部
  契约未变。
- 验证: `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked --
  -D warnings`、`cargo test --workspace --locked -- --test-threads=1`、`cargo build -p
  bridge-cli --bin bridge --locked`、`RUSTDOCFLAGS='-D warnings' cargo doc --workspace
  --no-deps --locked`、`cargo xtask check-boundaries`、`bash -n setup.sh start.sh package.sh`
  和 `git diff --check` 全部通过。CLI 行为测试的 approval/card/directory/session/files/
  minimal 套件包含在 workspace 串行测试中并全部通过。

### C `test/protocol-contracts` — 未开始
### D `build/tooling` — 未开始
### E/F — 按 PLAN.md 串行化约定,待 A–D 合并后开工

## 汇总分支合并记录

1. `4283bb3` merge: rustix termios feature prerequisite for work package A
2. `d363708` merge: work package A fix/cli-lifecycle
3. `844ebbf` merge: work package B refactor/runtime-flow

## 待人工确认/授权事项(统一留到最后)

- 推送各分支到 origin(本次检查点执行)、创建 PR 并审批合并到远程汇总分支;
- A–D 全部合入后完整验证并记录 B1;E、F 依次开工;
- 最终验收后由维护者决定合入主分支;不自动部署;
- 真实飞书交互、目标主机生命周期、长时间稳定性属于部署验收,本轮不覆盖。
