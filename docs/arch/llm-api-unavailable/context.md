# LLM API 不可用 — 架构上下文

> **交叉引用（2026-10-12）**：`docs/arch/turn-output-and-feedback/`（已实现）**有意收窄**本文「场景 4
> （合法空回复 = 正常）」的判定 —— 它把「无内容、无工具、`finish_reason=stop`」判为可见的非正常收尾
> （`StopReason::NoOutput`），并声明了**顺序依赖**：传输层空（本文管辖，应为 `Err`）在该上游落地前
> 也会先显示为 `NoOutput`，待本文落地后由其接管成 `RunFailed`。

## 概述

需求：**`ys-coding-agent` 在 LLM API 不可用时必须报错并终止执行。**

「不可用」不是一个错误，而是一族：从连接层（DNS / 拒绝 / TLS / 无应答）到 HTTP 层（4xx/5xx）
再到流层（中断、半截、非 SSE 错误体）。本文件梳理现状：哪些族**已经**报错终止、哪些族
**静默地装作成功**或**永远挂起**，并界定这个需求的边界与未澄清问题。

## 现有架构

### 模块边界

错误从 provider 到用户经过三层，**每层的职责已经定死**：

| 层 | 文件 | 职责 | 错误类型 |
|---|---|---|---|
| 适配器 | `adapters/model-openai-compatible/src/{lib,stream}.rs` | 传输、状态码、SSE 解析；**只报传输/协议错，不解读业务语义** | `ModelError::Provider(String)` |
| 循环 | `crates/ys-loop/src/basic.rs` | Step 6 `ctx.model.complete`；失败即发终局事件 + 返回 `Err` | `LoopError::Model(..)` |
| 应用 | `apps/coding-agent/src/{main,app_loop}.rs` | TUI 落 Error 行后继续；`-p`/`--json` 冒泡到 `main` 退出 | `Box<dyn Error>` |

### 核心抽象

- `Model::complete(&self, ModelRequest, &mut dyn ModelEventSink) -> Result<ModelResponse, ModelError>`
- `ModelError`（`crates/ys-model/src/error.rs`）只有三个变体：`Sink` / `Provider(String)` / `Serde`。
  **没有 `Timeout` / `Unavailable`** —— 上层无法区分「网络不可达」与「provider 明确报错」。
- `FinishReason`（`crates/ys-model/src/request.rs`）：`Stop` / `ToolCalls` / `Length` / `ContentFilter` /
  `Other(String)`；`Default = Other("")` 的语义是「provider 没给结论」，**刻意不等于 `Stop`**。
- `StreamAccumulator::finish()`（`lib.rs`）：`error.is_some()` 才返回 `Err`；否则**无条件**用已累积的
  delta 收敛成一个 `Ok(ModelResponse)`。
- `parse_sse_stream`（`stream.rs`）：只把**传输层**字节错误转成 `StreamEvent::Error`；
  流**干净结束**（对端关闭、无 `[DONE]`）返回 `Ok(())`，不报错。

### 关键数据流（失败路径）

```text
complete()
  ├─ send() ──── reqwest Err ──────────────► ModelError::Provider("HTTP request failed: …")
  ├─ status !success ──────────────────────► ModelError::Provider("API error {status}: {body}")
  ├─ parse_sse_stream()
  │    ├─ 字节流 Err ──► StreamEvent::Error ─► finish() = Err("SSE stream error: …")
  │    └─ 流结束（含无 [DONE]）───────────► finish() = Ok(累积到的内容)   ← 缺口在此
  └─ 非 SSE 响应体（HTML / JSON error）───► 零 delta ─► finish() = Ok(空响应) ← 缺口在此
                                                        │
ys-loop basic.rs Step 6  ◄──────────────────────────────┘
  ├─ Err(e) ─► emit(RunFailed{error}) ─► return Err(LoopError::Model(e))
  └─ Ok(resp) ─► append(assistant_msg) ─► tool_calls 空？ ─► RunFinished{Completed}
```

