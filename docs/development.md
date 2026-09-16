# Development

The workspace is pinned by `rust-toolchain.toml` and `Cargo.lock`. Run `cargo xtask check` before handing off a change. Use `cargo fmt --all` only when intentionally formatting edits; CI uses check mode.

Read [PLAN.md](../PLAN.md) before remediation work. It distinguishes completed documentation cleanup from pending code changes. `cargo xtask check` is the single validation entry; `cargo xtask fetch` separates dependency prefetch from the fully offline `cargo xtask check --offline`. `cargo xtask package [输出目录]` builds and verifies the release package (`package.sh` forwards to it), and `cargo xtask check-boundaries` re-runs the dependency boundary gate alone.

Integration tests use temporary directories, in-memory transports, local sockets and Cargo-built fake Codex executables. They must not read real `bridge.toml`, credentials or user state, or contact external services. Shell-entrypoint tests currently use Shell fakes. Protocol fixtures under `fixtures/`, production request serialization and real decoding are checked against the versioned schemas under `schemas/` by Rust tests; the protocol baseline version is pinned once in `bridge_codex::protocol::CODEX_SCHEMA_BASELINE`. Schema snapshots change only through a reviewed maintenance PR that regenerates the snapshot directory and updates the constant together; an explicit `cargo xtask codex-schema` maintenance command is planned to automate this.

Keep dependencies directional: core types must not know transports; application behavior depends on ports; Feishu, Codex and local storage implement boundaries; CLI performs composition. Verify this with `cargo xtask check-boundaries`.

For Feishu callbacks, cards, Codex protocol changes, supervision, or packaging, add focused offline regression coverage and record the required real-platform validation separately. Do not treat a successful build as a production deployment.
