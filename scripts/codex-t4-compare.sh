#!/bin/sh
# 用 codex 跑基准提示 t4，输出**墙钟与 token 用量** —— 外部实现对照的那一半。
#
# 为什么要有它：判断「我们快不快」不能只看自己的前后对照，得有一个外部基准。
# 这条命令就是那个基准的一半；另一半是我们自己的基准（见下）。两侧**必须分开跑、
# 不并发** —— 两边打同一个网关（opencode.ai/zen/go），并发会互相抬高延迟，
# 而墙钟正是要量的东西。
#
# 前提：本机装了 codex，且 `~/.codex/config.toml` 的 `model_providers.custom.base_url`
# 指向可用端点（`wire_api` 只能是 `responses` —— codex 0.160+ 已不支持 `chat`）。
#
# 用法：
#     sh scripts/codex-t4-compare.sh              # 跑 t4
#     sh scripts/codex-t4-compare.sh "自定义提示"
#
# 我们那一半（另开一个终端、等这半跑完再跑）：
#     YUSHAN_BENCH_ONLY=exp/t4 cargo test -p ys-coding-agent --test e2e_bench -- --ignored --nocapture
#
# 读数出处：codex 把 `tokens used` 写在 **stderr 尾部**；我们把逐轮读数写进
# `$YUSHAN_LOG_DIR`（基准默认保留临时目录并打印路径）。
set -u

PROMPT="${1:-这个仓库接下来该补什么？请基于代码与文档的实际状态给出建议。}"

# 证据落 `tmp/`（已 gitignore）。用固定的两个文件，便于与上一次逐行 diff。
OUT="tmp/codex-t4.out"
ERR="tmp/codex-t4.err"
mkdir -p tmp

start=$(date +%s)
# `-s read-only`：基准提示是只读问题，且与我们的基准同条件（我们也不许写工作区）。
codex exec -s read-only "$PROMPT" >"$OUT" 2>"$ERR"
code=$?
end=$(date +%s)

echo "codex 退出码=$code  墙钟=$((end - start))s"
echo "  输出：$OUT（$(wc -l <"$OUT") 行）"
echo "  诊断：$ERR（$(wc -l <"$ERR") 行）"
echo "  tokens used（stderr 尾部，口径未必含缓存命中）："
grep -A1 "^tokens used" "$ERR" | tail -1 | sed 's/^/    /'
