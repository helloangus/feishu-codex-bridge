# Repository Guidelines

## Project Structure & Module Organization

`bridge.py` contains the Feishu event handlers, cards, Codex app-server JSON-RPC adapter, session state, attachments, and delivery handling. `service.py` owns the process lock, health file, log rotation, and restart supervision. Use `start.sh` for normal service lifecycle and `setup.sh` for first-device setup. Runtime configuration comes from `.env.example`; never commit a real `.env`.

Tests live in `tests/`: `test_bridge.py` covers bridge behavior with fakes, and `test_service.py` covers supervisor helpers in temporary directories. Keep user-facing and operational documentation in `README.md` and `docs/` (`architecture.md`, `design.md`, `development.md`, `deployment.md`, `operations.md`). Update `PLAN.md` when planned work or its status changes.

## Build, Test, and Development Commands

```sh
python -m unittest discover -s tests -v  # run offline regression tests
python -m py_compile bridge.py service.py # check Python syntax
git diff --check                          # find whitespace errors
./setup.sh --check                        # validate deployment prerequisites
./start.sh foreground                     # run under the normal supervisor
./start.sh status                         # inspect service and Feishu health
```

Use `start.sh` instead of directly running `bridge.py`; direct execution skips locking, log rotation, and restart behavior.

## Coding Style & Naming Conventions

Use Python with four-space indentation, standard-library-first imports, `snake_case` for functions and variables, `PascalCase` for classes, and `UPPER_CASE` for module constants. Prefer small, explicit helpers over broad abstractions. User-triggered or RPC work must catch exceptions and respond through the existing Feishu card/text path; do not let exceptions escape callback threads.

`CodexServer` is the sole owner of app-server stdin/stdout JSON-RPC. Route new notifications through `handle_event()` and `Bridge.codex_event()` rather than reading stdout elsewhere. Do not interpolate user input into shell commands.

## Testing Guidelines

Add a focused `unittest` regression for every offline-verifiable change. Name test methods `test_<behavior>`. Tests must not load a real `.env`, call the network, start long-running processes, or alter the user working directory; use fakes and `TemporaryDirectory`. Changes to Feishu callbacks, cards, or app-server protocol also require manual Feishu validation against the installed CLI version.

## Commit & Pull Request Guidelines

Follow the established Conventional Commit-like style: `feat:`, `fix:`, `test:`, and `docs:` with a concise imperative summary. Keep commits scoped. PRs should explain behavior changes, list validation commands, link relevant issues, and include screenshots or card captures for Feishu UI changes. Update README and relevant `docs/` when commands, architecture, deployment, or operations change. Never include credentials, tokens, logs, session files, or downloaded attachments.
