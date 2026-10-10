# U3 深审报告 —— panic 边界（Task 5 [impl] + Task 6 [test]）

- **基线 ref**：`9e0b492faa1ce65108d257269904364db9dfe10d`
- **审查对象**：`crates/ys-tui-coding/src/run.rs` 的 panic 边界
- **设计依据**：`docs/arch/tui-markdown-render/design.md` §2.11 + 不变量 8；`docs/adr/0030-tui-markdown-render.md` 决策 7；`docs/exec-plans/2026-10-10-tui-markdown-render.md` Task 5 / Task 6；测试合同 N9（`docs/design-plans/2026-10-10-tui-markdown-render.md`）
- **结论**：**可放行**。P0 = 0，P1 = 0；仅 2 条 P2（均为**测试守不住**的覆盖缺口，代码本身正确）。

---

## 命令原文

### `cargo test -p ys-tui-coding run::`

（首次运行因并发单元改到 `draw.rs:594`（`build_text_with` 少传 `Instant`）而编译失败 —— **落在 `draw.rs`，与本单元无关**；等待后重跑，全绿。）

```
running 8 tests
test run::tests::test_modal_move_wraps_both_ways ... ok
test run::tests::test_modal_move_single_option_stays ... ok
test run::tests::test_modal_move_on_empty_list_is_noop ... ok
test run::tests::panic_hook_body_restores_then_delegates ... ok
test run::tests::restore_sequence_contains_terminal_state_restore ... ok
test run::tests::exit_payload_starts_with_terminal_restore ... ok
test run::tests::exit_payload_replays_without_absolute_positioning ... ok
test run::tests::exit_payload_with_no_lines_still_breaks_the_line ... ok

test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 229 filtered out; finished in 0.00s
```

### `cargo test -p ys-tui-coding`（在 `/tmp` 干净副本中跑，避开并发编辑）

```
test result: ok. 242 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.06s
```

### `cargo clippy --all-targets -p ys-tui-coding`

```
Checking ys-tui-coding v0.0.1 (...)
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.82s
```

`ys-tui-coding` **0 warning**。输出里唯一的 warning 来自 `ys-protocol`（`derivable_impls`，`crates/ys-protocol/src/lifecycle.rs:13`），**与本单元无关**。

---

## P0

无。

## P1

无。

## P2

### P2-1　`PopKeyboardEnhancementFlags` 无任何测试守护（变异存活）

- **文件:行**：`crates/ys-tui-coding/src/run.rs:480`（`restore_sequence()` 的 `PopKeyboardEnhancementFlags`）
- **证据**：变异 G —— 从 `restore_sequence()` 删掉 `PopKeyboardEnhancementFlags` 后，`cargo test -p ys-tui-coding run::` 仍 **8 passed / 0 failed**。
- **为什么会出问题**：`restore_sequence()` 被 `exit_payload`（正常退出）与 `restore_terminal_on_panic`（panic 退出）共用。它推出的字节是 `\x1b[<1u`。`EnterAlternateScreen`/`?1049l` **不保证**复位键盘增强标志；若这里少写这一条，退出后 shell 会继续收到「消歧义转义码」，Shift/Alt+Enter 之类的键在 shell 里表现为乱码，且这条缺口在正常退出与 panic 退出**两条路径同时**存在。当前代码**是对的**（含该命令），缺的只是**回归守卫**。
- **什么样的输入会触发**：无明显触发输入 —— 是「未来有人手抖删掉这一项」的回归风险。N9 ① 只要求 3 条序列，故这是**超出合同范围**但实质影响存在的覆盖缺口。
- **注**：既有路径（`exit_payload`）本就未 pin 该命令，非本单元引入；本单元把这序列抽成共享函数后，缺口被放大到两条路径。

### P2-2　`disable_raw_mode()` / `Show` 在 panic hook 中无测试守护（变异存活）

- **文件:行**：`crates/ys-tui-coding/src/run.rs:555`（`disable_raw_mode()`）、`:557`（`execute!(out, Show)`）
- **证据**：变异 E/F —— 删掉 `restore_terminal_on_panic` 里的 `disable_raw_mode()` 与 `execute!(out, Show)`（只留 `write!` + `flush`），`cargo test -p ys-tui-coding run::` 仍 **8 passed / 0 failed**（仅有 `unused import: crossterm::cursor::Show` 的编译 warning）。
- **为什么会出问题**：这两步正是「终端不留 raw mode」「光标不留在隐藏态」的最后保障；一旦被删，panic 后终端会留在 raw mode / 光标隐藏，用户仍需 `reset`。单测不接触真终端，故**原理上测不到**。
- **触发条件**：手抖删除，或重构时误将这两步并入别处。
- **裁定**：N9 ③ **明文**规定该项「由 review subagent 代码走查确认」而**非**单测 —— 即合同**认可**此缺口。故这是**已声明的可接受缺口**，不构成本单元返工项；列出以备后续（若要加强，可用注入探针断言「恢复动作内部步骤顺序」，但那会把 `restore_terminal_on_panic` 的 IO 也抽象化，属额外设计）。

