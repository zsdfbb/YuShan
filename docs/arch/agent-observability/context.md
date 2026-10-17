# agent-observability — 架构上下文

> 日期：2026-10-17
> 性质：`arch-explore` 产出（现状 + 约束 + 范围）。**不做方案设计** —— 方案交 `arch-design`。
> 缘起（用户原话）：「先提高 agent 执行过程的可观测性，每次执行在启动 debug 选项时要输出充分的
> 日志信息，提供给问题定位、性能分析等。」

---

## 概述

给 `ys-coding-agent` 加**一个显式开关的、结构化的执行日志**：不开时一个字节不多写，
开时每一轮、每一次工具调用、每一次模型交互、每一次内部状态变化都留下**带时间戳、可机器聚合**的记录。

它要回答的是两类问题：

- **问题定位**：「这次为什么失败 / 为什么慢 / 卡在哪」
- **性能分析**：「墙钟花在哪一段」「重试了几次」「缓存命中多少」「压缩何时触发」

**直接动因（本轮实测暴露的盲区）**：

| 现象 | 现状能不能解释 |
|---|---|
| 33 轮里 3 个离群轮吃掉 36% 墙钟 | ❌ 只知道"这一轮一共 N 秒" |
| 一轮里单个 `bash` 吃掉 **73.7 秒** | ✅ 逐调用计时已就位（本轮刚加） |
| `gen` 某次 46 s、某次 **143 s** | ❌ **思考与正文混在一起**，无法归因 |
| 同一提示墙钟 75 s vs 196 s | ❌ 无法回答「差在哪一层」 |
| 缓存命中率 | ✅ 已修字段方言后可读 |

---

## 现有架构

### 模块边界与现有可观测面

| 面 | 内容 | 去处 | 是否开关 |
|---|---|---|---|
| **日志文件** `apps/coding-agent/src/logging.rs` | 35 个调用点：config/auth 解析失败、model refresh、回合失败、**逐轮 `round=` / 回合 `turn=` / 逐调用 `tools round=`** | `$YUSHAN_LOG_DIR/yushan.log`（默认 `~/.yushan/logs/`） | ❌ 恒开 |
| **`--json`** | 逐行 `Envelope`（完整事件流） | stdout | ✅ 模式选择 |
| **`--stats`** | 信道背压读数（`channel.rs`） | stderr（仅非 TUI） | ✅ |
| **启动期** | 容量 clamp、`No model configured` | stderr | ❌ |
| **退出时** | `session: <id>` | stderr（ADR-0032） | ❌ |

### 核心抽象（本需求相关的三个）

```rust
// apps/coding-agent/src/logging.rs —— 手写极简 logger
pub fn log(msg: &str);                       // 追加到默认路径，best-effort
pub fn log_to(path: &Path, msg: &str);       // 指定路径
pub const LOG_DIR_ENV: &str = "YUSHAN_LOG_DIR";   // 目录覆盖

// apps/coding-agent/src/round_clock.rs —— 逐轮/逐调用计时（本轮新增逐调用）
struct RoundClock {
    // 三段：ttft（排队+prefill）/ gen（首个可观测输出→生成终点）/ tools（首末 ToolResult）
    // 逐调用：in_flight: HashMap<ToolCallId, (name, Instant)> → calls: Vec<(name, ms)>
}

// crates/ys-event/src/event.rs —— 事件流（天然的观测载体，`--json` 已在用）
enum AgentEvent { UserMessage, RoundStarted, ModelTextDelta, ModelThinkingDelta,
                  ToolCall, ToolResult, RunFinished, RunFailed,
                  ContextCompacted, ContextCompactionFailed, ResponseTruncated }
```

### 关键数据流

```text
ys-loop 产出 AgentEvent ──► ChannelSink ──► 消费侧（两条入口）
                                              ├─ TUI: app_loop 的事件泵
                                              └─ -p/--json: main::consume_events
                                                    └─ RoundClock 在这里计时 → logging::log
```

