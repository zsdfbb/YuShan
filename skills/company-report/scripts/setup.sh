#!/usr/bin/env bash
#
# company-report skill 的环境自举：建隔离 venv 并装 pdfplumber。
#
# 为什么自带 venv 而不复用小系统的 python：
# - pdfplumber 依赖 pdfminer.six + pypdfium2 + Pillow，装进系统 Python 会污染环境，
#   且在 Homebrew / Debian 12+ 上会被 PEP 668 的 externally-managed-environment 挡住。
# - 本 skill 需要**表格结构**（合并表头、跨页），这只有 pdfplumber 的版面分析能给；
#   pdfminer 的 extract_text 只能给扁平文本。
#
# 用法：
#   bash setup.sh             # 检测；缺则建 venv 并安装
#   bash setup.sh --check     # 只检测，绝不安装（退出码 0=就绪 / 3=未就绪）
#   bash setup.sh --dry-run   # 只打印将执行的命令
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VENV="$HERE/.venv"
MODE="install"

die() { printf 'setup: %s\n' "$1" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --check)   MODE="check"; shift ;;
        --dry-run) MODE="dry-run"; shift ;;
        -h|--help) sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "未知选项: $1" ;;
    esac
done

probe() {
    [ -x "$VENV/bin/python" ] || return 1
    "$VENV/bin/python" -c "import pdfplumber" >/dev/null 2>&1
}

# 造/修补 venv + 装依赖。--dry-run 与 install 共用这份命令，两者不会漂移。
install_cmds() {
    echo "python3 -m venv \"$VENV\""
    echo "\"$VENV/bin/pip\" install --quiet --upgrade pip"
    echo "\"$VENV/bin/pip\" install --quiet pdfplumber"
}

case "$MODE" in
    check)
        if probe; then
            printf 'PDF_BACKEND=pdfplumber\nPDF_READY=1\n'
            "$VENV/bin/python" -c "import pdfplumber; print('pdfplumber', pdfplumber.__version__)"
            exit 0
        fi
        printf 'PDF_BACKEND=\nPDF_READY=0\n'
        exit 3
        ;;
    dry-run)
        install_cmds
        exit 0
        ;;
esac

if probe; then
    printf '已就绪：pdfplumber 可用（%s）\n' "$VENV"
    exit 0
fi

command -v python3 >/dev/null 2>&1 || die "找不到 python3"
[ -x "$VENV/bin/python" ] || python3 -m venv "$VENV" || die "创建 venv 失败：$VENV"
"$VENV/bin/pip" install --quiet --upgrade pip || die "升级 pip 失败"
"$VENV/bin/pip" install --quiet pdfplumber || die "安装 pdfplumber 失败"

# 不相信安装命令的退出码，装完自己验一次。
probe || die "装完仍无法 import pdfplumber，请检查网络或 pip 源"
printf '安装完成：%s\n' "$VENV/bin/python"
