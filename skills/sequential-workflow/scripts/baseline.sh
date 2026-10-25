#!/usr/bin/env bash
# baseline.sh — Phase 3 前建立字节级基线, 供 review 做 git diff 比对
#
# 用法: bash scripts/baseline.sh [ROOT] <slug>
#   ROOT:   项目根目录 (默认当前目录)
#   slug:   本次工作流的 feature slug, 例如 2026-09-19-parallel-phase3
#
# 行为 (按工作区状态分支):
#   干净   → 不产生 commit, 直接以 HEAD 作为基线
#   不干净 → 先把 HEAD 存为备份分支 sw-backup/<slug> (幂等, 已存在则复用),
#            再把当前工作区整体提交为基线 commit
#   非 git → 无法建立安全网, 报告 NONE 并退出 1 (orchestrator 需停下问用户)
#
# 输出的 BASELINE ref 可让 review subagent 执行:
#   git diff <baseline-ref> -- <files>
# 从而对新文件 (git 中显示为 ??) 也拿到字节级证据。
#
# 退出码:
#   0 = 已建立基线 (stdout 首行 BASELINE: <ref>)
#   1 = 未能建立基线 (stdout 首行 NONE: <reason>)
#
# 输出 (每行):
#   BASELINE: <ref>    — 基线 ref (commit SHA)
#   BACKUP: <branch>   — 工作区原本不干净时额外创建的备份分支
#   NONE: <reason>     — 失败原因
set -euo pipefail

ROOT="${1:-$(pwd)}"
shift || true
slug="${1:?用法: $0 [ROOT] <slug> (slug 必填)}"

git_cmd() { git -C "$ROOT" "$@"; }

# --- 非 git 仓库: 无安全网可言 -------------------------------------------------
if ! git_cmd rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "NONE: $ROOT 不是 git 仓库, 无法建立基线; 请人工备份后继续"
  exit 1
fi

backup_branch="sw-backup/$slug"

# --- 工作区是否干净 ------------------------------------------------------------
dirty=0
if [[ -n "$(git_cmd status --porcelain)" ]]; then
  dirty=1
fi

if [[ $dirty -eq 0 ]]; then
  head_sha="$(git_cmd rev-parse HEAD)"
  echo "BASELINE: $head_sha"
  echo "  (工作区干净, 未产生 commit)"
  exit 0
fi

# --- 工作区不干净: 先存备份分支, 再提交基线 --------------------------------------
base_sha="$(git_cmd rev-parse HEAD)"

if git_cmd rev-parse --verify --quiet "$backup_branch" >/dev/null; then
  echo "BACKUP: $backup_branch (已存在, 复用)"
else
  git_cmd branch "$backup_branch" "$base_sha"
  echo "BACKUP: $backup_branch"
fi

changed=$(git_cmd status --porcelain | wc -l | tr -d ' ')
git_cmd add -A

if ! git_cmd commit --no-verify -q -m "chore(sw): baseline before $slug"; then
  echo "NONE: 基线 commit 失败, 工作区仍有未提交改动; 请人工备份后继续"
  exit 1
fi

baseline_sha="$(git_cmd rev-parse HEAD)"
echo "BASELINE: $baseline_sha"
echo "  (已提交 $changed 项工作区改动作为基线; 原始 HEAD 存于 $backup_branch)"
echo "  (如需还原: git reset --soft $base_sha)"
exit 0