> 无其它实质问题。风格 / 措辞 / 文档建议不在本档范围。

---

## 测试合同 N9 逐条覆盖

| 条目 | 结论 | 证据 |
|---|---|---|
| ① `restore_sequence()` 含 `\x1b[?1049l` / `\x1b[?7h` / `\x1b[?2004l` | **真被覆盖** | 用例 `restore_sequence_contains_terminal_state_restore`（`run.rs:651-657`）：`assert!(seq.contains("\x1b[?1049l"))` / `"\x1b[?7h"` / `"\x1b[?2004l"`，三条各一断言。变异 B（删 `LeaveAlternateScreen`）令其变红。 |
| ② `panic_hook_body` 在注入恢复动作被调用后**仍调用** delegate | **真被覆盖（强）** | 用例 `panic_hook_body_restores_then_delegates`（`run.rs:664-686`）：三个 `AtomicBool` 探针 —— `restored`（恢复被调用）、`delegated`（delegate 被调用）、`restored_before_delegate`（在 delegate 内回读 `restored`，钉住**先后顺序**）。变异 A/C/D 均令其变红。**不触发真实 panic、不接触真终端** —— 与并行测试隔离良好。 |
| ③ `disable_raw_mode()` 作为系统调用排在恢复序列之后 | **代码走查确认（合同即如此规定，非单测）** | 代码走查 `run.rs:551-558`：`write!(restore_sequence())` → `flush()` → `disable_raw_mode()` → `execute!(Show)`，顺序符合 §2.11 与 Task 5 描述。变异 E/F 证明单测覆盖不到（见 P2-2）；N9 ③ 已明文将此条划归走查。 |
| 附加：既有 `exit_payload_starts_with_terminal_restore` 保持绿 | **真被覆盖** | `cargo test ... run::` 中该用例 `ok`；其断言 `?1049l` 位置 < 首个 `\r\x1b[2K` 位置（`run.rs:634-644`）。变异 B 令其变红，说明抽取 `restore_sequence()` 后该断言仍有效。 |

小结：①② 强覆盖；③ 依合同为走查覆盖（单测**确实**测不到，已如实标注）。

---

## 变异测试结果（在 `/tmp/ys-review-u3` 副本中做，事后已 `rm -rf`）

每次变异前把 `run.rs` 重置为工作区版本，改完编译跑 `cargo test -p ys-tui-coding run::`。

| # | 注入的 bug | 应变红的用例 | 结果 |
|---|---|---|---|
| A | `panic_hook_body` 改为**先 `delegate(info)` 再 `restore()`** | `panic_hook_body_restores_then_delegates` | **红**（见下） |
| B | 从 `restore_sequence()` 删 `LeaveAlternateScreen` | `restore_sequence_contains_terminal_state_restore` **且** `exit_payload_starts_with_terminal_restore` | **红 ×2**（见下） |
| C | `panic_hook_body` 删掉 `restore()` 调用 | `panic_hook_body_restores_then_delegates` | **红** |
| D | `panic_hook_body` 删掉 `delegate(info)` 调用 | `panic_hook_body_restores_then_delegates` | **红** |
| E/F | 从 `restore_terminal_on_panic` 删 `disable_raw_mode()` + `Show` | （预期：无） | **存活** → 缺口，见 P2-2（合同已声明为走查项） |
| G | 从 `restore_sequence()` 删 `PopKeyboardEnhancementFlags` | （预期：无） | **存活** → 缺口，见 P2-1 |

### 失败信息原文

**变异 A（先 delegate 后 restore）：**

```
thread 'run::tests::panic_hook_body_restores_then_delegates' (2044946) panicked at crates/ys-tui-coding/src/run.rs:682:9:
恢复必须发生在 delegate 之前
...
test result: FAILED. 7 passed; 1 failed; 0 ignored; 0 measured; 235 filtered out; finished in 0.00s
```

**变异 B（删 LeaveAlternateScreen）：**

