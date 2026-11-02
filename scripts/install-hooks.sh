#!/bin/sh
# 安装本仓库的 Git 钩子（目前只有 `pre-push`）。
#
# # 为什么需要这个脚本
#
# git **不会**从仓库里自动加载钩子 —— `.git/hooks/` 是本地目录，不受版本控制。
# 故钩子本体放在 `scripts/git-hooks/`（受控），再由本脚本经 `core.hooksPath`
# 指过去。clone 之后必须跑一次；不跑就是没有钩子。
#
# # 与 ADR-0014 / ADR-0021 无关
#
# 那两条 ADR 决定的是**运行时 Agent 不建通用 hook 系统**（`AgentEvent` vs Hook 的
# 边界）。本脚本装的是 **git 钩子**，两回事。
#
# # 用法
#
#   ./scripts/install-hooks.sh          # 安装（幂等）
#   ./scripts/install-hooks.sh --status # 只报告状态
#   ./scripts/install-hooks.sh --uninstall
set -u

cd "$(dirname "$0")/.." || exit 1
target=scripts/git-hooks

if [ ! -d "$target" ]; then
    printf '找不到 %s\n' "$target" >&2
    exit 1
fi

case "${1:---install}" in
    --status)
        current=$(git config --get core.hooksPath || true)
        if [ "$current" = "$target" ]; then
            printf '已安装：core.hooksPath = %s\n' "$target"
        else
            printf '未安装：core.hooksPath = %s\n' "${current:-（未设置）}"
        fi
        ;;
    --uninstall)
        git config --unset core.hooksPath 2>/dev/null || true
        printf '已卸载（core.hooksPath 已清除）\n'
        ;;
    --install)
        # 钩子必须是可执行的，否则 git 静默跳过 —— 这是最容易踩的坑。
        for f in "$target"/*; do
            [ -f "$f" ] || continue
            chmod +x "$f" || exit 1
        done
        git config core.hooksPath "$target" || exit 1
        printf '已安装：core.hooksPath = %s\n' "$target"
        printf '  生效的钩子：'
        ls "$target" | tr '\n' ' '
        printf '\n  跳过用 git push --no-verify\n'
        ;;
    *)
        printf '用法：%s [--status|--install|--uninstall]\n' "$0" >&2
        exit 2
        ;;
esac