另有一条**与 `complete` 无关**的挂起路径：`ctx.model.complete` 内部的 `await` 期间，
`BasicLoop` 完全不在自己的 `select` 上 —— 轮边界的 `Boundary::Abort` 只在每轮开头
（上下文压缩检查之后、Step 5 组装请求之前）被读取。

### 外部依赖

| 依赖 | 用途 | 版本 | 备注 |
|---|---|---|---|
| `reqwest` | HTTP/SSE 传输 | 0.12（`json` + `stream`） | chat 客户端用 `Client::new()`，**未设任何 timeout** |
| `tokio` | runtime / 超时原语 | 1 | `ys-model` 内**不含** tokio；`ys-loop` 含 `tokio::time` |
| 已有先例 | `/v1/models` 拉取 | — | `provider.rs::fetch_models` 显式 `.timeout(5s)`，失败**静默**回退静态表 |

## 现状实测矩阵

对本地回环 TCP 服务器实测（探针已删除，结论如下）：

| 场景 | 现状产出 | 判定 |
|---|---|---|
| 连接被拒 / DNS 失败 / TLS 失败 | `Err(Provider("HTTP request failed: …"))` | ✅ 报错终止 |
| HTTP 4xx / 5xx | `Err(Provider("API error {status}: {body}"))` | ✅ 报错终止 |
| 流中途字节错误 | `Err(Provider("SSE stream error: …"))` | ✅ 报错终止 |
| 200 + 非 SSE 错误体（网关/门户/代理错误页） | `Ok(空响应, Other(""))` → `RunFinished{Completed}` | ❌ **静默成功，零输出** |
| 200 + SSE 半截后断流（无 `[DONE]`、无字节错误） | `Ok(半截文本, Other(""))` → `RunFinished{Completed}` | ❌ **把残缺当完整** |
| 服务端接受连接但不应答（黑洞 / 半开） | 客户端 2s 内**未返回**；无超时 → 无限挂起 | ❌ **不报错、不终止** |

### 三个缺口的落点

1. **无超时**（最硬）：`OpenAICompatibleModel::new` → `reqwest::Client::new()`，`reqwest` 默认
   `timeout` / `connect_timeout` 均为 `None`。端点黑洞时 `complete` 永久挂起。
   - `-p` / `--json`：进程永久卡住，无输出、无退出码。
   - TUI：`is_turning` 恒真、Working 动画空转；**Ctrl-C 此刻是 `Abort` 而非退出**，
     而 `Abort` 只在轮边界被读 → 用户按 Ctrl-C 毫无反应，只能 Ctrl-D 强退（UI 退出时
     `shutdown_timeout(APP_THREAD_GRACE=1s)` 把在途请求连同 runtime 一起丢弃）。
   - 没有任何超时旋钮：CLI 只有 `-p` / `--json` / `--stats`，也没有 `YUSHAN_*` 超时环境变量。
2. **非 SSE 响应体不报错**：`feed_sse_bytes` 对不带 `data:` 前缀的行一律跳过。代理/网关返回
   200 + HTML 或 200 + `{"error": …}` 时，累积到零 delta，`finish()` 仍然 `Ok`。
3. **流干净结束不等于响应完整**：`parse_sse_stream` 内部**知道**自己是否见过 `[DONE]`
   （见到即提前 `Ok(true)` 返回），但这个事实**没有**向 `finish()` 传递 —— 两条路径都收敛成
   `Ok(())`；`finish()` 也不检查 `finish_reason` 是否为 `Other("")`。

### 已经正确的部分（不要动）

- `basic.rs` Step 6 的错误分支：**先发 `RunFailed` 再返回 `Err`** —— 满足 ADR-0004 决策点 5
  的「终局事件不变式」，UI 靠它复位 `is_turning`。