```
thread 'run::tests::exit_payload_starts_with_terminal_restore' (2045914) panicked at crates/ys-tui-coding/src/run.rs:635:9:
必须先退出 alt-screen 再回放："\u{1b}[?7h\u{1b}[?2004l\u{1b}[<1u\r\u{1b}[2Kx\u{1b}[0m\r\n"
---
thread 'run::tests::restore_sequence_contains_terminal_state_restore' (2045917) panicked at crates/ys-tui-coding/src/run.rs:653:9:
须退出 alt-screen："\u{1b}[?7h\u{1b}[?2004l\u{1b}[<1u"
...
test result: FAILED. 6 passed; 2 failed; 0 ignored; 0 measured; 235 filtered out; finished in 0.00s
```

（顺带确认 `restore_sequence()` 的真实字节：`\x1b[?7h` + `\x1b[?2004l` + `\x1b[<1u`，即 `EnableLineWrap` / `DisableBracketedPaste` / `PopKeyboardEnhancementFlags`。）

**变异 C（无 restore()）**：`panicked at run.rs:679: 恢复动作必须被调用` → FAILED。
**变异 D（无 delegate()）**：`panicked at run.rs:682: delegate 必须仍被调用` → FAILED。
**变异 E/F**：`test result: ok. 8 passed; 0 failed`（仅 `unused import: crossterm::cursor::Show` warning）。
**变异 G**：`test result: ok. 8 passed; 0 failed`。

结论：合同要求的两个**行为**（恢复先于 delegate、序列含三条）都能被注入 bug 抓住；两条**走查项/未列项**（`disable_raw_mode`+`Show`、`PopKeyboardEnhancementFlags`）无单测守护 —— 其中前者合同已声明为走查，后者为真实缺口（P2-1）。

---

## 代码走查结论（重点回答 brief 的三问）

### 1. `install_panic_hook()` 确实在 `setup_terminal()` 之前？

**是。** `run.rs:110` `install_panic_hook();`，`run.rs:111` `let mut terminal = setup_terminal()?;`。且注释（`run.rs:107-109`）说明了理由：`enable_raw_mode()` 已在 setup 内把终端切进 raw mode，半途 panic 也需覆盖。✔

### 2. hook 里没有 `process::exit`、没有 `catch_unwind`？

**是。** `grep -n "process::exit\|catch_unwind" run.rs` 仅命中**注释**（`:518/:521/:522`），无真实调用；全仓 `set_hook`/`take_hook` 唯一真实调用点即 `run.rs:525/526`；无任何 `catch_unwind`。✔ 且 `run()` 由**主线程**调用（`apps/coding-agent/src/main.rs:1031`，紧邻其后的 `rt.shutdown_timeout` + `tui_result?`），主线程 panic 展开到 `main` ⇒ 进程随之结束，**不存在**「UI 线程死了、进程带伤续跑」的路径。✔

### 3. `restore_terminal_on_panic()` 每步真的吞错、不可能二次 panic？—— 重点核查

**是，未发现二次 panic 路径。** 逐行（`run.rs:551-558`）：

```rust
fn restore_terminal_on_panic() {
    let mut out = std::io::stdout();
    let _ = write!(out, "{}", restore_sequence());
    let _ = out.flush();
    let _ = disable_raw_mode();
    let _ = execute!(out, Show);
}
```

- **`write!` / `flush` / `disable_raw_mode` / `execute!` 的返回值**：全部 `let _ =` 吞掉。`sed -n '510,558p' | grep -n "unwrap\|expect\|?"` → **NONE**，无 `.unwrap()` / `.expect()` / 直接 `?` 残留。
- **宏展开**（crossterm 0.29，`Cargo.lock:148` `crossterm 0.29.0`）：`queue!` = `Ok($writer.by_ref())(.and_then(queue)).map(|_| ())`；`execute!` = `queue!(...).and_then(|()| flush())` —— 两者**返回 `Result<(), io::Error>`**，**不含** `unwrap`。故 `let _ =` 安全。`restore_sequence()` 内部的 `queue!` 写的是 `Vec<u8>`（`std::io::Write for Vec<u8>` 的写**永不失败**），`from_utf8_lossy` 亦不 panic。
- **`std::io::stdout()`**：不 panic（`Stdout` 的写走可重入锁，不做 poison 触发）。
- **不触碰 `TuiPrompter` 的 `Mutex`**：故「panic 发生在持有该锁时 ⇒ 二次获取死锁」这条不可能发生。
- **分配**：`restore_sequence()` 有 `Vec`/`String` 分配，但 OOM 在 Rust 是 **abort** 而非 panic，不构成「二次 panic」。
- 净结论：**「在 panic hook 里再 panic ⇒ double-panic abort」这条路径在本实现下无入口。** ✔

