# Coding Agent TUI 设计 — 架构质量分析报告

> 分析日期：2026-09-14
> 对象：`docs/arch/coding-agent-tui/design.md`（路线 B：拆 crate + 分线程 + 信道）
> 基线：`docs/arch/tui-repl/context.md`（已反转）+ 实际代码核实

## 分析范围

- 对象：**设计方案文档**（未实现）
- 维度：全量（可行性 / 可维护性 / 可理解性 / 性能与可靠性）
- 方法：对设计中每处代码级断言**对照实际代码核实**

---

## 0. 先自纠一处（我此前说错的）

> 我在讨论中说过「**`Prompter` 随 inquire 一起作废**」——**这是错的**。

`Prompter`（`commands/mod.rs:52`）的 `select` / `text` **正是"选择器可测"的答案**：

| 实现 | 用途 |
|---|---|
| `InquirePrompter`（现状，`builtin.rs:6`） | 包装 inquire → **死** |
| **TUI Prompt**（新） | TUI 内的浮层选择器 / 输入行 → **活** |
| `FakePrompter`（现状，`builtin.rs:740`） | 测试注入 → **活** |

**死的是 `InquirePrompter`，trait 留着换实现。** 命令决策逻辑照样单测 —— 与设计 §3「测试价值不能丢」一致。

**但 `design.md` 的作废清单没更新这一点**（仍写着 `Prompter trait + InquirePrompter + FakePrompter` 全废）→ 见 R6。

---

## 1. 各维度判断

| 维度 | 判断 | 关键发现 |
|------|------|----------|
| 1.1 技术可实现性 | 🟢 | `ratatui 0.29` / `crossterm 0.28` 已在 workspace；`Block::title_bottom` / `PushKeyboardEnhancementFlags` 均已核实存在 |
| 1.2 依赖成熟度 | 🟢 | 无新增第三方依赖（除 `ys-protocol` / `ys-tui-coding` 两个自有 crate） |
| 1.3 实现周期 | 🟡 | 现有 `select!` 结构要**拆成两个循环**（不是增量改动），周期被低估 |
| 2.1 模块边界 | 🟢 | 依赖单向无环；`ys-tui-coding` **不依赖 `ys-runtime`** 由编译器强制 |
| 2.2 接口稳定性 | 🟡 | `Request` 的变体清单**未定**（阻塞 exec-plan）；命令表在 TUI 侧、能力实现在 app 侧 → **两处要同步**（漏实现是编译错误，可接受，但要知道） |
| 2.3 测试难度 | 🟢 | **比现状好**：TUI 可用假信道单测（不依赖 agent）；命令仍走 `Prompter` 假实现 |
| 2.4 ⚡ 错误处理 | 🔴 | **14 处 `eprintln!` 会冲掉 TUI 屏幕**（见 R2）；`CommandError` 在新拓扑下的传播未设计 |
| 2.5 ⚡ 并发安全 | 🟢 | 2 线程、全信道、无共享可变状态（`Inbox` 若废则锁也没了）；`CancelToken` 移除后无跨线程原子；忙等风险：两条 `recv().await` 都在循环里 |
| 2.6 ⚡ 资源管理 | 🟢 | 信道 drop → `None` 语义（ADR-0009）沿用 |
| 3.1 概念一致性 | 🟡 | `Prompter` 叙述不一致（见 R6）；`inbox` 概念在 B 下已成为死代码（见 R1） |
| 3.2 抽象层次 | 🟢 | `ys-protocol`（能力面）vs `ys-tui-coding`（视图与渲染）分层清楚 |
| 3.3 文档完整度 | 🟡 | `design.md` 与 `tui-repl/context.md` 已部分互相矛盾；后者需归档 |
| 4.1 性能模型 | 🟢 | 双线程消掉"长回合冻结"（虽然现状 `select!` 也已交错）；信道开销可忽略（`gap-closure` 已算过 0.02% CPU） |
| 4.2 可观测性 | 🟢 | `ChannelStats`（背压）保留 |

---

## 2. 风险排序