**两条入口都经过 `RoundClock`**（本轮刚统一），故新增的逐轮/逐调用读数天然覆盖两条路径。

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| 无新增 crate | 日志器手写、JSON 序列化已有 `serde_json` | `docs/architecture.md` §13：**不引新依赖** |
| `~/.yushan/logs/` | 落盘目录 | `YUSHAN_LOG_DIR` 可覆盖（测试/基准用） |

---

## 约束

### 技术

- **不引新依赖**（`docs/architecture.md` §13 明文）。JSONL 用现有的 `serde_json` 即可。
- **日志器必须 best-effort**：任何 IO 错误静默吞掉，返回 `()`，不 panic、不传播
  （`logging.rs` 的既有契约，开 debug 也不能变）。
- **`capabilities.rs` 绝不 print**；命令期间的输出走 `Outbound::Output`（§13）。
- **TUI 起 alt-screen 后 stdout/stderr 会被冲掉** ⇒ debug 只能落**文件**，不能指望终端。
- **`ys-core` / `ys-event` / `ys-protocol` 零 tokio、零 IO** ⇒ 计时/序列化只能在 app 层做。
- **事件只观察**（`docs/design.md`）：不得为观测而新增改变执行路径的机制。

### 性能

- 不开 debug 时**零额外开销**（一个字节不多写、不做字符串格式化）。
- 开 debug 时**不得改变被测对象的时序特征**：日志写盘若同步阻塞，会把被测的 `ttft` 撑大
  ⇒ 需要明确"写盘是否在计时区间内"。
- 单文件体积要有**上界或轮转** —— 完整请求体单条就可达数万 token（用户已排除该档，见范围外）。

### 演进

- 记录是**外部格式**：字段名一旦写进文档就是契约（与 `--json` 的 `Envelope` 同级）。
  ⇒ 需要 `schema` 版本字段。
- 与 `--json` 的关系要定：**`--json` 是给消费者的产物，debug 是给排障的痕迹**，两者可共享序列化
  但**不共享开关与去向**。

### 组织

- 单人 + AI 协作；文档与注释用中文；关键逻辑要有**变异测试**。

---

## 需求范围

### 已澄清（2026-10-17 用户裁定）

| 问题 | 裁定 | 含义 |
|---|---|---|
| 开关形态 | **`--debug` + `YUSHAN_DEBUG`**（环境变量优先，便于 CI / 后台） | 不开时保持现状 |
| 格式 | **结构化 JSONL**（一行一记录） | 可 `jq` / python 聚合 |
| 内容边界 | **工具调用元数据 + 内部状态变化 + 已有计时的扩充**；**不记完整请求/响应体** | 控制体积与敏感面 |
| 落点 | **独立 debug 文件**（一 session 一文件） | 不污染日常日志、好清理 |
| 时间 | **每条记录都要有** | 见下「时间口径」 |

### 范围内

1. **`--debug` / `YUSHAN_DEBUG` 开关**（环境变量优先），与现有 `--stats` / `--json` 并列。
2. **独立 debug 文件** `$YUSHAN_LOG_DIR/debug-<session>.jsonl`；一 session 一文件。
3. **每条记录带时间与关联 id**：
   - `t_ms`（unix 毫秒，可与外部日志对齐）、`dt_ms`（相对本次运行的单调毫秒，**做时长用这个**）、
     `seq`（单调序号，保证同一毫秒内的顺序）
   - `run` / `turn` / `round` / `tool_call_id` / `session_id` 按适用性带上
   - `schema` 版本字段
4. **工具调用元数据**（逐调用）：名字、参数（**摘要**，非全文）、耗时、结果字节数与行数、
   `is_error`、是否超时。
5. **内部状态变化**：压缩触发（前/后 token 估算）、缓存命中/未命中、重试次数与退避、
   预算剩余、边界（Abort/Steer）的产生与观察时机。
