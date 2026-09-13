# 行式 REPL（rustyline）— 架构上下文

> 目标：**新增行式 REPL 作为交互入口，与现有 ratatui TUI 并存（各为一个 feature），REPL 为默认。**
> 产出本文后进入 `arch-design`。
>
> 前置文档：
> - `../tui-stack-revisit/analysis.md` —— ratatui vs 行式 的存量分析（why）
> - `../tui-stack-revisit/design.md` —— 上一版整体方案（**注意：该文写的是「删除 ui/」，与本轮的「保留并存」冲突，需回改**）
> - `../tui-pi-codewhale-borrow/design.md` —— pi / CodeWhale 借鉴对照
> - `../gap-closure/context.md` —— 产品定位（后台 agent + 简洁 TUI）

---

## 概述

把交互入口从「单一 ratatui 全屏 TUI」改为「**两套并存**」：

```
tui-repl    （默认）行式 REPL —— rustyline 输入，对话进 native scrollback
tui-ratatui （保留）现有全屏 TUI —— 1684 行，不删
```

驱动原因：用户报告的 5 个实际使用问题（详见 `tui-stack-revisit/design.md` §1），其中 **#1「slash 命令时退出 TUI」的根因是 alt-screen**——而 alt-screen 的原始动机（侧栏 / 鼠标 / sticky footer）已被产品决策砍掉或默认关闭。

---

## 现有架构

### 模块边界（`apps/coding-agent/src/`）

**实测行数**（`total / 生产代码`）：

| 模块 | 行数 | UI 专属？ | 被谁用 |
|---|---|---|---|
| `ui/`（5 文件） | 1684 / 1111 | **是**（文件级 feature 门控） | 仅 `main.rs:484` Interactive 分支 |
| `main.rs` | 923 / 510 | 入口 | 三模式共用分发 |
| `wiring.rs` | 372 / 178 | 否 | 三模式都建 `Wiring` |
| `channel.rs` | 666 / 290 | 否 | **三模式都建 `ChannelSink`** |
| `commands/`（mod+builtin） | 1673 / 876 | 否（架构上 UI 无关） | registry 无条件构建（`main.rs:415`），但**只有 `ui/run` 消费** |
| `view.rs`（`AppView`） | 173 / 116 | 否（**零 ratatui 依赖**） | 当前唯一消费者是 `ui/` |
| `format.rs` | 59 / 40 | **是** | 唯一生产消费者 `ui/draw.rs` |
| `status.rs`（`TurnStats`） | 75 / 20 | 否 | 仅 Interactive |
| `config.rs` / `provider.rs` / `state.rs` / `prompt.rs` | 221 / 445 / 142 / 247 | 否 | 三模式共用 |
| **`ansi.rs`** | 105 / 33 | — | **死代码**：生产零消费者 |
| `test_env.rs` | 59 | 测试 | `#[cfg(test)]` |

**关键**：
- **`format.rs` 只有 `ui/draw.rs` 一个消费者** → 客观上是 UI 专属（虽在 `ui/` 外）
- **`AppView` 不依赖 ratatui** → 两套 UI 都能直接复用，无需改动
- **`ansi.rs` 是死代码** → 可顺手删

### 核心抽象

```rust
// ui/mod.rs:48 —— 现有唯一入口，7 个参数
pub async fn run(
    agent: &mut Agent, wiring: &mut Wiring, config: &mut Config,
    commands: &CommandRegistry, stats: &mut TurnStats,
    state_store: &mut StateStore, rx: mpsc::Receiver<Envelope>,
) -> Result<(), Box<dyn std::error::Error>>;
```

REPL 需要**几乎相同的输入集合**，二者签名可保持同构。

### 关键数据流（三模式已共用）

```
parse_args → Mode{Print|Json|Interactive}
   ↓
ChannelSink::new(capacity, StopWhenConsumerGone)     ← 三模式统一
   ↓
Wiring::ephemeral（-p/--json）  |  Wiring::persistent（Interactive）
   ↓
tokio::join!(agent.run(ports, &inbox), 消费者(rx))    ← -p/--json 已是这个形状
```

**可复用性最高的发现**：`consume_print_events`（`main.rs:254-265`，12 行）+ `consume_events`（:185-203，19 行）+ `print_delta_text`（:164-169）+ `is_terminal`（:172-177）**就是行式 REPL 需要的流式打印**——无换行、按序增量写 stdout、终局即返回、`W: Write` 可测。

