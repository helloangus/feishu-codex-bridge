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

validate_workspace_config() {
  if [ -z "${CODEX_WORKSPACE_ROOT:-}" ]; then
    echo "缺少 CODEX_WORKSPACE_ROOT：请在 $ENV_FILE 中设置允许切换的工作区根目录。" >&2
    echo "该目录必须包含 CODEX_BRIDGE_CWD；例如：CODEX_WORKSPACE_ROOT=$(dirname -- "$CODEX_BRIDGE_CWD")" >&2
    return 1
  fi
  if [ ! -d "$CODEX_BRIDGE_CWD" ]; then
    echo "CODEX_BRIDGE_CWD 不存在或不是目录：$CODEX_BRIDGE_CWD" >&2
    return 1
  fi
  if [ ! -d "$CODEX_WORKSPACE_ROOT" ]; then
    echo "CODEX_WORKSPACE_ROOT 不存在或不是目录：$CODEX_WORKSPACE_ROOT" >&2
    return 1
  fi
  bridge_cwd_path="$(CDPATH= cd -- "$CODEX_BRIDGE_CWD" && pwd -P)"
  bridge_workspace_path="$(CDPATH= cd -- "$CODEX_WORKSPACE_ROOT" && pwd -P)"
  case "$bridge_cwd_path" in
    "$bridge_workspace_path"|"$bridge_workspace_path"/*) ;;
    *)
      echo "CODEX_BRIDGE_CWD 必须位于 CODEX_WORKSPACE_ROOT 内。" >&2
      return 1
      ;;
  esac
}

case "${1:-start}" in
  start|restart|foreground) validate_workspace_config || exit 1 ;;
esac

exec python "$BRIDGE_DIR/service.py" "${@:-start}"
