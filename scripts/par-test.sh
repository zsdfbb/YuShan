#!/bin/sh
# 全量测试的**默认入口**。
#
# # 为什么不用 `cargo test --workspace`
#
# `cargo test` 对各个测试二进制是**串行**的（先 lib 单测，再逐个 `tests/*.rs`），
# 于是总耗时 = 各二进制耗时**之和**。而单个二进制内部的用例本来就已经并行
# （libtest 默认 `test-threads` = CPU 核数）。本脚本补上**跨二进制**的并行。
#
# 实测（8 核 Android，1545 用例，热缓存）：
#   cargo test --workspace       ≈ 34s   （串行二进制）
#   ./scripts/par-test.sh 8      ≈ 7.4s  （本脚本默认；快 4.6 倍）
#   cargo nextest run --workspace ≈ 14.1s（每**用例**独立进程；本机 fork 贵）
#
# 用例完全相同 —— 差别只在调度方式，不是「少跑了」。
#
# # 用法
#
#   ./scripts/par-test.sh                  # 全量，默认 8 路
#   ./scripts/par-test.sh --jobs 4         # 指定并行度（越少越保守）
#   ./scripts/par-test.sh -- --ignored     # 追加参数原样传给每个测试二进制
#   ./scripts/par-test.sh --no-build       # 跳过构建（二进制已是最新时）
#
# 追加参数用 `--` 分隔，会原样传给**每个**测试二进制（如过滤名、`--nocapture`、
# `--ignored`）。注意：过滤名只匹配到某个二进制里的用例时，其余二进制会报「0 个用例」，
# 那是正常现象（libtest 以 0 退出）。
#
# 退出码：0 = 全部通过；1 = 有二进制失败或有构建错误。
set -u

JOBS=8
DO_BUILD=1
PASSTHRU=""

while [ $# -gt 0 ]; do
  case "$1" in
    --jobs|-j)
      JOBS="${2:?--jobs 需要一个数值}"
      shift 2
      ;;
    --no-build)
      DO_BUILD=0
      shift
      ;;
    --)
      shift
      # 剩余全部原样透传
      PASSTHRU="$*"
      break
      ;;
    -h|--help)
      sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      # 第一个位置参数兼容旧用法：`par-test.sh 4`
      JOBS="$1"
      shift
      ;;
  esac
done

cd "$(dirname "$0")/.." || exit 1

WORK="${TMPDIR:-/tmp}"
EXES="$WORK/yushan-par-test-exes.$$"
FAILED="$WORK/yushan-par-test-failed.$$"
BUILD_OUT="$WORK/yushan-par-test-build.$$.json"
BUILD_ERR="$WORK/yushan-par-test-build.$$.err"
mkdir -p "$FAILED" || exit 1
trap 'rm -f "$EXES" "$BUILD_OUT" "$BUILD_ERR"; rm -rf "$FAILED"' EXIT

# ── 1) 构建并取出全部测试二进制 ──
#
# 走 `cargo-build.sh` 而不是裸 `cargo`：测试构建同样是 `.rcgu.o` 的生产者，
# 而它是 agent 最常跑的一类命令（一次 `--tests` 就净增数千条目）。
if [ "$DO_BUILD" -eq 1 ]; then
  # 构建输出先落盘再解析：管道会把退出码交给最后一个命令（python3），
  # cargo 的失败会被静默吞掉（本脚本刻意不开 `set -e`，见文件头）。
  ./scripts/cargo-build.sh test --workspace --no-run --message-format=json \
    >"$BUILD_OUT" 2>"$BUILD_ERR"
  build_status=$?
  if [ "$build_status" -ne 0 ]; then
    echo "par-test: 构建失败（退出码 $build_status）" >&2
    # stderr 只装 cargo 自己的提示（如 `Blocking waiting for file lock`）；
    # **`--message-format=json` 会把真正的 error 诊断写进 stdout**，故两路都要打。
    # 只打 stderr 时，链接器 / rustc 的报错会完全丢失，日志里只剩一行「构建失败」，
    # 无从定位。
    cat "$BUILD_ERR" >&2 2>/dev/null
    # `SIGXFSZ`（signal 25）不是代码错误：链接器在 `ftruncate` 把输出文件撑到最终
    # 大小时撞上 `RLIMIT_FSIZE`。本仓的测试二进制（带调试符号）实测已达 65 MiB，
    # 若进程的 `ulimit -f` 小于此值就必然失败 —— rust-lld 只报一句
    # 「PLEASE submit a bug report」加堆栈，极易被误判为编译器 bug。
    if grep -q 'SIGXFSZ' "$BUILD_ERR" 2>/dev/null || \
       grep -q 'signal 25' "$BUILD_OUT" 2>/dev/null; then
      fsize=$(ulimit -f)
      echo "par-test: ⚠ 构建遇到 SIGXFSZ（文件大小超限）" >&2
      echo "par-test:   当前 ulimit -f = $fsize 块 = $((fsize * 512)) 字节" >&2
      echo "par-test:   本仓测试二进制实测已到 65 MiB；请用 `ulimit -f unlimited` 重试" >&2
    fi
    # 注意：**不能**写 `>&2 2>/dev/null` —— 那会把 stderr 重定向到 /dev/null，
    # 顺序是左到右生效的。python 自己把诊断写到 stderr 即可。
    python3 -c '
