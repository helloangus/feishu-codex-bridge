#!/data/data/com.termux/files/usr/bin/bash
set -eu

BRIDGE_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
ENV_FILE="${BRIDGE_ENV_FILE:-$BRIDGE_DIR/.env}"

if [ ! -f "$ENV_FILE" ]; then
  echo "缺少配置文件：$ENV_FILE" >&2
  echo "请先执行：cp $BRIDGE_DIR/.env.example $ENV_FILE，然后填写飞书凭据。" >&2
  exit 1
fi

set -a
. "$ENV_FILE"
set +a

if [ -z "${CODEX_BRIDGE_CWD:-}" ]; then
  export CODEX_BRIDGE_CWD="$(pwd)"
fi

exec python "$BRIDGE_DIR/bridge.py"
