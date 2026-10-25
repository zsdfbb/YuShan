#!/usr/bin/env python3
"""配色主题（TUI theme switch）的变异测试：注入 bug，期望对应用例变红。

用法：
    python3 scripts/theme-mutation.py            # 跑全部
    python3 scripts/theme-mutation.py --list     # 只列清单
    python3 scripts/theme-mutation.py 1 3        # 只跑指定编号

与 `scripts/wheel-mutation.py` 的区别：**本脚本不用 `git checkout --` 还原**。
滚轮那批变异落在已提交的文件上，故「还原」等价于丢弃工作区改动；而主题这批
改动本身就躺在工作区里（尚未提交），`git checkout --` 会把整批新代码一起抹掉。
这里改为**在内存里保存原文、`finally` 写回**，粒度是「只还原自己动过的那几行」。

判定口径：每条变异跑**对应用例**（`cargo test <filter>`），
- 编译失败 ⇒ 不算有效变异（注入把类型改坏了，红的原因不是测试）；
- 对应用例 FAILED ⇒ 抓到；
- 用例仍 pass ⇒ BUG：测试抓不到。
"""

import pathlib
import re
import subprocess
import sys

TUI = "ys-tui-coding"
APP = "ys-coding-agent"

APP_RS = pathlib.Path("crates/ys-tui-coding/src/app.rs")
RUN_RS = pathlib.Path("crates/ys-tui-coding/src/run.rs")
STATE_RS = pathlib.Path("apps/coding-agent/src/state.rs")
CAP_RS = pathlib.Path("apps/coding-agent/src/capabilities.rs")


def run_tests(crate, target, filter_):
    """跑 `cargo test -p <crate> <target> <filter>`，返回 (returncode, 红灯用例名, 是否编译通过)。

    `target` 是 `--lib` / `--bins`：`ys-coding-agent` **没有 lib target**（单测长在
    bin crate 里），对它必须用 `--bins`。
    """
    r = subprocess.run(
        ["cargo", "test", "-p", crate, target, filter_],
        capture_output=True,
        text=True,
    )
    red = sorted(
        set(re.findall(r"^test (\S+) \.\.\. FAILED", r.stdout, re.M))
    )
    compiled = (
        "error[E" not in r.stderr
        and "error: could not compile" not in r.stderr
        and "error[E" not in r.stdout
    )
    return r.returncode, red, compiled, r.stderr + r.stdout


# (name, path, 原文锚点, 注入后的替换, crate, cargo target, 用例过滤器, 期望变红的用例名, 备注)
MUTATIONS = [
    (
        "M1 set_theme 漏作废 markdown 缓存",
        APP_RS,
        "        self.md_cache.borrow_mut().set_theme(kind.palette());\n",
        "",
        TUI,
        "--lib",
        "test_set_theme_invalidates_both_caches",
        "app::tests::test_set_theme_invalidates_both_caches",
        "",
    ),
    (
        # 变异点**不能**是「删 #[serde(default)]」：`Option<T>` 缺键在 serde 里本就解成
        # `None`，删掉该属性用例仍绿（等价变异，已实测）。
        # 也不能给 `AppState` 加「非 Option 且无默认值」的必填字段来验 C9 —— 那会先因为
        # 既有构造点缺字段而**编译失败**，到不了「用例变红」（已实测；要验需同时改多处
        # 构造点，超出单点变异脚本的能力）。C9 的守护强度由它自身的哨兵断言 +
        # `!= AppState::default()` 保证（见用例内注释）。
        #
        # 这里选一个用户真实可观测、且与 C8（持久化写方向）直接相关的回归：
        # `login` 若整份重建 `AppState`，会把用户选的主题一起抹掉
        # （症状：「重启后主题丢了」，且几乎无法归因到 login）。
        "M2 login 整份重建 AppState 清掉 theme",
        CAP_RS,
        """    let mut st = state.load();
    st.last_active_provider = Some(provider_name.to_string());
    st.last_active_model = Some(model_name.clone());
    if let Err(e) = state.save(&st) {""",
        """    if let Err(e) = state.save(&crate::state::AppState {
        last_active_provider: Some(provider_name.to_string()),
        last_active_model: Some(model_name.clone()),
        ..Default::default()
    }) {""",
        APP,
        "--bins",
        "test_login_and_logout_preserve_theme",
        "capabilities::tests::test_login_and_logout_preserve_theme",
        "",
    ),
    (
        "M3 theme_modal_step 的 Esc 改成 Confirm",
        RUN_RS,
        "        ThemeKey::Esc => ThemeModalStep::Restore(state.opened_with),",
        "        ThemeKey::Esc => ThemeModalStep::Confirm(state.selected),",
        TUI,
        "--lib",
        "test_theme_modal_escape_restores_opened_with",
        "run::tests::test_theme_modal_escape_restores_opened_with",
        "",
    ),
]


