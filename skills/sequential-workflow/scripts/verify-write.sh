#!/usr/bin/env bash
# verify-write.sh — 验证 subagent 写文件后落盘状态
#
# 用法: bash scripts/verify-write.sh [ROOT] <relpath1> [relpath2 ...]
#   ROOT:     项目根目录 (默认当前目录)
#   relpath:  相对于 ROOT 的文件路径, 例如 docs/design-plans/2026-06-02-foo.md
#
# 退出码:
#   0 = 全部文件存在且非空
#   1 = 任一文件缺失或为空
#
# 输出 (每行):
#   OK: <path> (<bytes> bytes)        — 文件存在且非空
#   MISSING: <path>                   — 文件不存在或为空
set -euo pipefail

ROOT="${1:-$(pwd)}"
shift || { echo "用法: $0 [ROOT] <file1> [file2 ...]" >&2; exit 2; }

fail=0
for rel in "$@"; do
  path="$ROOT/$rel"
  if [[ ! -s "$path" ]]; then
    echo "MISSING: $path"
    fail=1
  else
    bytes=$(wc -c < "$path" | tr -d ' ')
    echo "OK: $path ($bytes bytes)"
  fi
done

exit $fail
