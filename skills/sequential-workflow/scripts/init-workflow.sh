#!/usr/bin/env bash
# init-workflow.sh — 创建 sequential-workflow 所需的 docs/ 子目录
#
# 用法: bash scripts/init-workflow.sh [ROOT]
#   ROOT: 项目根目录 (默认当前目录)
#
# 退出码:
#   0 = 全部目录创建成功
#   1 = 任何目录创建失败
#
# 子目录:
#   docs/design-plans/        — 当前活跃的设计计划
#   docs/design-plans/completed/ — 已归档
#   docs/exec-plans/          — 当前活跃的执行计划
#   docs/exec-plans/completed/   — 已归档
#   docs/reports/             — 修改报告
#   docs/design-final/        — 最终设计文档 (Finalize 阶段追加)
set -euo pipefail

ROOT="${1:-$(pwd)}"

dirs=(
  "$ROOT/docs/design-plans"
  "$ROOT/docs/design-plans/completed"
  "$ROOT/docs/exec-plans"
  "$ROOT/docs/exec-plans/completed"
  "$ROOT/docs/reports"
  "$ROOT/docs/design-final"
)

for d in "${dirs[@]}"; do
  mkdir -p "$d"
done

echo "Initialized sequential-workflow dirs under $ROOT:"
ls -la "$ROOT/docs/design-plans" "$ROOT/docs/exec-plans" "$ROOT/docs/reports" "$ROOT/docs/design-final"
