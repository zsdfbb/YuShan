#!/usr/bin/env bash
#
# 从本仓库源码构建并安装 `ys-coding-agent`（macOS / Ubuntu）。
#
# 用法：
#   ./scripts/install.sh                 # 构建 release 并装到 ~/.local/bin
#   ./scripts/install.sh --prefix DIR    # 换安装目录（等价 --dest DIR）
#   ./scripts/install.sh --with-skills   # 顺带把仓库 skill 铺到 ~/.yushan/skills
#   ./scripts/install.sh --dry-run       # 只报告将做什么，不构建、不落盘
#   ./scripts/install.sh --uninstall     # 删除已安装的二进制
#
# 选项：
#   --dest DIR / --prefix DIR   安装目录（默认 ${YUSHAN_INSTALL_DIR:-$HOME/.local/bin}）
#   --with-skills               同时运行 ./scripts/install-skills.sh（软链方式）
#   --uninstall                 删除安装目录里的 ys-coding-agent 后退出
#   --dry-run                   打印计划（源目录 / 目标 / 工具链检查结果）后退出
#   -h | --help                 打印本帮助
#
# 退出码：0 成功；1 参数 / 环境错误（缺工具链、缺 OpenSSL 头文件、构建失败）。
#
# # 设计约束
#
# - **不联网下载二进制**：只从当前 checkout 构建（ADR-0015 的自我定位）。升级 = `git pull`
#   后重跑本脚本，它会原地覆盖安装。
# - **不改 PATH、不写 shell 配置**：目标目录不在 PATH 时只打印一行提示，由用户决定。
# - **不自动装系统依赖**：缺什么就打印对应的 `apt-get` / `rustup` 命令后失败，不代跑 `sudo`。
# - 可重复执行：构建走 cargo 增量（第二次起只重链），安装用 临时文件 + `mv` 原子替换。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="ys-coding-agent"
MIN_RUST="1.88"                 # 与 Cargo.toml 的 `rust-version` 一致
DEST="${YUSHAN_INSTALL_DIR:-$HOME/.local/bin}"
DO_SKILLS=0
DRY_RUN=0
UNINSTALL=0

# 打印文件头的注释块（跳过 shebang，遇到第一行非注释即停）—— 不硬编码行号，
# 免得注释改了而 help 悄悄截断。
usage() {
    awk 'NR == 1 { next } /^#/ { sub(/^# ?/, ""); print; next } { exit }' "${BASH_SOURCE[0]}"
}

die() { printf '错误: %s\n' "$1" >&2; exit 1; }
say() { printf '%s\n' "$1"; }
step() { printf '\n==> %s\n' "$1"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --dest|--prefix)  [ $# -ge 2 ] || die "$1 需要一个目录参数"; DEST="$2"; shift 2 ;;
        --dest=*|--prefix=*) DEST="${1#*=}"; shift ;;
        --with-skills)    DO_SKILLS=1; shift ;;
        --dry-run)        DRY_RUN=1; shift ;;
        --uninstall)      UNINSTALL=1; shift ;;
        -h|--help)        usage; exit 0 ;;
        *)                die "未知选项: $1（用 --help 看用法）" ;;
    esac
done

