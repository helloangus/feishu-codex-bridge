#!/usr/bin/env bash
set -euo pipefail
bridge_repo="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
case "${1:-}" in
  --help|-h) printf '%s\n' '用法：bash setup-rust.sh [--check]' '默认：构建 Rust 并引导生成配置，不启动服务。--check：只检查现有二进制和配置。'; exit 0 ;;
  --check)
    command -v codex >/dev/null || { printf '%s\n' '未找到 codex，请先安装并登录。' >&2; exit 1; }
    exec "${BRIDGE_RUST_BIN:-$bridge_repo/target/debug/bridge}" config check --file "${BRIDGE_RUST_CONFIG:-$bridge_repo/bridge.toml}" ;;
  '') ;;
  *) printf '%s\n' '未知参数，请使用 --help。' >&2; exit 2 ;;
esac
command -v cargo >/dev/null || { printf '%s\n' '请先安装 Rust 工具链和 C 编译器。' >&2; exit 1; }
command -v codex >/dev/null || { printf '%s\n' '请先安装并登录 Codex CLI。' >&2; exit 1; }
(cd -- "$bridge_repo" && cargo build -p bridge-cli --locked)
bridge_binary="${BRIDGE_RUST_BIN:-$bridge_repo/target/debug/bridge}"
bridge_config="${BRIDGE_RUST_CONFIG:-$bridge_repo/bridge.toml}"
if [[ -e "$bridge_config" || -L "$bridge_config" ]]; then
  "$bridge_binary" config check --file "$bridge_config"
  printf '%s\n' '已有配置已保留；如需修改，请编辑该文件。'
  exit 0
fi
[[ -t 0 ]] || { printf '%s\n' '交互配置需要终端；自动化请使用 bridge config init --help。' >&2; exit 1; }
read -r -p '允许工作的根目录（绝对路径）：' bridge_root
read -r -p '初始工作目录（绝对路径）：' bridge_cwd
read -r -p "状态目录（默认 $bridge_repo/.bridge-state）：" bridge_state
bridge_state="${bridge_state:-$bridge_repo/.bridge-state}"
read -r -p '用户 open_id（逗号分隔，留空启用配对）：' bridge_users
bridge_args=(config init --output "$bridge_config" --root "$bridge_root" --cwd "$bridge_cwd" --state-dir "$bridge_state")
if [[ -n "$bridge_users" ]]; then
  IFS=',' read -r -a bridge_user_list <<< "$bridge_users"
  for bridge_user in "${bridge_user_list[@]}"; do bridge_args+=(--allowed-user "$bridge_user"); done
fi
printf '%s\n' '默认 workspaceWrite 沙箱。dangerFullAccess 会关闭 Codex 沙箱隔离，仅在明确需要时选择。'
read -r -p '如需关闭沙箱，输入 dangerFullAccess；否则直接回车：' bridge_sandbox
case "$bridge_sandbox" in
  dangerFullAccess) bridge_args+=(--danger-full-access) ;;
  '') ;;
  *) printf '%s\n' '无法识别沙箱选择，未生成配置。' >&2; exit 2 ;;
esac
"$bridge_binary" "${bridge_args[@]}"
printf '%s\n' '准备完成。启动：bash start-rust.sh start；状态：bash start-rust.sh status。'
