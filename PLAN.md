# Current plan

## Completed in the native-only retirement change

- Removed the retired service implementation, compatibility transport, dependency manifests, tests, state conversion commands, and legacy configuration fields.
- Promoted `setup.sh`, `start.sh`, and `package.sh` as the only public lifecycle scripts.
- Replaced interpreter-backed Codex fakes and protocol validation with Rust test targets.
- Updated CI and documentation for a single native runtime.
- Offline validation passed on 2026-09-14: workspace tests/doctests, fmt, Clippy with warnings denied, native binary build, rustdoc, dependency boundaries, shell syntax, configuration check, repository hygiene, and diff whitespace checks.

## Required before production sign-off

- Build and inspect a clean release package.
- Complete real Feishu/Codex interaction checks on the current Linux deployment host.
- Verify start/status/restart/stop, lock ownership, health, log rotation, reconnect, backoff, and descendant cleanup.
- Complete and record a 72-hour stability run. Until these items pass, the code change is complete but production acceptance is not.

Legacy state is intentionally discarded. There is no state conversion or rollback command.
