# TUI 技术栈重估：ratatui vs 行式 REPL（rustyline）

> 触发：`docs/arch/gap-closure/context.md` 的 Q7（当初记录为「本轮先不替换，只记为待决方向」）。
> 时机：后台那半（事件信道 + 流式 + `-p`/`--json`）已完成，交互入口的取舍不再被阻塞。
> 本文只做分析，不含实现方案。

## 结论

**当初选 ratatui 的理由已经被产品决策消解，而它的代价仍在。倾向换回行式 REPL（rustyline）**，但**先做一次小规模原型**消掉三个不确定项（见 §8），再决定是否整体迁移。

---

## 1. 当年为什么选 ratatui

证据：`docs/arch/ratatui-replace/context.md:24-32` 列举了 rustyline 的「根本性天花板」：

| 不满 | 具体后果 |
|---|---|
| 无 alt-screen | footer / banner / transcript 挤成一行行 |
| 无持久位置 | 长 transcript 时 **footer 滚出可视区** |
| 无独立滚动区 | 历史依赖终端 scrollback |
| 无侧栏/多组件布局 | 只能纯纵向 |
| 无鼠标 | 只有键盘 |

诱因是 `docs/arch/tui-resident-status/context.md:117-118` 已经撞到的真实痛点：**常驻 footer 会被卷上去**（行式输出的通病）。

核心命题（`ratatui-replace/design.md:37`）：**「ratatui 是 renderer，不是 framework」**——只取渲染能力，不用它的 app 模型。

---

## 2. ratatui 的实际代价（实测）

换过去之后，**产生了 5 个修 bug 的 commit**。按「是否为 ratatui 特有」分类：

### 2.1 ratatui / crossterm **特有**（换走能消掉）

| 问题 | commit | 根因 | 代价（实测行） |
|---|---|---|---|
| slash 命令与 alt-screen 冲突（箭头字节裸露、错位） | `ec5e57c` | alt-screen + raw mode 下，`inquire` 自绘终端、自读 stdin，绕过 ratatui | `suspend_terminal` + `resume_terminal` ≈ **43** |
| 退出后 alt-screen **残影** | `aba1f04` | `LeaveAlternateScreen` 只切回主屏、不清 alt buffer，shell 把最后一帧并入 scrollback | `restore_terminal` 改写 + `exit_document_lines` ≈ **30-60** |
| turn 期间**界面冻结**（零刷新） | `aba1f04` | 立即模式必须显式 `draw()`；`dispatch_input().await` 内联在 event_loop 的 events 分支 → 外层 select 整体挂起 | event loop 重写 + 内层三路 select + Working 动画 ≈ **250** |
| crossterm raw mode 下 `tokio::signal::ctrl_c()` **永不触发** | `645e339` | `cfmakeraw` 清 ISIG，`^C` 变成 KeyEvent 而非 SIGINT | 小（删死 arm），但**排错成本高** |

合计约 **350-400 行生产代码**，且是**持续产生 bug 的来源**。

### 2.2 换走也省不掉（**不是 ratatui 的锅**）

| 问题 | 为什么省不掉 |
|---|---|
| turn 中 Ctrl-C 无效 | **rustyline 时代更糟**：`improvements-p1-p2-p3.md:96` 明说「`tui.rs` 不处理 SIGINT，Ctrl-C 直接终止进程，不输出任何提示」 |
| slash 命令决策逻辑不可测（inquire 不可 mock） | 与用哪个 REPL 无关 |
| footer/status 数据 stale、`AppView` 字段膨胀 | 通用状态管理问题，`view_dirty` 早在 rustyline 主循环就存在 |
| `exit`/`quit` 需按两次键 | 通用事件循环顺序 bug |
| `Agent::cancel_handle` | 语义需求，与渲染器无关（**且它已存在**） |

---

## 3. 关键：当初的理由**已被产品决策消解**

这是本次重估的**核心发现**。

`docs/arch/gap-closure/context.md` 已定：

> **产品定位**：YuShan 是**后台 agent**，TUI 展示保持简洁。
> 纯 UI 花活（Markdown 渲染、Diff Viewer、Theme、**鼠标**、可配快捷键）**正式砍掉**。

