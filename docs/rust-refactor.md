# Native runtime status

The workspace now has a single native implementation. Feishu WebSocket/REST, Codex JSON-RPC, business behavior, state, service supervision, testing and packaging are implemented in Rust. Transitional transports, state conversion, alternate lifecycle scripts and interpreter-backed tests have been removed.

Repository retirement is complete when the full offline suite passes. Production acceptance remains separate and requires the real Linux/Feishu/Codex checklist and 72-hour run documented in [operations.md](operations.md).
