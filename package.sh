#!/usr/bin/env bash
set -euo pipefail
bridge_repo="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
  printf '%s\n' '用法：bash package.sh [输出目录]' '转发 cargo xtask package --release：构建当前主机 release 二进制，先在临时目录组装并校验 bridge、bridge.example.toml、DEPLOYMENT.md、BUILD-INFO.txt 和 SHA256SUMS，再发布到输出目录（默认 dist/）。不会上传或部署。'
  exit 0
fi
cd -- "$bridge_repo"
exec cargo xtask package --release "$@"
