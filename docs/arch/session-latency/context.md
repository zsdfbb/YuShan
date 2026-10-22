# session-latency — 架构上下文

> 日期：2026-10-22
> 性质：arch-explore 产出（现状 + 约束 + 范围）。**不做方案设计** —— 方案交 `arch-design`。
> 缘起（用户原话）：「考虑昨晚的 session 为什么工作这么慢，有什么改进方向吗？」
>
> **与既有文档的关系**：本文**不重复**以下三份的结论，只补它们没覆盖的一层 ——
> **「一次真实长会话的时间账单」**（既有文档都是「形状」分析，本文是「实测耗时分解」）：
>
> - `docs/arch/explore-speed/`（2026-10-14）—— 只读批并行；其 §先行结论断言
>   「并发已做完，不是 explore 慢的主因，真正的杠杆是轮数」。**本文实测支持该结论**，
>   并给出轮数爆炸的**量化成因**（上下文密度，而非探索欲）。
> - `docs/arch/tool-call-efficiency/`（2026-10-10）—— 少绕弯的调用；其 R1/R2/R3 三条根因
>   本文在真实会话上**复现了 R2**（结构化工具被绕开），但**未复现 R3 的影响面**。
> - `docs/arch/execution-efficiency/` —— 后台命令执行；本文的 subagent 同步阻塞数据与它互补。
>
> **本文的口径**：全部数字来自**本机真实会话** `~/.yushan/sessions/1792579023_125340094.jsonl`
> 与配套 debug 日志 `~/.yushan/logs/debug-1792579023_125340094.jsonl`（1.8 MB / 4819 条）。
> 统计脚本见 §附录。**不是构造语料**，是本仓 2026-10-21 晚的真实开发会话。

---

## 概述

**一个 15.1 小时的真实会话，实际「干活」只有 3.22 小时（21%），其余 11.9 小时是空闲等待。**
干活的那 3.22 小时里，时间分布是 **工具 41% / 生成 30% / 等首 token 28%** ——
**没有一块是「模型本身慢」**。

本文要回答的是：**慢的时间花在哪、哪些杠杆真的能动它、哪些是既有文档已覆盖的。**

**先行结论（详见 §时间账单 与 §杠杆分析）**：

1. **最大的单块不是性能问题，是「不在干活」** —— 11.9 小时的空闲是**交互式 TUI 等待用户输入**
   （用户在 22:35 离开，次日 09:43 才退出），**不是 bug**。但它让「昨晚工作多久」这个
   问题的分母失真 ⇒ 度量口径必须先澄清（§度量口径）。
2. **干活期间的第一大块是工具（41%）**，而其中**可回收的部分是明确的**：
   `bash` 占全部工具调用的 **68%**，其中 **56% 是 grep**（本该用结构化 `grep` 工具）；
   99% 带 `cd <abs> && ` 前缀（纯噪音，每条约 110 字符）。
3. **轮数爆炸的成因是「上下文密度」，不是「探索欲」** —— 687 轮里每次请求的
   `input_tokens` 长期在 **80k–96k**，压缩后 40 轮又涨回去。TTFT 与输入量的
   斜率 **76.7 ms / 1k tokens** ⇒ 上下文每多 20k，单轮多等 1.5 秒。
4. **有一个纯粹的性能缺陷**：15.1 小时的会话里，8 次 429 的退避量级只有 **420–556 ms** ——
   对 429 而言偏短，且是**固定量级**而非指数退避。

---

## 度量口径（先澄清，否则结论会被误读）

「昨晚工作多久」这个问题有**两个不同的答案**，必须分开：

| 口径 | 数值 | 定义 |
|---|---|---|
| **进程存活时长** | **15.10 h** | `run_start` → `run_end` 的 `duration_ms` = 54,369,262 ms |
| **实际执行时长** | **3.22 h** | 21 个 `turn_end` 的 `elapsed_ms` 之和 = 11,594,228 ms |
| **空闲时长** | **11.88 h** | 两者之差 = 42,775 s |

**这 11.88 小时的性质（已核实，不是推测）**：

- 最后一个 `turn_end` 在 **10-21 22:35:25**（seq 4816），其后 `background_tasks`（seq 4817）
  显示 `running: 0`，然后**直到 10-22 09:43:12 才写 `run_end`**（seq 4818）。
- `run_end` 由 `app_loop.rs:346` 的 `flush_debug(..., 0)` 写入，触发条件是
  **`request_rx.recv()` 返回 `None`** —— 即 **UI 侧 drop 了发送端**（用户退出 TUI）。