6. **计时扩充**：
   - **把 `gen` 拆成「思考」与「正文」两段**（`gen` 现在混着两者，是当前最大的归因盲区）
   - **模型交互分段**：请求发出 → 首字节 → 首个思考增量 → 首个正文增量 → 结束；
     **每次重试单独记一条**（这是 74 秒/离群轮的疑点所在）
7. **运行起点事实**（一次）：版本、provider/model、cwd/workspace、工具表、skills 数量、
   limits（`max_rounds` / wall-clock 预算 / context window）、生效的配置来源。

### 范围外（明确不做的）

- **完整请求/响应体**（用户已排除）：单条数万 token，且代码内容落盘涉及敏感性。
- 日志级别体系（`error/warn/info/debug/trace` 五档）—— 本轮只做**开/关**。
- 上报 / 遥测 / 远端收集。
- TUI 内展示 debug 流（alt-screen 下无处可放；需要另立需求）。
- 改变任何执行路径的观测（事件只观察）。

### 关键场景

- **S1 · 定位一次慢回合**：从 `turn` 汇总 → 找出离群 `round` → 在该轮内按 `tool_call_id`
  看到每个调用的耗时与参数摘要 → 若是模型侧，按分段看到底卡在排队、思考还是正文。
- **S2 · 定位一次失败**：`RunFailed` 那条记录 + 其前的重试记录（每次 attempt 的错误类与退避）。
- **S3 · 跨运行对比**：同一提示跑两次，按 `dt_ms` 对齐，直接看出差在哪个阶段。
- **S4 · 不开 debug**：行为与今天**逐字节相同**（既有 `yushan.log` 的内容不变）。

### 异常 / 边界场景

- **debug 文件写失败**（磁盘满 / 目录不可写）：必须**静默降级**，绝不影响 agent 运行。
- **开 debug 但进程被杀**：已写出的行必须**完整成行**（不能留半行 JSON）⇒ 一行一次写。
- **同一 session 被 `/resume` 续跑**：是追加到同一文件还是新建？待定。
- **两条入口（TUI / `-p`）都要有**，且 `-p` 的 stdout 必须保持干净（debug 只进文件）。

---

## 未澄清问题

- [ ] **Q1 写盘时机**：debug 行写入是否同步？若同步，会把被测的 `ttft`/`gen` 撑大
  （尤其高频的 `ModelTextDelta` 级记录）。候选：只记"段"不记"增量"／异步缓冲／接受误差并标注。
  **待用户确认**。
- [ ] **Q2 体积上界**：一个 session 的 debug 文件要不要设上限（如 64 MB）或轮转？
  超出后是截断还是丢弃最旧？**待用户确认**。
- [ ] **Q3 `/resume` 续跑**：追加同一文件，还是另起 `debug-<session>-<n>.jsonl`？
- [ ] **Q4 参数摘要的粒度**：工具参数记到什么程度？（如 `bash` 命令全文 vs 前 N 字节；
  `read` 的 path 全文）。**涉及敏感面**，需与 Q2 一起定。
- [ ] **Q5 与 `--json` 是否共享序列化**：`Envelope` 已有 serde；debug 记录另立 struct 还是复用？
- [ ] **Q6 运行起点事实的落点**：单独一条 `run_start` 记录，还是每条都带？（倾向前者）

---

## 后续建议

1. **交 `arch-design` 做方案设计**，重点解开 Q1（写盘时机对时序的污染）与 Q4（摘要粒度）。
2. **先落最小可用的一刀**：`--debug` 开关 + 独立文件 + 逐轮/逐调用（已有数据）+ **`gen` 拆分**。
   后者是当前最大的归因盲区（`gen` 46 s vs 143 s 无法解释），且**改动很小**。
3. **变异测试**：至少覆盖「不开 debug 时零写入」「写盘失败静默」「一行一写不留半行」。
4. 设计定稿后，用它复测本轮悬着的两件事：**74 秒的工具轮**与**`gen` 的方差来源**。