### 4. `exit_payload` 抽取后行为逐字节未变？

**是，逐字节未变。** 对比 diff：旧体前段是
```rust
let mut restore: Vec<u8> = Vec::new();
let _ = crossterm::queue!(restore, EnableLineWrap, LeaveAlternateScreen, DisableBracketedPaste, PopKeyboardEnhancementFlags);
let mut out = String::from_utf8_lossy(&restore).into_owned();
```
新体是 `let mut out = restore_sequence();`，而 `restore_sequence()` 的体正是
```rust
let mut restore: Vec<u8> = Vec::new();
let _ = crossterm::queue!(restore, EnableLineWrap, LeaveAlternateScreen, DisableBracketedPaste, PopKeyboardEnhancementFlags);
String::from_utf8_lossy(&restore).into_owned()
```
—— 命令列表、顺序**完全一致**；后段回放循环（`run.rs:497-507`）在 diff 中**未改动**。既有用例 `exit_payload_replays_without_absolute_positioning` / `exit_payload_with_no_lines_still_breaks_the_line` / `exit_payload_starts_with_terminal_restore` 全绿佐证。✔

### 5. `panic_hook_body` 泛型化 `T` 是否引入真实调用路径偏差？

**无偏差。** 真实调用点 `install_panic_hook`（`run.rs:526-528`）：
```rust
let delegate = std::panic::take_hook();          // Box<dyn Fn(&PanicHookInfo<'_>) + Sync + Send + 'static>
std::panic::set_hook(Box::new(move |info| {
    panic_hook_body(restore_terminal_on_panic, &*delegate, info);   // T = PanicHookInfo<'_>
}));
```
`T` 在此被推断为 `PanicHookInfo<'_>`，`info` 原样转交给 `delegate(info)` —— 与「不泛型化、直接写 `restore(); delegate(info);`」的字节语义一致；泛型仅为单测可用假 `T=()`（见 N9 ② 用例）。编译通过（`cargo test`/`clippy` 均绿）。✔

---

## 对实现者自述 5 条的裁定

| # | 自述 | 裁定 | 证据 |
|---|---|---|---|
| 1 | 抽出 `fn restore_sequence() -> String`（`run.rs:473`），`exit_payload` 以它起头 | **属实** | `run.rs:473` 定义；`:495` `let mut out = restore_sequence();` 为该函数首行。 |
| 2 | `install_panic_hook()`（`:524`）先 `take_hook()` 取上游默认 hook 再 `set_hook` | **属实** | `run.rs:524` 定义；`:525` `let delegate = std::panic::take_hook();`；`:526` `std::panic::set_hook(...)`。顺序正确。 |
| 3 | `run()` 入口 `:110` 调 `install_panic_hook()`，在 `setup_terminal()` 之前 | **属实** | `run.rs:110` `install_panic_hook();`；`:111` `let mut terminal = setup_terminal()?;`。 |
| 4 | `panic_hook_body<T>(restore, delegate, info)`（`:537`），先 `restore()` 再 `delegate(info)` | **属实** | `run.rs:537-544`：体为 `restore(); delegate(info);`，顺序与签名一致。 |
| 5 | `restore_terminal_on_panic()`（`:551`）：best-effort 写 `restore_sequence()` → flush → `disable_raw_mode()` → `execute!(out, Show)`，每步吞错；无 `process::exit`、无 `catch_unwind` | **属实** | `run.rs:551-558` 逐句一致；`let _` 吞错四处；无 `process::exit`/`catch_unwind` 真实调用（`grep` 仅命中注释）。 |

---

## 越界改动核查

`git diff 9e0b492 --stat` 显示除 `run.rs` 外还有 `app.rs` / `draw.rs` / `lib.rs` / `Cargo.toml` / `Cargo.lock` / `md.rs`(untracked) / `cache.rs`(untracked) / `styles.rs`(untracked) 等 —— 这些属**其它并发单元**（md 渲染 Task 1–4），**非本单元**。就 U3 而言：

- `run.rs` 的 diff 仅涉及：`use crossterm::cursor::Show`（1 行）、`install_panic_hook()` 调用（4 行）、`restore_sequence()` 抽取、panic 边界小节、2 个新测试 —— **未**引入对 `md`/`cache`/`styles` 的任何依赖（`run.rs` 顶部 `use` 无相关项）。
- 未发现 U3 越界改到 `run.rs` 以外的文件。

## 收尾

- 变异副本 `/tmp/ys-review-u3` 已 `rm -rf`，工作区文件**未被修改**（变异全部在副本内完成）。
- 本报告为唯一新增文件。
