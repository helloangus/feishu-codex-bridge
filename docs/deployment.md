# Deployment

## Source checkout setup

On the target Linux host, install and log in to Codex CLI, then run:

```sh
./setup.sh
```

The script checks or installs the Rust/C toolchain on Ubuntu/Debian, builds only the `bridge` binary, creates `bridge.toml` with mode `0600`, prompts for credentials, and starts the supervised service. Use `--no-start` to build and configure only, or `--check` to validate an existing binary and configuration.

Non-interactive setup that starts the service requires `FEISHU_APP_ID` and `FEISHU_APP_SECRET`; restricted deployments without a static allowlist or previously paired users also require `FEISHU_PAIRING_CODE`. `--no-start` does not require credentials. The code must be 16–256 bytes without whitespace. Configuration paths must be absolute and the initial directory must remain inside the workspace root.

## Source checkout lifecycle

```sh
./start.sh start
./start.sh status
./start.sh restart
./start.sh stop
./start.sh foreground
```

Do not bypass the guard/supervisor for normal operation. Only one instance may consume an App ID on a host.

## Binary package deployment

This guide is also shipped as `DEPLOYMENT.md` alongside `bridge`, `bridge.example.toml`, `BUILD-INFO.txt` and `SHA256SUMS`. That package does not include source checkout scripts. Use the commands in this section from the package directory; Rust, Cargo and the source checkout are not needed to run the binary. The target must be a compatible Linux host with Codex CLI installed and logged in for the service user. Building from source still requires the Rust/C toolchain; the dependency chain includes native cryptography and Linux system interfaces.

Verify the package before use:

```sh
sha256sum -c SHA256SUMS
./bridge --version
```

Generate a configuration, replacing the absolute example paths with your existing workspace and intended state directory. The initial directory must be within the root. Configuration generation refuses to overwrite an existing file and does not start the service:

```sh
./bridge config init --output ./bridge.toml \
  --root /absolute/workspace --cwd /absolute/workspace \
  --state-dir /absolute/private/bridge-state
./bridge config check --file ./bridge.toml
```

For a static allowlist, add `--allowed-user ou_your_open_id` when generating the configuration; the option may be repeated. Otherwise the generated configuration uses pairing. Supply `FEISHU_APP_ID`, `FEISHU_APP_SECRET` and, when required, `FEISHU_PAIRING_CODE` through the service user's environment before starting. The binary currently does not prompt for them. Do not put secret values into shell history or the TOML file. For an existing configuration with custom environment variable names, supply those named variables instead. `config check` is an offline configuration check, not a credential or connectivity check.

```sh
./bridge service --config ./bridge.toml start
./bridge service --config ./bridge.toml status
./bridge service --config ./bridge.toml restart
./bridge service --config ./bridge.toml stop
```

For supervised foreground operation, use `./bridge guard --config ./bridge.toml`. A successful start means the guard has started; query status to verify the Feishu connection. Use `service ... status` as shown here: the separate `bridge status` command has a known timer initialization defect awaiting remediation. The current CLI may suggest `./start.sh status`; in a binary package use the equivalent `service ... status` command above.

Configuration generation sets mode `0600`. Runtime state, health and rotating logs reside under the configured state directory; preserve this directory when updating an existing native installation. Stop the old service before switching to a replacement package. Real-platform acceptance still requires pairing, messages/cards, approvals/questions, attachments, reconnect, service lifecycle and a recorded 72-hour stability run; offline checks alone do not establish production readiness.

## Packaging from a source checkout

```sh
bash package.sh
```

Each invocation creates a new host-native release directory containing the `bridge` binary, example configuration, deployment guide, build information, and `SHA256SUMS`. It contains no credentials, state, logs, attachments, compatibility runtime, or state conversion tools. Verify checksums before deployment.

The current script assumes the default Cargo target directory and a host-native build. Custom target-directory handling and atomic package publication are pending remediation. Do not set `CARGO_TARGET_DIR` or a custom build target for this script until that work lands. A packaged build is not evidence that production acceptance has passed.
