# TUI Markdown 渲染 — 架构质量分析报告

> 分析时间 2026-10-10。**本报告的每条"事实"都对照过工作区代码**（行号在文中给出），
> 不是对设计文档的自述照抄。

## 分析范围

- **对象**：设计方案（`docs/arch/tui-markdown-render/{design,context,research}.md` + `docs/adr/0030-tui-markdown-render.md`）
- **维度**：全量（可行性 / 可维护性 / 可理解性 / 性能与可靠性）
- **基线**：`crates/ys-tui-coding/` 现存代码（`draw.rs` / `app.rs` / `run.rs` / `transcript.rs` / `Cargo.toml`）
- **注**：设计已按 ADR-0030 落地了**第一步**（ratatui 0.30 升级，commit `87bd1ec`）；其余（`md.rs`/`styles.rs`/`cache.rs`）为待实现。

---

## 各维度判断

| 维度 | 判断 | 关键发现 |
|---|---|---|
| 1.1 技术可实现性 | 🟢 | 调库 + 定色 + 缓存，路径清晰；**但库集成本身尚未编译验证**（见 A2）|
| 1.2 依赖成熟度 | 🟡 | 库自述 PoC、0.3.x API 未冻结；**license 未记录**；`tracing` 被拉入但本仓无 subscriber |
| 1.3 实现周期 | 🟢 | 增量路径明确（§6 八步），第一步已实证 |
| 2.1 模块边界 | 🟢 | `md.rs`/`styles.rs`/`cache.rs` 三文件职责单一；唯 `App` 首次持有**渲染产物**（`RefCell<RenderCache>`）—— 它是 TUI crate 内部类型，不破层 |
| 2.2 接口稳定性 | 🟡 | `lines_for(&mut self) -> &[Line<'static>]` 在 `RefCell` 后面**返回值逃不出 `RefMut` 守卫**，借用纪律未写明（A5）|
| 2.3 错误处理 | 🟡 | 渲染路径**无 panic 边界**；第三方解析库遇畸形输入 panic ⇒ 终端留在 raw mode（A4）|
| 2.4 并发安全 | 🟢 | UI 单线程；`RefCell` 无争用。但**重入即 panic**的不变量未像 `TuiPrompter` 的 `Mutex` 那样写下来 |
| 2.5 资源管理 | 🟡 | 缓存有内存上界；但**流式热路径拿不到缓存收益**，>256 KiB 单条每帧重解析（A3）|
| 3.1 概念一致性 | 🟡 | 三处内部不自洽：样式落点、bound 处数、表格最小宽（B1）|
| 3.2 上手成本 | 🟢 | 「调库 + 一个 `impl StyleSheet` + 缓存」易懂 |
| 4.1 性能模型 | 🔴 | **§4 back-of-envelope 完全没算「每帧解析成本」，而流式正是文本增长时唯一在跑的路径**（A3）|
| 4.2 故障模式 | 🟡 | 表格溢出有守卫（好）；库 panic 无兜底 |
| 4.3 可观测性 | 🟡 | 本层无 metrics/tracing（与既有 TUI 一致，可接受）；新拉入的 `tracing` 无人消费 |

---

## 风险排序

| # | 风险 | 影响 | 可能性 | 优先级 | 建议 |
|---|---|---|---|---|---|
| A1 | **几何常量散落 4 处，设计只点名 1 处；`inner_h` 被完全遗漏** | 高 | 高 | **P0** | 改为从 `Block::inner(area)` 取内区，消灭手动减法 |
| A2 | **「编译实证」的范围被高估** —— `tui-markdown` 尚未进 `Cargo.lock`，库集成零验证 | 高 | 高 | **P0** | 先加依赖跑通 `cargo build`，再宣告 R2 消解 |
| A3 | **流式热路径无缓存收益，且无性能模型** | 中 | 高 | **P1** | 补每帧解析成本估算；必要时对尾部重渲染做节流 |
| A4 | **渲染路径无 panic 边界** —— 库 bug 会把终端留在 raw mode | 高 | 低 | **P1** | `run()` 加 panic hook（恢复终端）或渲染处 `catch_unwind` |
| A5 | `lines_for` 的借用纪律未写明，易写出不编译/每帧多拷贝的代码 | 中 | 中 | P2 | 明确「必须在 `borrow_mut()` 作用域内 `clone` 进 `out`」 |
| B1 | 文档三处不自洽（样式落点 / bound 处数 / 表格最小宽） | 低 | 高 | P2 | 一并改口 |
| B2 | license 未记录；`tracing` 拉入但无 subscriber | 低 | 中 | P2 | 记录 license；说明 `tracing` 是库传递依赖 |

---

## 详细发现