- 退出路径 `exit_code: 0`、`stop_reason: "RunFinished"` ⇒ **优雅退出，不是崩溃、不是挂死**。
- 会话文件 mtime 为 **22:48:56**，晚于最后一个 `turn_end` ⇒ 退出前还有一次会话写入
  （meta 落盘），**不改变「22:35 后无新回合」的事实**。

⇒ **那 11.9 小时是「用户离开、TUI 空等输入」的正常空闲**。**不是 bug，不该按 bug 修。**

**但由此得出两条对改进方向有决定性影响的推论**：

- **推论 1（对用户）**：一次会话的真实「工作时长」是 3.22 h，不是 15.1 h。
  说「昨晚干了 15 小时」是**度量口径错误**。
- **推论 2（对系统）**：**空��期不产生任何事件**（无 `turn_start`、无 `round_start`），
  所以「本次会话到底慢在哪」**只能靠 debug 日志的 `turn_end.elapsed_ms` 分解**，
  不能靠会话文件（`sessions/*.jsonl` 只有消息，不含耗时）。

**口径的第二个坑**：`run_end.duration_ms`（15.1 h）被当作「会话耗时」直接呈现会误导；
真正该看的是 `turn_end.elapsed_ms` 之和。**任何新的度量/展示都应区分这两个量。**

---

## 时间账单（687 轮 / 21 回合 / 3.22 h 活跃期）

`round_end` 自带细分字段（`ttft_ms` / `gen_ms` / `tools_ms` / `total_ms`），可直接加总，
无需推测：

| 阶段 | 时长 | 占活跃期 | 含义 |
|---|---|---|---|
| **工具执行** | **4,720 s (1.31 h)** | **41.5%** | 本地 I/O + 模型等待 |
| **模型生成** | 3,443 s (0.96 h) | 30.3% | token 流出速度 |
| **等首 token (TTFT)** | 3,211 s (0.89 h) | 28.2% | 上下文大小主导 |
| 其他 | 1 s | 0.0% | — |

**这三条互相咬合，构成一个闭环**：

```
上下文大（80–96k）──► TTFT 长（3.2–5.0 s/轮）──► 每轮贵
      ▲                                              │
      │                                              ▼
   轮数多（687 轮）◄── 每轮要新的工具结果 ──► 工具调用多（818 次）
```

**逐回合的形态（top）**：

| 回合 | 轮数 | 墙钟(s) | TTFT(s) | 生成(s) | 工具(s) | 工具占比 |
|---:|---:|---:|---:|---:|---:|---:|
| 19 | 252 | 2,569 | 1,297 | 760 | 432 | 17% |
| 16 | 60 | 2,458 | 213 | 340 | 1,870 | **77%** |
| 21 | 134 | 1,782 | 727 | 223 | 778 | 45% |
| 12 | 59 | 911 | 238 | 671 | 2 | 0% |
| 15 | 34 | 947 | 131 | 312 | 505 | 53% |
| 2 | 35 | 715 | 129 | 347 | 239 | 33% |

**两个极端形态值得注意**：

- **回合 19（252 轮 / 43 分钟）**：TTFT 占该回合 **50%**（1,297 s / 2,569 s），
  工具只占 17% ⇒ **纯上下文问题**。这是**压缩反复回弹**最严重的一回合
  （见 §上下文膨胀）。
- **回合 16（60 轮 / 41 分钟）**：工具占 **77%**（1,870 s）⇒ **纯工具问题**，
  其中含 3 次 subagent（单次最长 **965 s**）。

⇒ **「慢」不是一个问题，是两个**：`TTFT 型`（上下文大）与 `工具型`（本地动作重）。
**同一个 15 小时的会话里两种形态交替出现**，任何单一杠杆都只能解决一半。

---

## 现状（问题证据）

### A · 上下文膨胀 —— 轮数爆炸的真正成因

**观测 1：每次请求的输入量长期贴着窗口跑。**

`round_usage` 的 `gen_ai.usage.input_tokens` 在 687 轮里的分布：

| 输入量区间 | 轮数 | TTFT 中位 | 耗时中位 |
|---|---:|---:|---:|
| 12,919 – 30,733 | 68 | 3.23 s | 4.89 s |
| 37,882 – 47,331 | 69 | 4.33 s | 8.27 s |
| 74,807 – 82,661 | 69 | **5.00 s** | 6.81 s |
| 82,731 – 96,362 | 69 | 4.45 s | 6.17 s |

