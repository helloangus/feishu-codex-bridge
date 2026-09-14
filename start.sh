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
  if [[ -t 0 ]]; then
    if [[ -z "${FEISHU_APP_ID:-}" ]]; then read -r -p 'App ID：' FEISHU_APP_ID; fi
    if [[ -z "${FEISHU_APP_SECRET:-}" ]]; then read -r -s -p 'App Secret：' FEISHU_APP_SECRET; printf '\n'; fi
    if [[ -z "${FEISHU_PAIRING_CODE:-}" && "${BRIDGE_RUST_PAIRING_CODE_PROMPTED:-}" != "1" ]]; then
      printf '%s\n' '可在另一个终端生成配对码：openssl rand -hex 16'
      read -r -s -p '配对码（首次或无白名单时必填，16–256 字节且不能含空白；已有白名单/已配对用户可留空）：' FEISHU_PAIRING_CODE
      printf '\n'
    fi
    export FEISHU_APP_ID FEISHU_APP_SECRET FEISHU_PAIRING_CODE
  fi
  bridge_pairing_bytes="$(printf '%s' "${FEISHU_PAIRING_CODE:-}" | LC_ALL=C wc -c)"
  if [[ -n "${FEISHU_PAIRING_CODE:-}" && ( "$bridge_pairing_bytes" -lt 16 || "$bridge_pairing_bytes" -gt 256 || "$FEISHU_PAIRING_CODE" =~ [[:space:]] ) ]]; then
    printf '%s\n' '配对码无效：必须为 16–256 字节且不能包含空白。' >&2
    exit 1
  fi
fi
if [[ "$bridge_action" == foreground ]]; then
  exec "$bridge_binary" guard --config "$bridge_config"
fi
if [[ "$bridge_action" == start || "$bridge_action" == restart ]]; then
  "$bridge_binary" service --config "$bridge_config" "$bridge_action"
  if [[ -n "${FEISHU_PAIRING_CODE:-}" ]]; then
    printf '%s\n' '若当前飞书用户尚未授权，请在飞书私聊机器人发送：/pair <刚才输入的配对码>'
  fi
  exit 0
fi
exec "$bridge_binary" service --config "$bridge_config" "$bridge_action"
