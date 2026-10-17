#!/usr/bin/env bash
#
# 文档一致性检查 —— 为「运行期 /resume」这批文档订正建立可复跑的对账点。
#
# 纯 bash + grep，无新依赖；可反复运行；任一断言失败即打印诊断并以非零码退出。
# 断言只钉「这批改动引入/消灭的措辞」，不是全文校验 —— 详见 docs 内各节。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ARCH="docs/architecture.md"
CONTEXT="docs/CONTEXT.md"
DESIGN="docs/arch/runtime-resume/design.md"
CTX="docs/arch/runtime-resume/context.md"

fail=0

ok()  { printf 'ok   %s\n' "$1"; }
bad() { printf 'FAIL %s\n' "$1"; fail=1; }

# 断言：$1 文件命中扩展正则 $2（$3 为描述）。失败打印命中/未命中诊断。
must_have() {
    local file="$1" pat="$2" desc="$3"
    if grep -Eq -- "$pat" "$file"; then
        ok "$desc"
    else
        bad "$desc"
        printf '     %s 未匹配 /%s/（期望命中）\n' "$file" "$pat" >&2
    fi
}

# 断言：$1 文件**不**命中扩展正则 $2。
must_not_have() {
    local file="$1" pat="$2" desc="$3"
    if grep -Eq -- "$pat" "$file"; then
        bad "$desc"
        printf '     %s 命中 /%s/（期望为零）：\n' "$file" "$pat" >&2
        grep -nE -- "$pat" "$file" >&2 || true
    else
        ok "$desc"
    fi
}

# 1. architecture.md §14 命令表含 /resume 与 /effort，且过期计数已消失。
must_have     "$ARCH" '/resume' "architecture.md 命令表含 /resume"
must_have     "$ARCH" '/effort' "architecture.md 命令表含 /effort"
must_not_have "$ARCH" '11 条'   "architecture.md 不再出现「11 条」"
must_not_have "$ARCH" '13 条'   "architecture.md 不再出现「13 条」"
must_have     "$ARCH" '14 条'   "architecture.md 命令计数为 14 条"

# 2. CONTEXT.md 有 resume 术语条目，且写明「替换而非合并」。
must_have "$CONTEXT" 'resume'       "CONTEXT.md 含 resume 术语条目"
must_have "$CONTEXT" '替换而非合并' "CONTEXT.md 说明恢复是「替换而非合并」"

# 3. design.md 的 A6 降级与 A4 措辞订正已落地。
must_not_have "$DESIGN" '常态'    "design.md 不再出现「常态」"
must_not_have "$DESIGN" 'id 短码' "design.md 不再出现「id 短码」"

# 4. context.md 约束 7/8 标注「已被既有机制化解」。
must_have "$CTX" '已被既有机制化解' "context.md 约束 7/8 标注「已被既有机制化解」"

# 5. design.md §3.3 写明候选模型构建「不得插入 await」。
must_have "$DESIGN" '不得插入 `await`' "design.md §3.3 含「不得插入 await」"

# 6. U2 契约：Listing 类型已整体退场，apps / crates 下零残留。
hits="$(grep -rn -- 'Listing' apps crates || true)"
if [ -n "$hits" ]; then
    bad "apps / crates 下仍存在 Listing"
    printf '%s\n' "$hits" >&2
else
    ok "apps / crates 下 grep Listing 零命中"
fi

echo
if [ "$fail" -ne 0 ]; then
    echo "doc-consistency: FAILED"
    exit 1
fi
echo "doc-consistency: OK"