**TTFT 与输入量呈正相关**，Theil-Sen 稳健斜率 = **76.7 ms / 1k input tokens**
⇒ **上下文多 20k，单轮多等 ≈1.5 s**。687 轮累计即 **数十分钟**。

**观测 2：压缩在「压下去 → 涨回来」之间循环。**

本次会话 11 次压缩尝试（8 成功 / 3 失败），每次成功后输入量立刻回弹：

| 回合 | 压缩后实际 input | 40 轮后 | 峰值 |
|---:|---:|---:|---:|
| 7 | 18,314 | — | 79,199 |
| 12 | 20,971 | 46,710 | 80,634 |
| 16 | 22,392 | — | 92,739 |
| 19 | 23,796 | 87,492 | 92,443 |
| 21 | 26,485 | — | 96,735 |

**压缩掉了约 70k token，但几十轮内又涨回 90k+** ⇒ 压缩**没有降低**长期平均输入量，
只是**锯齿状地削峰**。

**观测 3：压缩自身的成本被低估。**

每次压缩是一次**独立的模型往返**（`round_usage.purpose == "compaction"`），
且发生在**上下文最满的时候**（90k+ 输入）⇒ **一次压缩 ≈ 一次满载请求**。
本次会话 11 次压缩 ⇒ **11 次满载往返**，纯开销。

**观测 4：压缩失败 3 次，失败原因是「模型没返回文本」。**

`context_compaction_failed` 的 reason 三次都是 `summarization failed: the model returned no text`。
按 `docs/design-final/compaction.md` 的 B-2，这正是「模型回 tool-call（响应无 `Text` 块）」的形态
⇒ **摘要调用本身回了一个 tool call**，被 B-2 正确拦下（历史零副作用）。
但它意味着：**压缩失败的那两轮，模型请求带着完整的 92k 上下文**（观测：`92,307` / `92,739`），
**且压缩要重试**。

**观测 5：缓存命中率高，但救不了 TTFT。**

`run_end.usage`：`input_tokens` 51,477,156，其中 `prompt_cache_hit_tokens` 46,831,794 ⇒
**命中率 91%**。缓存把**成本**压下来了，但 **TTFT 与输入量正相关**这件事不变
⇒ **缓存不能替代上下文治理**。这是本条最容易被误判的地方。

**现状机制定位**（`crates/ys-component/src/compact.rs`）：

```rust
// CompactPolicy::default()
trigger_ratio: 0.85,   // 达到窗口 85% 即压
keep_ratio: 0.10,     // 压后保留 10%
keep_min_ratio: 0.01,
keep_max_ratio: 0.10,
output_reserve: 4096,
```

本次会话窗口 = 128,000（`run_start.limits.context_window`）⇒
`trigger_at` ≈ 108,800、`keep_budget` ≈ **12,800**。

**关键落差**：实际压缩后输入 **20,971–26,485**，而 `keep_budget` 只有 12,800 ——
压缩后还带着 **8k–14k 的「固定开销」**（system prompt + 工具表 + skills）。
按 `docs/design-final/compaction.md` §已知遗留 1，**估算器不计 system prompt / 工具表**
⇒ **系统性低估** ⇒ 真实占用高于估算 ⇒ 压缩刚做完就又逼近阈值。

**观测 6：输出侧也在烧。**

`run_end.usage.output_tokens` = 655,592，其中 `reasoning_tokens` = 388,677 ⇒
**59% 的输出是 reasoning**。而 `run_start` 里 `gen_ai.request.reasoning_effort` = **null**
（未下发），模型是 `minimax-m3.1-flash-free` ⇒ **60% 的输出 token 花在未被显式控制的推理上**。

**⇒ 结论**：轮数爆炸不是「模型爱探索」，而是
**「上下文密度高 → 每轮贵 → 同样的工作要更多轮 → 上下文涨得更快」的正反馈**。

### B · 工具形态 —— 可回收部分明确

**观测 1：`bash` 主导。**

818 次工具调用中：

