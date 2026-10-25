# persistent-memory — 同类实现调研（Codex）

> 日期：2026-10-25。性质：**跨产品取舍对照**（按 `AGENTS.md` 的文档分工，竞品对照归 `docs/arch/**/research.md`）。
> **方法**：`openai/codex` 仓库源码直读（`main`，经 GitHub API / raw 拉取）+ 本机 `~/.codex/` 实测
> （`codex-cli 0.164.0`）。
> **置信度**：标 `[源]` 的结论读过源码或实测过；未标的是推断。
>
> **状态：仅记录，未定方案。** 本文只回答「Codex 怎么做的」，不做取舍推荐。

---

## 0. 结论速览

| 维度 | Codex 的答案 |
|---|---|
| 归属 | **独立 crate**：`memories/write`、`memories/read`、`ext/memories`；编排在 `core/src/memories/` |
| 写入 | **后台两阶段管线**，会话启动时异步触发；模型**碰不到**记忆正文 |
| 存储 | **双轨**：sqlite（作业/计数）+ `~/.codex/memories/`（文件，且是 git 仓库） |
| 读取 | **注入一段固定操作说明 + 摘要**（上限 2 500 token）+ 4 个只读工具 |
| 「积极」怎么实现 | 提示里的**决策边界文本**（含明确的 skip 清单），**不是**系统每轮自动召回 |
| 「受限」怎么实现 | **append-only note 收件箱** + 后台合并，**不是**审批面 |
| 默认 | **关闭**（需 `Feature::MemoryTool` + `config.memories.use_memories`） |
| 反馈闭环 | 解析 shell 命令统计 `usage_count` → **回流到 Phase 2 排序与衰减** |

---

## 1. 分层 `[源]`

```
codex-rs/memories/write   # 写入：phase1 / phase2 / 提示模板 / 存储 / 限流门禁
codex-rs/memories/read    # 读取：citation 解析 / usage 归类
codex-rs/ext/memories     # 缝：注入片段 + 4 个工具 + 本地后端
codex-rs/core/src/memories/   # 运行时编排（README 自述仍在 core）
```

`memories/README.md` 原话：*"Runtime orchestration for Phase 1 and Phase 2 still lives in `codex-core`"*。
⇒ **crate 边界切的是「纯逻辑」，编排没搬出去** —— 这与本仓「接口留 runtime、实现可外置」的形状一致。

---

## 2. 写入：两阶段 `[源]`

**触发条件**（README §When it runs）：根会话启动 + 非 ephemeral + 功能开启 + **非子 agent 会话** + state DB 可用。
异步后台跑，Phase 1 后 Phase 2。

### Phase 1 — 逐 rollout 抽取（可并行）

- 从 state DB 按启动期认领规则选 rollout：来源合法、在时限窗口内、**空闲够久**
  （避免摘要仍在活动的会话）、未被其他 worker 持有、受扫描/认领上限约束
- 内容过滤成「与记忆相关的 response item」
- 每个 rollout 交给模型（**并行、有并发上限**），要求结构化输出：
  `raw_memory`（详细）/ `rollout_summary`（紧凑）/ `rollout_slug`（可选）
- **脱敏**后写回 `stage1_outputs`
- 认领是 DB lease ⇒ 跨 worker 不重复；失败带退避重试（不热循环）

三种结局：`succeeded` / `succeeded_no_output` / `failed`。

### Phase 2 — 全局合并（串行）

- 先抢**单一全局锁**，保证只有一个 consolidation 在碰记忆根
- 按规则选 top-N 的 stage-1 输出：
  - `last_usage` 超出 `max_unused_days` ⇒ **丢弃**（衰减）
  - 无 `last_usage` 的回落 `generated_at`（让新生成但没被用过的也能入选）
  - 排序：`usage_count` 优先，再 `last_usage` / `generated_at`
- 同步文件：`raw_memories.md`（按 thread-id **升序**，避免 usage 排序抖动）、`rollout_summaries/`
- 清理不再入选的旧 summary；清理过期的 extension 资源
- **用 git diff 判断有没有活干**：工作区无变化 ⇒ 直接标成功退出，**不调模型**
- 有变化才启动**内部 consolidation sub-agent**：无审批、无网络、只写本地、禁 collab（防递归派发）
- 期间心跳续租全局 lease；成功后**重置 git 基线**（diff 文件在重置前删掉）

