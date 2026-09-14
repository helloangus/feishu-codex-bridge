# Architecture

`bridge-cli` loads `bridge.toml`, validates credentials supplied through named environment variables, acquires the App ID lock, and assembles the native runtime. Its `guard → supervise → run` process chain provides restart policy, health publication, log rotation, and descendant cleanup.

`bridge-feishu` owns native WebSocket framing, heartbeat/reconnect behavior, proxy handling, REST calls, cards, media and event decoding. Accepted events enter bounded application channels and are acknowledged only after durable admission or deliberate rejection.

`bridge-app` owns authorization, command routing, scheduling, sessions, interactions, cards, approvals, Plan flow, attachment delivery and result presentation. It depends on ports rather than transport implementations.

`bridge-codex` is the sole owner of Codex app-server stdin/stdout JSON-RPC. It converts protocol messages to typed application events and owns the Codex process group.

`bridge-local` owns versioned JSON state, deduplication, workspace containment, snapshots and delivery staging. Writes are atomic and state access is lock protected.

Credentials are never stored in TOML or logs. Configuration, state, runtime health, logs and downloaded attachments remain separate boundaries.