- `app_loop.rs`：`RunFailed` 已由 UI 落地，app 只落日志、不重复刷 Error 行。
- `-p` / `--json`：`run_result?` 把 `LoopError` 冒泡到 `main -> Result<.., Err>`，
  Rust 的 `Termination` 打印 `Error: Model(Provider(..))` 到 **stderr** 并 **exit 1**；
  消费与 turn 并发（`tokio::join!`），终局事件即停，不会死锁。
  （实测：`YUSHAN_API_BASE=http://127.0.0.1:9 … -p "hi"` → stdout 空、stderr 一行
  `Error: Model(Provider("HTTP request failed: …"))`、`exit=1`。）
- 终局事件在 `-p` / `--json` 的 sender 侧不会被 overflow 吞掉：终局事件走的是
  `RunFailed`，循环内 `emit()` 走异步路径（ADR-0009/0013）。

## 约束

- **技术**：`ys-model` / `ys-core` / `ys-event` / `ys-protocol` **不得**引入 tokio —— 超时必须落在
  适配器（`reqwest` builder 或 `tokio::time::timeout`）或 `ys-loop`，不能进 `Model` trait。
- **技术**：`ModelEventSink` 必须保持**同步 `fn`**（不跨动态库边界传 trait object）。超时/取消
  信息只能经 `Result` / `ModelError` 回传，不能加 async sink 方法。
- **技术**：取消即消息（ADR-0013），`CancelToken` 已删。要打断**在途** HTTP 请求，只剩
  「drop future」这一条路（超时即是它的自然形态），不能引入共享取消标志。
- **技术**：`reqwest::Client` 是适配器构造时建的，TUI 模式下一个 client 存活整个会话；
  超时若做成 per-request，可用 `RequestBuilder::timeout`，不必重建 client。
- **契约**：终局事件不变式（ADR-0004 点 5）—— 每个 run 恰好一个终局事件；新增任何失败路径
  都必须发 `RunFailed` 后再 `Err`。
- **契约**：`FinishReason` / `ModelError` 的语义不能在 `ys-loop` 里解读 provider 方言
  （方言归适配器，词汇归 `ys-model`）。
- **演进**：`ModelError` 不是 `#[non_exhaustive]`，加变体对外部实现/匹配是 breaking；
  `FinishReason` 是 `#[non_exhaustive]`。
- **性能**：超时值要能容纳长思考模型（`deepseek-reasoner` 首字节可达数十秒），
  单一的「总超时」会误杀；流式下「首字节超时」与「流空闲超时」语义不同。
- **测试**：验证这些路径需要本地 TCP 服务器（沙箱内 bind 会被拒），且必须 `.no_proxy()`
  直连回环；临时目录/端口需按 `docs/design.md` 的并行安全约定构造。

## 需求范围

### 范围内

- **报错**：上述三个缺口都要变成**可见的失败**，而不是成功或挂起：
  - 非 SSE / 空响应体（`data:` 行一条都没有、且 `content` 与 `finish_reason` 皆空）判为错误；
  - 流结束但从未见过 `[DONE]`（或被判断为不完整）判为错误；
  - 请求无应答时按超时报错。
- **终止执行**：一次 `run_turn` **必然**走到终局（`RunFailed` + `Err`），
  不在「看起来成功」的 `Completed` 上停住。
- **`-p` / `--json`**：所有新的失败路径同样以非零退出码 + stderr 消息收场。
- **TUI**：`is_turning` 必须复位、transcript 必须出现 Error 行；进程保留（可重试/换模型）。

### 范围外（明确不做）

- **不做自动重试 / 退避 / 多 provider 故障转移** —— 需求是「报错并终止」，不是「自愈」。
- **不做网络健康检查 / 熔断 / 离线检测** —— 这是上层产品的职责。
- **不动 `Agent` 无状态、Boundary、信道三条不变式**；不恢复 `CancelToken`。
- **不改 `/v1/models` 启动期拉取的静默降级策略**（它按设计失败即静默）。
- **不追求统一的「API 健康度」抽象**；`ModelError` 是否需要新增变体留作未澄清问题。