| 工具 | 次数 | 占比 | 累计耗时 | 中位耗时 | p90 | 最大 |
|---|---:|---:|---:|---:|---:|---:|
| `bash` | 560 | **68%** | 1,207 s | 0.03 s | 5.70 s | 147.2 s |
| `read` | 118 | 14% | ~0 s | 0.00 s | 0.00 s | 0.0 s |
| `edit` | 94 | 11% | 45 s | 0.00 s | 0.01 s | 28.2 s |
| `grep` | 16 | 2% | 2 s | 0.10 s | 0.11 s | 0.1 s |
| `subagent` | 14 | 2% | **3,605 s** | **237.7 s** | 510.5 s | **964.9 s** |
| 其余 | 26 | 3% | ~0 s | — | — | — |

**`read`/`grep` 累计几乎为 0，而 `bash` 累计 1,207 s** ⇒ 慢的动作**全部在 `bash` 里**。

**观测 2：`bash` 里 56% 是 grep。**

560 次 `bash` 中 **316 次命令里含 `grep`**（56%），**554 次以 `cd ` 开头（99%）**。
对照 `docs/arch/tool-call-efficiency/context.md` 的 **R2**：「结构化工具只吃一个 `path`，
多目标只能退回 bash」—— 本次会话**复现了 R2**（模型想一次搜多个目标 ⇒ 退回 bash）。

**观测 3：99% 的 `cd` 前缀是纯噪音。**

ADR-0039 已定「冗余 `cd` 采**条件式事实回显**」（不剥离、不拒绝、不常驻），
本次会话里 `cd` 前缀**仍在 99% 的调用上出现** ⇒ **事实回显机制没有降低前缀率**
（ADR-0039 自己也记了「措辞通道的效应不稳定」）。
每条约 110 字符 ⇒ 560 次 ≈ **6 万字符 ≈ 1.5 万 token 的输入噪音**。

**观测 4：只读批次被整批判定卡住。**

`crates/ys-loop/src/basic.rs:660` 的准入是**全批**判定：

```rust
let parallel_ok = pending.len() > 1
    && pending.iter().all(|&i| … .is_concurrency_safe());   // 「每一个」是承重的
```

本次会话 143 个多调用轮中：

| 形态 | 轮数 | 现状 | 分段并行后 |
|---|---:|---|---|
| ≥2 个只读工具 | **18** | 整批判定 ⇒ `read`/`grep`/`find`/`ls` 全为 true ⇒ **已并行** | 已并行 |
| 只读 + 其它混合 | **21** | **整批退化串行** | 可并行其中只读段 |
| 单调用 | 104 | — | — |

⇒ **`explore-speed` 设计的分段并行（连续只读段并行 + 非安全工具作串行屏障）尚未落地**，
落地后可并行 **21 轮**。**但**：这 21 轮的只读段本身是**毫秒级**（`read` 中位 0.00 s、
`grep` 中位 0.10 s）⇒ **并行它们省不下多少墙钟**。
**这与 `explore-speed/context.md` 的先行结论一致**：「并发已做完，不是 explore 慢的主因」。

**⇒ 结论 A 的重要限定**：`explore-speed` 的分段并行**必须做**（消除 21 轮的退化、且为后续
`bash` 拆分铺路），但**它不是本次「慢」的主因**。真正在工具侧花钱的是
**`bash` 里的长命令**（`cargo test` / `cargo build`），而不是只读工具的串行。

**观测 5：`bash` 里的长命令是什么。**

按命令内容归类 560 次 `bash`：

| 类别 | 次数 |
|---|---:|
| 含 `grep` | 249 |
| 其它 | 199 |
| `cargo test` | 44 |
| `ls`/`find`/`wc` | 25 |
| `cargo build`/`check` | 24 |
| `cargo fmt`/`clippy` | 19 |

`bash` 的 p90 = **5.70 s**、最大 = **147.2 s** ⇒ **长尾是 `cargo` 系命令**
（编译/测试，单次几十秒到两分钟）。回合 16 的 1,870 s 工具时间主要在此。

**观测 6：subagent 同步阻塞。**

14 次 subagent 累计 **3,605 s（60 分钟）**，单次中位 **238 s**、最大 **965 s**。
它们**在回合内同步等待** ⇒ 期间父回合**完全不能推进**。
`docs/arch/execution-efficiency/` 已有后台命令执行机制（`bash_bg`/`bash_poll`），
**但 subagent 没有走这条路**（本次会话 7 次 `bash_bg` + 7 次 `bash_poll` 都是普通命令）。

### C · 压缩与工具的交互 —— 一个隐藏的成本放大器

会话里有 **98 次 `read` 带 `offset`/`limit`**（分块读），其中
`docs/arch/tool-call-efficiency/context.md` 的 R2 指出 `read` 只吃一个 `path`/`offset`/`limit`
⇒ 读一个大文件要 N 次分块 ⇒ **每次分块是一轮工具调用 + 一份回喂进上下文**。

