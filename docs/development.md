# Development

The workspace is pinned by `rust-toolchain.toml` and `Cargo.lock`. Run the validation sequence from README before handing off a change. Use `cargo fmt --all` only when intentionally formatting edits; CI uses check mode.

Read [PLAN.md](../PLAN.md) before remediation work. It distinguishes completed documentation cleanup from pending code changes. `cargo xtask check` and `cargo xtask package` are planned, not available commands; the current xtask only provides `check-boundaries`.

Integration tests use temporary directories, in-memory transports, local sockets and Cargo-built fake Codex executables. They must not read real `bridge.toml`, credentials or user state, or contact external services. Shell-entrypoint tests currently use Shell fakes. Protocol fixtures under `fixtures/` are checked against the versioned schemas under `schemas/` by Rust tests; extending this to actual production serialization is a pending plan item.

Keep dependencies directional: core types must not know transports; application behavior depends on ports; Feishu, Codex and local storage implement boundaries; CLI performs composition. Verify this with `cargo xtask check-boundaries`.

For Feishu callbacks, cards, Codex protocol changes, supervision, or packaging, add focused offline regression coverage and record the required real-platform validation separately. Do not treat a successful build as a production deployment.
