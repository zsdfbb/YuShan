#!/usr/bin/env python3
"""应用内选区（ADR-0051）的变异测试：注入 bug，期望对应用例变红。

用法：
    python3 scripts/selection-mutation.py            # 跑全部
    python3 scripts/selection-mutation.py --list     # 只列清单
    python3 scripts/selection-mutation.py 1 3        # 只跑指定编号

前提：无。工作区可以带未提交改动 —— 还原靠启动时的**进程内快照**，不用 `git checkout`。

> 为什么不用 `git checkout --`：它还原到 **HEAD**，会把工作区里尚未提交的改动
> （往往就是你要验的那批新代码）一起抹掉。快照还原的是「脚本启动那一刻的文件内容」。

## 注入 #3 的形态为什么不是文档里那个

`docs/arch/tui-text-selection/design.md` §6 与执行计划的 T22 表都写
「`skip = max(skip, w) - 1` → `skip = w - 1` ⇒ CJK 用例红」。**实测打不红**，而且
这不是测试漏洞：`skip` 是循环内的状态变量，`w <= 1` 时 `max` 不改变值，两种写法
逐轮产出完全相同的序列 —— 那是一个**语义等价**变异。

真正能改变行为的注入有两条，本脚本取 `skip = 0`（完全不跳续格），实测稳定打红 V4。
"""

import pathlib
import re
import subprocess
import sys

SEL = pathlib.Path("crates/ys-tui-coding/src/selection.rs")
EV = pathlib.Path("crates/ys-tui-coding/src/events.rs")

# 启动时的原文快照：`restore()` 写回它（不用 `git checkout`，见模块文档）。
SNAPSHOT: dict[pathlib.Path, str] = {}

SEL_TESTS = ["cargo", "test", "-p", "ys-tui-coding", "--lib", "selection::"]
EV_TESTS = ["cargo", "test", "-p", "ys-tui-coding", "--lib", "events::"]


def run_tests(cmd):
    r = subprocess.run(cmd, capture_output=True, text=True)
    red = sorted({m for m in re.findall(r"^test (\S+) \.\.\. FAILED", r.stdout, re.M)})
    compiled = "error[E" not in r.stderr and "error: could not compile" not in r.stderr
    return r.returncode, red, compiled


# (编号, 说明, 文件, 原文, 替换, 期望变红的用例, 测试命令)
MUTATIONS = [
    (
        1,
        "缓存键漏 inner.width（只比 height）",
        SEL,
        "&& self.key.inner == key.inner",
        "&& self.key.inner.height == key.inner.height",
        "inner_geometry_change_forces_rebuild",
        SEL_TESTS,
    ),
    (
        2,
        "shift 方向取反（rotate_left → rotate_right）",
        SEL,
        "            self.order.rotate_left(n);",
        "            self.order.rotate_right(n);",
        "v3_shift_matches_full_rebuild",
        SEL_TESTS,
    ),
    (
        3,
        "续格规则失效（skip = 0，完全不跳续格）",
        SEL,
        "        let cell_width = if grid.is_cont(y, x + 1) { 2 } else { 1 };",
        "        let cell_width = 1u16;",
        "v4_wide_chars_round_trip_without_extra_spaces",
        SEL_TESTS,
    ),
    (
        4,
        "高亮边界不调整宽字符（head_col 直返 x）",
        SEL,
        "        if self.is_cont(y, x) {\n            x.saturating_sub(1)\n        } else {\n            x\n        }",
        "        x",
        # `head_col` 同时被 `extract`（按内容裁剪）与 `rects`（高亮边界）使用，
        # 故改坏它会先打红字形对帐用例 —— 接受任一相关用例。
        ("rects_widen_wide_char_right_edge", "glyph_grid_matches_whole_window_render"),
        SEL_TESTS,
    ),
    (
        5,
        "行尾不 trim_end（尾部空格带出）",
        SEL,
        "    let trimmed = out.trim_end_matches(' ');",
        "    let trimmed = out.as_str();",
        "read_back_never_leaves_trailing_spaces",
        SEL_TESTS,
    ),
    (
        6,
        "Resize 不清选区",
        EV,
        "        Event::Resize(_, _) => app.selection.clear(),",
        "        Event::Resize(_, _) => {}",
        "v9_resize_clears_selection",
        EV_TESTS,
    ),
    (
        7,
        "自动滚动不压回 follow（删掉 drag_tick 第 5 步）",
        SEL,
        "    app.follow = false;\n"
        '    debug_assert!(!app.follow, "自动滚动期间必须脱离底部跟随");',
        "    // MUTANT: 压回被删除",
        "v8_drag_tick_never_restores_follow",
        SEL_TESTS,
    ),
    (
        8,
        "高亮把内区相对坐标当绝对坐标（漏平移）",
        SEL,
        "            let abs = Rect::new(\n"
        "                r.x.saturating_add(area.x),\n"
        "                r.y.saturating_add(area.y),",
        "            let abs = Rect::new(\n"
        "                r.x,\n"
        "                r.y,",
        "highlight_translates_relative_rects_by_area_origin",
        SEL_TESTS,
    ),
    (
        9,
        "松手不复制（回到「等右键才拷」）",
        EV,
        "    app.selection.mark();\n    copy_marked_selection(app);\n",
        "    app.selection.mark();\n",
        "drag_then_release_copies_text",
        EV_TESTS,
    ),
    (
        10,
        "松手清高亮（复制成功与否只能猜）",
        SEL,
        "        self.phase = Phase::Marked;\n        self.edge = 0;",
        "        self.phase = Phase::Marked;\n        self.clear();\n        self.edge = 0;",
        "drag_then_release_copies_text",
        EV_TESTS,
    ),
    (
        11,
        "左键不清旧选区（旧高亮只能靠 resize 消失）",
        EV,
        "fn begin_selection(app: &mut App, m: MouseEvent, width: u16, height: u16) {\n"
        "    app.selection.clear();",
        "fn begin_selection(app: &mut App, m: MouseEvent, width: u16, height: u16) {\n"
        "    // MUTANT: 不清理旧选区",
        "left_down_outside_inner_clears_finished_selection",
        EV_TESTS,
    ),
    (
        12,
        "右键补救只认 `Down`（缺该位的终端按了没反应）",
        EV,
        "        MouseEventKind::Down(MouseButton::Right)\n"
        "            | MouseEventKind::Up(MouseButton::Right)\n"
        "            | MouseEventKind::Drag(MouseButton::Right)",
        "        MouseEventKind::Down(MouseButton::Right)",
        "right_release_alone_recopies_marked_selection",
        EV_TESTS,
    ),
    (
        13,
        "复制不挂状态行提示（高亮不消失 ⇒ 用户无法区分成功与被抢）",
        EV,
        '            app.note_status(format!("已复制 {chars} 字"));',
        "            // MUTANT: 不挂提示",
        "drag_then_release_copies_text",
        EV_TESTS,
    ),
]