按文件看：`adapters/model-openai-compatible/src/lib.rs` 被分块读 **25 次**、
`crates/ys-loop/src/basic.rs` **17 次**、`app_loop.rs` **11 次**。
⇒ **这些文件的完整内容被反复喂进上下文**，是 §A 上下文膨胀的**直接贡献者**。

---

## 现有架构

### 模块边界

```text
apps/coding-agent
├── app_loop.rs      回合循环 / Request 分发 / 边界 / 计时 / debug 收尾
├── prompt.rs        system prompt 派生（cwd 契约、并发槽位文案、压缩口径）
├── toolset.rs       工具装配 + 「并发准入表」断言
├── subagent.rs      子 agent 派发（同步阻塞）
├── round_clock.rs   逐轮计时状态机
└── debug_log.rs     结构化执行日志（run_start/turn_end/round_end/…）

crates/ys-loop
├── basic.rs         Step 4b 自动压缩 / Step 9 工具执行（整批并行判定）
├── compact.rs       plan() + execute()（压缩算法唯一宿主）
├── token.rs         估算器（不计 system prompt / 工具表 ⇒ 系统性低估）
└── turn_budget.rs   wall-clock 预算

crates/ys-component
└── compact.rs       CompactPolicy（纯数据 + 阈值纯函数）

adapters/tools-basic
├── bash.rs          每次新 shell，current_dir(workspace)
├── read.rs / grep.rs / find.rs / ls.rs   结构化只读工具
└── bg_tools.rs      bash_bg / bash_poll（后台命令）
```

### 核心抽象（与本需求相关的四个）

```rust
// crates/ys-component/src/compact.rs — 压缩策略（纯数据 + 纯函数）
pub struct CompactPolicy {
    pub trigger_ratio: f64,     // 0.85
    pub keep_ratio: f64,        // 0.10
    pub output_reserve: usize,  // 4096
    pub summary_system_prompt: String,
}
```

```rust
// crates/ys-tool/src/lib.rs:53 — 并行准入，默认 false
fn is_concurrency_safe(&self) -> bool { false }
```

```rust
// crates/ys-loop/src/basic.rs:660 — 整批判定（「每一个」承重）
let parallel_ok = pending.len() > 1
    && pending.iter().all(|&i| … .is_concurrency_safe());
```

```rust
// crates/ys-component/src/limits.rs:8 — 并发上限（单一真值）
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 4;
```

### 关键数据流

```text
Request::Prompt
  └─ app_loop::run_turn_once
       ├─ 新建 BoundarySource（ADR-0013：每回合必须新建）
       ├─ Agent::run → BasicLoop
       │    ├─ Step 4b: 阈值 → plan() → execute() → session.replace_all
       │    │            └─ 独立模型往返（purpose="compaction"）
       │    ├─ Step 8: 模型调用 → model_attempt（ttfb_ms / duration_ms）
       │    └─ Step 9: 工具执行（整批并行判定）→ ToolCall / ToolResult
       └─ 回合末排空事件信道 → turn_end（elapsed_ms / cache_hit / cache_miss）
```

### 外部依赖

| 依赖 | 用途 | 版本 | 备注 |
|------|------|------|------|
| `opencode-go` provider | 模型后端 | — | 本次会话用 `minimax-m3.1-flash-free` |
| `ys-loop` | 回合执行 | 内部 | 压缩 / 工具执行 / 预算 |
| `ys-component` | 策略与限额 | 内部 | `CompactPolicy` / `RunLimits` |
| `tools-basic` | 工具实现 | 内部 | `bash` / `read` / `grep` / `find` / `ls` |
| `ys-session` | 会话持久化 | 内部 | JSONL + `replace_all` |

---

## 约束