### A1（P0）几何常量散落 4 处，`inner_h` 遗漏 —— **最容易变成真 bug 的一条**

设计 §2.8 只写了「`inner_w` 从 `area.width - 2` 变 `area.width`（`draw.rs:109`）」。实际代码里
**同一个几何约束出现在两个函数、共 4 个常量**：

| 站点 | 现在 | `Borders::TOP` 后应为 |
|---|---|---|
| `draw.rs:96`（`max_scroll_offset`）| `chat.width.saturating_sub(2)` | `chat.width` |
| `draw.rs:97`（`max_scroll_offset`）| `chat.height.saturating_sub(2)` | `chat.height - 1` |
| `draw.rs:109`（`draw_transcript`）| `area.width.saturating_sub(2)` | `area.width` |
| `draw.rs:110`（`draw_transcript`）| `area.height.saturating_sub(2)` | `area.height - 1` |

设计**只提了宽度、只提了 109 一处**：

- `draw.rs:96` 是 `max_scroll_offset` —— 它算**滚动上限**，与 `draw_transcript` 的 `line_count`
  **必须逐字一致**，否则滚动位置与画面失配。这正是 `context.md` 自己强调的「新增样式不得改变字符宽度，
  否则滚动上限与画面失配」的同类问题。
- `inner_h` 从 `-2` 变 `-1`**完全没写**。漏改 ⇒ `follow` 贴底时最后一行被裁、`max_scroll_offset` 差 1 行。

**根因**：`max_scroll_offset` 与 `draw_transcript` 各自手写 `saturating_sub(2)`，重复了 `Block` 的内部几何。
**建议**：不要改常数，改为**从同一个 `Block` 实例取内区**（`block.inner(area)`，`draw.rs:133/327` 已是此写法），
让三处（算滚动、画帧、`line_count`）不可能漂移。这是一处**顺带还债**的机会，成本低于改 4 个常数。

### A2（P0）「编译实证」覆盖的范围 ≠ 设计宣称的范围

`design.md §2.1/§4` 与 ADR-0030 Consequences 都写「**已编译实证**」「R2 已消解」。但实测：

```
$ grep -n "tui-markdown" Cargo.lock crates/ys-tui-coding/Cargo.toml
（无）
```

**`tui-markdown` 根本不在依赖树里。** 那次实证（`cargo build --workspace` + 212 测试）只覆盖了
**ratatui 0.29→0.30 升级 + `run.rs` bound**，**没有覆盖本方案的任何核心动作**：

- 未验证 `tui_markdown::from_str_with_options` 返回的 `Text` 与本仓 `ratatui::text::Text` **是同一类型**
  （依赖 `ratatui` 0.30 与库的 `ratatui-core 0.1` 版本统一——这是全案的技术地基）；
- 未验证 `Options` 的 builder 用法、`table_width` 的真实语义、`StyleSheet` 十余个方法的**实际名字与签名**
  （设计说"约十余个、多为一行"，这些名字是从源码读的，**编译器还没认过**）；
- 未验证 `default-features = false` 后 `uuid`/`termwiz` 确实不出现（那是反对方向，但同属未验证）。

**这不否定方案**（升级那步确实实证了，值得记），但**结论的措辞必须收窄**：当前只能声称
「**前置升级**已实证」，不能声称「本方案已实证」。§4 验收目标①、R2 的"已消解"、ADR 的 ✅ 段都应改口径。

> 另：设计 §2.3 接口、§2.7 覆写清单**全部来自阅读源码**，不是编译结论。落第一步 `cargo build` 时
> 这些是**预期会需要返工**的地方（尤其 `StyleSheet` 的方法名与 `Options` 的 `#[non_exhaustive]`）。

### A3（P1）性能模型缺了「唯一真正在跑的那条路径」

设计 §2.5 论证了缓存键的正确性，§2.4 提了"~20fps 重绘"，但 **§4 back-of-envelope 里没有任何一项是
「每帧解析成本」**。而缓存键含 `src.len()` 意味着：

- **消息 N 流式增长期间，每来一个 delta，缓存键就变 ⇒ 整条消息全量重解析。**
- 也就是说：**缓存对"正在生成的那条消息"收益为零**，它只在消息**结束后**（下次重绘命中）才起作用。
- 单条上限 256 KiB ⇒ 最坏情况每条 delta 后重解析 256 KiB，×20fps ≈ 5 MB/s 的解析吞吐。

对常见长度（几千字节）这没问题；但「长回答 + 表格」正是本轮要支持的场景，也恰好是解析最贵的输入。
**建议**：① 在 §4 补一行「流式期间每帧解析成本 ≈ O(当前消息字节数)」；② 若要上保险，对**尾部那条**
的重渲染做**时间节流**（如 50–100 ms 一次，与 `POLL_INTERVAL` 同量级），而非每个 delta 必算；
③ 用一条**长流式消息**做实测（这比 S4「数百条消息」更贴近风险，S4 测的是已完成的会话）。

