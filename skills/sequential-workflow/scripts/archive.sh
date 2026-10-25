#!/usr/bin/env bash
# archive.sh — 归档 design-plan + exec-plan 到 completed/
#
# 用法: bash scripts/archive.sh [ROOT] <YYYY-MM-DD-feature>
#   ROOT:   项目根目录 (默认当前目录)
#   slug:   文件名 slug, 例如 2026-06-02-fix-subagent-write
#
# 退出码:
#   0 = 两条 mv 都成功
#   1 = 任一源文件缺失或 mv 失败
#
# 输出 (每行):
#   archived: <dst>  — 移动成功
#   MISSING: <src>   — 源文件不存在, 未移动
set -euo pipefail

ROOT="${1:-$(pwd)}"
shift
slug="${1:?用法: $0 [ROOT] <YYYY-MM-DD-feature> (slug 必填)}"

moved=0
for kind in design-plans exec-plans; do
  src="$ROOT/docs/$kind/$slug.md"
  dst="$ROOT/docs/$kind/completed/$slug.md"
  if [[ ! -f "$src" ]]; then
    echo "MISSING: $src"
    exit 1
  fi
  mv "$src" "$dst"
  echo "archived: $dst"
  moved=$((moved + 1))
done

if [[ $moved -ne 2 ]]; then
  echo "ERROR: expected 2 archives, got $moved" >&2
  exit 1
fi

echo "---"
echo "Remaining in docs/design-plans/:"
ls "$ROOT/docs/design-plans/" 2>/dev/null || echo "(empty)"
echo "Remaining in docs/exec-plans/:"
ls "$ROOT/docs/exec-plans/" 2>/dev/null || echo "(empty)"