- **技术**：
  - **`ys-core` / `ys-event` / `ys-protocol` 不含 tokio**（硬性约束）⇒ 任何计时/度量设施
    若要进这三层，得先解决依赖方向。
  - **`ys-tui-coding` 不依赖 `ys-runtime`**（编译器强制）⇒ TUI 侧不能直接读 runtime 状态。
  - **压缩算法唯一宿主是 `ys-loop/src/compact.rs`**；`ys-component` 只放**纯数据 + 阈值纯函数**
    （零依赖、零 async）⇒ 改压缩策略**必须**改 `CompactPolicy`，
    不能在 app 侧私设阈值。
  - **触发判定已计固定开销，但事件口径未计**（⚠️ 修正自 [`review.md`](./review.md) R2）：
    `RunLimits.fixed_prompt_tokens`（`main.rs:1085` `fixed_prompt_tokens()`）**已存在**，
    `crates/ys-loop/src/basic.rs:226-228` **已把它计入压缩触发判定** ⇒
    **「该不该压」不漏算**。但 `compact.rs:259` 的 `estimated_tokens_after` **只数会话消息**
    ⇒ `ContextCompacted` 事件的数字**系统性低估固定开销**
    （实测 8 次压缩一致低估 8,785~11,658 token，均值 ~10,260）
    ⇒ **「压完还剩多少」漏算** ⇒ 这是回弹的真实机理。
  - **`replace_all` 非原子**（先赋值后 flush）⇒ 压缩失败时历史零副作用（B-3）这条性质**必须保住**。
- **性能**：
  - 本次会话实测基线（**可作为改进的对照基准**）：3.22 h 活跃 / 687 轮 / 818 工具调用 /
    51.5M input token / 41.5% 工具时间 / 28.2% TTFT / 30.3% 生成。
  - `bash` p90 = 5.70 s、max = 147.2 s；`subagent` 中位 238 s、max 965 s ⇒ **长尾极重**。
  - TTFT ∝ 输入量（76.7 ms / 1k tokens）⇒ **削减上下文直接削减 TTFT**。
  - 缓存命中 91% ⇒ **成本已低，延迟未低** ⇒ 别把「缓存」当延迟方案。
- **演进**：
  - `explore-speed` 的分段并行设计**已存在**（`docs/arch/explore-speed/design.md`），
    标注「实施后需另开 **ADR-0040** 承接 ADR-0022 决策 11/13/14 的修订」——
    **ADR-0040 目前不存在**（`docs/adr/` 里 0039 之后是 0041）⇒ **该设计未落地，且其 ADR 缺口需补**。
  - `execution-efficiency` 的后台命令机制**已落地**（`bash_bg`/`bash_poll`）⇒
    subagent 异步化可复用其形态，不必新造机制。
  - `CompactPolicy` 的 `keep_ratio` / `trigger_ratio` 是**可配置项**
    （`Config::compact_policy()`，`apps/coding-agent/src/config.rs`）⇒
    调参是**零代码改动**的可行手段，应先于改算法。
- **组织**：
  - 本仓约定：设计 → 实现 → 归档三段走；实现顺序遵循路线图
    （最小闭环 → 可用适配器 → 静态组件生态 → 动态插件 → Coding Agent MVP）。
  - **动态插件有意推迟**（ADR-0021）⇒ 改进必须落在**静态组合**（crate + feature）上。
  - 文档/注释/commit message 用**中文**，代码标识符用英文。
  - **事实性断言须引用 `code/` 或带 `[已实现]`/`[已关闭]` 标记的 `docs/design-final/`**
    ⇒ 本文所有现状断言均已指到具体文件行号或 `design-final` 章节。

---

## 需求范围

### 范围内

- **会话耗时的度量口径**：区分「进程存活」与「实际执行」，并在可见处（`/status`？debug 日志？
  退出行？）给出**实际执行时长**，避免「15 小时会话」的误读（§度量口径）。
- **上下文治理**（对应 §A）：
  - `keep_ratio` / `trigger_ratio` 的**调参**（零代码改动）；
  - 压缩后**回弹抑制**（为什么压完 40 轮又涨回 90k？保留段的构成是什么？）；
  - 估算器**漏计固定开销**（system prompt / 工具表）导致「压完仍近阈值」的修正；
  - 压缩失败（3 次「模型返回 tool call」）的**兜底路径**。
- **工具形态**（对应 §B）：
  - `bash` 里的 **56% grep** → 结构化 `grep` 工具的可用性（多目标 / 多 pattern）；
  - **99% `cd` 前缀**（ADR-0039 事实回显未生效）的再评估；
  - **98 次分块 `read`** 对上下文的贡献；
  - **`cargo` 系长命令**（p90 5.7 s / max 147 s）的形态 —— 编译/测试能否不占回合墙钟
    （`bash_bg` 机制已存在）。
- **subagent 同步阻塞**（3,605 s / 60 min）：改为后台任务 + 轮询，形态对齐 `bash_bg`/`bash_poll`。
- **429 退避量级**（420–556 ms 固定值）：`docs/adr/0028-model-call-retry-and-timeout.md`
  记的重试策略与本次实测的偏差。