| # | 风险 | 影响 | 可能性 | 优先级 | 说明 |
|---|------|------|--------|--------|------|
| **R1** | `inbox` 语义在路线 B 下**已被信道取代**，但设计未点明 | 中 | **确定** | **P1** | `Message` 级的路由（新输入 vs 插话）由 `Request::Prompt` / `Boundary::Steer` 承担；`Inbox` 的 two-vec + `Intent` + `QueueMode` **全部多余**。若不删，同一语义两套实现并存。**注意**：`envelope.turn` 的「保留」理由（消费者不必自己数）在 `Message` 级同样已被取代（看终局事件即可）。`BaseLoop` 的 `[Context Summary]` 合成机制**独立于 inbox，保留** |
| **R2** | **14 处生产 `eprintln!` 会冲掉 TUI 屏幕** | 高 | **确定** | **P1** | `main.rs`(8) / `builtin.rs`(4) / `state.rs`(1) / `provider.rs`(1)。alt-screen 下直接写 stderr/stdout 会撕裂画面。新设计里**命令在 app 侧执行、输出走 `Outbound::CommandOutput`** —— 这 14 处**必须一起改**。**设计文档完全没提** |
| **R3** | `Request` 的**变体清单未定** | 高 | 高 | **P1** | 它决定 `ys-protocol` 的公开面、TUI 命令表的形状、app 侧要实现哪些能力。**提案**：`Prompt(Message)` / `SetModel` / `Login` / `Logout` / `NewSession` / `Compact` / `Export`（`/help` `/status` 是 TUI 本地；`/copy` 待定——TUI 手里有最后一条回复） |
| **R4** | **接线器 app 侧循环未设计** | 中 | 高 | **P2** | 它是现有 `select!`（`ui/mod.rs`）的**重写**，不是增量。要点：何时 `recv` ①（空闲才收）、如何驱动 `turn_fut`、如何把事件转成 `Outbound`、`CommandError` 怎么回给 UI |
| **R5** | 测试资产处置未明 | 中 | 高 | **P2** | `Prompter` 的 `Fake` **保留**；`ui/` 的 ~500 行 `TestBackend` 断言**重写**；`ys-channel` 的 15 个单测随 `Inbox` 废而**作废**；协议的 contract tests 需要新落点（Rust 侧，不是 TypeScript） |
| **R6** | 文档一致性 | 低 | **确定** | **P2** | `design.md` §3 的作废清单含 `Prompter`（**该保留**）；`tui-repl/context.md` 整份已反转、与本文矛盾 |
| **R7** | 阻塞提前返回可能丢 `begin_turn` | 低 | 低 | **P3** | `Wiring` 若在建 `ChannelSink` 前因 app 侧阻塞而早退，`begin_turn` 不会发生 → `turn` 恒 0。属实现边角 |

---

## 3. 改进建议

### 易修复（低风险）

1. **R6**：更新 `design.md` §3 的作废清单 —— `Prompter` 改为「**保留 trait，换 TUI 实现**」；`InquirePrompter` 才是死的
2. **R6**：把 `tui-repl/context.md` 的「存活」结论并入本文后**整份标注作废**（避免两份文档互相矛盾）

### 需讨论（需决策）

3. **R3**：定 `Request` 的变体清单（P1，阻塞 exec-plan）
4. **R5**：确认测试资产的处置范围（尤其"旧 `ui/` 的 ~500 行断言全重写"是否接受）

### 架构级（影响面大）

5. **R2（P1）**：**`eprintln!` 的三个去处**——这是新拓扑下的真问题，必须现在定：

   | 类别 | 例子 | 去处 |
   |---|---|---|
   | **命令的输出** | `/login` 的 "✓ Logged in to …" | `Outbound::CommandOutput` → UI 进 transcript |
   | **配置/凭证的警告** | "Warning: Could not persist credentials" | 同上（用户该看到） |
   | **真正的进程级诊断** | `main.rs` 的启动错误 | **启动期**（TUI 未起）可以直写；**运行期**必须走信道 |
   | **库内部诊断**（`provider.rs` / `state.rs`） | "Warning: failed to parse auth.json" | 需要一条**通用诊断通道**，或收敛为返回值由调用方决定 |

   **倾向**：`Outbound` 加一个 `Diagnostic { level, text }` 变体 —— 通用、免得上游各自决定往哪写。

6. **R1（P1）**：明确 `Inbox` 废除的**连带范围** —— `ys-channel` 的 `Inbox`/`Intent`/`QueueMode` 废；`Envelope`/`LifecyclePolicy` 迁入 `ys-protocol`；`ys-component` 的 `RuntimeContext.inbox` 改为 `BoundarySource`（trait，零 tokio）；`BasicLoop` 轮边界改写；15 个单测作废

---

## 4. 总体评价

**整体健康度：良好。** 核心架构（拆 crate + 分线程 + 三条信道 + 视图归产品）**自洽且有编译器强制**，比现状的"靠自觉"强；测试性**明确改善**（TUI 可脱离 agent 单测）。

**最大风险是 R2** —— 它是设计**完全遗漏**的一块：命令从 TUI 挪到 app 执行后，**所有原本直写终端的输出都失去了去处**，而这类输出有 14 处。这不是"实现细节"，是拓扑决定的**必答题**。

**R1 是第二个必答题** —— 但它其实是**好事**：路线 B 让 `Inbox` 彻底多余，删掉它是简化而非损失。

**R3/R4 属"设计未完成"**（不是"设计有误"）：`Request` 变体与 app 侧循环都需要在设计阶段补出形状，否则 exec-plan 写不出来。

**推荐下一步**：
1. 先解 **R2**（`eprintln!` 的去处）与 **R3**（`Request` 变体）—— 两条都是 P1 且相互关联（能力清单决定输出形态）
2. 再补 **R4**（app 侧循环）的形状
3. 然后写 exec-plan

**一句话**：**结构选对了，但"命令搬去 app 执行"的连带影响（输出、错误、诊断的通道）还没设计** —— 这是设计还差的一步。
