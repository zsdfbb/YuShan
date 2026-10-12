# ADR: 单回合输出规模与可见性（turn-output-and-feedback）

## 状态

提议（2026-10-12）

## 上下文

一个真实会话（`~/.yushan/sessions/1791776859_054505669.jsonl`）暴露两个问题：单条 `grep` 结果
45KB（≈11k token）**无任何提示**；模型返回空 / 纯思考响应时回合被 `BasicLoop` 判成
`StopReason::Completed` + `final_message = None`，**静默收场**（`crates/ys-loop/src/basic.rs:448-469`），
用户连问两次「汇报状态」都拿不到答复。

完整背景、约束与量化验收见 [`context.md`](./context.md)，方案对比见 [`design.md`](./design.md)。

## 方案对比

| 方案 | 描述 | 优点 | 代价 |
|---|---|---|---|
| A 最小复杂度 | 空响应复用 `RunFailed`；软阈值与钳制只写在 `grep.rs` | 改动最小（~15 行） | 「模型空回复」与「基础设施失败」混淆：`-p/--json` 退出码变非零、`stop_reason` 无法表达 |
| B 可扩展优先 | `ys-tool` 建可配置 `ResultBudget` + `AssistantOutcome` + `AgentEvent::TurnNotice` | 抽象完备、易扩展 | 本需求内无真实配置需求 ⇒ 过度设计；新增跨 crate 事件消费者面大 |
| C 性能优先 | 流式字节预算渲染 + `StopReason::NoOutput`，仅靠符号表达 | 渲染内存最小、零额外分配 | 45KB 本就被硬闸门限住，非瓶颈；只给符号信息量不足 |
| **融合（推荐，v2）** | 候选 A 的落点 + 候选 C 的诚实停止原因 + 候选 B 的最小复用点 + **review R1 的「真收窄」** | 诚实、可测、**当轮体积真下降** | `grep` 专用预算会丢部分有用结果；需同步术语与测试矩阵；阈值需校准 |

## 决策

采纳**融合方案**，分两条：

### 1. 空 / 纯思考响应 ⇒ 新终止原因 `StopReason::NoOutput` + 一行可见提示

- `crates/ys-core/src/stop.rs` 加法式新增 `NoOutput`（照 `Truncated` 先例；`#[non_exhaustive]`，下游已有 `_ =>` 兜底）。
- `crates/ys-loop/src/basic.rs:448` 的空分身改判 `NoOutput`（**不再** `Completed`，**也不**复用 `RunFailed`）；
  判定用 `text.trim().is_empty()`，**空白响应也算**。
- `crates/ys-tui-coding/src/app.rs:263` 在 `RunFinished` 臂推**一行可见 `System` 提示**；`draw.rs:605` 补符号。
- `apps/coding-agent/src/main.rs:1166` 的 `single_turn_exit_code`：`NoOutput ⇒ 1` + stderr 一行（**不可静默装作成功**）。

**理由**：本仓 ADR-0020（`Truncated`）已确立「给终止一个诚实的名字」；`RunFailed` 会把它和传输/基础设施失败
混为一谈，破坏 `llm-api-unavailable` 的边界。只给符号不够（用户实测会继续追问），故叠加一行文字。

### 2. 工具输出规模 ⇒ **收窄（主）+ 提示（辅）** + `context` 钳制

- `grep` 设**专用结果预算** `GREP_MAX_RESULT_BYTES = 16 KiB`：超预算即截断，截断 notice **带收窄指引**
  （`adapters/tools-basic/src/grep.rs`）。这是治「一次性读取太多」的**主手段** —— 使**当轮体积**真下降。
- `ys-tool` 增 `SOFT_RESULT_BYTES`（8 KiB）与 `size_notice(...)` 文案助手（`grep`、`read` 复用）；`context` 钳到 `MAX_CONTEXT = 5`。
- `prompt.rs` 的 `tool_guidelines` 加「先窄后宽」一条。

**理由**：`tool-call-efficiency` 约束「不放宽任何输出上限」—— **收紧**合规。单靠提示治不了当轮体积
（`review.md` R1：观察用例 `context=4` 既不触发钳制也不触发 50KB 截断），故引入专用预算；
不引入可配置 `ResultBudget`（本需求内无配置需求），不做流式字节渲染（非瓶颈）。

### 3. 边界：传输层空归 `llm-api-unavailable`，合法空归本需求

- 传输层空（200 + 非 SSE 体 / 零 delta）应为 `Err` → `RunFailed`，**归 `llm-api-unavailable`**（待其落地）。
- 合法空 / 纯思考（`finish_reason = stop`，无正文无工具）⇒ `NoOutput`，**归本需求**。
- **顺序依赖**：上游落地前，传输层空也会显示为 `NoOutput`（好于 `Completed`，但仍需上游接管成错误）。
- 落地时在 `docs/arch/llm-api-unavailable/context.md` 顶部加交叉引用，声明本设计**收窄其场景 4**。

### 4. 范围：第一批只做上述；第二批另计

回合级工具输出总量闸门与空响应自动重试列为**第二批**（`context.md` 已记录），本 ADR 不覆盖。

## 后果

- **正面**：空产出有诚实、可测的终止原因 + 可见提示 + 明确退出码；**大工具结果的当轮体积真下降**（45KB → ≤16 KiB）；两项都能单测 + 变异测。
- **负面 / 代价**：`grep` 专用预算会**丢部分有用结果**（故带收窄指引）；`StopReason` 加变体需同步
  `docs/CONTEXT.md`、`docs/design.md` §5 与 §12；软阈值存在噪声风险；本需求**有意收窄** `llm-api-unavailable` 场景 4，
  需两处交叉引用；`NoOutput` 退出码改为 1 需与其「终止粒度」未决项一并确认。
- **待验证**：阈值（16 KiB / 8 KiB / `MAX_CONTEXT = 5`）、`NoOutput` 命名、`read` 软阈值是否与既有提示重复。

## 关联

- [design.md](./design.md) — 方案与对比矩阵
- [context.md](./context.md) — 上下文、约束、量化验收、复算脚本
- `docs/adr/0020-truncation-guard.md` — `StopReason` 加法式新增先例
- `docs/arch/llm-api-unavailable/context.md` — 边界（场景 4）
- `docs/arch/tool-call-efficiency/context.md` — 「不放宽输出上限」约束出处
