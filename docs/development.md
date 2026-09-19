# 开发指南

工具链、校验入口、测试体系、协议快照维护与边界约定。校验步骤、依赖白名单与打包细节的完整展开见 [工具链与脚本](crates/tooling.md)；各 crate 的实现细节见 [实现参考](crates/README.md)。

## 工具链与入口

- 工具链由 `rust-toolchain.toml`（channel stable、minimal profile、rustfmt + clippy）与 `Cargo.lock` 固定；CI 基准是 1.85.1（workspace `rust-version = "1.85"`）。
- 交付任何改动前运行：

```sh
cargo xtask check          # 全量校验（见下）
cargo xtask fetch          # 预取依赖；之后可 cargo xtask check --offline 完全离线
```

`cargo fmt --all` 只在有意的格式化改动时使用；CI 用 check 模式。工作区 lints：`unsafe_code = "forbid"`、clippy `unwrap_used/expect_used = deny`——错误处理一律显式传播。

## 校验流水线

`cargo xtask check` 按固定顺序执行 11 步，任一步失败立即中止：fmt → clippy（`-D warnings`，`-j 1`）→ 串行 workspace 测试 → 产品构建（debug）→ rustdoc（warnings 视为错误）→ 依赖边界 → Codex 协议快照校验 → 仓库卫生（零 Python 门禁）→ Shell 语法（bash -n 三个脚本）→ `git diff --check`（空白/冲突标记）→ 打包结构校验（临时目录里跑真实的组装 + 校验函数）。

可单独运行：`cargo xtask check-boundaries`、`cargo xtask codex-schema check`、`cargo xtask package [输出目录]`（`package.sh` 是其转发）。

## 依赖边界

保持方向性：核心类型不知道传输；应用行为只依赖端口；飞书、Codex、本地存储实现边界；CLI 组装。`cargo xtask check-boundaries` 按真实包名强制白名单：

- 生产 crate 依赖 `xtask`/`test-support` 的 normal/build 依赖一律拒绝（test 工具只能进 dev-dependencies）。
- 每条依赖必须在 crate 的 kind 级白名单内；新增依赖 = 修改 boundaries 规则 + 评审。
- 当前完整白名单表见 [工具链与脚本](crates/tooling.md)。

## 测试体系

集成测试使用临时目录、内存传输、本地 socket 与 cargo 构建的假 Codex 可执行文件。硬约束：

- 不读取真实 `bridge.toml`、凭据或用户状态；不访问外部服务；不启动长期服务；不改变用户工作目录；测试不得递归调用 cargo。
- 假进程与共享装配在非发布包 `crates/test-support`：`fake-codex-process`（协议层最小假）与 `fake-codex-runtime`（行为层脚本化假：runtime/directory/compact 三场景 + 8 个 compact 模式，经 `Actor` 挂到真实串行运行时）。场景契约见 [test-support](crates/test-support.md)——**新增场景必须同步更新假进程文档与 `test-support/tests/runtime_flows.rs`**。
- 协议 fixture（`fixtures/codex/0.153.4/`）、生产请求序列化与真实解码都对照 `schemas/` 下版本化 schema 校验；协议基线版本集中Pin在 `bridge_codex::protocol::CODEX_SCHEMA_BASELINE`。
- Shell 入口测试（scripts.rs）用假 bridge 二进制驱动仓库根的 setup.sh/start.sh。

各层测试布局：

| 位置 | 覆盖 |
|---|---|
| 各 crate 内联 `#[cfg(test)]` | 纯逻辑：Scheduler、Interactions、cards、presentation、safeio、transport、wire、快照 |
| `crates/bridge-codex/tests/` | 协议契约（fixture ↔ 生产序列化 ↔ schema） |
| `crates/bridge-cli/tests/` | 真实 runtime + 内存 RPC 的行为回归（审批、卡片、目录、文件、会话、配置、脚本、CLI） |
| `crates/test-support/tests/` | 跨 crate 运行时行为与资源上限（容量、洪峰、背压） |
| `xtask/tests/` | 边界门、schema 校验与打包（全部离线、stub 可执行文件） |

## 协议快照维护

schema 快照只在评审过的维护 PR 里变更，流程由 `cargo xtask codex-schema export --codex <路径> [--version x.y.z] [--force]` 显式驱动（**绝不进 CI、绝不自动运行**）：

1. export 校验给定版本与可执行文件一致，在临时目录生成 schema 并构建确定性 manifest（逐文件 SHA-256）。
2. **兼容性门**：候选 schema 必须仍接受仓库全部已记录 fixture，否则拒绝（除非显式 `--force`）。
3. staging 组装校验后原子发布到 `schemas/codex/<版本>/`。
4. 之后在同一 PR 里：重新记录它列出的 fixtures、运行 bridge-codex 协议契约测试、更新 `CODEX_SCHEMA_BASELINE`——目录名、manifest `cli_version` 与常量三者必须一致，测试会互相钳制。

`cargo xtask codex-schema check`（check 流水线的一步）离线验证快照与 fixture 的成对完整性。

## 文档与仓库约定

- 行为、命令、架构、部署或运维状态变化时，同步更新 README 与相关 docs 页面；架构文档描述「当前实现」，实现职责变化时更新它而不是绕开它。
- 仓库零 Python：任何跟踪文件不得包含 Python 源码/字节码/依赖清单或解释器调用（hygiene 门禁强制，含 workflow 文件）。
- 提交摘要使用作用域化 Conventional 风格：`feat:`、`fix:`、`test:`、`docs:`。
- PR 模板要求：Rust 格式/Clippy/测试/边界四项、零 Python 与 Shell 语法、README 与 docs 与实际阶段一致、飞书/Codex 接口变更附联调记录与脱敏截图。
- 为飞书回调、卡片、Codex 协议、监督或打包的改动添加聚焦的离线回归覆盖，并把需要的真实平台验证单独记录（见运维指南）——构建成功不等于生产部署完成。
