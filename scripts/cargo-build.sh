#!/bin/sh
# 构建/检查的统一入口：先查缓存健康度，必要时清理，再交给 cargo。
#
# # 为什么需要它
#
# rustc 的增量编译会在 `target/debug/deps` 里保留中间对象（`*.rcgu.o`），
# **每次构建留下新的一份，旧的从不删除**。实测本机（macOS / M4）：
#
#   | 场景                    | 每次 `.o` 增量 | 从 7k 到 400k |
#   |-------------------------|---------------:|--------------:|
#   | 改 `ys-core`（底层）    |         +6,247 |     约 63 次 |
#   | 改 `app_loop.rs`（叶）  |           +512 |    约 770 次 |
#
# 累积到 92 万条目时，瓶颈不再是编译（user 15s）而是文件系统（sys 105s）：
# 改 `ys-core` 的增量从 4.20s 劣化到 163s，全量测试从 7.55s 劣化到 184s，
# 全量 `cargo clean` 要 92s（96 万文件 / 65 GiB）。清理后全部回到正常。
#
# # 为什么是「先清理、再构建」
#
# 单次构建净增 6,247（`ys-core`）或 512（叶文件）—— 构建**后**查到的数字已经
# 包含这一次的增量，那时再清等于刚建完就拆掉，下次还得重建。先清则本次构建
# 直接落在干净基线上。
#
# # 阈值为什么放宽到 400,000
#
# 卡太死会让每次构建都触发清理，白付重建时间。400,000 约为干净态的 55 倍，
# 按 +6,247/次 算约 63 次 `ys-core` 级构建才触发一次 ⇒ 正常开发很久才碰一次，
# 而它又远低于病态量级（92 万）。检查本身只要 0.11s，所以「查得勤、清得少」。
#
# # 清理什么（实测对比）
#
#   · 清 **4 个高 churn crate**：51,938 → 45,953（效果弱 —— 未清的 crate 同样在堆）
#   · 清**全部 14 个 workspace 成员**：81,500 → **928**，耗时 **5.54s**
#   · 清后的构建：**16.04s**（外部依赖保留，只重编 14 个自有 crate）
#   · 对照全量 `cargo clean`：**92s**，且会清掉 248 个外部依赖的编译结果
#
# 所以默认清**全部 workspace 成员**，外部依赖（冷构建成本大头）不动。
#
# # 用法
#
#   ./scripts/cargo-build.sh                    # 等价于 cargo build
#   ./scripts/cargo-build.sh --tests            # 等价于 cargo build --tests
#   ./scripts/cargo-build.sh check -p ys-loop   # 等价于 cargo check -p ys-loop
#   ./scripts/cargo-build.sh test -p ys-loop    # 等价于 cargo test -p ys-loop
#   ./scripts/cargo-build.sh clippy --all-targets
#
# 任何子命令都走同一套门禁（`clippy` / `test` 同样产 `.o`）。
#
# 环境变量：
#   CARGO_GUARD_LIMIT   deps 条目阈值，默认 400000；设 0 关闭门禁
#   CARGO_GUARD_KEEP    清理时**保留**的 crate（空格分隔）；默认空 = 清全部 workspace 成员
#   CARGO_GUARD_QUIET  非空则不打印清理信息
set -u

cd "$(dirname "$0")/.." || exit 1

LIMIT="${CARGO_GUARD_LIMIT:-400000}"
DEPS="target/debug/deps"

# 全部 workspace 成员的**包名**（从成员表自动派生，不硬编码）。
#
# 为什么清全部成员而不是只清高 churn 的几个：实测清 4 个只把 51,938 → 45,953
# （因为未清的 crate 同样在堆），清全部 14 个则 81,500 → 928，耗时 5.54s。
# 外部依赖（248 个包的编译结果）**不动**——它们几乎不变，重编它们才是冷构建成本。
workspace_packages() {
    sed -n '/^members *= *\[/,/\]/p' Cargo.toml \
        | grep -o '"[^"]*"' \
        | tr -d '"' \
        | while read -r m; do
            # 包名通常与目录名不同（apps/coding-agent → ys-coding-agent），
            # 故从各自 Cargo.toml 读 `name`。
            sed -n 's/^name *= *"\(.*\)"/\1/p' "$m/Cargo.toml" 2>/dev/null
        done
}

guard() {
    [ "$LIMIT" -gt 0 ] || return 0
    [ -d "$DEPS" ] || return 0

    count=$(ls "$DEPS" 2>/dev/null | wc -l | tr -d ' ')
    [ "${count:-0}" -gt "$LIMIT" ] || return 0

    kill="${CARGO_GUARD_KEEP:-}"
    pkgs=$(workspace_packages | tr '\n' ' ')
    # 从全集中剔除要保留的。
    for k in $kill; do
        pkgs=$(printf '%s\n' $pkgs | grep -vx "$k" | tr '\n' ' ')
    done
    # shellcheck disable=SC2086
    set -- $pkgs
    [ $# -gt 0 ] || return 0

    [ -n "${CARGO_GUARD_QUIET:-}" ] || \
        echo "cargo-build: deps ${count} 条目（阈值 ${LIMIT}）→ 清理 $# 个 workspace crate" >&2

    # `-p` 逐个传：某些 cargo 版本不接受逗号分隔。
    for c in "$@"; do
        cargo clean -p "$c" >/dev/null 2>&1
    done

    [ -n "${CARGO_GUARD_QUIET:-}" ] || \
        echo "cargo-build: 剩余 $(ls "$DEPS" | wc -l | tr -d ' ') 条目" >&2
}

# 首参是 cargo 子命令 ⇒ 原样透传；否则（如 `--tests` / `-p x`）补上 `build`。
#
# 这条分支是必需的：把 `--tests` 当成子命令会让 cargo 报 `no such command`。
# **先清理、再构建**：单次构建净增 6,247（`ys-core`）或 512（叶文件），
# 构建后查到的数字已经包含这一次的增量 —— 那时再清等于刚建完就拆掉，
# 下次还要重建。先清则本次构建直接落在干净基线上。
guard

case "${1:-}" in
    build|check|test|clippy|doc|run|bench|fmt|clean|tree|metadata|rustc|rustdoc)
        exec cargo "$@"
        ;;
esac

exec cargo build "$@"
