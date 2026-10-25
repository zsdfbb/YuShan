#!/usr/bin/env python3
"""滚轮接管（ADR-0050）的变异测试：注入 bug，期望对应用例变红。

用法：python3 tmp/mutation-wheel.py
前提：events.rs / run.rs 与 HEAD 一致（脚本用 `git checkout --` 还原）。
"""

import pathlib
import re
import subprocess
import sys

EV = pathlib.Path("crates/ys-tui-coding/src/events.rs")
RU = pathlib.Path("crates/ys-tui-coding/src/run.rs")


def run_tests():
    r = subprocess.run(
        ["cargo", "test", "-p", "ys-tui-coding", "--lib"],
        capture_output=True,
        text=True,
    )
    red = sorted({
        m for m in re.findall(r"^test (\S+) \.\.\. FAILED", r.stdout, re.M)
    })
    # 编译失败也算「抓到」——但要说清楚是哪一种
    compiled = "error[E" not in r.stderr and "error: could not compile" not in r.stderr
    return r.returncode, red, compiled, r.stderr


MUTATIONS = [
    (
        "M1 滚轮 ScrollUp 接到输入历史",
        EV,
        "        MouseEventKind::ScrollUp => scroll_clearing_selection(app, width, height, 1),",
        "        MouseEventKind::ScrollUp => {\n"
        "            if let Some(t) = app.history.prev(&app.input.text) {\n"
        "                app.input.clear();\n"
        "                app.input.insert_str(&t);\n"
        "            }\n"
        "        }",
        "test_wheel_scrolls_chat_and_never_input_history",
    ),
    (
        "M2 滚轮步长改成一页",
        EV,
        "        MouseEventKind::ScrollUp => scroll_clearing_selection(app, width, height, 1),",
        "        MouseEventKind::ScrollUp => {\n            scroll_clearing_selection(app, width, height, page_rows(app, width, height))\n        }",
        "test_wheel_scrolls_chat_and_never_input_history",
    ),
    (
        "M3 其余鼠标事件也滚动（抢终端拖选）",
        EV,
        "        // 水平滚动没有语义（对话是单列竖排）；悬停不绑（不做 hover）。\n        _ => {}",
        "        _ => scroll_clearing_selection(app, width, height, 1),",
        "test_other_mouse_events_are_ignored",
    ),
    (
        "M4 删掉 handle_event 的 Mouse 分支",
        EV,
        "        Event::Mouse(m) => handle_mouse(m, app, width, height),\n",
        "",
        "test_handle_event_routes_mouse_to_scroll",
    ),
    (
        "M5 restore_sequence 漏关鼠标捕获",
        RU,
        "        DisableBracketedPaste,\n        DisableMouseCapture,\n        PopKeyboardEnhancementFlags",
        "        DisableBracketedPaste,\n        PopKeyboardEnhancementFlags",
        "restore_sequence_contains_terminal_state_restore",
    ),
    (
        "M6 ScrollDown 方向反了",
        EV,
        "        MouseEventKind::ScrollDown => scroll_clearing_selection(app, width, height, -1),",
        "        MouseEventKind::ScrollDown => scroll_clearing_selection(app, width, height, 1),",
        "test_wheel_scrolls_chat_and_never_input_history",
    ),
]


def restore():
    subprocess.run(["git", "checkout", "--", str(EV), str(RU)], check=True)


def main():
    dirty = subprocess.run(
        ["git", "diff", "--quiet", "--", str(EV), str(RU)]
    ).returncode
    if dirty != 0:
        print("前置失败：events.rs / run.rs 有未提交改动，拒绝变异", file=sys.stderr)
        return 1

    print("基线：跑一次全绿", flush=True)
    code, red, compiled, _ = run_tests()
    if code != 0:
        print(f"前置失败：基线不是绿的（red={red}）", file=sys.stderr)
        return 1
    print("  基线绿\n", flush=True)

    rows = []
    try:
        for name, path, old, new, expect in MUTATIONS:
            orig = path.read_text()
            if old not in orig:
                rows.append((name, "锚点未命中（脚本失效）", "—"))
                print(f"{name}: 锚点未命中", flush=True)
                continue
            path.write_text(orig.replace(old, new, 1))
            code, red, compiled, err = run_tests()
            restore()
            if not compiled:
                verdict = "编译失败（不算有效变异）"
            elif code != 0 and any(expect in t for t in red):
                verdict = "红（抓到）"
            elif code != 0:
                verdict = f"红但非预期用例（红={red[:3]}）"
            else:
                verdict = "仍绿（BUG：测试抓不到）"
            rows.append((name, verdict, expect))
            print(f"{name}: {verdict}", flush=True)
    finally:
        restore()

    print("\n| 变异 | 结果 | 期望变红的用例 |")
    print("|---|---|---|")
    for name, verdict, expect in rows:
        print(f"| {name} | {verdict} | `{expect}` |")
    ok = all("红（抓到）" in v for _, v, _ in rows)
    print(f"\n结论：{len(rows)} 处注入，{sum(1 for _,v,_ in rows if '红（抓到）' in v)} 处被抓到")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
