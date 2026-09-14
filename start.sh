#!/usr/bin/env bash
set -euo pipefail
# Never inherit shell tracing when handling credentials.
set +x
bridge_repo="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
bridge_binary="${BRIDGE_RUST_BIN:-$bridge_repo/target/debug/bridge}"
bridge_config="${BRIDGE_RUST_CONFIG:-$bridge_repo/bridge.toml}"
bridge_action="${1:-status}"
case "$bridge_action" in
  --help|-h) printf '%s\n' '用法：bash start.sh start|stop|restart|status|foreground' '使用生成的 bridge.toml；凭据从环境继承，交互终端可隐藏输入。不读取 .env，不保存凭据。' '自定义配置/二进制：BRIDGE_RUST_CONFIG、BRIDGE_RUST_BIN。'; exit 0 ;;
  start|restart|foreground|stop|status) ;;
  *) printf '%s\n' '未知操作，请使用 --help。' >&2; exit 2 ;;
esac
[[ -x "$bridge_binary" ]] || { printf '%s\n' 'Rust 二进制不存在，请先运行 bash setup.sh。' >&2; exit 1; }
if [[ "$bridge_action" == start || "$bridge_action" == restart || "$bridge_action" == foreground ]]; then
  # The Rust CLI reads, hidden-prompts and validates credentials according to
  # the configured environment variable names and prints export statements.
  bridge_credentials_output="$("$bridge_binary" credentials --file "$bridge_config" --print)" || exit 1
  eval "$bridge_credentials_output"
  unset bridge_credentials_output
fi
if [[ "$bridge_action" == foreground ]]; then
  exec "$bridge_binary" guard --config "$bridge_config"
fi
if [[ "$bridge_action" == start || "$bridge_action" == restart ]]; then
  "$bridge_binary" service --config "$bridge_config" "$bridge_action"
  if [[ "${BRIDGE_RUST_PAIRING_PRESENT:-}" == "1" ]]; then
    printf '%s\n' '若当前飞书用户尚未授权，请在飞书私聊机器人发送：/pair <配对码>'
  fi
  exit 0
fi
exec "$bridge_binary" service --config "$bridge_config" "$bridge_action"
