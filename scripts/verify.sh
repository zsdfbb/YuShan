#!/bin/sh
# **提交前的唯一验证入口** —— 全量测试 + 全部变异测试。
#
# # 为什么需要它
#
# `./scripts/par-test.sh` 只跑测试，**不跑变异测试**。而本仓的多次真 bug
# （死锁、UTF-8 损坏、`/new` 失效、拖选被删）都是「测试全绿时依然存在」——
# 拦它们的唯一手段是变异测试。变异脚本此前**没有任何自动化调用**，
# 全靠人记得敲，于是「代码改了、新测试把 bug 写成契约、脚本锚点失效」
# 这条链一路畅通。
#
# 本脚本把两者串起来，作为**发版 / 推送前必须跑过一次**的门。
#
# # 用法
#
#   ./scripts/verify.sh                # 全量：测试 + 5 个变异脚本
#   ./scripts/verify.sh --tests-only   # 只跑测试（改动很小、想快速过一遍时）
#   ./scripts/verify.sh --jobs 4       # 传给 par-test.sh 的并行度
#
# # 退出码
#
# 0 = 全部通过；1 = 测试失败、任一变异未被抓到、或脚本锚点失效。
# 锚点失效**必须**算失败：它意味着代码变了而变异脚本没跟上，
# 此后该脚本对这段代码不再有防护力（这正是拖选失守的形态）。
#
# # 变异脚本的前置约束
#
# `selection-mutation.py` 要求 `selection.rs` / `events.rs` **无未提交改动**
# （它在启动时快照这两份源码）。故本脚本在跑变异前不检查工作区，由各脚本
# 自己把关；带着未提交改动跑会在前置阶段硬失败，属预期行为。
#
# `vision-mutation.py` **不要**这个前置：它与 `cancellation-mutation.py` 一样靠
# **进程内快照**还原（不用 `git checkout`），因此可以带着未提交改动跑。
# 它在跑第一个 mutant 之前会做一遍**锚点自检**：任何 `old` 文本不再唯一命中就
# 整体失败（退出码 1）—— 那是「代码变了而脚本没跟上」，必须算失败。
set -u

cd "$(dirname "$0")/.." || exit 1

tests_only=0
par_args=""
for arg in "$@"; do
    case "$arg" in
        --tests-only) tests_only=1 ;;
        *) par_args="$par_args $arg" ;;
    esac
done

fail=0

printf '=== 1/2 全量测试 ===\n'
# shellcheck disable=SC2086
if ! ./scripts/par-test.sh $par_args; then
    printf '\n✗ 全量测试失败\n'
    fail=1
fi

if [ "$tests_only" -eq 0 ]; then
    printf '\n=== 2/2 变异测试（注入 bug，期望测试变红）===\n'
    for s in cancellation-mutation wheel-mutation selection-mutation theme-mutation vision-mutation; do
        printf '\n--- %s ---\n' "$s"
        if python3 "scripts/$s.py"; then
            printf '✓ %s\n' "$s"
        else
            printf '✗ %s 有未抓到（或锚点失效）\n' "$s"
            fail=1
        fi
    done
fi

if [ "$fail" -ne 0 ]; then
    printf '\n✗ 验证未通过\n'
    exit 1
fi
printf '\n✓ 验证通过\n'
