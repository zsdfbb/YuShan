#!/usr/bin/env bash
#
# pdf-analysis skill 的环境自举：检测 + 安装一个可用的 PDF 文本提取器。
#
# 由 skill 正文在需要提取 PDF 时调用（见 ../SKILL.md「第 2 步」），也可手动跑。
#
# 用法：
#   bash setup.sh                 # 检测；缺则装第一个可用后端
#   bash setup.sh --check         # 只检测并打印状态，**绝不装任何东西**（退出码见下）
#   bash setup.sh --dry-run       # 打印将要执行的安装命令，不执行
#   bash setup.sh --backend X     # 指定后端：pdftotext | mutool | pdfminer | markitdown
#   bash setup.sh --ocr           # 额外装 OCR（扫描件用）：ocrmypdf 或 tesseract
#   bash setup.sh --hint          # 只给「该装什么」的一行提示（供模型转述给用户）
#
# --check 退出码：
#   0 = 已有可用提取器；3 = 没有（但不致命）；头两行固定为机器可读的 KEY=VALUE
#
# 设计约束：
# - **不默认 sudo**：brew 与 pip --user 都不需要提权。只有 Linux 的系统包管理器
#   需要，那种情况**打印命令让用户跑**，不擅自动 sudo（agent 不该替用户提权）。
# - **不联网做别的事**：只调包管理器装 PDF 工具。
# - **可重入**：已装则直接成功；重复跑无副作用。
# - 装完**必须自己验一次**（真的提取一个 PDF），而不是相信「安装命令退出码 0」。
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

MODE="install"     # install | check | dry-run | hint
BACKEND=""
WITH_OCR=0

die() { printf 'setup: %s\n' "$1" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --check)   MODE="check"; shift ;;
        --dry-run) MODE="dry-run"; shift ;;
        --hint)    MODE="hint"; shift ;;
        --ocr)     WITH_OCR=1; shift ;;
        --backend) [ $# -ge 2 ] || die "--backend 需要参数"; BACKEND="$2"; shift 2 ;;
        --backend=*) BACKEND="${1#--backend=}"; shift ;;
        -h|--help) sed -n '2,28p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "未知选项: $1（用 --help）" ;;
    esac
done

# ─────────────────────────── 探测 ───────────────────────────

have() { command -v "$1" >/dev/null 2>&1; }

# 每个后端对应一个「能否真的提取」的探测，而不只是命令是否存在。
# 返回 0 = 可用。探测必须**便宜**（--check 会被模型频繁调用）。
#
# ⚠️ `pdfminer` / `markitdown` 装在 skill 自己的 venv 里，**不在 PATH 上** ——
# 只查 `python3`/`command -v` 会漏掉它们（实测：安装成功却被判「仍探测不到」）。
# 故先查 venv，再查系统。
probe() {
    case "$1" in
        pdftotext)  have pdftotext ;;
        mutool)     have mutool ;;
        pdfminer)   venv_python -c "import pdfminer" >/dev/null 2>&1 \
                        || python3 -c "import pdfminer" >/dev/null 2>&1 ;;
        markitdown) [ -x "$(venv_bin markitdown)" ] || have markitdown ;;
        ocrmypdf)   have ocrmypdf ;;
        tesseract)  have tesseract ;;
        *)          return 1 ;;
    esac
}

# venv 里的 python（不存在时回落到系统 python3，使调用点无需分支）。
venv_python() {
    local v
    v="$(venv_bin python3)"
    if [ -x "$v" ]; then "$v" "$@"; else python3 "$@"; fi
}

# 按优先级列出全部后端（与 SKILL.md 的表一致）。
BACKENDS="pdftotext mutool pdfminer markitdown"

first_available() {
    local b
    for b in $BACKENDS; do
        probe "$b" && { printf '%s' "$b"; return 0; }
    done
    return 1
}

# ─────────────────────────── 安装命令（唯一定义处） ───────────────────────────

