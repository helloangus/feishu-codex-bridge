# Repository Guidelines

## Active handoff

Read [PLAN.md](PLAN.md) for the active remediation backlog and its acceptance criteria. Unchecked items describe intended changes, not implemented capabilities. The previous migration plan and transitional ADRs have been retired. The structure below describes current ownership; update it when implementing the planned responsibility changes instead of treating it as a prohibition on refactoring.

## Structure

This is a Rust workspace. `bridge-cli` assembles configuration, runtime, health, and supervision; `bridge-app` owns application behavior; `bridge-codex` owns app-server JSON-RPC; `bridge-feishu` owns native Feishu WebSocket/REST; `bridge-local` owns state and workspace access. `crates/test-support` holds the fake processes and shared test assembly; production crates may reference it only as a dev-dependency, enforced by the boundary gate. Use `setup.sh`, `start.sh`, and `package.sh` for lifecycle and packaging. Never commit `bridge.toml`, credentials, state, logs, sessions, or downloaded attachments.

## Validation

```sh
cargo xtask check
```

The unified entry runs fmt, Clippy (`-D warnings`), serial workspace tests, the product build, rustdoc with warnings as errors, dependency boundaries, Codex protocol snapshot checks, repository hygiene, shell syntax, diff checks, and package structure checks. `cargo xtask fetch` prefetches dependencies; `cargo xtask check --offline` then runs fully offline. `cargo xtask check-boundaries`, `cargo xtask codex-schema check`, and `cargo xtask package [输出目录]` can also be invoked individually.

Tests must stay offline, use temporary directories and fakes, and must not change the user working directory or launch long-running services. The repository must not add Python source, bytecode, dependency manifests, interpreter calls, or test tooling.

## Style and safety

Use stable Rust 1.85.1, rustfmt, explicit error propagation, bounded channels, and typed protocol boundaries. `bridge-codex` remains the sole app-server stdin/stdout owner. Route Feishu events through the native ingress and application runtime. Never interpolate user input into shell commands. User-triggered failures must be returned through the existing card/text delivery path.

Update README, relevant `docs/`, and `PLAN.md` when behavior, commands, architecture, deployment, or operational status changes. Use scoped Conventional Commit-like summaries: `feat:`, `fix:`, `test:`, and `docs:`.
