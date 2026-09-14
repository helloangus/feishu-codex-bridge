# Deployment

## First setup

On the target Linux host, install and log in to Codex CLI, then run:

```sh
./setup.sh
```

The script checks or installs the Rust/C toolchain on Ubuntu/Debian, builds only the `bridge` binary, creates `bridge.toml` with mode `0600`, prompts for credentials, and starts the supervised service. Use `--no-start` to build and configure only, or `--check` to validate an existing binary and configuration.

Non-interactive setup requires `FEISHU_APP_ID` and `FEISHU_APP_SECRET`; restricted deployments without a static allowlist also require `FEISHU_PAIRING_CODE`. The code must be 16–256 bytes without whitespace. Configuration paths must be absolute and the initial directory must remain inside the workspace root.

## Lifecycle

```sh
./start.sh start
./start.sh status
./start.sh restart
./start.sh stop
./start.sh foreground
```

Do not bypass the guard/supervisor for normal operation. Only one instance may consume an App ID on a host.

## Packaging

```sh
bash package.sh
```

Each invocation creates a new host-native release directory containing the `bridge` binary, example configuration, deployment guide, build information, and `SHA256SUMS`. It contains no credentials, state, logs, attachments, compatibility runtime, or state conversion tools. Verify checksums before deployment.

This project does not support importing or exporting retired state formats. Start with a fresh state directory.