# 目标目录统一成绝对路径，避免在子 shell 里 `cd` 后变成相对路径。
case "$DEST" in
    /*) ;;
    *)  DEST="$PWD/$DEST" ;;
esac

# ---- 卸载：只删二进制，不碰 ~/.yushan（会话 / 凭证 / skill 属用户数据）------------
if [ "$UNINSTALL" -eq 1 ]; then
    if [ -f "$DEST/$BIN" ]; then
        if [ "$DRY_RUN" -eq 1 ]; then
            say "将删除：$DEST/$BIN"
        else
            rm -f "$DEST/$BIN"
            say "已删除：$DEST/$BIN"
        fi
    else
        say "$DEST/$BIN 不存在，无需卸载"
    fi
    say "（未改动 ~/.yushan：会话、凭证与 skill 保留）"
    exit 0
fi

# ---- 1. 确认是在本仓库里跑 ------------------------------------------------
[ -f "$ROOT/Cargo.toml" ] || die "找不到 $ROOT/Cargo.toml —— 请在仓库的 checkout 里运行（git clone 后执行 ./scripts/install.sh）"
grep -q 'apps/coding-agent' "$ROOT/Cargo.toml" \
    || die "$ROOT 不是 YuShan 仓库（workspace 成员表里没有 apps/coding-agent）"

[ -z "${SUDO_USER:-}" ] \
    || say "警告：正以 sudo 运行。rustup 工具链与 cargo 缓存都在 $SUDO_USER 的家目录，建议去掉 sudo 重跑。"

# ---- 2. 工具链检查 --------------------------------------------------------
# 版本比较用 sort -V，避免依赖 GNU coreutils 以外的工具（macOS 自带 sort 支持 -V）。
ver_ge() { [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | head -1)" = "$2" ]; }

check_toolchain() {
    command -v cargo >/dev/null 2>&1 || die "找不到 cargo。先装 Rust 工具链：
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  source \"\$HOME/.cargo/env\""
    command -v rustc >/dev/null 2>&1 || die "找不到 rustc（cargo 存在但 rustc 缺失，检查 PATH）"

    local rv
    rv="$(rustc -vV 2>/dev/null | sed -n 's/^release: //p')"
    [ -n "$rv" ] \
        || die "rustc 起不来（cargo 在 PATH 里但 rustc 无法运行）。常见原因：HOME 被改写（sudo / env -i），rustup 找不到已装工具链 —— 用普通用户、正常的 \$HOME 重跑，或 rustup default stable"
    ver_ge "$rv" "$MIN_RUST" \
        || die "rustc $rv 过旧，需 >= ${MIN_RUST}（edition 2024）。升级：rustup update stable"

    command -v cc >/dev/null 2>&1 || die "找不到 C 链接器 cc。Ubuntu：sudo apt-get install -y build-essential；macOS：xcode-select --install"

    # reqwest 用默认的 native-tls（openssl），Linux 上需要 pkg-config + openssl 头文件。
    # macOS 用 Security.framework，不需要额外包。
    if [ "$(uname -s)" = "Linux" ]; then
        command -v pkg-config >/dev/null 2>&1 || die "找不到 pkg-config。Ubuntu：sudo apt-get install -y pkg-config"
        pkg-config --exists openssl 2>/dev/null || die "缺少 OpenSSL 开发文件（openssl 依赖编译不过）。Ubuntu：sudo apt-get install -y libssl-dev"
    fi
    printf '    rustc %s / cargo %s\n' "$rv" "$(cargo --version | awk '{print $2}')"
}

# ---- 3. 报告 + 预演 -------------------------------------------------------
step "安装 $BIN"
printf '    源目录 : %s\n' "$ROOT"
printf '    安装到 : %s\n' "$DEST"
printf '    版本   : %s\n' "$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"

if [ "$DRY_RUN" -eq 1 ]; then
    step "工具链检查"
    check_toolchain
    say ""
    say "dry-run：未构建、未写入任何文件。"
    [ "$DO_SKILLS" -eq 1 ] && say "（也会运行 ./scripts/install-skills.sh 装 skill）"
    exit 0
fi

step "检查工具链"
check_toolchain

# ---- 4. 构建 --------------------------------------------------------------
# 走仓库的统一入口（AGENTS.md：不要裸 cargo build）：它按需清理 debug 增量垃圾。
step "构建 release（首次全量编译约 2~5 分钟）"
if [ -x "$ROOT/scripts/cargo-build.sh" ]; then
    ( cd "$ROOT" && ./scripts/cargo-build.sh build --release --locked -p "$BIN" )
else
    ( cd "$ROOT" && cargo build --release --locked -p "$BIN" )
fi

BUILT="$ROOT/target/release/$BIN"
[ -x "$BUILT" ] || die "构建结束但找不到 $BUILT"

# ---- 5. 安装（临时文件 + mv，正在运行的旧二进制也能被替换）----------------
step "安装到 $DEST"
mkdir -p "$DEST"
tmp="$DEST/.$BIN.tmp.$$"
trap 'rm -f "$tmp"' EXIT
cp "$BUILT" "$tmp"
chmod 755 "$tmp"
mv -f "$tmp" "$DEST/$BIN"
trap - EXIT

# 自检：`--help` 不读凭证、不建运行时（见 apps/coding-agent/src/help.rs），适合做冒烟测试。
"$DEST/$BIN" --help >/dev/null || die "$DEST/$BIN 自检失败（--help 未正常退出）"
printf '    已安装：%s（%s）\n' "$DEST/$BIN" "$("$DEST/$BIN" --help | head -1)"

# ---- 6. 可选：装 skill ----------------------------------------------------
if [ "$DO_SKILLS" -eq 1 ]; then
    step "安装仓库自带 skill 到 ~/.yushan/skills"
    [ -x "$ROOT/scripts/install-skills.sh" ] \
        || die "找不到 $ROOT/scripts/install-skills.sh —— 无法装 skill（二进制已安装完成）"
    # 已装好的让别人顶过的同名 skill 会被那个脚本「跳过并提示」（它不覆盖非本仓库条目），
    # 故这里把非零退出码当作提示而非失败：agent 本身已装好。
    ( cd "$ROOT" && ./scripts/install-skills.sh ) \
        || say "注意：skills 未全部安装（见上方提示；常见原因是 ~/.yushan/skills 里已有同名条目）。二进制不受影响。"
fi

# ---- 7. 收尾提示 ----------------------------------------------------------
case ":$PATH:" in
    *":$DEST:"*) ;;
    *)
        say ""
        say "提示：$DEST 不在 PATH 里。加一行到 ~/.zshrc 或 ~/.bashrc："
        say "  export PATH=\"$DEST:\$PATH\""
        ;;
esac

say ""
say "下一步："
say "  1) 启动后输入 /login 选 provider 并填 API key（凭证存在 ~/.yushan）"
say "  2) $BIN -p \"这个项目是做什么的\"    # 一次性输出"
say ""
say "升级：git pull && ./scripts/install.sh"
say "卸载：./scripts/install.sh --uninstall"