**为什么拆两阶段**（README 自述）：Phase 1 要在大量 rollout 上横向扩展；Phase 2 要串行化以保证共享产物一致。

---

## 3. 存储：双轨 `[源]`

本机 `~/.codex/memories_1.sqlite` 实测 schema：

| 表 | 作用 |
|---|---|
| `stage1_outputs` | `thread_id` 主键 + `raw_memory` / `rollout_summary` / `rollout_slug` / `usage_count` / `last_usage` / `selected_for_phase2` |
| `jobs` | `(kind, job_key)` 主键 + `status` / `worker_id` / `lease_until` / `retry_at` / watermark |
| `consolidation_progress` | 单行 `max_thread_count` |

文件侧 `~/.codex/memories/`（记忆根，**本身是 git 仓库**，`.git` 由 `codex-git-utils` 初始化）：

| 文件 | 作用 |
|---|---|
| `MEMORY.md` | **可搜索的注册表，模型主要查这个** |
| `memory_summary.md` | 紧凑摘要，**直接注入**，不让模型再打开 |
| `raw_memories.md` | 合并后的原始记忆 |
| `rollout_summaries/` | 每个 rollout 一份 recap（append-only jsonl） |
| `skills/<name>/` | 记忆里沉淀出的 skill 目录 |
| `extensions/ad_hoc/notes/` | **模型唯一能写的落点** |

> **本机实测：`stage1_outputs` 0 行、`memories/` 目录不存在** —— 因为 `~/.codex/config.toml`
> 未开 `use_memories`。⇒ **默认关闭，不是默认开启的隐形功能。**

---

## 4. 读取：注入操作说明 + 工具 `[源]`

### 4.1 注入的不是召回结果，是「怎么查」

`ext/memories/src/lib.rs`：`MEMORY_TOOL_DEVELOPER_INSTRUCTIONS_SUMMARY_TOKEN_LIMIT = 2_500`。
V2 路径按 8 900 字节边界切片注入（`floor_char_boundary`，保 UTF-8）。

模板 `ext/memories/templates/memories/read_path.md`（V1，节选）：

```text
Decision boundary: should you use memory for a new user query?
- Skip memory ONLY when the request is clearly self-contained...
- Hard skip examples: current time/date, simple translation, simple sentence rewrite,
  one-line shell command, trivial formatting.
- Use memory by default when ANY of these are true: ...
- If unsure, do a quick memory pass.

Quick memory pass: 1. Skim MEMORY_SUMMARY 2. Search MEMORY.md 3. 只开 1-2 个最相关文件
Quick-pass budget: ideally <= 4-6 search steps before main work

========= MEMORY_SUMMARY BEGINS =========
{{ memory_summary }}
========= MEMORY_SUMMARY ENDS =========
```

**这是「积极」的真正实现方式**：靠一段带 **skip 清单** 的决策边界，让模型自己判断要不要查 ——
而不是系统每轮自动召回。另有「执行中撞到重复错误/困惑时，**重跑一次 quick pass**」的再入指令。

V2 模板（`read_path_v2.md`）明显收敛，多了两条：
- *"Memory is not proof of current behavior"* —— 按漂移风险 / 验证成本 / 危害决定要不要核实
- *"do not retrieve history speculatively"* —— 抑制无谓检索

### 4.2 工具 `[源]`

`ext/memories/src/lib.rs` 常量：namespace `memories`，四个工具
`list` / `read` / `search` / `add_ad_hoc_note`。

| 限额 | 值 |
|---|---|
| `DEFAULT_LIST_MAX_RESULTS` | 2 000 |
| `DEFAULT_SEARCH_MAX_RESULTS` | 200 |
| `DEFAULT_READ_MAX_TOKENS` | 20 000 |

V1 没有专用工具（`dedicated_tools` 开关），模型用 **shell** 读文件 —— 所以 `read/src/usage.rs`
要**解析 shell 脚本**判断读了哪个记忆文件（`ParsedCommand::Read` / `Search`），遇到 `Unknown` 直接放弃统计。

---

## 5. 写入为什么「受限」：单向收件箱 `[源]`

模板原话：

```text
Updating memories:
You can update the memories **only** when explicitly asked by the user.
- Write your update in .../extensions/ad_hoc/notes/
- Each update must be one small file containing what you want to add/delete/update
- The name of this file must be `<timestamp>-<short slug>.md`
- Do not try to edit the memory files yourself, only add one update note
```

