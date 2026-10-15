# explore-speed — 设计：工具批按连续只读段调度

> 日期：2026-10-15
> 性质：架构设计（承接 [`context.md`](./context.md) 的需求范围与 [`research.md`](./research.md) §9 建议 1）。
> **本文只设计**，「为什么该做」的三条论据（实测缺口、同类形状、用户裁定）见那两份。
> 实施后需另开 **ADR-0040** 承接 [`ADR-0022`](../../adr/0022-subagent-dispatch.md) 决策 11/13/14 的修订。

---

## 1. 问题

**准入规则太粗，不是工具声明不够。**

`crates/ys-loop/src/basic.rs:601-608` 的准入是**全批**判定：

```rust
let parallel_ok = pending.len() > 1
    && pending.iter().all(|&i| … .is_concurrency_safe());   // 「每一个」是承重的
```

含一个非安全工具 ⇒ **整批**退化串行。而 `bash` 占全部工具调用的 **83%**。

实测（`research.md` §3.1，`tmp/exp` 15 条事件流 / 212 个有工具调用的轮）：

| 指标 | 值 |
|---|---|
| 每轮 ≥2 个调用的轮 | **165 / 212 = 78%**（模型在积极成批） |
| ⇒ 其中拿不到任何并行的 | **160 / 165 = 97%** |
| `bash` 占全部调用 | 337 / 407 = **83%** |

⇒ `d553740` 给 `read`/`grep`/`find`/`ls` 声明 `is_concurrency_safe() == true` 那一刀，
实战里只覆盖到 **3%** 的多调用轮。

---

## 2. 目标与非目标

**目标**：把准入从「整批」改为「**连续只读段并行 + 非安全工具作串行屏障**」；
把 `DEFAULT_MAX_PARALLEL_TOOLS` 从 4 提到 10。

**非目标**（本轮明确不做）：
- **不**引入「按命令契约判 `bash` 只读」（`research.md` §9 第 2 条 —— 从命令字符串猜副作用，须先单独论证误判代价）。
- **不**拆第二个并发常量。
- **不**动 `Tool` trait 的契约面（`is_concurrency_safe()` 仍是唯一的单工具布尔声明）。

---

## 3. 两处已定的取舍

### 3.1 事件**逐段**发

`[read, read, bash]` ⇒ `C1,C2,R1,R2,C3,R3`。

**理由**：两条退化路径同时逐字节不变 ——
- 全安全批 = **单个** `Parallel` 段 ⇒ `C1..Cn, R1..Rn`，与今日并行路径相同；
- 全不安全批 = N 个单元素 `Serial` 段 ⇒ `C1,R1,C2,R2,…`，与今日串行路径相同。

而「先发全部 `C` 再逐段发 `R`」会让全不安全批变成 `C…C R…R`，**推翻**第二条。

**变更面可精确刻画**：只改「存在一段长度 ≥2 的连续只读段」的批次。
`[read, bash, read]`（只读段长度都是 1）新旧事件流**完全相同**。

### 3.2 上限用**单一常量**改 10

``subagent` 也是 `is_concurrency_safe` 的工具，与只读工具**共用同一个 `buffered(n)` 执行点**。
要给它单独的上限，调度器必须能区分它 —— 而现有唯一信号（`is_concurrency_safe`）区分不了，
只能在调度器里按工具名特例（与项目一贯做法相左），或给 `Tool` trait 加上限方法（动核心契约）。

**接受**单一真值不拆分（ADR-0022 决策 14 的「单一真值 ⇒ 前缀稳定」是承重的），代价是
`apps/coding-agent/src/prompt.rs:283-288` 那句 subagent 槽位文案从 4 变 10。
实测支撑：批次的众数是 2、最大 6，`subagent` 调用在 212 轮里总共 2 次 ⇒ 4→10 几乎从不生效。
「拆成两个常量」列为**已考虑并推迟**，触发条件是实测成本超标。

---

## 4. 调度设计

### 4.1 分段（纯函数，可单测）

新增于 `crates/ys-loop/src/basic.rs`，与 `tool_budget`（`:43-50`）并列：

```rust
/// `Parallel` = 一段**连续**的并发安全工具（段内 `buffered(n)` 并发、保序）；
/// `Serial` = 单个非安全工具，是**串行屏障** —— 它左右两段不得与它重叠。
#[derive(Debug, PartialEq)]
enum Segment { Parallel(Vec<usize>), Serial(usize) }