差别只在 `-p` 是**一次性**（入队一次、join 一次即退出），REPL 需把它包进 `while` 循环。`run_print_mode`（:287-318）已含全部正确性要点（**并发消费防有界信道死锁**、`Agent::run` 驱动 `begin_turn`）。

> ⚠ 这些函数目前是 `main.rs` 私有。**它们是并存方案最该抽出的共享层**——否则 REPL 会复制这段并发/防死锁逻辑。

### 外部依赖

| 依赖 | 用途 | 现状 |
|---|---|---|
| `ratatui` 0.29 | 全屏 TUI | optional，`tui-ratatui` |
| `crossterm` 0.28 | 事件流 + 键盘协议 | optional，`tui-ratatui`（features = `event-stream`） |
| `futures` | `StreamExt` | optional，`tui-ratatui` |
| `inquire` 0.8 | 交互式选择器（`/login` `/model`） | 非 optional，**两套 UI 都要** |
| `rustyline` | 行编辑 | **已删除，需重新引入** |

**已验证可用的 API**（本机 crate 源码）：
- `crossterm::event::PushKeyboardEnhancementFlags` / `PopKeyboardEnhancementFlags` / `KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES`（Shift+Enter 的前提）
- `ratatui::terminal::Viewport::Inline(u16)` + `Terminal::insert_before()`（若将来走 inline 路线）

---

## 约束

### 技术

- **两套 UI 各为一个 feature**：`tui-repl`（默认）/ `tui-ratatui`（保留）
- 现有 `ui/` 5 文件是**文件级** `#![cfg(feature="tui-ratatui")]`；`main.rs:17` 是 item 级 `#[cfg] mod ui;`
- **`--no-default-features` 现状**：Interactive 分支**直接报错**（`main.rs:499-502`），无任何回退
- 契约层零 tokio（ADR-0012）；`apps/*` 可用
- `ModelEventSink` 必须保持同步（动态插件 ABI）
- **`AppView` 快照 + `Prompter` trait 是既有抽象，两套 UI 共用**，不改

### 性能

- 信道有界、容量可配（`YUSHAN_CHANNEL_CAPACITY`，下限 16）
- 行式下消费者在 turn 期间**顺序打印**；打印阻塞时背压经有界信道反作用于 agent（`StopWhenConsumerGone`）

### 演进

- **「ratatui 保留」意味着这不是替换，而是并存** —— 两套都得能编译、能跑、能测
- 关键风险：两套 UI 共享的层（turn 驱动 / 命令桥 / 快照）**必须抽出**，否则会分叉出两份并发/防死锁逻辑

### 组织

- 单人维护 → 优先「低耦合、可独立验证」的切分
- 产品定位：**后台 agent + 简洁 TUI**；富展示已正式砍掉

---

## 需求范围

### 范围内

1. **新增 `tui-repl` feature**（默认）：rustyline 行式 REPL
   - 对话直接写 stdout，进 native scrollback
   - 输入：多行（Shift+Enter / Alt+Enter）、历史、补全、粘贴
   - 状态：turn 中一行 spinner（`\r` 原地刷）+ turn 结束一行摘要（**无常驻状态栏**）
   - slash 命令直接执行（无 suspend/resume）
   - turn 中 `^C` → 真 SIGINT → `cancel_handle()`
2. **保留 `tui-ratatui` feature**：现有 1684 行不动（或仅做必要的最小适配）
3. **抽出共享层**：turn 驱动（`consume_events` 等）+ 命令桥（`CommandContext` 组装 + `execute` + `CommandResult` 处理）
4. **修两个真 bug**（与架构无关，可先做）：
   - `ui/events.rs:131` 中文退格 **panic**
   - 补全 popup **从未渲染**（`CompletionState` 无渲染层）

### 范围外（明确不做）

**产品层红线**（`gap-closure/context.md:10-11` 已正式砍掉）：
- Markdown 渲染、Diff Viewer、Theme、**鼠标**、可配快捷键

**字段层**（`tui-status-display/context.md:111-117` 已否决）
- cache 命中率、cost 美元、context 占比、git branch、thinking level、extension status
- **`tui-pi-codewhale-borrow` §10 想引入的 `ctx% · $cost · ttft · tok/s` 属此类，不得重新引入**（见「矛盾」§D）