# 打印「装这个后端」要跑的命令（每行一条，按顺序执行）。
# 这是**唯一**的安装知识来源：`--dry-run` 打印的、`install` 执行的、`--hint` 转述的
# 都来自这里，三者不可能漂移。
install_cmds() {
    local b="$1" os
    os="$(uname -s)"

    # 包管理器探测：只在真正需要系统包时用。
    local syspm=""
    for m in brew apt-get dnf pacman apk; do
        have "$m" && { syspm="$m"; break; }
    done

    case "$b" in
        pdftotext)
            case "$syspm" in
                brew)    echo "brew install poppler" ;;
                apt-get) echo "apt-get install -y poppler-utils" ;;
                dnf)     echo "dnf install -y poppler-utils" ;;
                pacman)  echo "pacman -S --noconfirm poppler" ;;
                apk)     echo "apk add poppler-utils" ;;
                *)       return 1 ;;
            esac
            ;;
        mutool)
            case "$syspm" in
                brew)    echo "brew install mupdf-tools" ;;
                apt-get) echo "apt-get install -y mupdf-tools" ;;
                dnf)     echo "dnf install -y mupdf-tools" ;;
                pacman)  echo "pacman -S --noconfirm mupdf-tools" ;;
                apk)     echo "apk add mupdf-tools" ;;
                *)       return 1 ;;
            esac
            ;;
        pdfminer)
            # 首选隔离 venv：不污染系统 Python，且绕开 PEP 668（Homebrew / Debian 12+
            # 的 externally-managed-environment 会让 `pip install` 直接失败）。
            echo "python3 -m venv \"$HERE/.venv\""
            echo "\"$HERE/.venv/bin/pip\" install --quiet --upgrade pip"
            echo "\"$HERE/.venv/bin/pip\" install --quiet pdfminer.six"
            ;;
        markitdown)
            echo "python3 -m venv \"$HERE/.venv\""
            echo "\"$HERE/.venv/bin/pip\" install --quiet --upgrade pip"
            # 只要 pdf extra：markitdown[all] 会拉 docx/xlsx/pptx/音频等一大堆，
            # 而本 skill 只承诺 PDF。
            echo "\"$HERE/.venv/bin/pip\" install --quiet 'markitdown[pdf]'"
            ;;
        ocrmypdf)
            case "$syspm" in
                brew)    echo "brew install ocrmypdf" ;;
                apt-get) echo "apt-get install -y ocrmypdf" ;;
                dnf)     echo "dnf install -y ocrmypdf" ;;
                pacman)  echo "pacman -S --noconfirm ocrmypdf" ;;
                *)       return 1 ;;
            esac
            ;;
        tesseract)
            case "$syspm" in
                brew)    echo "brew install tesseract tesseract-lang" ;;
                apt-get) echo "apt-get install -y tesseract-ocr tesseract-ocr-chi-sim" ;;
                dnf)     echo "dnf install -y tesseract tesseract-langpack-chi_sim" ;;
                pacman)  echo "pacman -S --noconfirm tesseract tesseract-data-chi_sim" ;;
                *)       return 1 ;;
            esac
            ;;
        *) return 1 ;;
    esac
}

# 装 pdfminer/markitdown 后，它们的可执行文件落在 venv 里 —— 后续调用要走 venv 路径。
venv_bin() { printf '%s/.venv/bin/%s' "$HERE" "$1"; }

# 装完之后「怎么用」——写进 stdout，供模型照抄。
usage_hint() {
    case "$1" in
        pdfminer)
            printf 'python3 提取（venv 已就绪）：\n  %s -c "from pdfminer.high_level import extract_text; print(extract_text(\\"%s\\"))"\n' \
                "$(venv_bin python3)" '<file.pdf>'
            ;;
        markitdown)
            printf 'markitdown 提取：\n  %s "%s" -o %s\n' \
                "$(venv_bin markitdown)" '<file.pdf>' '<out.txt>'
            ;;
        *) printf '%s 已在 PATH 上，直接用。\n' "$1" ;;
    esac
}

# ─────────────────────────── 模式分发 ───────────────────────────

if [ "$MODE" = "check" ]; then
    found="$(first_available || true)"
    if [ -n "$found" ]; then
        printf 'PDF_BACKEND=%s\n' "$found"
        printf 'PDF_READY=1\n'
        if [ "$WITH_OCR" -eq 1 ]; then
            for o in ocrmypdf tesseract; do
                probe "$o" && { printf 'PDF_OCR=%s\n' "$o"; break; }
            done
        fi
        exit 0
    fi
    printf 'PDF_BACKEND=\n'
    printf 'PDF_READY=0\n'
    exit 3
fi