import sys, json
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("reason") != "compiler-message":
        continue
    d = m.get("message") or {}
    if d.get("level") != "error":
        continue
    print(d.get("rendered") or d.get("message") or "", file=sys.stderr)
' <"$BUILD_OUT"
    exit 1
  fi
  python3 -c '
import sys, json
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("reason") != "compiler-artifact":
        continue
    if not m.get("profile", {}).get("test"):
        continue
    exe = m.get("executable")
    if exe:
        print(exe)
' <"$BUILD_OUT" >"$EXES"
  parse_status=$?
  rm -f "$BUILD_OUT" "$BUILD_ERR"
  if [ "$parse_status" -ne 0 ]; then
    echo "par-test: 解析构建输出失败（退出码 $parse_status）" >&2
    exit 1
  fi
else
  # 不构建时直接复用上一次的清单（最快的路径：改完代码忘了构建会跑到旧二进制，
  # 故仅在显式要求时使用）。
  if [ ! -f "$WORK/yushan-par-test-exes.cache" ]; then
    echo "par-test: --no-build 但没有缓存清单，先跑一次不带它的" >&2
    exit 1
  fi
  cp "$WORK/yushan-par-test-exes.cache" "$EXES"
fi

if [ ! -s "$EXES" ]; then
  echo "par-test: 没有找到测试二进制" >&2
  exit 1
fi
cp "$EXES" "$WORK/yushan-par-test-exes.cache"

n_bins=$(grep -c '' "$EXES")
echo "par-test: $n_bins 个测试二进制，并行度 $JOBS${PASSTHRU:+，透传参数: $PASSTHRU}"

# ── 2) 跨二进制并行 ──
# 正常路径不打印输出（避免上千行刷屏）；失败才留档。
export YUSHAN_PAR_FAILED="$FAILED"
export YUSHAN_PAR_PASSTHRU="$PASSTHRU"
xargs -P "$JOBS" -I{} sh -c '
  # shellcheck disable=SC2086
  out=$("$1" $YUSHAN_PAR_PASSTHRU 2>&1) || printf "%s\n" "$out" > "$YUSHAN_PAR_FAILED/$(basename "$1").log"
' _ {} < "$EXES"

# ── 3) 汇总 ──
n_fail=$(ls "$FAILED" 2>/dev/null | wc -l | tr -d ' ')
if [ "$n_fail" -eq 0 ]; then
  echo "par-test: 全部通过"
  exit 0
fi

echo "par-test: $n_fail 个测试二进制失败" >&2
for f in "$FAILED"/*.log; do
  echo "===== $(basename "$f") =====" >&2
  # 只摘失败用例名与 panic 首行，完整输出落盘
  grep -E '^(test .* FAILED|failures:|---- |thread .* panicked|assertion)' "$f" | head -20 >&2
  cp "$f" "$WORK/yushan-par-test-keep-$(basename "$f")" 2>/dev/null
done
echo "完整输出已保留在 $WORK/yushan-par-test-keep-*.log" >&2
exit 1
