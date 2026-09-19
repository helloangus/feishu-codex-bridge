# 运行时序图

时序图只画真实存在的通道与函数；参与者的名字都能在代码里找到（`runtime::Input`、`Done::Task`、`CardBook::resolve_click` 等）。系统结构与机制说明见 [architecture.md](architecture.md)，各 crate 的模块与常量清单见 [实现参考](crates/README.md)。

## 文本任务执行（准入 → 准备 → 执行 → 交付）

```mermaid
sequenceDiagram
    participant U as 用户
    participant WS as transport 任务
    participant GW as gateway（route_events）
    participant RT as select! 循环（state::Runtime）
    participant JB as 后台作业（JoinSet）
    participant CX as bridge-codex / Codex
    participant SN as sender 任务

    U->>WS: 文本消息
    WS->>GW: incoming(128) Received{Event::Message} + Acceptance
    GW->>RT: input(64) Input + Ack
    RT->>RT: Admission：白名单 → Command 解析 → admit_task
    RT->>JB: store.claim（持久去重，seen-messages.json）
    JB-->>RT: TaskDone::Admission → commit_admission + ack(true)
    Note over RT: maintain::start_next_task（空闲时）
    RT->>JB: validate_directory → prepare_files（附件下载+staging）
    RT->>JB: sessions::prepare_configured（resume/start 线程 → store.bind 落盘）
    JB-->>RT: TaskDone::Prepared → 建 Execution 门（early 缓冲 64）
    RT->>JB: backend.start_turn
    JB-->>RT: TaskDone::Started → gate.bind(turn) 回放早期事件
    CX-->>RT: item/agentMessage/delta → Output 追加 active.output
    RT->>SN: delivery(128) Progress（3 秒节流预览面板）
    U->>WS: （可选）点击审批 / 回答问题
    CX-->>RT: turn/completed → Finished
    RT->>RT: flow::finish → scheduler.finish → Outcome
    RT->>SN: delivery Answer{outcome, body}
    SN->>U: 预览面板收尾 + Markdown 分片回复
    RT->>JB: task_files.finish_files（diff 面板 + 成果上传）
    JB-->>RT: DeliveryDone::FilesDelivered → FileDelivery::Idle
```

## 审批流（命令 / 文件修改 / 网络）

```mermaid
sequenceDiagram
    participant CX as Codex
    participant RT as select! 循环
    participant IA as Interactions
    participant U as 用户

    CX->>RT: item/commandExecution/requestApproval（服务端请求）
    RT->>RT: permissions.rs 渲染权限 overlay → can_allow 判定
    RT->>IA: 注册 Pending{token=approval-{epoch}-{n}, deadline=600s}
    Note over RT: maintain::deliver_approval_cards 补发卡片
    RT->>U: 审批卡（同意/拒绝按钮，按钮令牌 panel-…-{i}）
    U->>RT: 点击按钮 → card:{token} 输入
    RT->>RT: resolve_click 五重校验 → text="/approve {token}"
    RT->>IA: approve()（条目在异步写之前消费）
    RT->>CX: reply {"decision":"accept"} 或 {"decision":"decline"}
    alt 10 分钟未答复
        RT->>IA: expire → 必需拒绝恰好一次
        RT->>CX: reply {"decision":"decline"}
    else grant_root / 展示不完整
        Note over RT: 同意按钮不渲染；拒绝恒可用
    end
```

## 逐题问答（Plan 模式）

```mermaid
sequenceDiagram
    participant CX as Codex
    participant RT as select! 循环
    participant U as 用户

    CX->>RT: item/tool/requestUserInput{blocking, questions}
    RT->>RT: 三道门：Plan 模式存活 turn / 可完整展示 / 1–32 题
    RT->>U: 问答卡（第 i/n 题 + 选项按钮）
    U->>RT: /choice {token} {i} {option} 或点击选项
    RT->>RT: answer_choice：记录答案 → 下一题（重发新卡）
    opt 选择「其他／自行回答」
        RT->>U: 提示发送 /answer {token} {i} 答案文本
        U->>RT: /answer 文本（≤16 KiB）
    end
    RT->>CX: reply {"answers":{qid:{"answers":[...]}}}
    Note over RT: 任一题缺答案 → 组不完整，什么都不提交；超时 → 停止本次运行
```

## /cd 切换目录（变更门 + 创建确认）

```mermaid
sequenceDiagram
    participant RT as select! 循环
    participant JB as 后台作业
    participant ST as bridge-local（AsyncState）

    RT->>RT: begin_session_mutation（要求全局空闲）
    RT->>JB: store.claim → SessionDone::CreationClaim
    JB-->>RT: 认领成功 → DirectoryClaim
    JB->>ST: propose_directory（解析，绝不创建）
    JB-->>RT: DirectoryProposed
    alt 目标已存在
        JB->>ST: change_directory（解析+持久化一次串行完成）
        JB-->>RT: DirectoryChanged → end_session_mutation
    else 目标不存在
        JB-->>RT: cd-{epoch}-{n} 确认令牌 + 创建确认卡（600 秒）
        RT->>RT: 等待 /cd-confirm（须同用户同聊天同目录）
        RT->>JB: CreationClaim → create_directory（重算提案须一致）
        JB->>ST: safeio 逐组件 mkdirat(0o700)
        JB-->>RT: DirectoryCreated → end_session_mutation
    end
    Note over RT: 成功后该用户世代 +1，全部旧按钮失效
```

