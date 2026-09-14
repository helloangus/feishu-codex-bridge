# Development

The workspace is pinned by `rust-toolchain.toml` and `Cargo.lock`. Run the validation sequence from README before handing off a change. Use `cargo fmt --all` only when intentionally formatting edits; CI uses check mode.

Integration tests use temporary directories, in-memory transports, local socket pairs and Cargo-built fake Codex executables. They must not read `bridge.toml`, credentials, user state, or the network. Protocol fixtures under `fixtures/` are checked against the versioned schemas under `schemas/` by Rust tests.

Keep dependencies directional: core types must not know transports; application behavior depends on ports; Feishu, Codex and local storage implement boundaries; CLI performs composition. Verify this with `cargo xtask check-boundaries`.

For Feishu callbacks, cards, Codex protocol changes, supervision, or packaging, add focused offline regression coverage and record the required real-platform validation separately. Do not treat a successful build as a production deployment.