- **`explore-speed` 分段并行的落地**（ADR-0040 缺口）：**必须做，但定位为「消除退化」而非「提速」**。

### 范围外（明确不做的）

- **不做「让那 11.9 小时变短」** —— 它是**用户离开时的正常空闲**，不是缺陷。
  只做**度量口径**上的澄清。
- **不做缓存优化** —— 命中率已 91%，成本不是本次瓶颈。
- **不做沙箱 / 权限 / 审批 / 多租户**（项目硬性边界：安全由上层解决）。
- **不做动态插件系统**（ADR-0021 有意推迟）。
- **不引入新的第三方依赖**（`rg`/`fd` 已在工具面里，ADR-0015）。
- **不做 subagent 的语义改造**（角色定义等属 `docs/TODO.md` A4，另案）。
- **不改 `ys-core`/`ys-event`/`ys-protocol` 的 tokio-free 约束**。

### 关键场景

- **场景 1（度量）**：用户在 TUI 里过了 15 小时但只干了 3 小时 ⇒ `/status` 或退出行显示
  「本次会话执行 3.2 h / 共 687 轮」，而不是让用户从时间戳自己推。
- **场景 2（上下文）**：一个长开发会话跑到 90k 输入 ⇒ 压缩后**稳在** 25k 而不是 40 轮后
  涨回 90k；TTFT 从 5.0 s 降到 3.5 s 附近。
- **场景 3（工具）**：模型想一次搜 5 个文件 ⇒ 用结构化 `grep`（多目标）而不是
  `bash` 里串 5 条 `grep` ⇒ 少一轮、少一份回喂。
- **场景 4（长命令）**：模型跑 `cargo test` ⇒ 走 `bash_bg` ⇒ 回合不阻塞，可继续推理，
  之后 `bash_poll` 取结果。
- **场景 5（subagent）**：派一个 10 分钟的子 agent ⇒ 父回合不被冻结 10 分钟，
  而是在后续轮次里 poll 它的产出。
- **场景 6（限流）**：服务端 429 ⇒ 退避量级与「服务商给出的 Retry-After」相称，
  而不是固定 420–556 ms。

---

## 未澄清问题

- [ ] **P0 是否算「慢」** —— 11.9 h 空闲是 TUI 等输入（已核实为正常），
      但**它是否应该计入任何度量**？比如「会话时长」是否该只报执行时长？
- [ ] **`reasoning_effort` 为什么没下发** —— `run_start` 里是 `null`，而
      `AGENTS.md` 记载「默认 `high`，仅 provider 声明支持时随请求发送，当前为 `opencode-go`」。
      本次会话的 provider 记录是 `opencode-go`（session meta）而模型是 `minimax-m3.1-flash-free`
      ⇒ **是 provider 不支持？还是接线漏了？** 这决定「59% 输出是 reasoning」能不能调。
- [ ] **压缩后回弹的主因是「保留段构成」还是「工具结果密度」** —— 需要看
      `plan()` 保留的是哪几条（`kept` 数量：4 / 13 / 12 / 4 / 10 / 20 / 40 / 21）。
      `kept=4`（回合 7、15）与 `kept=40`（回合 19）回弹速度差异明显 ⇒ **保留段策略**是嫌疑点。
- [ ] **`compact` 的 3 次失败是否值得单独兜底** —— 「模型返回 tool call」在
      `basic.rs` 的摘要调用路径上是否应该**强制 `tool_choice: none`**？
      `docs/design-final/compaction.md` §片 2 未做项 K-1 提到「摘要复用活对话前缀 + `tool_choice`」。
- [ ] **`bash` 的 `cd` 前缀该用哪种手段治** —— ADR-0039 选了「事实回显」，
      实测前缀率仍 99% ⇒ **是否要换手段**（工具层剥离？描述层改写？或接受它）？
- [ ] **`cargo` 系命令的形态是否值得单独设计** —— 它占 `bash` 长尾的主体，
      但它**不是探索**，是**构建/验证** ⇒ 与 `execution-efficiency` 的关系待定。
- [ ] **目标是什么量级** —— 「回合墙钟减半」？还是「同等工作的 input token 减半」？
      两个目标的杠杆集合不同（前者偏工具，后者偏上下文）。