> 参考：`research.md §3` 自己记了 `grain-ai-agent-tui` 的 `MarkdownCache`「缓存已完成行、**只重解析流式尾**」
> —— 那正是本方案用整体缓存换掉的、恰恰对热路径最有效的能力。

### A4（P1）渲染路径没有 panic 边界

`run()`（`run.rs:109-113`）的形状是：

```rust
let result = event_loop(...);   // ← 这里 panic
restore_terminal(&mut terminal, &app);   // ← 被 unwind 跳过
result
```

全仓 `grep set_hook|catch_unwind` **零命中**。即：**渲染路径 panic ⇒ 终端留在 raw mode + alt-screen**。

本方案之前，绘制逻辑全是自家代码；本方案**第一次把「模型输出」这种不可信输入喂给第三方解析器**，
而该库自述 *experimental PoC*。库在畸形/嵌套过深输入上 panic 一次，用户就要 `reset` 终端。
设计 §5 的 R1/S5 只覆盖了「不 panic」的**期望**，没有覆盖「万一 panic 了怎么办」的**兜底**。

**建议**（二选一，都很小）：① 在 `run()` 入口装 panic hook，hook 里执行终端恢复（`disable_raw_mode` +
`?1049l`）——这是终端 TUI 的常规做法；② 在 `md::render` 调用处 `catch_unwind`，失败即退回 raw 文本。
①更彻底（覆盖全部绘制路径），且与本仓"恢复无条件发生"的既有精神一致。

### A5（P2）`lines_for` 的借用纪律

设计 §2.3 签名：

```rust
pub fn lines_for(&mut self, idx: usize, src: &str, width: usize, styles: AssistantStyles) -> &[Line<'static>];
```

调用方在 `&App` 上只能经 `app.md_cache.borrow_mut()` 得到 `RefMut`。此时返回的 `&[Line]` **生命周期绑在
`RefMut` 上**，无法存进 `out: Vec<Line>` 之外、也无法跨语句存活。可行的写法只有「在守卫作用域内 clone」：

```rust
{
    let mut c = app.md_cache.borrow_mut();
    out.extend(c.lines_for(i, s, w, styles).iter().cloned());
}
```

能编译，但**每次命中都要 clone 一遍**（缓存省的是解析，不是拷贝），且一旦有人把 `lines_for` 的返回值
带出作用域就会撞上借用检查。**建议**：在 §2.5 明确写下这条纪律（如同 `TuiPrompter` 写下「禁止重入」），
并把返回值设计成显式的 `Ref<'_>`/允许 clone 的形态，避免实现者绕路。

### B1（P2）三处文档不自洽

| # | 位置 | 冲突 |
|---|---|---|
| 1 | `design.md:14` vs `2.2`/`2.7` | §0 item 3 写「样式映射点 = **在 `draw.rs` 写**一个 `impl StyleSheet`」，而 §2.2 模块表与 §2.7 标题都是**新文件 `styles.rs`**（ADR-0030 决策 3 也是 `styles.rs`）。§0 是唯一说 draw.rs 的 |
| 2 | `design.md:13` vs `:73` | §0 item 2 写「`run.rs` **7–8 处** bound」，§2.1 与 ADR 写 **8 处**。实际是 8（`grep` 过）|
| 3 | `design.md:167` | §2.6 称公式"精确、实测"，但 **2 列 ASCII 最小 = 9**（3·2+1+2=9 ✓）与 **2 列 CJK = 10** 对同一公式不自洽（应为 11）。**守卫是"渲染后校验行宽"，不依赖该常数**，所以这是纯文档问题——但「精确」二字会诱使后人把它硬编码成阈值，建议要么订正数字，要么标注「仅作直觉示意，守卫不读它」|

### B2（P2）依赖元数据

- **license 未记录**：`research.md §2.1` 给了体量与元素覆盖，但没有 license 字段（`tui-markdown` 为
  MIT/Apache 双许可，用前应确认一次并记档——尤其本仓是会在意分发条款的场合）。
- **`tracing` 是**库的传递依赖，本仓无 subscriber；§4 把它列入"边际新依赖"没错，但可加一句"本仓不消费"。

---

## 改进建议

### 易修复（低风险、可立刻做）

1. **A1 的替代方案**：`max_scroll_offset` / `draw_transcript` 改为 `block.inner(area)` 取内区，
   顺手消掉 `saturating_sub(2)` 的 4 处重复——比"改对 4 个常数"更省且更防回归。
