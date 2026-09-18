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

### C `test/protocol-contracts` — 已完成并合入汇总分支

- 分支 tip:`3dd44c3`;合并 commit:`d101cca`(基于 `d2e1215`,与 A/B 同一汇总基线)。
- 交付内容:
  - 新增 `bridge-codex::protocol`:协议基线版本 `CODEX_SCHEMA_BASELINE` 与
    schema/fixture 路径根集中管理,消除测试中重复的 `"0.153.4"` 路径常量;
    manifest `cli_version`、快照目录名与常量三方互证。
  - 新增 `tests/protocol_contract.rs` 五个契约测试:server-requests fixture 驱动
    真实 `requests::decode` 并逐字段断言类型化应用事件;fixture 派生负例(缺
    threadId/turnId/itemId/startedAtMs/isBlocking/questions 等)必须被拒绝;
    真实 `CodexBackend::start_turn` 线上捕获 turn/start 参数与 fixture 等值并过
    `TurnStartParams` schema;真实审批回复句柄线上捕获 result 与
    `{"decision":"accept"}` 等值并过 Response schema;生产 `payload()` 覆盖
    accept/decline/writeStdin/fileChange/逐题答案各变体并过 schema。
  - `acceptForSession` 在 schema 上合法但生产映射按设计不发送,测试钉住该不变量。
  - `schemas/`、`fixtures/` 内容零改动;生产代码逻辑零变化(仅 `#[cfg(test)]` 与
    注释);bridge-app 冻结接口未动。
  - 已交付 schema 维护命令规格(`cargo xtask codex-schema export/check/
    record-fixtures`),待工作包 E 补接。
- 验证:fmt、clippy -D warnings、workspace 串行测试 238 通过、产品 debug 构建、
  rustdoc、依赖边界、git diff --check 全部通过(-j 1;两次 OOM kill 后按预案重试通过)。

### D `build/tooling` — 已完成并合入汇总分支

- 分支 tip:`fe55b7c`;合并 commit:`351cbd5`(基于 `d2e1215`)。
- 交付内容:
  - `cargo xtask package [输出目录]`:从本次构建的 Cargo 消息解析 executable 路径
    (兼容 cargo 1.85 package_id 三种格式),无旧产物回退;非主机 target 明确报错;
    脏树(含未跟踪文件)默认拒绝,`--allow-dirty` 放行并记录;临时目录组装、
    五文件清单精确校验、SHA256SUMS 自校验、包内文档相对链接检查后原子发布
    (覆盖旧包,失败回滚);确定性 `BUILD-INFO.txt`(无时间戳,重复打包幂等)。
  - `cargo xtask check` 统一验证入口(10 步):fetch(可选)→fmt→clippy→串行测试
    →产品构建→rustdoc(-D warnings)→边界→卫生(含 .yml/.yaml)→shell 语法
    →diff 检查→打包结构;失败即停;`--offline` 支持完全离线执行。
  - 边界门禁强化:每个 package 显式登记(生产 6 crate + 开发工具 xtask),未分类
    即失败;normal/build/dev 三类依赖分别 allowlist;禁止生产链路依赖开发工具;
    7 个合成 metadata 门禁测试。
  - `package.sh` 改为纯转发(`exec cargo xtask package --release "$@"`);
    Dependabot 删除 pip 块;CI 主入口改为 `cargo xtask fetch` + `cargo xtask check
    --offline`,新增 release 打包冒烟(包内 `--version`/`--help`/`sha256sum -c`)。
  - 依赖:仅 xtask 新增 `sha2.workspace = true`(Cargo.lock +1 行,无新外部包)。
- 验证:xtask 38 测试(28 单元 + 10 集成,覆盖自定义 target 目录、空格路径、
  重复打包幂等、失败无半成品、旧产物不误用、篡改校验失败、文档链接);dogfood
  `cargo xtask check` 全绿;两次真实 release 打包端到端(31m46s/25m55s)暴露并修复
  2 个真实缺陷后,`bash package.sh /tmp/fcb-d-dist` 产物恰五文件、校验和全 OK、
  包内冒烟通过。CI 变更未在真实 GitHub Actions 运行(本地无法触发),已做 YAML
  解析级自查,列为遗留验证项。

### E `refactor/test-infrastructure` — 已完成并合入汇总分支

- 分支 tip:`02a37cf`;合并 commit:`3471bde`(基于 `b075a91` = B1 + 文档同步)。
  前一开发者交付 3 个提交后被暂停封存(`63cd66b` WIP),续作者审阅后保留前 3 个、
  重作 WIP 为 `02a37cf`。
- 交付内容:
  - 新增非发布测试支持包 `crates/test-support`:两个 fake 进程 bin(自
    bridge-cli/bridge-codex 迁出,生产包不再含 fake bin 目标)、共享测试装配
    (Input 构造、RecordingMessenger 端口 fake、进程绑定 runtime harness);
    原 minimal_runtime/directory_switch 的 runtime 场景 1:1 迁入,bridge-cli
    审批/卡片/会话套件改用共享装配,断言不减;生产 crate 仅允许 dev-dependency
    引用,由边界门禁强制。
  - workspace `default-members`(6 个生产 crate)、bridge-cli `default-run = "bridge"`;
    tokio/tokio-util/rustix 转为无默认 features 的工作区条目,各 crate 按需启用
    (CLI 保留 rustix `termios`)。
  - `cargo xtask codex-schema`:xtask 仅新增已在 lock 中的 `jsonschema`/`tempfile`,
    对生产 crate 零依赖。`check` 目录驱动校验全部快照(manifest 摘要、版本互证、
    必需 9 文件、schema 合法性、fixtures 过对应 schema),接入 `cargo xtask check`
    管线(11 步);`export` 按规格实现七步流程(版本发现/冲突、临时目录生成、
    兼容预检、staging 原子发布、失败清理、stderr 脱敏、不插值),绝不进 CI,
    不自动改 Rust 源码。`record-fixtures` 裁剪:驱动生产序列化必然要求 xtask
    依赖生产 crate,与依赖约束冲突;其保障已由 bridge-codex 契约测试在同管线
    强制,替代路径为 export 后按打印指引人工重录 + 契约测试验证。
  - 冻结接口零改动(bridge-app 全分支仅 Cargo.toml 一行 features);bridge-cli/
    bridge-codex/bridge-local/bridge-feishu 生产源码零改动;发布包内容不变。
