#!/usr/bin/env bash
#
# 安装本仓库内置的 skills 到用户级 skill 目录。
#
# 默认目标：~/.yushan/skills（与 Config::default_skill_roots 的用户级根一致）。
# 只**软链**，不复制 —— 这样 `git pull` 更新仓库后 skill 立刻生效，不必重跑。
#
# 用法：
#   ./scripts/install-skills.sh              # 安装全部（软链）
#   ./scripts/install-skills.sh --copy       # 复制而非软链（跨机器分发 / 不支持软链时）
#   ./scripts/install-skills.sh pdf-analysis # 只装指定 skill
#   ./scripts/install-skills.sh --dest DIR   # 换目标目录
#   ./scripts/install-skills.sh --list       # 只列出可装的 skill
#   ./scripts/install-skills.sh --dry-run    # 只报告将做什么，不落任何改动
#
# 退出码：0 成功；1 参数/环境错误；2 部分 skill 安装失败。
#
# 设计约束（与仓库约定一致）：
# - **不联网、不下载**（ADR-0015 的自我定位），只做本机文件操作。
# - **不覆盖**已存在的非本仓库条目：目标已存在且不是指向本仓库的软链时会**跳过并提示**，
#   避免把用户自己装的同名 skill 静默顶掉（skill 冲突语义是 project > user，整条替换）。
# - `--copy` 与软链的差别只在分发方式，两者的 frontmatter 解析结果**完全相同**。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$ROOT/skills"

MODE="link"        # link | copy
DEST=""
LIST_ONLY=0
DRY_RUN=0
SELECTED=()

usage() {
    sed -n '2,25p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

die() { printf '错误: %s\n' "$1" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --copy)    MODE="copy"; shift ;;
        --link)    MODE="link"; shift ;;
        --dest)    [ $# -ge 2 ] || die "--dest 需要一个目录参数"; DEST="$2"; shift 2 ;;
        --dest=*)  DEST="${1#--dest=}"; shift ;;
        --list)    LIST_ONLY=1; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        -*)        die "未知选项: $1（用 --help 看用法）" ;;
        *)         SELECTED+=("$1"); shift ;;
    esac
done

[ -d "$SRC" ] || die "找不到 skills 源目录: $SRC"

# 默认目标：$HOME/.yushan/skills。
# HOME 为空或相对路径时**拒绝**——相对路径会解析到 CWD（= 用户仓库），
# 把 skill 装进仓库里是静默污染（与 tools-web 的落盘目录同一条铁律）。
if [ -z "$DEST" ]; then
    home="${HOME:-${USERPROFILE:-}}"
    [ -n "$home" ] || die "HOME 未设置；用 --dest 显式指定目标目录"
    case "$home" in
        /*) ;;
        *)  die "HOME=\"$home\" 不是绝对路径；用 --dest 显式指定目标目录" ;;
    esac
    DEST="$home/.yushan/skills"
else
    case "$DEST" in
        /*) ;;
        *)  die "--dest 必须是绝对路径（相对路径会落到当前工作目录）: $DEST" ;;
    esac
fi

# 收集候选：skills/ 下**直接含 SKILL.md** 的一级目录。
# 这正是 `skills::scan` 的布局契约：含 SKILL.md 的目录即一个 skill，不再下探。
discover() {
    local dir
    for dir in "$SRC"/*/; do
        [ -f "${dir}SKILL.md" ] || continue
        basename "$dir"
    done
}

all_skills() { discover; }

# 目标是否已是指向本仓库的软链（重复安装时的正常情形）。
is_our_link() {
    local target="$1" dir="$2"
    [ -L "$target" ] || return 1
    local resolved
    resolved="$(cd "$target" 2>/dev/null && pwd -P)" || return 1
    [ "$resolved" = "$(cd "$dir" && pwd -P)" ]
}

