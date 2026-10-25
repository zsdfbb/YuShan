#!/usr/bin/env python3
"""对比同类 coding agent CLI 的启动内存。

每个 CLI 在 `script(1)` 分配的伪终端里起 TUI，等固定秒数后采一次样，不发起任何模型调用。

# 统计口径

**RSS 合计**，含目标进程的全部相关进程。两个容易踩的坑：

1. **有的 CLI 会把辅助进程重新挂到 init / systemd 下**（Codex 的 `app-server` daemon
   就是这样，200 MiB 左右）。只按 `ppid` 往下走会漏掉它，数字少一半。
   所以除了进程树，还要把「名字匹配目标」的进程一并计入。
2. **匹配必须带边界**。`pi` 这个裸词会撞上 `pipewire`、`pipewire-pulse` 之类。

只算进程、不算线程 —— 线程与进程共享 RSS，逐个计入会重复计数。

用法：
    python3 scripts/ram-compare.py                  # 默认清单，每个跑 1 遍
    python3 scripts/ram-compare.py --rounds 3       # 每个跑 3 遍看稳定性
    python3 scripts/ram-compare.py -v               # 逐个进程打印
    python3 scripts/ram-compare.py yushan:./target/release/ys-coding-agent

每条参数形态 `<标签>:<命令>`。每轮会先清掉上一轮残留的同名进程，否则会把旧进程算进来。
某个 CLI 没装就跳过并说明，不算失败。
"""
import argparse
import os
import re
import shutil
import signal
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# (标签, 命令, 进程匹配正则)。正则要用进程命令行去匹配，带边界 —— 见模块头的坑 2。
DEFAULT = [
    ("yushan", os.path.join(HERE, "target/release/ys-coding-agent"), r"ys-coding-agent"),
    ("codewhale", "codewhale", r"codewhale"),
    ("pi", "pi", r"(^|[\s/])pi(\s|$)"),
    ("claude", "claude", r"claude"),
    ("codex", "codex", r"codex"),
    ("opencode", "opencode", r"opencode"),
]


def snapshot():
    """pid -> (ppid, rss_kb, cmdline)。只看进程（Tgid == pid）。"""
    table = {}
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        pid = int(entry)
        try:
            with open(f"/proc/{pid}/status") as f:
                status = f.read()
            if int(re.search(r"Tgid:\s*(\d+)", status).group(1)) != pid:
                continue  # 线程
            with open(f"/proc/{pid}/cmdline", "rb") as f:
                cmd = f.read().replace(b"\0", b" ").decode("utf-8", "replace").strip()
        except (OSError, AttributeError):
            continue  # 采样途中退出
        rss = re.search(r"VmRSS:\s*(\d+)", status)
        table[pid] = (
            int(re.search(r"PPid:\s*(\d+)", status).group(1)),
            int(rss.group(1)) if rss else 0,
            cmd,
        )
    return table


def ancestors():
    """本脚本及其祖先链。命令行里含目标名字（比如脚本参数），必须排除。"""
    table, chain, pid = snapshot(), set(), os.getpid()
    while pid:
        chain.add(pid)
        pid = table[pid][0] if pid in table else 0
    return chain


def matched(pattern, skip):
    """名字匹配目标的进程。`script(1)` 只是分配伪终端的脚手架，不算。"""
    return {
        pid: v
        for pid, v in snapshot().items()
        if pid not in skip and "script -qec" not in v[2] and re.search(pattern, v[2], re.I)
    }


def measure(cmd, pattern, settle):
    skip = ancestors()
    for pid in matched(pattern, skip):  # 清场：上一轮的残留会把数字抬高
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
    time.sleep(1.5)
    subprocess.Popen(
        ["script", "-qec", cmd, "/dev/null"],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    try:
        time.sleep(settle)
        return matched(pattern, ancestors())
    finally:
        for pid in matched(pattern, ancestors()):
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("targets", nargs="*", help="<标签>:<命令>；缺省跑内置清单")
    ap.add_argument("--settle", type=float, default=6, help="启动后等多少秒采样（默认 6）")
    ap.add_argument("--rounds", type=int, default=1, help="每个跑几遍（默认 1）")
    ap.add_argument("-v", "--verbose", action="store_true", help="逐个进程打印")
    args = ap.parse_args()

    targets = []
    for spec in args.targets or [f"{label}:{cmd}" for label, cmd, _ in DEFAULT]:
        label, _, cmd = spec.partition(":")
        if not cmd:
            label, cmd = spec, spec
        pattern = next((p for l, _, p in DEFAULT if l == label), re.escape(label))
        targets.append((label, cmd, pattern))

    for label, cmd, pattern in targets:
        exe = cmd.split()[0]
        if os.sep in exe and not os.access(exe, os.X_OK):
            print(f"{label}: 跳过（{exe} 不存在或不可执行）")
            continue
        if os.sep not in exe and shutil.which(exe) is None:
            print(f"{label}: 跳过（PATH 上无 {exe}）")
            continue
        for i in range(args.rounds):
            procs = measure(cmd, pattern, args.settle)
            tag = f"{label} #{i + 1}" if args.rounds > 1 else label
            total = sum(v[1] for v in procs.values()) / 1024
            print(f"{tag}: {total:.0f} MiB, {len(procs)} 个进程")
            if args.verbose:
                for pid, (_, rss, c) in sorted(procs.items(), key=lambda x: -x[1][1]):
                    print(f"   {rss / 1024:6.0f} MiB  {pid:>7}  {c[:80]}")


if __name__ == "__main__":
    sys.exit(main())