**架构层**
- alt-screen / 全屏渲染器（`Frame`/`Buffer`/cell diff）
- 「footer 永远在最后一行」这类 sticky 机制
- skill 系统 / `ys-skill` crate（`tui-pi-codewhale-borrow` 引入，**超出命名方案且非 TUI 范围**）
- Plan/Act/Operate 模式、Subagent UI、Workflow、plugin/marketplace
- 多进程 / daemon 化

### 关键场景

| # | 场景 | 期望 |
|---|---|---|
| S1 | 多轮对话 | 对话按序进 scrollback，可回滚查看、`Cmd+F` 可搜 |
| S2 | 跑 slash 命令（含 `/login` 的 inquire） | 无切屏、无闪烁；命令输出留在 scrollback |
| S3 | turn 进行中 | 文本逐块出现；一行 spinner 原地刷新；不冻结 |
| S4 | turn 中打断 | `^C` → 立即（步骤边界）取消，以 `Cancelled` 收场 |
| S5 | 输入 `/` | 候选列表（含 `arg_hint`） |
| S6 | 输入中文后退格 | 正确删一个**字符**，不 panic |
| S7 | 粘贴多行 | 作为一次输入，不被当多次提交 |
| S8 | 切换 feature 构建 | 两套都能编译、能跑；默认 = REPL |

---

## 必须继承的既有结论

**产品红线**：后台 agent + 简洁 TUI；Markdown / Diff / Theme / 鼠标 / 快捷键**正式砍掉**。

**面板默认关闭**：`show_status` / `show_footer` 默认 `false`；**不做常驻状态栏**。

**取消语义**（ADR-0002）：协作式、**回合边界**，`Ok(RunResult{Cancelled})` 非 Err，最坏延迟 = 单次调用耗时。
> ⚠ 新设计若写「立即中断」需与此对齐措辞。

**`cancel_handle` 保留且自转后更需要**（ADR-0010）。

**数据/显示分离**：`AppView` 快照 + `Prompter` trait 保留。

**`ys-` 命名方案 A 已批准待执行**（`gap-closure/context.md:275-286`）——**注意该方案里没有 `ys-skill`**。

**历史决策链**（避免重复推导）：

| 阶段 | 动机 | 现状 |
|---|---|---|
| 裸 `read_line` | — | 无 footer/补全/Ctrl-C |
| 引入 **rustyline** | 历史 / 补全+ghost text / 多行 / Ctrl-C 免 signal | **4 条动机全部仍然成立** |
| 换成 **ratatui** | alt-screen：sticky footer / 独立滚动区 / 侧栏 / 鼠标 / 富展示 | **5 条里 3 条被砍、1 条默认关闭** |
| **本轮**：加回行式 | ratatui 的理由已消解 | 两套并存 |

---

## 已发现的文档矛盾（新设计须先澄清）

来源：`../tui-pi-codewhale-borrow/design.md`（下称「borrow 文档」）与本轮两份文档的交叉检查。

| # | 矛盾 | 处置建议 |
|---|---|---|
| **A** | borrow §6 说 footer「默认关闭，`/status on` 挂载」，§13.3 说「footer **永远在最后一行**」——后者是 alt-screen 概念，**行式 REPL 物理上做不到** | 去掉 §13.3；footer 只作**瞬时行**（spinner / turn 摘要） |
| **B** | borrow §6 发明 `/status on` 切换挂载，而 origin `design.md:125` 的 `/status` 是**打印多行详情** | 二选一，倾向 origin（无状态 toggle 更轻） |
| **C** | borrow footer 文案含 `esc to interrupt` —— 行式下 **agent 运行期间不在 `readline` 内，Esc 根本捕获不到** | 改为 `Ctrl-C to interrupt` |
| **D** | borrow §10 要引入 `ctx% · $cost · ttft · tok/s` —— **这些字段已被 `tui-status-display` 明确否决**（无基建） | 不引入；若确需，须先记录决策反转 |
| **E** | borrow §10 的 4 色方案 vs `improvements-p1-p2-p3.md:238`「ANSI 颜色 user 决策：暂不引入」 | 需显式决策反转或放弃 |
| **F** | 净收益行数：`analysis.md:134` 说保守 **600-800**，`design.md:78` 说 **-1150**（对应 analysis 的「乐观」） | 统一口径 |
| **G** | borrow §7 P5（强类型 `TranscriptLine::Thinking`）**不解决**用户问题 #3（provider 内联 think 标签）；与 origin P4 语义重叠 | 合并或明确分工 |
| **H** | borrow §8 引入 `ys-skill` crate + `@skill` inline —— 与 `gap-closure` 命名方案冲突，且 §10 明确**不做**同类的 `@file` mention | 拆出本文档 |
| **I** | borrow §11 的分期骨架依赖 **`v1 补遗`（该文档不存在）**，P5–P8 全部悬空 | 补写或删除这四期 |