## /new 与 /resume（会话绑定）

```mermaid
sequenceDiagram
    participant RT as select! 循环
    participant JB as 后台作业
    participant ST as AsyncState
    participant CX as Codex

    Note over RT,CX: /new：先 claim 再 clear，防重复命令抹掉新绑定
    RT->>RT: begin_session_mutation
    RT->>JB: store.claim → store.clear（仅解除该用户该目录绑定）
    JB-->>RT: SessionDone::Reset → end_session_mutation
    Note over RT,CX: /resume 指定线程：先校验再提交
    RT->>JB: ThreadClaim（resume）
    JB->>CX: thread/read（校验 id/非 active/目录一致）
    JB->>CX: thread/resume
    JB->>ST: store.bind（持久化）
    JB-->>RT: SessionDone::SessionChanged → end_session_mutation
    Note over JB: 任一步失败：保留旧绑定，不自动重试
```

## 归档与对账

```mermaid
sequenceDiagram
    participant RT as 运行时
    participant CX as Codex
    participant ST as AsyncState

    Note over RT,CX: 启动期（bootstrap::reconcile_archives，60 秒超时）
    RT->>ST: bound_threads()（全部已绑定线程）
    RT->>CX: thread/list{archived:true}（分页 ≤100×100，游标环检测）
    RT->>ST: clear_archived_bindings（一次原子提交）
    Note over RT,CX: 运行期（thread/archived 通知）
    CX-->>RT: AgentEvent::Archived{thread}
    RT->>RT: 积压 archived_threads（≤128，超出停机）
    RT->>ST: begin_invalidation 门 → clear_thread（同一提交清除全部绑定）
    ST-->>RT: ArchiveSynced → 释放门
```

## Compact 生命周期（/compact）

```mermaid
sequenceDiagram
    participant RT as select! 循环
    participant JB as 后台作业
    participant CX as Codex

    RT->>RT: begin_session_mutation
    RT->>JB: store.claim → Active{kind: Compact{acknowledged:false}}
    JB->>CX: thread/read + thread/resume（要求非 active、目录一致）
    JB-->>RT: CompactPrepared（先记录期望 thread，再建 Execution 门）
    RT->>JB: CompactSubmitted：backend.compact(thread)
    alt 终态先于 ACK 到达
        CX-->>RT: Finished → park 到 kind.terminal，门保持关闭
        JB-->>RT: submitted=true → 取回 park 的终态 → finish
    else 正常顺序
        JB-->>RT: acknowledged=true
        CX-->>RT: Finished → Outcome::Compact → finish + end_session_mutation
    end
    Note over RT: 全程持有变更门；/new 等被拒；owner /stop 可中断
```

## /stop 与中断

```mermaid
sequenceDiagram
    participant U as 用户
    participant RT as select! 循环
    participant CX as Codex

    U->>RT: /stop
    RT->>RT: cancel_queued（移除本会话排队任务）
    alt 有活动任务
        RT->>RT: active.stopping = true
        RT->>CX: turn/interrupt（校验 TurnRef.epoch 匹配）
        CX-->>RT: turn/completed{interrupted}
        RT-->>U: Outcome::Stopped →「任务已停止」
    else 无活动任务
        RT-->>U: 没有可停止的当前任务
    end
    Note over RT: 他人 /stop 不影响属主任务；压缩终态后不再中断
```

## Plan 确认（/plan-action）

```mermaid
sequenceDiagram
    participant RT as select! 循环
    participant CX as Codex
    participant U as 用户

    CX-->>RT: item/completed{plan} → active.plan（16 KB 截断）
    CX-->>RT: turn/completed{completed}
    RT->>RT: 构造 plans::Offer（token=plan-{task}，600 秒）
    RT->>U: Plan 确认卡（空闲时经 maintain 补发）
    U->>RT: /plan-action {token} implement|fresh|stay（仅原卡点击）
    RT->>RT: 校验 offer 与身份 → end_session_mutation
    alt implement
        RT->>RT: 关闭 Plan 模式 → 计划文本作为新任务直接入实施队列
    else fresh
        RT->>RT: store.clear（清空上下文）→ 计划作为新任务入队
    else stay
        RT->>U: 保持 Plan 模式，继续讨论
    end
```

## 卡片令牌生命周期