def restore():
    for path, text in SNAPSHOT.items():
        path.write_text(text)


def main():
    args = sys.argv[1:]
    if "--list" in args:
        for num, name, _p, _o, _n, expect, _c in MUTATIONS:
            label = expect if isinstance(expect, str) else " / ".join(expect)
            print(f"{num}. {name}  ⇒ 期望 {label} 红")
        return 0

    only = {int(a) for a in args if a.isdigit()}
    todo = [m for m in MUTATIONS if not only or m[0] in only]

    # 快照必须在**任何注入之前**取：`restore()` 写回的就是这份内容。
    SNAPSHOT[SEL] = SEL.read_text()
    SNAPSHOT[EV] = EV.read_text()

    print("基线：跑一次全绿", flush=True)
    base_code, base_red, base_compiled = run_tests(SEL_TESTS)
    if base_code != 0 or not base_compiled:
        print(f"前置失败：基线不是绿的（red={base_red}）", file=sys.stderr)
        return 1
    print("  基线绿\n", flush=True)

    rows = []
    try:
        for num, name, path, old, new, expect, cmd in todo:
            expects = (expect,) if isinstance(expect, str) else tuple(expect)
            label = " / ".join(expects)
            orig = path.read_text()
            if old not in orig:
                rows.append((num, name, "锚点未命中（脚本失效）", label))
                print(f"{num}. {name}: 锚点未命中", flush=True)
                continue
            path.write_text(orig.replace(old, new, 1))
            code, red, compiled = run_tests(cmd)
            restore()
            if not compiled:
                verdict = "编译失败（不算有效变异）"
            elif code != 0 and any(e in t for e in expects for t in red):
                verdict = "红（抓到）"
            elif code != 0:
                verdict = f"红但非预期用例（红={red[:3]}）"
            else:
                verdict = "仍绿（BUG：测试抓不到）"
            rows.append((num, name, verdict, label))
            print(f"{num}. {name}: {verdict}", flush=True)
    finally:
        restore()

    print("\n| # | 变异 | 结果 | 期望变红的用例 |")
    print("|---|---|---|---|")
    for num, name, verdict, expect in rows:
        print(f"| {num} | {name} | {verdict} | `{expect}` |")
    caught = sum(1 for _, _, v, _ in rows if "红（抓到）" in v)
    print(f"\n结论：{len(rows)} 处注入，{caught} 处被抓到")
    return 0 if caught == len(rows) else 1


if __name__ == "__main__":
    sys.exit(main())
