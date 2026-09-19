# Operations

Use `./start.sh status` as the primary health view. Interpret supervisor liveness together with its latest snapshot; an old `running` snapshot alone does not prove the process is alive. Runtime and supervisor events are stored under the configured state directory with bounded log rotation.

The supervisor's lifecycle is the `Phase` state machine in [architecture.md](architecture.md)（Starting → Running → Backoff/Stopping → Stopped/Failed）; the control socket answers each phase query with one JSON `LivePhase` object (`{"phase":"backoff","retry_delay_seconds":30}`), and `bridge status` renders it directly — there is no free-form phase string to parse. The heartbeat watchdog runs only in the inner supervise layer: a bridge that produces no heartbeat for 120 s at startup (grace must exceed the 10 s heartbeat interval plus the 30 s freshness window) or 45 s once heartbeats flow is stopped and retried under the bounded backoff schedule (2 s doubling to a 30 s cap; a run shorter than 60 s never resets the budget).

For incidents, prefer `status`, targeted logs, and verified service actions. Do not delete lock files or use broad process-kill commands. `stop` and `restart` validate ownership and clean the supervised process tree.

Before production sign-off on the current Linux host:

1. Verify pairing/allowlist behavior and unauthorized rejection.
2. Exercise text, cards, session commands, model/Plan settings, approvals, questions, attachments, streaming and stop.
3. Force WebSocket reconnect and a Codex child failure; confirm recovery, bounded backoff and no duplicate execution.
4. Verify start/status/restart/stop, lock exclusion, health freshness, log rotation and descendant cleanup.
5. Run continuously for 72 hours without lost connectivity, duplicate consumption, state leakage or orphan processes.

Record timestamps, versions, sanitized logs and card captures. Offline tests and a local package build do not satisfy this checklist.