```mermaid
sequenceDiagram
    participant RT as select! 循环
    participant CB as CardBook
    participant U as 用户

    RT->>RT: tokens::panel_prefix(epoch, ++next_panel)
    RT->>CB: views.insert(CardView)；actions.insert(按钮令牌→命令串)
    U->>RT: 点击（choice=run，source=消息 id）
    RT->>CB: resolve_click：存在/属主/来源/期限/快照 五重校验
    alt 校验通过
        CB->>CB: take 一次性移除 → 命令串回填 input.text
        alt List 卡片
            RT->>RT: 失效整卡 → refreshes 队列（≤128）重发新面板
        else 交互卡片
            RT->>RT: 通知视图已消费；maintain 生成按钮裁剪更新
        end
    else 任一校验失败
        RT->>U: 「卡片操作无效、已使用或已过期」
    end
```

## 飞书 WebSocket 会话与重连

```mermaid
sequenceDiagram
    participant FS as 飞书云
    participant WS as websocket::Client
    participant APP as runtime

    WS->>FS: POST /callback/ws/endpoint（AppID+Secret）
    FS-->>WS: wss URL + ClientConfig
    WS->>FS: TLS WebSocket 握手（经代理或直连，20 秒限时）
    WS->>APP: ConnectionState::Connected → health.json
    loop 会话循环
        WS->>FS: ping（ping_interval，默认 120s）
        FS-->>WS: pong（payload 可热更新 ClientConfig）
        FS-->>WS: 数据帧（pbbp2，可分片）
        WS->>APP: incoming(128) Received + Acceptance
        APP-->>WS: Ack::settle(bool)
        WS->>FS: 回执帧（code 200/500 + biz_rt）
    end
    alt pong 超时（2×interval+5s）或传输错误
        WS->>APP: ConnectionState::Reconnecting
        WS->>WS: 抖动 rand()×30s → 固定 120s 重试；存活 ≥60s 才重置预算
        WS->>FS: 重新发现端点并重连
    end
```

## 监督层重启与看门狗

```mermaid
sequenceDiagram
    participant G as guard
    participant S as supervise
    participant R as bridge run
    participant H as health.json

    S->>R: spawn（publish Running）
    loop 每 5 秒（仅 supervise 层）
        S->>H: 读心跳（须属于当前子进程 pid）
        alt 从未收到心跳且启动超 120 秒 / 收到过但静默超 45 秒
            S->>R: SIGTERM → 20 秒宽限 → SIGKILL
            S->>S: Backoff（心跳停滞固定 30 秒）→ 重启
        end
    end
    alt 子进程异常退出
        S->>S: descendants::clean() 收割孙进程
        S->>S: Backoff：2、4、8、16、30 秒封顶；>10 次放弃；运行 ≥60 秒才重置
    end
    Note over G: supervise 自身异常退出由 guard 以同样策略重启并清理后代
```

## 优雅关机（cancel 触发）

```mermaid
sequenceDiagram
    participant SIG as signal/错误路径
    participant RT as select! 循环
    participant CX as Codex
    participant SN as sender

    SIG->>RT: CancellationToken 触发
    RT->>CX: 3 秒内 interrupt 活动执行
    RT->>SN: Answer{outcome: BridgeStopped}（未完成任务不会自动重跑）
    RT->>RT: jobs.abort_all() + 排空
    RT->>CX: interactions.drain() → 逐条回传拒绝（3 秒预算）
    RT->>SN: 关闭 delivery → 等待 sender 退出（5 秒）
    Note over RT: bootstrap 收尾：join 五任务 → health.finish 发布最终相位
    Note over CX: 子进程由 AppServer shutdown 收割：TERM → 100ms → KILL 进程组
```

## 结果分类与 tone 映射

`Outcome`（bridge-app/src/outcome.rs）是纯数据；`presentation::label/tone/finished_status` 是它到文案、卡片颜色与诊断状态的唯一映射：

| Outcome | 用户可见首句 | 卡片 tone | 诊断状态 |
|---|---|---|---|
| Completed | 执行完成 | Success | Ok |
| Stopped | 任务已停止 | Warning | Failed |
| BridgeStopped | 桥接已停止；未完成任务不会自动重跑。 | Warning | Failed |
| Failed | 执行失败：… | Error | Failed |
| PrepareFailed | 准备失败：… | Error | Failed |
| StartUnknown | 启动结果未知或失败：…；不会自动重试。 | Error | Failed |
| Compact(Completed) | 上下文压缩完成。 | Info | Failed |
| Compact(Stopped) | 上下文压缩已停止。 | Info | Failed |
| Compact(Failed) | 上下文压缩失败：… | Info | Failed |
| Compact(PrepareFailed) | 压缩准备失败：…；不会自动重试。 | Info | Failed |
| NotStarted | 卡片正文原样（如「系统繁忙…」） | Info | Failed |

完整回复按 5500 字节分片（UTF-8 边界安全，代码围栏跨片自动闭合重开）；预览面板在答案发出前改为「本轮已结束」占位，避免旧进度误导。进度预览与最终回复共享同一面板句柄：预览失败后停止重试，但最终回复仍按分片补发。