install_one() {
    local name="$1"
    local src_dir="$SRC/$name"
    local target="$DEST/$name"

    if [ -e "$target" ] || [ -L "$target" ]; then
        if is_our_link "$target" "$src_dir"; then
            if [ "$DRY_RUN" -eq 1 ]; then
                printf '  已是本仓库软链，跳过: %s\n' "$name"
                return 0
            fi
            # 软链已正确，不必重建（重建会改动 mtime，虽无害但无意义）。
            printf '  已安装（软链），跳过: %s\n' "$name"
            return 0
        fi
        # 已存在但**不属于本仓库**：不覆盖。
        printf '  跳过（目标已存在且不是本仓库软链）: %s\n' "$target" >&2
        printf '    如需替换，请先手动移走该目录\n' >&2
        return 1
    fi

    if [ "$DRY_RUN" -eq 1 ]; then
        printf '  将%s: %s -> %s\n' \
            "$([ "$MODE" = copy ] && echo 复制 || echo 软链)" "$src_dir" "$target"
        return 0
    fi

    if [ "$MODE" = "copy" ]; then
        cp -R "$src_dir" "$target"
        printf '  已复制: %s\n' "$name"
    else
        ln -s "$src_dir" "$target"
        printf '  已软链: %s -> %s\n' "$name" "$src_dir"
    fi
    return 0
}

# ---------- --list ----------

if [ "$LIST_ONLY" -eq 1 ]; then
    printf '可安装的 skills（来自 %s）：\n\n' "$SRC"
    for name in $(all_skills); do
        desc="$(awk '
            fm == 0 { if ($0 ~ /^---[[:space:]]*$/) { fm=1 }; next }
            fm == 1 && $0 ~ /^---[[:space:]]*$/ { exit }
            fm == 1 && $0 ~ /^[A-Za-z][A-Za-z0-9_-]*:/ {
                key=$0; sub(/:.*/, "", key)
                grab = (key == "description")
                if (grab) {
                    v=$0; sub(/^description:[[:space:]]*/, "", v)
                    # 行内值是 `|` / `>` 块标记时剥掉，正文在后续缩进行里
                    if (v ~ /^[|>][-+]?[[:space:]]*$/) v=""
                    seen=1
                }
                next
            }
            fm == 1 && grab && $0 ~ /^[[:space:]]+/ {
                t=$0; sub(/^[[:space:]]+/, "", t)
                v = (v=="" ? t : v " " t)
                next
            }
            END {
                if (seen) { gsub(/[[:space:]]+/, " ", v); sub(/^ /, "", v); sub(/ $/, "", v); print v }
            }
        ' "$SRC/$name/SKILL.md" 2>/dev/null)"
        printf '  %-20s %s\n' "$name" "${desc:-（无 description —— 不会被加载）}"
    done
    exit 0
fi

# ---------- 选择要装的 ----------

if [ ${#SELECTED[@]} -gt 0 ]; then
    NAMES=()
    for want in "${SELECTED[@]}"; do
        [ -f "$SRC/$want/SKILL.md" ] || die "没有名为 '$want' 的 skill（用 --list 看清单）"
        NAMES+=("$want")
    done
else
    # 不用 `mapfile`：macOS 自带 bash 3.2 没有它（本项目脚本要开箱可跑）。
    NAMES=()
    while IFS= read -r name; do
        [ -n "$name" ] && NAMES+=("$name")
    done <<EOF
$(all_skills)
EOF
fi

[ ${#NAMES[@]} -gt 0 ] || die "$SRC 下没有任何含 SKILL.md 的目录"

# ---------- 落地 ----------

printf '源目录:  %s\n' "$SRC"
printf '目标:    %s\n' "$DEST"
printf '方式:    %s\n' "$([ "$MODE" = copy ] && echo 复制 || echo 软链)"
[ "$DRY_RUN" -eq 1 ] && printf '（dry-run：不做任何改动）\n'
printf '\n'

if [ "$DRY_RUN" -eq 0 ]; then
    mkdir -p "$DEST" || die "无法创建目标目录: $DEST"
fi

failed=0
for name in "${NAMES[@]}"; do
    install_one "$name" || failed=$((failed + 1))
done

printf '\n'
if [ "$failed" -gt 0 ]; then
    printf '%d 个 skill 未能安装（见上方提示）\n' "$failed" >&2
    exit 2
fi

if [ "$DRY_RUN" -eq 0 ]; then
    printf '完成。重启 ys-coding-agent 后生效（skill 目录在启动期扫描一次）。\n'
fi
