#!/usr/bin/env bash
set -euo pipefail
# Do not inherit shell tracing into a setup that may later start the service.
set +x
bridge_repo="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
bridge_mode="${1:-}"
bridge_no_start=0
case "$bridge_mode" in
  --help|-h)
    printf '%s\n' \
      '用法：./setup.sh [--check|--no-start]' \
      '默认：必要时引导安装 Rust/C 工具链，生成配置、收集飞书凭据并启动服务。' \
      '--no-start：只完成构建和配置，不提示飞书凭据，也不启动服务。' \
      '--check：只检查现有二进制和配置。' \
      '可选环境变量：BRIDGE_RUST_ROOT、BRIDGE_RUST_CWD、BRIDGE_RUST_STATE_DIR、BRIDGE_RUST_ALLOWED_USERS、BRIDGE_RUST_SANDBOX、BRIDGE_RUST_AUTO_INSTALL。'
    exit 0
    ;;
  --check)
    exec "${BRIDGE_RUST_BIN:-$bridge_repo/target/debug/bridge}" config check --file "${BRIDGE_RUST_CONFIG:-$bridge_repo/bridge.toml}" ;;
  --no-start) bridge_no_start=1 ;;
  '') ;;
  *) printf '%s\n' '未知参数，请使用 --help。' >&2; exit 2 ;;
esac

install_rust_toolchain() {
  local bridge_answer
  if [[ "${BRIDGE_RUST_AUTO_INSTALL:-}" != "1" ]]; then
    if [[ ! -t 0 ]]; then
      printf '%s\n' '未找到完整 Rust/C 工具链。请在交互终端重新运行，或设置 BRIDGE_RUST_AUTO_INSTALL=1 自动安装。' >&2
      return 1
    fi
    read -r -p '未找到完整 Rust/C 工具链，是否立即安装？[Y/n]：' bridge_answer || true
    case "${bridge_answer:-Y}" in
      Y|y|yes|YES) ;;
      *) printf '%s\n' '已取消安装；安装工具链后可重新运行 ./setup.sh。' >&2; return 1 ;;
    esac
  fi
  command -v apt-get >/dev/null || { printf '%s\n' '默认按 Ubuntu 环境安装；未找到 apt-get。请在 Ubuntu/Debian 中运行此脚本。' >&2; return 1; }
  local -a bridge_apt=(apt-get)
  if [[ "$(id -u)" != "0" ]]; then
    command -v sudo >/dev/null || { printf '%s\n' '需要 sudo 安装 Ubuntu 工具链；请安装 sudo 或以 root 身份重试。' >&2; return 1; }
    bridge_apt=(sudo apt-get)
  fi
  printf '%s\n' '正在安装 Ubuntu 编译依赖…'
  "${bridge_apt[@]}" update
  "${bridge_apt[@]}" install -y build-essential curl
  if ! command -v rustup >/dev/null; then
    printf '%s\n' '正在通过 Rust 官网 rustup 脚本安装 Rust…'
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  fi
}

if ! command -v cargo >/dev/null && [[ -x "${HOME:-}/.cargo/bin/cargo" ]]; then
  # A non-login shell often does not load rustup's PATH setup.
  PATH="${HOME}/.cargo/bin:$PATH"
fi
if ! command -v cargo >/dev/null || ! command -v rustup >/dev/null || ! command -v cc >/dev/null; then
  install_rust_toolchain || exit 1
  if ! command -v cargo >/dev/null && [[ -x "${HOME:-}/.cargo/bin/cargo" ]]; then
    PATH="${HOME}/.cargo/bin:$PATH"
  fi
fi
command -v cargo >/dev/null || { printf '%s\n' 'Rust 安装后仍未找到 cargo；请重新打开终端后再运行。' >&2; exit 1; }
command -v rustup >/dev/null || { printf '%s\n' 'Rust 安装后仍未找到 rustup；请重新打开终端后再运行。' >&2; exit 1; }
command -v cc >/dev/null || { printf '%s\n' '未找到 C 编译器；请安装 clang 或 build-essential 后重试。' >&2; exit 1; }
(cd -- "$bridge_repo" && cargo build -p bridge-cli --bin bridge --locked)
bridge_binary="${BRIDGE_RUST_BIN:-$bridge_repo/target/debug/bridge}"
bridge_config="${BRIDGE_RUST_CONFIG:-$bridge_repo/bridge.toml}"
if [[ -e "$bridge_config" || -L "$bridge_config" ]]; then
  "$bridge_binary" config check --file "$bridge_config"
  printf '%s\n' '已有配置已保留；如需修改，请编辑该文件。'
