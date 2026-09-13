#!/usr/bin/env bash
set -euo pipefail
bridge_repo="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
bridge_output="${1:-$bridge_repo/dist}"
if [[ "$bridge_output" == --help || "$bridge_output" == -h ]]; then
  printf '%s\n' '用法：bash package-rust.sh [输出目录]' '构建当前主机 release 二进制，生成带 SHA256SUMS 和源码版本信息的独立目录。不会上传或部署。'
  exit 0
fi
(cd -- "$bridge_repo" && cargo build -p bridge-cli --release --locked)
mkdir -p -- "$bridge_output"
bridge_package="$(mktemp -d "$bridge_output/feishu-codex-bridge.XXXXXXXX")"
cp -- "$bridge_repo/target/release/bridge" "$bridge_package/bridge"
cp -- "$bridge_repo/bridge.example.toml" "$bridge_package/bridge.example.toml"
cp -- "$bridge_repo/docs/rust-deployment.md" "$bridge_package/DEPLOYMENT.md"
cp -- "$bridge_repo/docs/migration.md" "$bridge_package/migration.md"
(cd -- "$bridge_repo" && git rev-parse HEAD && git status --porcelain --untracked-files=no && rustc -vV) > "$bridge_package/BUILD-INFO.txt"
(cd -- "$bridge_package" && sha256sum bridge bridge.example.toml DEPLOYMENT.md migration.md BUILD-INFO.txt > SHA256SUMS)
printf '本机发布目录：%s\n' "$bridge_package"
printf '%s\n' '只适用于兼容的主机架构和系统运行库；未交叉编译，未发布到外部平台。'
