# Operations

Use `./start.sh status` as the primary health view. Interpret supervisor liveness together with its latest snapshot; an old `running` snapshot alone does not prove the process is alive. Runtime and supervisor events are stored under the configured state directory with bounded log rotation.

For incidents, prefer `status`, targeted logs, and verified service actions. Do not delete lock files or use broad process-kill commands. `stop` and `restart` validate ownership and clean the supervised process tree.

Before production sign-off on the current Linux host:

1. Verify pairing/allowlist behavior and unauthorized rejection.
2. Exercise text, cards, session commands, model/Plan settings, approvals, questions, attachments, streaming and stop.
3. Force WebSocket reconnect and a Codex child failure; confirm recovery, bounded backoff and no duplicate execution.
4. Verify start/status/restart/stop, lock exclusion, health freshness, log rotation and descendant cleanup.
5. Run continuously for 72 hours without lost connectivity, duplicate consumption, state leakage or orphan processes.

Record timestamps, versions, sanitized logs and card captures. Offline tests and a local package build do not satisfy this checklist.