对照 ratatui 的原始理由：

| 原始理由 | 现状 |
|---|---|
| 侧栏 / 多组件布局 | **status/footer 面板默认关闭**（`ui/app.rs:90-91`：`show_status: false` / `show_footer: false`） |
| 鼠标 | **明确砍掉** |
| 独立滚动区 | 设计未要求；TUI 定位「简洁 coding 界面」 |
| 持久位置（sticky footer） | 面板默认不显示 → 无需 sticky |

**四条理由里有三条被砍、一条被默认关闭。** 而 §2.1 的代价**全部保留**。

一句话：**ratatui 是为一个「不再想要的界面」付的复杂度。**

---

## 4. 澄清一个可能的误解：流式与 turn 取消在行式下**更简单**

一个常见顾虑是「行式 REPL 阻塞在 `readline`，无法边跑边打印」。**这个顾虑不成立**：

```
行式 REPL 的循环是：
    readline()  →  拿到输入  →  agent.run(inbox).await（此时不在 readline 里）
                              ↑ 这一段时间终端是 cooked mode
                →  回到 readline()
```

- **流式打印**：turn 期间**不在 `readline` 内**，直接 `println!`/写 stdout 即可。**不需要 select!、不需要 ticker、不需要帧管理**。
- **turn 中 Ctrl-C**：cooked mode 下 `^C` **就是真 SIGINT** → `tokio::signal::ctrl_c()` 直接可用 → 设 `CancelToken` 取消。

**而这条路当年已经规划过**（`tui-resident-status/adr-tui-resident-status.md:198`）：

> 长 turn 中 Ctrl-C：通过 `tokio::select!` 同时监听 `run_turn` 和 `ctrl_c` 信号——需要独立的 `cancel_handle` 或 `&mut Agent`

**`Agent::cancel_handle` 现在已经存在**，且与渲染器无关。

反过来看 ratatui 路径：因为 raw mode 吃掉了 SIGINT，它**不得不**把取消做成 KeyEvent 路由（`events.rs` 的 Esc/Ctrl-C 分支 + `run_turn_with_ticks` 的 select），复杂度**更高**。

> ⚠ 这一条是本文的**核心论断，也是最大的待验证项**。当年规划了但**从未实现**（随后就切到 ratatui 了）。必须用原型确认（§8）。

---

## 5. 量化：换掉的净收益

`ui/` 共 **1684 行**，按「为何存在」分类（实测）：

| 分类 | 位置 | 行数 | 行式下 |
|---|---|---|---|
| 终端生命周期（alt-screen + raw mode） | `mod.rs:93-174` | 79 | **删** |
| 事件循环 / turn 期并发渲染 | `mod.rs:176-224, 346-490` | 193 | **删** |
| 退出 scrollback 回灌链 | `draw.rs:228-289` | 60 | **删** |
| ratatui 生产渲染 | `draw.rs:10-226` | 210 | **删** |
| crossterm 键处理 + 补全 popup + 滚动 | `events.rs:19-172, 214-220` | 160 | **删** |
| `App` 的 ratatui 专属字段/类型 | `app.rs:42-70` 等 | ~80 | **删** |
| `completion.rs` 空占位 | 全部 | 8 | **删** |
| 三者相关的测试 | `mod.rs` / `draw.rs` / `events.rs` 的 test mod | ~500 | **大部分删** |
| **合计可删** | | **≈1290-1440** | |

**要写回的（抵消项）**：

| 项 | 历史基线 | 说明 |
|---|---|---|
| rustyline 主循环（readline + 分发 + turn 打印 + view 刷新） | ~205 行（`645e339` 删除的 `tui.rs`） | 但需新增流式打印与 SIGINT 处理 |
| `Completer` / `Hinter` / `Helper` | ~211 行（`tui_completer.rs`，含 ~80 行测试） | **逻辑没丢**——现 `events.rs:174-187` 的 `build_cmd_entries`/`complete_inline` 可搬回去 |
| 纯文本输出格式化 | 30-60 行 | `format.rs` 现已缩到 59 行（只剩 `format_tokens`）——`print_banner`/`print_footer`/`print_turn_summary` 当年被删了 |