fn split_segments(pending: &[usize], tool_calls: &[…], registry: &ToolRegistry) -> Vec<Segment>
```

两条**承重**的定义：

1. **边界定义在 `pending` 上，不是 `tool_calls`**。被审批拒绝的工具不进 `pending`（`:588-594` 的
   `continue`），它**一个字节都没执行** ⇒ 不是屏障，也不得把两段只读切碎。
   这与今日 `parallel_ok` 在 `pending` 上算**一致**，不是新增判断。
2. **registry 未命中归「非安全」**（沿用既有 `is_some_and` 语义）⇒ 自成 `Serial` 段，
   由该分支给「tool not found」兜底。**不可**把「未命中」当成「不存在所以跨过」——
   否则 `[read, 错名, read]` 会并发，而错名工具仍需一条错误结果。

### 4.2 Step 9 的替换

删掉 `parallel_ok`（`:603-608`），把 9b（`:609-709`）改为逐段循环，每段三步：
**保序 emit 段内全部 `ToolCall` → 执行 → 保序 emit 全部 `ToolResult`**。

- 并行段：`futures_util::stream::iter(futs).buffered(n)`，`n = ctx.limits.max_parallel_tools.max(1)`
  （与今日 `:645` 同一语义，**全批算一次**；段的有效并发 = `min(n, seg.len())`）。
  `ToolContext` 构造与 `tool_budget` 照抄今日 `:633-643`。
- `Serial` 段：照抄今日串行路径 `:662-708`（含 registry 未命中兜底）。今日 `:648-658` 的
  「先写 `outcomes` 再 emit」两圈可合并成一圈 —— 顺序不可观测（两条路径 emit 失败都直接 `?` 终止回合）。
- **9c（`:711-737`）零改动** —— 它按 `tool_calls` 原序迭代 `outcomes`，与调度无关。
- 连带重写注释 `:569-576`（Step 9 头）、`:601-602`（「每一个是承重的」）、`:660-661`。

### 4.3 边界

| 情形 | 新行为 | 与今日的关系 |
|---|---|---|
| 空批次 | 不可达（`:510-516` 早退） | 等价 |
| 全安全 | **单个** `Parallel` 段 | 与今日并行路径**逐字节相同** |
| 全不安全 | N 个 `Serial` 段 | 与今日串行路径**逐字节相同** |
| 单元素 | `Parallel([i])` / `Serial(i)`，均 `C,R` 相邻 | 事件等价 |
| 上限 0 / 1 | `.max(1)` ⇒ 严格逐个，形状仍是段内 `C…C R…R` | 与今日一致 |

### 4.4 不变量

- **墙钟单调不劣**：新墙钟 = Σ(段内 max) ≤ Σ(全部) = 旧串行。不存在比今日更慢的批次。
- **取消语义不变**：Step 9 期间不查 `ctx.boundary`，`buffered` 不因 abort 取消、必须整批 drain
  （最坏取消延迟 = `tool_budget` 兜底的 300s）。分段**不引入**新的取消盲区。
- **结果顺序**：段内 `buffered` 保序、段间按原序 ⇒ 仍满足「结果按请求顺序」，9c 的回喂消息块序不变。

### 4.5 风险

- **混合批首次出现 `C`/`R` 不相邻**。下游全部按 `ToolCallId` 配对、不要求相邻，但须在 ADR 里
  附消费者清单作为证据：`crates/ys-tui-coding/src/app.rs:247-276`（`tool_index: HashMap`）、
  `crates/ys-tui-coding/src/transcript.rs:116-122,178-181`（全局预扫）、`crates/ys-session/src/jsonl.rs`（原样存取）。
- **`subagent` 参与只读段**（`apps/coding-agent/src/subagent.rs:519` 声明 `true`）⇒
  `[read, subagent]` 由串行变并发。这是 ADR-0022 决策 11 已知的「并行写冲突无人管」缺口**面扩大**，须点名。
- **`bash` 屏障会切碎 subagent 组**：`[sub, sub, bash, sub]` 跑成 `(s1‖s2) → bash → s3`。
  这是**正确**的（屏障语义），但比「忽略屏障」的假想世界慢 —— 相比今日仍是变快。
- **前缀变更 ⇒ prompt cache 一次性失效**（`prompt.rs:283-288` 的字节变了）。
  「前缀稳定」的口径应写成「稳定 = 只由单一真值决定」，而非「永不改」。

---

## 5. 上限常量

`crates/ys-component/src/limits.rs:8`：`DEFAULT_MAX_PARALLEL_TOOLS: usize = 4` → `10`。
`RunLimits::default()`（`:69`）取该常量，自动跟随。

`apps/coding-agent/src/main.rs:522-531` 的 `resolve_limits` **没有**设置该字段 ⇒ 线上恒取默认值（本轮不动）。
`config.json`（`apps/coding-agent/src/config.rs:24-40`）与 env 均**无**覆盖路径，本轮不加。

---

## 6. 测试计划

`crates/ys-loop/src/basic.rs` 的 `mod tests`。`peak` 只观测全批最大值、**分不出「2+2 两段」与「4 一段」**
⇒ 必须加**时间线探针**。

**夹具改造**：`ConcTool`（`:1335-1365`）加 `spans: Arc<Mutex<Vec<CallSpan>>>`，记录每次调用的
`{ name, start, end, started_in_flight }`（`started_in_flight == 1` ⇒ 启动时无人在跑）。
**锁只在单条语句内取，绝不跨 `.await`**（`clippy::await_holding_lock`）。
`conc_tool_registry`（`:1442-1460`）改返回 4 元组，4 个既有调用点补 `, _`。
`MultiToolModel`（`:1368-1427`）**不改** —— 已按 `(id, name)` 对构造，天然支持混合批。

| # | 用例 | 断言要点 |
|---|---|---|
| T1 | `segments_split_only_on_unsafe_tools`（纯函数） | 表驱动：`[s,s,u,s]` ⇒ `[Parallel, Serial, Parallel]`；`[s, u(被拒不在 pending), s]` ⇒ `[Parallel([0,2])]`；未命中的名字 ⇒ 自成 `Serial` |
| T2 | **改写** `mixed_batch_falls_back_to_serial`（`:1527`） | `[a,b safe 200ms, c unsafe 60ms, d,e safe 60ms]`：形状 `C1,C2,R1,R2,C3,R3,C4,C5,R4,R5`；`peak == 2`；`overlaps(a,b)`、`overlaps(d,e)`、`!overlaps(c,…)`；`c.started_in_flight == 1`；墙钟 ≈ 320ms |
| T3 | `alternating_single_reads_keep_the_serial_shape` | `[read, bash, read]` ⇒ `C1,R1,C2,R2,C3,R3`、`peak == 1`、墙钟 ≈ sum。钉「变更面」 |
| T4 | `denied_tool_is_not_a_barrier` | `[a safe, c unsafe(被拒), b safe]` ⇒ 事件 `C1,R1,C3,R3`，**但** `overlaps(a,b)`、`peak == 2`；回喂消息 3 条结果、第 2 条 `is_error` |
| T5 | `single_tool_batch_keeps_adjacent_call_and_result` | 单安全 / 单非安全各一次 ⇒ `C1,R1` |
| T6 | `zero_or_one_max_parallel_tools_run_one_at_a_time` | 上限 0 与 1 ⇒ `peak == 1`、形状仍 `C1,C2,R1,R2`（`.max(1)` 的守卫） |
| T7–T9 | 保留 `:1483` / `:1502` / `:1546`，补变异说明 | 分别守住：非安全屏障无重叠、全安全批 = 单段、上限是**每段**的上限 |
| T10 | `crates/ys-component/src/limits.rs` 新增 | `DEFAULT_MAX_PARALLEL_TOOLS == 10` **且** `RunLimits::default().max_parallel_tools == 10`（引用常量，不写字面量） |
| T12 | `apps/coding-agent/src/toolset.rs:504-532` 改注释 | 断言集合**不变**（本轮不重分类任何工具）；把「批内每一个为 `true` 才走并行」改写为分段模型 |

**每条用例带变异说明注释**（「把 X 改成 Y ⇒ 本用例变红」），中文、只写当前设计。
本批全是纯内存用例（无临时目录、无 env），不需要 `env_lock` / `EnvRestore`。

---

## 7. 文档同步

**7.1 必须改（描述当前设计的真话）**
- `docs/architecture.md:396-398` —— 准入规则
- `docs/design.md:530` —— `BasicLoop` 测试矩阵那格
- `docs/design-final/coding-agent-tui.md:642-643` —— 顺带修掉既有过期说法（「今天只有 `subagent` 覆写为 `true`」早已不成立）
- `crates/ys-tool/src/lib.rs:46-53` —— `is_concurrency_safe` 的**公开契约文档**
- 源码注释：`basic.rs:569-576,601-602,660-661`、`toolset.rs:504-509`、`subagent.rs:516-518`

**7.2 只加顶部注记、正文一字不改（历史快照）**
- `docs/adr/0022-subagent-dispatch.md`（决策 11/13/14）
- `docs/adr/0029-subagent-landing-decisions.md`（`:38-44`、`:113-114`）
- `docs/arch/subagent-rebaseline/design.md`（§4.1 准入 + §4.4 验收 3/6）
- `docs/arch/subagent/design.md`（执行规则；`:832` 那行加 `✅ 已完成（ADR-0040）`）

**7.3 新 ADR — `docs/adr/0040-per-segment-tool-scheduling.md`**（实施时写，体例照 0022/0029）
- 背景：ADR-0022 决策 13 把「只读工具开并发」列为**待一次显式的行为变更评审**的事项 —— 本轮即那次评审。
- 决策：① 分段（§4.1）② 形状（§3.1）③ 上限 4→10（§3.2，含「拆两个常量」的推迟）④ 不做（§2）。
- Consequences：§4.5 四条 + 消费者清单 + 墙钟单调不劣 + 取消语义不变。

**7.4 可选**：`docs/CONTEXT.md` 收录两个本项目特有术语 —— **只读段**（连续 `is_concurrency_safe()` 的工具序列）
与**串行屏障**（非安全工具把批切成互不重叠的段）。

---

## 8. 验证

1. `cargo test -p ys-loop -p ys-component -p ys-tool -p ys-coding-agent`
2. `cargo test --workspace`（基线 1147 passed / 9 ignored）—— 重点看 `toolset.rs` 的准入表与前缀指纹测试
3. **变异验证**（本仓惯例）：手工注入 ① `split_segments` 把 unsafe 也并入段 ② 段内 `buffered(1)`
   ③ 输入从 `pending` 改成 `0..len` —— 确认 T1/T2/T4 各自变红
4. **端到端**：`cargo run -p ys-coding-agent -- --json "读一下 crates/ys-loop 和 docs 的结构"`，
   看事件流是否出现「只读段先 `C…C R…R`、`bash` 单独 `C,R`」的形状
5. **不要跑裸 `cargo fmt`**（本机 rustfmt 与该仓库 CJK 文本有既有分歧，会污染无关文件）