def selected(argv):
    """按 `--list` / 编号参数筛出要跑的变异。"""
    if "--list" in argv:
        for i, (name, *_rest) in enumerate(MUTATIONS, 1):
            print(f"{i}. {name}")
        return None
    nums = [a for a in argv if not a.startswith("-")]
    if not nums:
        return list(enumerate(MUTATIONS, 1))
    picked = []
    for a in nums:
        idx = int(a)
        if not 1 <= idx <= len(MUTATIONS):
            print(f"编号越界：{a}（共 {len(MUTATIONS)} 条）", file=sys.stderr)
            sys.exit(2)
        picked.append((idx, MUTATIONS[idx - 1]))
    return picked


def main():
    picked = selected(sys.argv[1:])
    if picked is None:
        return 0

    print("基线：先确认对应用例本来是绿的（依赖未落地的条目跳过）", flush=True)
    runnable = []
    pre_skip = []
    for idx, m in picked:
        name, path, _old, _new, crate, target, filt, expect, note = m
        code, red, compiled, _out = run_tests(crate, target, filt)
        if not compiled:
            reason = note or f"`{expect}` 所在 crate 当前编译不过"
            pre_skip.append((idx, name, f"跳过（{reason}）", expect))
            continue
        if code != 0:
            print(f"  前置失败：基线不是绿的（red={red}，变异={name}）", file=sys.stderr)
            return 1
        runnable.append((idx, m))
    for idx, name, verdict, _expect in pre_skip:
        print(f"{idx}. {name}: {verdict}", flush=True)
    print("  基线绿\n" if runnable else "  无可跑项\n", flush=True)

    rows = list(pre_skip)
    for idx, (name, path, old, new, crate, target, filt, expect, note) in runnable:
        orig = path.read_text()
        if old not in orig:
            verdict = f"跳过（锚点未命中{('：' + note) if note else ''}）"
            rows.append((idx, name, verdict, expect))
            print(f"{idx}. {name}: {verdict}", flush=True)
            continue
        try:
            path.write_text(orig.replace(old, new, 1))
            code, red, compiled, _out = run_tests(crate, target, filt)
        finally:
            # 只还原自己动过的那一处：写回完整原文（内存里那份）。
            path.write_text(orig)
        if not compiled:
            verdict = "编译失败（不算有效变异）"
        elif code != 0 and any(expect.rsplit("::", 1)[-1] in t for t in red):
            verdict = "红（抓到）"
        elif code != 0:
            verdict = f"红但非预期用例（红={red[:3]}）"
        else:
            verdict = "仍绿（BUG：测试抓不到）"
        rows.append((idx, name, verdict, expect))
        print(f"{idx}. {name}: {verdict}", flush=True)

    rows.sort(key=lambda r: r[0])
    print("\n| # | 变异 | 结果 | 期望变红的用例 |")
    print("|---|---|---|---|")
    for idx, name, verdict, expect in rows:
        print(f"| {idx} | {name} | {verdict} | `{expect}` |")

    caught = sum(1 for _, _, v, _ in rows if "红（抓到）" in v)
    skipped = sum(1 for _, _, v, _ in rows if v.startswith("跳过"))
    print(
        f"\n结论：{len(rows)} 处注入，{caught} 处被抓到"
        + (f"，{skipped} 处跳过（依赖未落地任务）" if skipped else "")
    )
    # 跳过不算失败（依赖的用例还没合入）；只要没有「仍绿」就算通过。
    bad = [r for r in rows if "仍绿" in r[2] or "编译失败" in r[2] or "红但非预期" in r[2]]
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
