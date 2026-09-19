# 运行时序图

时序图只画真实存在的通道与函数。文本任务的主循环见 [architecture.md](architecture.md) 的运行时数据流。

## 文本任务执行（准入 → 审批 → 交付）

```mermaid
sequenceDiagram
    participant U as 用户
    participant WS as websocket 任务
    participant GW as gateway（route_events）
    participant RT as select! 循环（state::Runtime）
    participant JB as 后台任务（JoinSet）
    participant CX as bridge-codex / Codex

    U->>WS: 消息
    WS->>GW: incoming(128) Received + Acceptance
    GW->>RT: input(64) Input + Ack
    RT->>RT: Admission：白名单/去重 → scheduler.reserve
    RT->>JB: Done::Task(Admission)（持久 claim）
    JB-->>RT: claim 结果 → commit_admission
    RT->>JB: Prepared：附件 staging + 会话准备
    JB-->>RT: gate = Execution::starting（缓冲早期事件）
    RT->>JB: Started：backend.start_turn
    JB-->>RT: bind(turn) → 回放缓冲事件
    CX-->>RT: Approval 请求 → 注册 Interactions
    RT->>JB: ApprovalSent：send_panel（panel-{epoch}-{n} 令牌）
    U->>WS: 点击同意
    WS->>GW: card: Input（card:{token} 回执 id）
    RT->>RT: CardBook::resolve_click 五重校验
    RT->>JB: ApprovalReplied（受控回传）
    CX-->>RT: turn/completed → Outcome
    RT->>SN: delivery(128) Answer{outcome, body}
    SN->>U: 卡片/文本（tone 由 Outcome 映射）
```

## /cd 切换目录（变更门 + 创建确认）

```mermaid
sequenceDiagram
    participant RT as select! 循环
    participant JB as 后台任务
    participant ST as bridge-local（AsyncState）

    RT->>RT: begin_session_mutation（要求全局空闲）
    RT->>JB: Session(DirectoryClaim)
    JB->>ST: claim（持久去重）
    JB->>ST: propose_directory
    JB-->>RT: DirectoryProposed
    alt 目标已存在
        JB->>ST: change_directory → 目录世代 +1
        JB-->>RT: DirectoryChanged → end_session_mutation
    else 目标不存在
        JB-->>RT: cd-{epoch}-{n} 确认令牌 + 卡片
        RT->>RT: 等待 /cd-confirm（10 分钟有效）
        RT->>JB: CreationClaim → create_directory
        JB-->>RT: DirectoryCreated → end_session_mutation
    end
```

## Compact 生命周期

```mermaid
sequenceDiagram
    participant RT as select! 循环
    participant JB as 后台任务
    participant CX as Codex

    RT->>RT: begin_session_mutation
    RT->>JB: Session(CompactClaim) → ActiveKind::Compact
    JB-->>RT: CompactPrepared（gate 就绪，记录期望 thread）
    RT->>JB: CompactSubmitted：backend.compact(thread)
    alt 终态先于 ACK 到达
        CX-->>RT: Finished → 挂起 terminal，门保持关闭
        JB-->>RT: submitted=true → 取回 terminal → finish
    else 正常顺序
        JB-->>RT: submitted=true，acknowledged=true
        CX-->>RT: Finished → Outcome::Compact → finish
    end
    Note over RT: 全程持有变更门，/new 等被拒绝
```

## 结果分类与 tone 映射

`Outcome`（`bridge-app::outcome`）是纯数据；`presentation::label/tone/finished_status` 是它到文案、卡片颜色与诊断状态的唯一映射：

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
| NotStarted | 卡片正文原样（如"系统繁忙…"） | Info | Failed |

## 卡片令牌生命周期

```mermaid
sequenceDiagram
    participant RT as select! 循环
    participant CB as CardBook
    participant U as 用户

    RT->>RT: tokens::panel_prefix(epoch, ++next_panel)
    RT->>CB: 视图插入 CardView{panel, kind}；动作登记 Action
    U->>RT: 点击（click:{token}）
    RT->>CB: resolve_click：存在/属主/来源/期限/快照
    alt 校验通过
        CB->>CB: 动作一次性移除（take）
        alt List 卡片
            RT->>RT: 失效整卡并重发新面板
        else 交互卡片
            RT->>RT: 通知视图已消费
        end
    else 任一校验失败
        RT->>U: "卡片操作无效、已使用或已过期"
    end
```