2. **B1**：§0 改口 `styles.rs`、`7–8` 改 `8`、表格最小宽订正或标注为示意。
3. **B2**：`research.md` 补 license 行。
4. **A2 措辞**：把 §2.1/§4/ADR 的"本方案已实证"收窄为"**前置升级**已实证"，并把"库集成未验证"
   显式列为落地第一步的验收项。

### 需讨论（要你拍板）

5. **A3 节流**：是否对「正在流式增长的那条消息」的重渲染加时间节流？加了会牺牲一点点"最末字立刻成型"
   的实时感（≤100 ms），换来解析成本与帧率解耦。**我倾向加**——`POLL_INTERVAL` 已是 50 ms，
   用户感知不到这点延迟，而它是唯一能防住"超长回答卡顿"的闸。
6. **A4 panic 兜底**：装 panic hook（推荐，覆盖面大）还是只包 `md::render`？

### 架构级（影响面大，需跨 phase）

7. **A2 的实质**：`tui-markdown` 与 `0.3.x API 未冻结` 是本方案最大的长期不确定性。设计已用
   「薄胶水 `md.rs` 隔离、将来可换」缓解——这条**建议在实现时真的做到**：`md.rs` 之外**不得**出现
   `tui_markdown::` 的路径（`styles.rs` 已必须 `impl tui_markdown::StyleSheet`，属不可避免的耦合，
   且集中在两文件内）。可在 CI 加一条 `grep` 断言把这条约束钉住。
8. **依赖方向**：本轮不引入任何违反既有硬约束的东西（仍不依赖 `ys-runtime`、UI 线程仍无 runtime、
   `ys-protocol` 仍零 tokio）。**这一点设计核对无误，无需改动。**

---

## 总体评价

**整体健康度：良好（可进入实现），但"验证"叙事需要收紧。**

方案的技术判断是扎实的：选库而非自研契合「优先成熟库」的既定偏好，升级代价确实只有约 10 行，
表格溢出的**「渲染后校验行宽、越界退回 raw」守卫**是本文档最漂亮的一处设计——它把一个
"库不保证宽度"的模糊风险，转成了不依赖任何魔数常数的**不变量**。文档分层（context/research/design/ADR）
与风险登记（R1–R9 + 变异测试义务）也远高于平均水准。

**最大风险点不是"选错了库"，而是"以为已经验证过了"。** A2 显示：被反复引用的"编译实证"
只覆盖了**前置升级**，而方案的核心（库能否集成、`StyleSheet` 方法名对不对、`Text` 类型统一不统一）
**一行都没编译过**。建议把落地第一步定义为「**加依赖 + 跑通一个最小 `md::render` 编译**」，
之后才更新 R2 的状态。

**次大风险是 A1**：它是典型的"文档说得对、但说得不全"——只点了 `inner_w`、只点了 109 一处，
漏掉 `inner_h` 和 `max_scroll_offset` 里的同源常量。这类遗漏在本仓有过前科（滚动/宽度失配属"血的教训"类），
且**编译器不会抓**（都是合法的 `usize` 减法）。改用 `block.inner()` 可从结构上消灭它。

**推荐下一步**：先付 A1 + B1（纯文档/重构，零风险），再做 A2 的"加依赖跑通最小渲染"，
然后按 §6 顺序推进；A3/A4 在实现 `cache.rs` 与接线 `run.rs` 时一并处理。

---

## 附：本次分析做过的事实核对

| 核对项 | 结论 |
|---|---|
| `draw.rs:109-110` 是否 `-2/-2` | ✅ 属实 |
| `draw.rs:96-97` **另有**同源 `-2/-2` | ⚠️ 设计未提 |
| `draw.rs:125` `Block::bordered()` | ✅ 属实 |
| `draw.rs:541` `transcript_plain_lines` 复用 `build_text_with` | ✅ 属实（故 §2.10 的 raw 改道是必须的；4 个测试调用点会受影响）|
| `/copy` 读原始串 | ✅ `commands.rs:404 copy_text` 直读 `app.transcript` |
| transcript 是否只增不减 | ✅ 全仓无 `drain/remove/truncate/retain/clear/pop`；仅 `push` 与 `last_mut()`（`app.rs:300`）⇒ **§2.5 的「长度相等即内容相同」前提成立** |
| `build_text` 每帧调用次数 | ⚠️ **2 次/帧**（`max_scroll_offset` 一次 + `draw_transcript` 一次）——缓存确有必要，设计结论正确 |
| `run.rs:109→113` panic 时跳过 `restore_terminal` | ✅ 属实；全仓无 panic hook |
| `tui-markdown` 是否已在依赖树 | ❌ **不在**（见 A2）|
