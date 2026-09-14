# Architecture

This describes the current implementation, including known responsibility overlap. [PLAN.md](../PLAN.md) specifies the intended refactoring; it does not imply that those changes have already landed.

`bridge-core` defines shared commands, task/session identities and presentation types. Application ports use explicit boxed futures for object-safe asynchronous calls and fake injection; no change of async-trait mechanism is required by the current plan.

`bridge-cli` loads `bridge.toml`, validates credentials supplied through named environment variables, acquires the App ID lock, and assembles the native runtime. Its `guard → supervise → run` process chain provides restart policy, health publication, log rotation, and descendant cleanup.

`bridge-feishu` owns native WebSocket framing, heartbeat/reconnect behavior, proxy handling, REST calls, cards, media and event decoding. Accepted events enter bounded application channels and are acknowledged only after durable admission or deliberate rejection.

`bridge-app` owns authorization, command routing, scheduling, sessions, runtime approvals/questions, Plan flow and result presentation. It depends on ports rather than transport implementations. Its runtime currently has a separate approval implementation from the standalone interaction Registry; unifying those paths is pending.

`bridge-codex` is the sole owner of Codex app-server stdin/stdout JSON-RPC. It converts protocol messages to typed application events and owns the Codex process group.

`bridge-local` owns versioned JSON state, deduplication, workspace containment, snapshots and delivery staging. Its Delivery adapter also currently selects results, renders delivery summaries and coordinates messaging. Moving those application policies into `bridge-app` is pending. Individual JSON writes are atomic and state access is lock protected; directory creation and state publication are not one cross-resource transaction.

The CLI currently performs part of the raw Feishu event-to-application conversion. Typed ingress conversion is intended to move fully into `bridge-feishu`. Runtime diagnostics currently mix a global Sink with direct stderr events; unified persistent diagnostics are also pending.

## Runtime invariants to preserve

Ordinary tasks execute serially. Directory, session and preference changes use the scheduler's idle mutation gate; this remediation does not relax it. Task directories are captured and revalidated before execution. User/directory preferences are persisted before their in-memory selection changes.

Message admission is durably claimed before side effects. Claimed operations with failed or unknown outcomes are not automatically replayed. Approvals and card actions remain scoped to the owning user, chat and current task/turn, and incomplete question answers must not be submitted. Directory creation may leave a created directory when the later state commit fails; the runtime reports failure without changing the selected directory or automatically deleting user files.

Credentials are never stored in TOML or logs. Configuration, state, runtime health, logs and downloaded attachments remain separate boundaries.
