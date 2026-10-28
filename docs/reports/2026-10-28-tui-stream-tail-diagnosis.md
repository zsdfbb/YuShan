# tui-stream-tail-半句 —— 诊断与修复

> 日期：2026-10-28
> 性质：diagnose 产出（复现 → 根因 → 修复 → 回归）。
> 症状：TUI 里某条 assistant 回复只显示半句话，后面直接跟时间/摘要行；`/resume` 之后内容完整。
> 用户补充的症状：**工具调用之间的输出也会只剩半句或几个字**。

## 1. 症状与反馈回路

**复现测试**：`crates/ys-tui-coding/src/layout.rs` 的
`streaming_tail_settles_after_idle_frames`。

它精确复现真实时序（关键在第 2 帧落在节流窗口内）：

1. 条目开始流式（`"第一段完整的"`）；
2. 源文本长完（`"第一段完整的话，后面还有很多内容要展开。"`），
   但**距上次渲染 < `RENDER_THROTTLE`(100ms)**；
3. 空闲，越过节流窗口，源文本**没再变**。

期望：第 3 帧显示完整文本。修复前实际：**永远只显示 `"第一段完整的"`**。

## 2. 已排除的假设

| 假设 | 证伪方式 | 结论 |
|---|---|---|
| 渲染层（`md::render`）丢尾 | 把会话里那段真实文本（1,570 字符）灌进 `md::render` | **否** —— 54 行，尾句完整 |
| 磁盘上的会话内容本身残缺 | 读 `sessions/*.jsonl` | **否** —— 完整；352 条 assistant 里没有半句 |
| 适配器内联思考过滤器吞字 | 读 `inline_thinking.rs` 的 `flush` | **否** —— 流末冲刷保证不吞字 |
| 信道溢出丢事件 | 读 `channel.rs` 的 overflow | **否** —— 满时缓冲不丢；且正文在会话里 |
| 视窗切片裁掉末行 | 读 `layout.rs::window_impl` | **否** —— 装不下就不记 `RowRef`，不产生错觉 |

## 3. 根因：两个缓存用**不同的失效键**，互相把对方的中间态当终态

两个缓存叠在一起：

| 缓存 | 位置 | 键 | 对流式的处理 |
|---|---|---|---|
| `RenderCache`（md 层） | `cache.rs` | `(slot, src.len(), width)` | **有意节流**：增长中且距上次渲染 < 100ms ⇒ 返回**旧的**（更短的）产物 |
| `TranscriptLayout`（折行层） | `layout.rs` | `entry_sig`（assistant 只哈希 `s.len()`） | sig 变了才重渲染 |

失败时序：

| 帧 | 事件 | `RenderCache` | `TranscriptLayout` |
|---|---|---|---|
| 1 | 开始流式 | 渲染「第一段」 | `memo.sig = hash(len=6)` |
| 2 | 增量到达，**距上帧 < 100ms** | **节流 → 返回旧产物** | sig 变了 ⇒ 重渲染，拿到**旧的**行，写回 `memo.sig = hash(len=20)`（源的新长度） |
| 3+ | 空闲 | 节流窗口已过，**本来会**给出完整产物 | `sig` 没变 ⇒ `continue`，**根本不读** |

⇒ 第 3 帧起，`TranscriptLayout` 的 memo 长期保存「哈希是最终长度、内容却只有前半截」的行。
**只要该条还是 transcript 末尾，就永久不刷新。**

### 为什么 `/resume` 后完整

`/resume` → `reset_for_replay()` 会同时清掉两个缓存（`RenderCache::reset` +
`TranscriptLayout::reset`），回放用完整文本重渲染 ⇒ 立刻正常。
**这也是「看起来像渲染 bug 但重启就好」的来源。**

### 为什么工具之间的输出也会半句

同一机制：任一条 assistant 的**最后一个增量落在节流窗口内**，它就带着
「新 sig + 旧产物」停在中间状态。工具调用会追加新条目、间接触发重算，掩盖了一部分；
但流式尾声恰好是工具调用前那段时，它就一直是半句，直到下一个条目出现或 `/resume`。

## 4. 修复

**原则**：`EntryMemo.sig` 的语义必须是「**这份产物对应的源状态**」，而不是「最近一次看到的源状态」。

1. `RenderCache::lines_for` 返回 [`Rendered`]（行 + `stale` 标志）。
   `stale = true` 表示「源比产物长」—— 只可能出现在节流窗口内。
2. `push_transcript_line` / `render_entry` 把 `stale` 透传上来。
3. `TranscriptLayout::sync` 在 `stale` 时把 memo 的 `sig` 写成 `STALE_SIG` 哨兵
   ⇒ **下一帧必然不命中**并重新渲染。

代价是节流窗口内的那一帧多做一次重算；收益是该条目不会永久停在半句话上。

### 为什么不改 `RenderCache` 的节流本身

节流是**有意的**性能设计（`cache.rs` 设计 §2.5）：不节流意味着每个 delta 都整条重解析
markdown。缺陷不在节流，而在**上层把这个中间态当成了终态**。

## 5. 验证

| 项 | 结果 |
|---|---|
| 复现测试（修复前） | **FAILED** —— 渲染出 `"第一段完整的"` |
| 复现测试（修复后） | ok |
| 变异：`layout` 忽略 `stale` | **变红** |
| 变异：`RenderCache` 永不报 `stale` | **变红** |
| 全量测试 | `par-test` 39 个二进制全绿 |
| `cargo clippy --all-targets` | 0 warning |
| `cargo fmt --check` | 干净 |

## 6. 教训

**两层缓存叠加时，各自的失效键必须表达同一件事。** 这里一层按 `(len, width)` 判、
另一层按「长度哈希」判，而其中一层会**故意**返回滞后产物 —— 于是「源最新 + 产物滞后」
这个组合被第二层误读成「已是最新」，永久定住。

**可预防的方向**（未实施，记录备查）：`TranscriptLayout` 其实可以完全依赖
`RenderCache` 的命中判定，不必自己再维护一份 `entry_sig`。两份键会再次漂移。