**净估算**：

- **乐观**（放弃流式、放弃 turn 取消）：净省 ~950-1000 行
- **保守**（**保留**流式 + turn 取消 —— 这是我们的目标）：净省 **~600-800 行**
- **激进**（纯行式，连 transcript 缓冲都不要）：`ui/` 可降到 ~380-450 行

**最大确定性收益来自 `draw.rs` 整体消失**（-570 行，其中 307 行是 TestBackend 测试）。

---

## 6. 建议的形态（若切换）

```
crates/                                          （不变）
adapters/                                        （不变）
apps/coding-agent/src/
  main.rs        模式分发（TUI 分支改为调 repl::run）
  repl.rs        ← 替代 ui/mod.rs：readline 循环 + slash 分发 + turn 驱动
  repl_complete.rs ← 替代 ui/events.rs 的补全部分
  format.rs       恢复 print_banner / print_turn_summary 等纯文本输出
  ui/             ← 删除（1684 行）
```

与现状**保持一致的部分**（不需要改）：

- `Wiring` / `AgentPorts` / `Inbox` —— 完全复用
- `ChannelSink` —— 复用（消费端从「select! 里收」变成「turn 期间顺序打印」）
- `AppView` 快照 —— 复用（`repl.rs` 用它刷新 footer 行）
- `Prompter` trait —— **保留**（inquire 不可 mock 与渲染器无关）

---

## 7. 风险

| 风险 | 说明 |
|---|---|
| **流式打印与 readline 的交替** | 需确认 rustyline `readline()` 返回后**确实恢复 cooked mode**（否则 turn 期间 raw mode 残留，打印与 SIGINT 都会异常）。**这是原型必须验证的第一件事** |
| 历史实现有前科 | 当年的 `tui.rs` 不处理 SIGINT（Ctrl-C 杀进程）。**不能照抄**，必须按 §4 补上 |
| 测试重写 | ~500 行基于 TestBackend buffer 的断言无法保留，需改为「stdout writer 快照 + 纯函数」测试 |
| 行为回退 | Up/Down 从「滚动 transcript」变为「history」（**这其实是净收益**，当前没有 history）；失去「可编程回看历史」 |
| 若将来要富 TUI | 只有 ratatui 一条路。**但这个可能性已被当前定位排除** |

---

## 8. 建议：先做原型，验证三件事

不要直接大改。先写一个**最小原型**（可以放在 `tmp/`，不入仓）验证：

1. **raw mode 交替**：`readline()` 返回后，终端是否回到 cooked mode？turn 期间 `println!` 是否正常？`^C` 是否是 SIGINT？
2. **turn 中取消**：`tokio::select!(agent.run(ports, &inbox), tokio::signal::ctrl_c())` 是否能让 Esc/Ctrl-C 立即中断 turn 并用 `cancel_handle` 收尾？
3. **流式 + 增量打印**：turn 期间从 `ChannelSink` 接收端取事件、逐块 `print!` + `flush`，观感是否可接受（无 Working 动画时是否需要别的进度提示）？

三项全过 → 值得整体迁移；任一项不过 → 重新评估（可能需要「rustyline 输入 + 线程渲染」的混合方案，那会吃掉大部分收益）。

---

## 9. 什么会改变这个结论

| 若发生 | 则 |
|---|---|
| 产品重新需要侧栏 / 富展示 / 鼠标 | **保留 ratatui**（原始理由复活） |
| 原型 §8 的三项有一项不过 | **保留 ratatui**（收益被抵消代价吃掉） |
| 需要「turn 期间可编程回看历史」 | 需重新设计（两者都不直接支持） |
| 前端数量增加（web / IDE） | 与本次决策正交；那时该考虑的是拆进程（见 `context.md` 的 pi 对照） |

---

## 附：一句话总结

**ratatui 是为「侧栏 + sticky footer + 鼠标」选的；这三样现在一个都不想要了，但它带来的 alt-screen 交互冲突、退出残影、raw mode 取消路由、立即模式帧管理全都还在。** 而当年被判定为「rustyline 天花板」的那几条，在「简洁展示」的定位下已不再是需求。