- [ ] **`explore-speed` 分段并行是否仍值得单独落地** —— 21 轮可并行、但只读段是毫秒级 ⇒
      **提速收益接近零**。它现在的价值是「为 `bash` 拆分铺路 + 消除退化」，
      是否还值得一个 ADR + 实现？

---

## 后续建议

- **先做零代码改动的调参**：`keep_ratio` / `trigger_ratio` 已在
  `Config::compact_policy()` 可配 ⇒ 用本次会话的实测曲线找「压后不涨回 90k」的参数，
  一次运行即可验证收益。
- **建议用 `arch-design` 做方案设计** —— 候选方向：
  ① 上下文治理（保留段策略 + 固定开销计入估算 + 压缩兜底）；
  ② 工具形态（多目标 `grep` + `cargo` 后台化 + subagent 异步化）；
  ③ 度量口径（执行时长可见化）。
  三者**耦合度低**，可分案推进；其中 ① 的收益最大（TTFT 占 28% 且可单轮验证）。
- **建议用 `prototype` 验证「保留段策略」假设** —— 用本次会话的真实消息序列做输入，
  对比不同 `kept` 策略下 40 轮内的输入量曲线，不需要接模型即可验证回弹机理。
- **先补 ADR-0040**（`explore-speed` 的设计已存在但 ADR 缺失），
  再决定它是否值得实现 —— 若只求提速，收益近零；若作为 ② 的铺路，则应做。
- **补 `TokenLedger`（估算器第二刀）** —— ⚠️ 修正自 [`review.md`](./review.md) R1/R2：
  **已落地**（`fixed_prompt_tokens` + `basic.rs:226` 已接入触发判定）⇒ 实际要做的是
  **修正压缩事件的 token 口径** + 决策 **`keep_budget` 是否含固定开销**。

---

## 附录：统计口径与复现

**数据源**（本机真实产物，非构造语料）：

- `~/.yushan/sessions/1792579023_125340094.jsonl`（313 KB / 271 行）—— 会话消息
- `~/.yushan/logs/debug-1792579023_125340094.jsonl`（1.8 MB / 4819 行）—— 结构化执行日志

**会话元信息**（session meta）：`provider: opencode-go`，`model: minimax-m3.1-flash-free`，
`turns: 21`，`input_tokens: 51,477,156`，`output_tokens: 655,592`。
`run_start`：`context_window: 128,000`，`max_rounds: 4,294,967,295`（不限），
`wall_clock_ms: 172,800,000`（48 h），`skills: {count: 57, budget: 32768}`。

**时间账单的推导链**（每一环都能在 debug 日志里直接读到，无推测）：

| 量 | 来源字段 |
|---|---|
| 进程存活时长 | `run_end.duration_ms` = 54,369,262 ms |
| 实际执行时长 | Σ `turn_end.elapsed_ms` = 11,594,228 ms |
| 阶段分解 | Σ `round_end.{ttft_ms, gen_ms, tools_ms, total_ms}` |
| 上下文量 | `round_usage.gen_ai.usage.input_tokens`（按轮配对） |
| 首字节延迟 | `model_attempt.ttfb_ms`（取该轮 `outcome=="success"` 的那次） |
| 工具耗时 | `tool_result.t_ms − tool_call.t_ms`（按 `tool_call_id` 配对） |
| 压缩行为 | `context_compacted` / `context_compaction_failed` 事件 |
| 限流 | `model_attempt` 中 `outcome != "success"`（8 次 429 + 1 次 transport） |

**TTFT 斜率用 Theil-Sen（稳健）而非最小二乘** —— 最小二乘被 429 重试与 compaction
轮（输入 90k+ 但走不同路径）污染，斜率虚高到 28.4 ms/1k；稳健估计为 **76.7 ms/1k**
（对比：最小二乘 28,400 ms/1k = 28.4 s/1k，**明显不合理**，故取稳健值）。

**已知的方法论边界**（如实记录）：

- **单会话、单一模型、单一仓库**（n=1）⇒ 只能作**定性依据**；
  本文所有结论依赖的是**量级**（工具占 41%、TTFT 占 28%、`bash` 占 68%、`cd` 占 99%），
  不依赖具体比例。
- `round_usage` 有 `turn` 字段而 `model_attempt` **没有** ⇒ 两者按
  `round_start → model_attempt → round_usage → round_end` 的**事件顺序配对**，
  非按 ID 关联。
- `tool_result` 事件**不含工具名** ⇒ 工具耗时经 `tool_call_id` 反查 `tool_call` 事件取得。