### 关键场景

- 场景 1（黑洞）：`api_base` 指向不可达主机 / 端口，TUI 按 Ctrl-C 必须能**真正停下**，
  并给出超时错误；`-p` 必须在超时后 exit ≠ 0。
- 场景 2（网关错误页）：反代返回 200 + HTML（登录页 / 502 页），必须报错而非静默 Completed。
- 场景 3（流被掐断）：SSE 推到一半连接断掉，必须报错而非把半截文本当完整回复。
- 场景 4（正常空回复）：模型合法地返回「无内容、无工具调用、`finish_reason=stop`」时
  —— 这是**正常**收尾，不能被新的空响应判定误伤。
- 场景 5（鉴权失败）：401/403 已有正确路径，改造后必须保持。

## 未澄清问题

- [ ] **「终止执行」的粒度**：TUI 下终止**该回合**（现状 `RunFailed` 语义，进程保留、可重试）
      还是终止**进程**（exit 1）？「可重试」与「终止」需要用户确认。
- [ ] **超时用什么形式**：总请求超时 / 连接超时 / 首字节超时 / 流空闲超时？分别取值多少？
      是否可配置（env 还是 CLI flag，或 `~/.yushan/config.json`）？
- [ ] **非流式回退**：超时/挂起时是否值得对同一请求做一次非流式重试（`stream: false`）？
      与「不做重试」的范围界定冲突，需拍板。
- [ ] **空响应判定边界**：仅「零 delta 且无 finish_reason」算错，还是「有 delta 但从未见
      `[DONE]`/见 `finish_reason`」也算错？后者能治好场景 3，但可能误伤「服务端不发 `[DONE]`」
      的合法兼容端点（该类端点在 OpenAI 兼容生态里真实存在）。
- [ ] **`ModelError` 是否加变体**：`Timeout` / `IncompleteStream` / `EmptyResponse` 之类，
      让上层可区分；加变体是 breaking，需确认外部（`adapters/`、测试 mock）影响面。
- [ ] **错误信息形态**：`API error {status}: {body}` 会把网关整页 HTML 原样塞进 TUI 的 Error 行，
      是否截断 / 只留前 N 字符 / 只留状态行？
- [ ] **超时后的会话状态**：Step 7 在 `tool_calls` 检查**之前**就 append 了 assistant 消息 ——
      空响应会被写进 session（并落 JSONL）。是否要在空响应时不落盘？
- [ ] **`-p` / `--json` 的错误措辞与退出码**：`main` 返回 `Err` 已给出 exit 1，但消息是
      Rust `Debug` 形态（`Model(Provider("…"))`）。是否需要改成面向用户的单行文案？
- [ ] **TUI 挂起时的 Ctrl-C 语义**：在途请求期间 Ctrl-C 当前是 `Abort`（且无效）。要不要改成
      「第一次 Ctrl-C 取消本回合、撤销在途请求」？这需要能真正 cancel future。

## 后续建议

- 建议用 `arch-design` 做方案设计：核心分叉点是**超时放在哪一层**
  （`reqwest` builder / `RequestBuilder::timeout` / `ys-loop` 的 `tokio::time::timeout`），
  以及**完整性判定放在哪一层**（适配器的 `finish()` 还是 `ys-loop` 的 Step 6 之后）。
- 建议用 `prototype` 验证「首字节超时 + 流空闲超时」两个阈值对 `deepseek-reasoner`
  这类长思考模型是否安全 —— 这是最容易写出「测试全绿但线上误杀」的地方。
- 本需求触及 `docs/design.md` §「失败与终止语义」与 §12 测试矩阵（`BasicLoop` 行的
  「模型错误」条目），设计定稿后需同步更新这两处。