else
  # Defaults make a fresh clone usable immediately.  An interactive terminal can
  # override them; automated invocations keep the same safe home-directory values.
  bridge_home="${HOME:-$bridge_repo}"
  bridge_root="${BRIDGE_RUST_ROOT:-$bridge_home}"
  bridge_cwd="${BRIDGE_RUST_CWD:-$bridge_home}"
  bridge_state="${BRIDGE_RUST_STATE_DIR:-$bridge_home/.local/state/feishu-codex-bridge}"
  bridge_users="${BRIDGE_RUST_ALLOWED_USERS:-}"
  bridge_sandbox="${BRIDGE_RUST_SANDBOX:-workspaceWrite}"
  if [[ -t 0 ]]; then
    read -r -e -p "允许工作的根目录 [$bridge_root]：" bridge_answer || true
    bridge_root="${bridge_answer:-$bridge_root}"
    read -r -e -p "初始工作目录 [$bridge_cwd]：" bridge_answer || true
    bridge_cwd="${bridge_answer:-$bridge_cwd}"
    read -r -e -p "状态目录 [$bridge_state]：" bridge_answer || true
    bridge_state="${bridge_answer:-$bridge_state}"
    read -r -p '允许使用的飞书 open_id（逗号分隔；留空使用配对码）：' bridge_answer || true
    bridge_users="${bridge_answer:-$bridge_users}"
  fi
  [[ -d "$bridge_root" ]] || { printf '工作区根目录不存在：%s\n' "$bridge_root" >&2; exit 1; }
  [[ -d "$bridge_cwd" ]] || { printf '初始工作目录不存在：%s\n' "$bridge_cwd" >&2; exit 1; }
  bridge_root="$(CDPATH= cd -- "$bridge_root" && pwd -P)"
  bridge_cwd="$(CDPATH= cd -- "$bridge_cwd" && pwd -P)"
  case "$bridge_state" in
    /*) ;;
    *) printf '状态目录必须为绝对路径：%s\n' "$bridge_state" >&2; exit 1 ;;
  esac
  bridge_args=(config init --output "$bridge_config" --root "$bridge_root" --cwd "$bridge_cwd" --state-dir "$bridge_state")
  if [[ -n "$bridge_users" ]]; then
    IFS=',' read -r -a bridge_user_list <<< "$bridge_users"
    for bridge_user in "${bridge_user_list[@]}"; do
      [[ -n "${bridge_user//[[:space:]]/}" ]] || { printf '%s\n' '白名单不能包含空用户 ID。' >&2; exit 1; }
      bridge_args+=(--allowed-user "$bridge_user")
    done
  else
    bridge_requires_pairing=1
  fi
  case "$bridge_sandbox" in
    dangerFullAccess) bridge_args+=(--danger-full-access) ;;
    workspaceWrite) ;;
    *) printf '%s\n' 'BRIDGE_RUST_SANDBOX 只能是 workspaceWrite 或 dangerFullAccess；未生成配置。' >&2; exit 2 ;;
  esac
  "$bridge_binary" "${bridge_args[@]}"
fi
if [[ "$bridge_no_start" == "1" ]]; then
  printf '%s\n' '准备完成。启动：./start.sh start；状态：./start.sh status。'
  exit 0
fi
# Credential reading, hidden input and validation live in the Rust CLI and
# follow the environment variable names declared by the configuration.
bridge_credentials_output="$("$bridge_binary" credentials --file "$bridge_config" --print)" || exit 1
eval "$bridge_credentials_output"
unset bridge_credentials_output
printf '%s\n' '凭据已就绪。若当前飞书用户尚未授权，请在飞书私聊机器人发送：/pair <配对码>'
exec "$bridge_repo/start.sh" start