**验证**：`v1 补遗` 经三重确认不存在（全仓 find / grep 只命中 borrow 文档自身 / `docs/` 下无对应结构）。

---

## 未澄清问题

- [ ] **Q1（共享层放哪）**：turn 驱动 + 命令桥抽到哪个模块？`gap-closure/context.md:257-273` 曾定 **`ys-tui`** 为「纯渲染消费者 crate」（看归 ys-tui、开归接线器），但本轮把 `repl.rs` 放回 `apps/coding-agent`，**从未提及 `ys-tui`**——该既有决策被静默搁置，需明确存废。
- [ ] **Q2（两个 feature 的互斥规则）**：`tui-repl` 与 `tui-ratatui` 同时开启时谁生效？`--no-default-features`（两者都关）时 Interactive 应如何？（现状是**直接报错**）
- [ ] **Q3（`crossterm` 归属）**：REPL 也需要 `crossterm`（键盘增强协议 + 终端尺寸）。它现在挂在 `tui-ratatui` 下，需重新分配 feature。
- [ ] **Q4（`format.rs` 归属）**：它只被 `ui/draw.rs` 用，客观是 UI 专属。REPL 需要**另写** `print_*`（banner / turn 摘要 / `/status`）。是抽公共的 `format` 还是各写各的？
- [ ] **Q5（原型归属与判据）**：三项验证（readline 后是否恢复 cooked mode / turn 中 `select!` 能否取消 / 键盘协议在本机终端是否支持）谁做、放哪、结果记到哪？**部分通过**（尤其第 3 项不支持）时是否仍推进？
- [ ] **Q6（`^C` 的实现位置）**：`^C` 在 turn 中由 `tokio::signal::ctrl_c()` 捕获——但 `readline` 期间 SIGINT 由谁处理？（rustyline 的 `Interrupted`）。两段之间的**边界**需明确，避免「turn 刚开始/刚结束」的窗口漏掉。
- [ ] **Q7（动画节拍）**：spinner 用 `\r` 原地刷需要定时器。用 `tokio::time::interval` 与 `select!` 组合，还是干脆不做动画（只打印一行 `Working…` 然后等）？
- [ ] **Q8（两套 UI 的测试策略）**：`tui-ratatui` 的 ~500 行 TestBackend 断言保留；REPL 用「`W: Write` 快照 + 纯函数」测试。二者是否需要共享测试工具？
- [ ] **Q9（旧 ratatui 资产去留）**：`App.show_status`/`show_footer`/`working_dot`/`exit_document_lines` 等——保留不动（因为 `tui-ratatui` 还在）？确认「不删」的边界。
- [ ] **Q10（`inquire` 与 rustyline 的终端状态交替）**：`inquire` 内部用 raw mode，rustyline 在 `readline` 期间也用 raw mode。二者交替时终端状态如何保证？（origin 只有一句「真终端正常跑」）
- [ ] **Q11（键盘协议的异常清理）**：`PushKeyboardEnhancementFlags` 必须在所有退出路径 `Pop`——用 RAII guard？panic hook？（否则污染用户终端）
- [ ] **Q12（`docs/architecture.md` 与 `gap-closure/context.md` 回改）**：前者仍写 TUI 是「四路 select!」、后者写「TUI 本轮不改」——**均已被推翻**，需同步。

---

## 后续建议

1. **先用 `prototype` 验证 Q5 的三件事**（尤其「readline 返回后是否恢复 cooked mode」——它是整个方案的地基；若不过，收益大打折扣）
2. **再用 `arch-design`** 出方案，重点解 Q1（共享层与 `ys-tui` 存废）、Q2/Q3（feature 切分）、Q4（format 归属）
3. **`borrow 文档` 的 9 处矛盾**（A–I）建议在进入设计前先修文档，尤其是 **I（`v1 补遗` 不存在）**——悬空引用比没有更糟
4. **两个真 bug 可独立先修**（与架构无关）