if [ "$MODE" = "hint" ]; then
    found="$(first_available || true)"
    if [ -n "$found" ]; then
        echo "PDF 提取器已就绪：$found"
        exit 0
    fi
    # 挑一个「本机最小代价」的后端来建议。
    if have brew || have apt-get || have dnf || have pacman || have apk; then
        suggest="pdftotext"
    elif have python3; then
        suggest="pdfminer"
    else
        echo "没有可用的包管理器，也没有 python3 —— 无法自动安装 PDF 提取器。" >&2
        exit 1
    fi
    cmd="$(install_cmds "$suggest" | head -1)"
    echo "未检测到 PDF 提取器。建议安装 ${suggest}："
    echo "  $cmd"
    exit 0
fi

# install / dry-run

target="${BACKEND:-$(first_available || true)}"

if [ -z "${BACKEND}" ] && [ -n "$target" ]; then
    printf '已就绪：%s（无需安装）\n' "$target"
    usage_hint "$target"
    # `--ocr` 是**独立**诉求：提取器已就绪不代表 OCR 就绪，
    # 不能因前者命中就跳过后者（否则「我要 OCR」会得到一句「已就绪」）。
    if [ "$WITH_OCR" -eq 1 ]; then
        printf '\n'
        for o in ocrmypdf tesseract; do
            probe "$o" && { printf 'OCR 已就绪：%s\n' "$o"; exit 0; }
        done
        oc="$(install_cmds ocrmypdf 2>/dev/null | head -1)" || \
            oc="$(install_cmds tesseract 2>/dev/null | head -1)" || true
        if [ -n "$oc" ]; then
            cat <<EOF
未检测到 OCR（扫描件需要）。安装命令：
  $oc
若需系统权限，请手动执行；不需提权的（brew）可跑：
  bash $HERE/setup.sh --backend ocrmypdf
EOF
        else
            printf '未检测到 OCR，且本机无可用包管理器 —— 请手动装 ocrmypdf 或 tesseract。\n'
        fi
    fi
    exit 0
fi

if [ -z "$target" ]; then
    # 没有任何后端：按「最小代价」挑一个。
    if have brew || have apt-get || have dnf || have pacman || have apk; then
        target="pdftotext"
    elif have python3; then
        target="pdfminer"
    else
        die "没有包管理器也没有 python3，无法安装任何后端。请手动安装 poppler-utils。"
    fi
fi

cmds="$(install_cmds "$target")" || die "不知道如何在 $(uname -s) 上安装 $target"

if [ "$MODE" = "dry-run" ]; then
    printf '# 将安装后端：%s\n' "$target"
    printf '%s\n' "$cmds"
    exit 0
fi

# 系统包管理器需要 root：**不擅自 sudo**，打印命令交给用户。
needs_root=0
case "$(printf '%s' "$cmds" | head -1)" in
    sudo\ *) needs_root=2 ;;   # 已带 sudo
    brew\ *) needs_root=0 ;;
    apt-get*|dnf*|pacman*|apk*) needs_root=1 ;;
esac

if [ "$needs_root" -eq 1 ]; then
    cat <<EOF
 需要系统权限才能安装 ${target}。请手动执行（我不替你 sudo）：
$(printf '%s\n' "$cmds" | sed 's/^/  sudo /')

装完再跑一次：bash $HERE/setup.sh --check
EOF
    exit 4
fi

printf '安装 %s ...\n' "$target"
# 逐行执行；首条失败即停（后续命令依赖前面的产物，如 venv 建好才有 pip）。
while IFS= read -r cmd; do
    [ -z "$cmd" ] && continue
    printf '  $ %s\n' "$cmd"
    if ! bash -c "$cmd"; then
        die "安装失败：$cmd"
    fi
done <<EOF
$cmds
EOF

# ─────────────────────────── 装完必须自验 ───────────────────────────

probe "$target" || die "安装命令成功退出，但 $target 仍探测不到（PATH 未刷新？）"

if [ "$WITH_OCR" -eq 1 ]; then
    for o in ocrmypdf tesseract; do
        if probe "$o"; then
            printf 'OCR 已就绪：%s\n' "$o"
            break
        fi
        oc="$(install_cmds "$o" 2>/dev/null | head -1)" || continue
        printf '  OCR 未装；如需扫描件识别：%s\n' "$oc"
    done
fi

printf '\n完成：%s\n' "$target"
usage_hint "$target"