`add_ad_hoc_note` 的 schema 用正则硬约束文件名：
`^\d{4}-\d{2}-\d{2}T\d{2}-\d{2}-\d{2}-[a-z0-9][a-z0-9-]{0,79}\.md$`，长度 24–128。

⇒ **模型只能往一个 append-only 目录丢一个小文件**，真正的 `MEMORY.md` 由 Phase 2 的
consolidation agent 合并。**不需要审批面就能隔离不可信写入** —— 这是结构性隔离，不是策略性拦截。

---

## 6. 反馈闭环：usage → 排序 → 衰减 `[源]`

- `read/src/usage.rs` 把「读了哪个记忆产物」分类成
  `MemoryMd` / `MemorySummary` / `RawMemories` / `RolloutSummaries` / `Skills`，并区分 V1 / V2
- 计数写回 `stage1_outputs.usage_count` / `last_usage`
- **Phase 2 的选择规则直接消费这两个字段** ⇒ 没人用的记忆自动掉出 top-N，进而被 prune

这是整条链路里最值得注意的一点：**记忆库有淘汰机制，且淘汰依据是真实使用而非时间**。

### 6.1 citation 协议 `[源]`

模型若用了记忆，最终回复**末尾**必须附一个块，供程序解析（`read/src/citations.rs`）：

```
<oai-mem-citation>
<citation_entries>
MEMORY.md:234-236|note=[...]
rollout_summaries/xxx.md:10-12|note=[...]
</citation_entries>
<rollout_ids>
019c6e27-e55b-73d1-87d8-4e01f1f75043
</rollout_ids>
</oai-mem-citation>
```

解析器手工 `split_once` 提取 `<citation_entries>` / `<rollout_ids>`，逐行按 `路径:起-止|note=[...]` 解析。
两条硬约束：**不许在 PR 消息里引用**、**不许引空行**。

---

## 7. 容易误读的一处 `[源]`

`memories/write/src/guard.rs` 名字像内容审查，**实际是限流门禁**：
取 Codex 后端 rate limit 快照，剩余配额低于 `config.memories.min_rate_limit_remaining_percent`
就**跳过整个记忆管线**（`"skipping memories startup because Codex rate limits are below the configured threshold"`）。
取不到快照时 `unwrap_or(true)` 放行。

⇒ **是配额经济学，不是安全机制。** 记忆生成不抢用户的模型额度。

---

## 8. 对 YuShan 的三条直接对照

> 只记事实差异，取舍留待 `design.md`。

| 本仓现状 / 约束 | Codex 的做法 | 差异性质 |
|---|---|---|
| `docs/design.md:32` 把「记忆」列为**可选组件**，不进最小核心 | 独立 crate，编排仍在 core | **一致** |
| `docs/arch/prompt-delivery/notes.md`：尾部注入不命中缓存，常量该进稳定前缀 | 注入固定说明 + 摘要，2 500 token 上限 | **一致**（正是「常量进稳定前缀」） |
| 本仓**有意不建 hook 系统**（ADR-0014） | 「积极」靠提示文本，无 hook | **一致** |
| 写入需要审批面（本仓有 `ApprovalHandler` 但产品接 `AutoApprove`） | **不用审批**，用 append-only note 收件箱 | Codex 提供了**不依赖审批**的替代解 |
| skills 扫描根硬编码 `[cwd, home]`，无环境变量（D2′） | 记忆根固定 `~/.codex/memories/`，**全局单层**，不分项目 | Codex **没有项目级作用域** |

**两处不能直接照抄的规模**：Phase 1（`phase1.rs` 27 KB）+ Phase 2（`phase2.rs` 23 KB）+
`startup_tests.rs` 56 KB，配套 sqlite lease、git 基线、sub-agent consolidation。
这套是为「多会话长期积累」设计的，对最小验证是过度工程。

---

## 参考

- `codex-rs/memories/README.md`（管线权威说明）
- `codex-rs/memories/write/src/{phase1,phase2,guard,storage}.rs`
- `codex-rs/memories/read/src/{usage,citations}.rs`
- `codex-rs/ext/memories/src/lib.rs`、`ext/memories/templates/memories/read_path{,_v2}.md`
- 本机实测：`~/.codex/memories_1.sqlite` schema、`codex-cli 0.164.0`
