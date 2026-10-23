#!/bin/sh
# 构建耗时基线：冷构建 / 两种增量 / 无改动，各测一次。
#
# 用途：对比不同构建配置（链接器、debuginfo 档位、sccache…）在同一台机器上的
# 相对收益。**只看相对值**，绝对值随机器差异很大。
#
# 用法：
#   scripts/build-baseline.sh <label>                 # 用当前默认配置
#   scripts/build-baseline.sh mold  RUSTFLAGS=...     # 追加 env 赋值
set -u

label="${1:?用法: build-baseline.sh <label> [env=val ...]}"
shift
cd "$(dirname "$0")/.." || exit 1

now_ms() { date +%s%N | cut -c1-13; }
run() { env "$@" cargo build --workspace --tests; }

out="${TMPDIR:-/tmp}"
start=$(now_ms)
run "$@" > "$out/bb-${label}-cold.log" 2>&1; cold=$?
t1=$(now_ms)

# 场景 A：改底层 crate（ys-core 被几乎所有 crate 依赖）—— 最坏增量
touch crates/ys-core/src/lib.rs
run "$@" > "$out/bb-${label}-incr-core.log" 2>&1; incr_core=$?
t2=$(now_ms)

# 场景 B：改叶子 app 的单个源文件 —— 最常见的编辑循环
touch apps/coding-agent/src/app_loop.rs
run "$@" > "$out/bb-${label}-incr-app.log" 2>&1; incr_app=$?
t3=$(now_ms)

# 场景 C：无改动 —— cargo 自身的开销下限
run "$@" > "$out/bb-${label}-noop.log" 2>&1; noop=$?
t4=$(now_ms)

printf 'label=%-12s cold=%6dms  incr_core=%6dms  incr_app=%6dms  noop=%5dms  status=%d/%d/%d/%d\n' \
  "$label" $(( t1 - start )) $(( t2 - t1 )) $(( t3 - t2 )) $(( t4 - t3 )) \
  "$cold" "$incr_core" "$incr_app" "$noop"
