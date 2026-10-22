# 0040 — 工具批按「连续只读段」调度（替代全批 all-or-nothing）

## Status

accepted（2026-10-22）

> 设计依据：[`docs/arch/explore-speed/design.md`](../arch/explore-speed/design.md) §3/§4
> （该设计稿早已完成，本 ADR 是它的落地记录）。
> 承接修订：[`ADR-0022`](./0022-subagent-dispatch.md) 决策 11/13/14 关于「批内每一个都安全才并行」的描述。

## 背景

**旧准入是整批 all-or-nothing**（`crates/ys-loop/src/basic.rs` 的 Step 9b）：

```rust
let parallel_ok = pending.len() > 1
    && pending.iter().all(|&i| … .is_concurrency_safe());   // 「每一个」是承重的
```

含一个非安全工具 ⇒ **整批退化串行**。而 `bash` 占全部工具调用的大头
（`explore-speed/research.md` §3.1 实测 83%；本次会话 `1792579023_125340094` 为 68%）
⇒ 绝大多数混合批次**拿不到任何并行**。

**实测收益面**（`research.md` §3.1，212 个有工具调用的轮）：

| 指标 | 值 |
|---|---|
| 每轮 ≥2 个调用 | 165 / 212 = 78% |
| ⇒ 其中拿不到任何并行 | **160 / 165 = 97%** |

`d553740` 给 `read`/`grep`/`find`/`ls` 声明 `is_concurrency_safe() == true` 那一刀，
实战里只覆盖到 **3%** 的多调用轮。

## 决策

把准入从「整批」改为「**连续只读段并行 + 非安全工具作串行屏障**」。

`split_segments()` 把待执行下标切成：

- `Segment::Parallel(Vec<usize>)` —— 一段**连续**的并发安全工具（段内 `buffered(n)` 并发、**保序**）；
- `Segment::Serial(usize)` —— 单个非安全工具，是**串行屏障**（左右两段不得与它重叠）。

**两条承重定义**：

1. **边界定义在 `pending` 上，不是 `tool_calls`** —— 被审批拒绝的工具不进 `pending`
   （它一个字节都没执行）⇒ 不是屏障，也不得把两段只读切碎。
2. **registry 未命中归「非安全」** ⇒ 自成 `Serial` 段，由该分支给 `tool not found` 兜底。
   **不可**把未命中当「不存在所以跨过」：否则 `[read, 错名, read]` 会并发，
   而并行段对「已确认命中」用 `.expect()` ⇒ **直接 panic**。

## 事件形状

`[read, read, bash]` ⇒ `C1,C2,R1,R2,C3,R3`。

三条退化路径**逐字节不变**（这是本方案可安全落地的核心理由）：

| 情形 | 新行为 | 与旧的关系 |
|---|---|---|
| 全安全批 | **单个** `Parallel` 段 | 与旧并行路径**逐字节相同** |
| 全不安全批 | N 个 `Serial` 段 | 与旧串行路径**逐字节相同** |
| 单元素 | `Parallel([i])` ⇒ `C,R` 相邻 | 事件等价 |
| 上限 0 / 1 | `.max(1)` ⇒ 严格逐个 | 与旧一致 |
| **`[read, bash, read]`** | 两段各 1 个元素 ⇒ **不并发** | 新旧事件流**完全相同** |

## 不变量

- **墙钟单调不劣**：新墙钟 = Σ(段内 max) ≤ Σ(全部) = 旧串行。不存在比旧更慢的批次。
- **取消语义不变**：并行段不查 `ctx.boundary`（`buffered` 不因 abort 取消、整批 drain）；
  `Serial` 段保留既有 abort 探针（在 emit `ToolCall` **之前**）。分段**不引入**新的取消盲区。
- **结果顺序**：段内 `buffered` 保序、段间按原序 ⇒ 回喂消息块序不变（9c 零改动）。
- **并发上限仍是单一真值** `DEFAULT_MAX_PARALLEL_TOOLS`：**全批算一次**，段的有效并发 = `min(n, 段长)`。

## 后果

**正面**：

- 混合批次里的连续只读段**真的并发**（`[safe, safe, unsafe]` ⇒ 前两个并发，峰值 2）。
- 三条退化路径逐字节不变 ⇒ 既有测试除一条命名外全部无需改动。

**负面 / 已知代价**：

- **`[read, bash, read]` 仍不并行**（屏障把只读切碎）—— 这是有意为之：
  跨过屏障并行会让写操作与读操作重叠。设计 §3.2 明确记录了这条代价。
- 分段是**纯调度改动**，对模型不可见（不改 `ToolSpec`、不改 prompt）⇒ 收益完全取决于
  模型是否把多个只读调用放进**同一批**。若模型逐轮单发，分段无效。

**未做（明确列为非目标）**：

- **按 bash 命令契约判只读**（`research.md` §9 第 2 条）—— 从命令字符串猜副作用，
  须先单独论证误判代价。
- **拆第二个并发常量**（§3.2 的「已考虑并推迟」）。

## 关联

- [`docs/arch/explore-speed/design.md`](../arch/explore-speed/design.md) —— 完整设计
- [`docs/arch/session-latency/design.md`](../arch/session-latency/design.md) —— C5 条目
- [`ADR-0022`](./0022-subagent-dispatch.md) —— 派发与并行的上游决策
- `crates/ys-loop/src/basic.rs` 的 `split_segments()` 与 Step 9b