- 验证:workspace 串行测试 290 通过(无预设环境变量);`cargo xtask check` 11 步
  全绿(含新 codex-schema 步骤);`bash -n`、`git diff --check` 通过。合并后在汇总
  分支 `3471bde` 重跑 `cargo xtask check` 全绿。遗留:CI 运行时验证(本地无法触发
  GitHub Actions);`export` 对真实 Codex 的端到端(机器无 codex,已用 stub 覆盖)。

## 基线记录

- B1:`351cbd5`(2026-09-17,工作包 A–D 全部合入;该 commit 上 `cargo xtask check`
  全绿,含 workspace 串行测试、边界、卫生与打包结构检查)。其后仅追加文档同步
  提交,无代码变化。
- B2:`3471bde`(2026-09-18,工作包 A–E 全部合入;该 commit 上 `cargo xtask check`
  11 步全绿)。其后仅追加文档同步提交,无代码变化。工作包 F 须从 B2 之后创建分支。

### F `refactor/ports-diagnostics` — 待开工(基于 B2)

- 按 PLAN.md 串行约定:E 合入并记录 B2 后开工;范围与写入边界见 PLAN.md
  「后续工作包」表格 F 行。

## 汇总分支合并记录

1. `4283bb3` merge: rustix termios feature prerequisite for work package A
2. `d363708` merge: work package A fix/cli-lifecycle
3. `844ebbf` merge: work package B refactor/runtime-flow
4. `d101cca` merge: work package C test/protocol-contracts
5. `351cbd5` merge: work package D build/tooling
6. `3471bde` merge: work package E refactor/test-infrastructure

## 待人工确认/授权事项(统一留到最后)

- 2026-09-17 检查点后经授权推送:`integration/rust-remediation` 与工作包分支
  `test/protocol-contracts`、`build/tooling`、`refactor/test-infrastructure` 推送
  到 origin;PR 创建与审批合并按需进行;
- F(`refactor/ports-diagnostics`,基于 B2 `3471bde` 之后的汇总分支)待开工;
  F 合入并最终验收后,由维护者决定合入主分支;不自动部署;
- CI 变更(D 起引入 xtask 入口、release 冒烟;E 的 codex-schema 步骤)在真实
  GitHub Actions 上的运行验证;
- 真实飞书交互、目标主机生命周期、长时间稳定性属于部署验收,本轮不覆盖。

## 工作包 F 与最终验收(2026-09-18)

- 分支:`integration/rust-remediation`(汇总分支上直接执行,由用户指示)
- 基线:B2(`f9bb0b3`),工具链切换(`0c15d03`)之后
- 交付内容(commit 系列):
  - `c4a61f2` 第 7 项:`TaskFiles`/`Messenger` 端口拆分;`bridge-app::files::Deliveries`
    承接成果选择、数量限额、上传顺序与失败反馈;`bridge-local` 保留文件原语
    (`LocalFiles`:安全打开、快照、差异、稳定句柄、附件发布);飞书事件→应用输入
    转换收回 `bridge-feishu::ingress`;同时以类型化诊断事件替换全部裸 stderr JSON,
    并修复按 `safe_name(task)` 作快照键导致工作区差异/成果从未投递的问题。
  - `d0c2682` 第 9 项:诊断改为可注入 `Diagnostics` 句柄(去除全局 OnceLock);
    补 `DeliveryFailed`/`CardFailed`/`Overloaded`/`HealthWriteFailed`/`ArchiveReconciled`
    事件;RuntimeExit 与 panic hook 移入持有 sink 的 bootstrap;新增有界脱敏的
    `startup_record` 启动失败路径;录制器假实现、可观测性与多实例隔离测试、
    真实二进制启动记录测试。
  - `df2b1aa` 第 8 项:`TaskId`(`{epoch}:{counter}`,含压缩任务)与 `CardToken`
    专用类型贯穿交互注册表、动作注册表、计划报价、ingress 解码与文本命令边界。
  - `ed88b6f` 第 8 项:`SessionStoreError` 携带 `StoreFailureKind` 类别;
    runtime 边界改为 `RuntimeError` 分类枚举(Connection/Capacity/Interaction/
    Maintenance/Backend/Storage/Directory/Internal),Display 保持安全用户文案。
  - `e3a3887` 第 10 项:慢传输(串行所有者响应性、有界 Capacity 停止)与
    命令洪峰(忙碌拒绝、控制路径存活)测试。
- 验证:`cargo xtask check` 全绿(stable 1.98.1,串行测试)。
- 已知既有问题:`test-support` 的 `compaction_requires_existing_valid_idle_session`
  在高并行负载下偶发 `Locked`(与 B2 基线上复现一致,非本批引入;串行执行稳定)。
