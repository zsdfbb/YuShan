# 截断保护（片 1）— 修改报告

> 日期：2026-09-22 ｜ 编排：sequential-workflow **quick** 方案 ｜ 决策：`docs/adr/0020-truncation-guard.md`
> 基线：`aeabbaf8`（工作区干净，未产生 commit）
> ⚠️ **本切片无 design-plan / exec-plan 存档** —— quick 方案跳过 Phase 2，设计内容直接进 subagent 提示。
> 这是 Phase 1 明确预告并经用户确认的副作用；设计记录由 ADR-0020 + 本报告 + `docs/design-final/truncation-guard.md` 承担。

## 结果

| 指标 | 基线 | 完成后 |
|---|---|---|
| `cargo test --workspace` | 813 passed / 7 ignored | **832 passed / 0 failed / 7 ignored**（+19） |
| `cargo clippy --all-targets` | 0 warning | **0 warning** |
| `cargo check --workspace --all-targets` | 通过 | 通过 |
| 改动面 | — | 19 改 + 2 新，**无计划外改动** |

## 要解决的问题

provider 输出预算耗尽时会截断响应并给 `finish_reason: "length"`。适配器**确实**抓到了这个值
（`adapters/model-openai-compatible/src/lib.rs` 的 `StreamAccumulator.finish_reason`），但 `ys-loop`
**从不读它**（全 crate grep 零命中）。于是循环把「被砍断」当成「答完了」：

1. **拿残缺参数执行工具** —— 工具块的产出条件只是「name 非空」，**不看参数是否完整** ⇒ 一个写到一半的
   `write` 照样执行、**覆盖用户的文件**，且工具返回 success、TUI 显示 ✓。
2. 参数烂到解析不出 ⇒ 适配器降级为 `Value::Null` ⇒ 工具收到 null，报谁也看不懂的错。
3. 连工具块都没产出 ⇒ 循环判「无 tool call」⇒ `Completed` + ✓。

## 交付内容

**新增**
- `docs/adr/0020-truncation-guard.md` — 决策记录（6 条决策 + 方案对比 + Consequences）
- `apps/coding-agent/tests/truncation_guard.rs` — 端到端（回环 SSE，N11）

**修改**
- `crates/ys-core/src/stop.rs` — `StopReason` 新增第四变体 **`Truncated`**
- `crates/ys-model/src/request.rs` — 新枚举 **`FinishReason`**（`Stop`/`ToolCalls`/`Length`/`ContentFilter`/`Other(String)`）
  + `ModelResponse.stop_reason: Option<String>` **改名并改类型**为 `finish_reason: FinishReason`
- `crates/ys-model/src/mock.rs` — `MockModel::push_truncated_tool_call` / `push_truncated_text`
- `crates/ys-event/src/event.rs` — 非终局事件 **`ResponseTruncated { dropped_tool_calls }`**
- `adapters/model-openai-compatible/src/lib.rs` — `normalize_finish_reason`：provider 方言 → `FinishReason`
- `crates/ys-loop/src/basic.rs` — **Step 8b 截断保护分支**（核心）
- `crates/ys-tui-coding/src/{draw,app}.rs` — `✂` 符号 + 截断警告行
- `apps/coding-agent/{src/app_loop.rs,tests/integration.rs}`、`crates/ys-runtime/tests/run_turn_boundary.rs` — 改名收尾
- `docs/CONTEXT.md`、`docs/design.md` — 终止语义与测试矩阵同步

**核心行为**：`finish_reason.is_truncated()` 为真时，那一批工具调用**一个都不执行**，全部合成 `is_error`
结果回喂（满足 ADR-0001 配对完整性），循环继续；纯文本被截断时回合以 `StopReason::Truncated` 诚实地收场。

## 评审中发现并修掉的真缺陷

deep review（U3）用变异测试抓到一条 **P1**，已由 U7 修掉：

> `truncated_tool_calls_satisfy_pairing_invariant`（N4）**对特性零判别力** —— 删掉整个截断保护分支，
> 它依然全绿。根因：N4 用的两个工具名是 `"a"`/`"b"`，registry 里只有 `"spy"`；guard 失效后循环走
> 「registry 未命中」路径，照样合成 2 条 id 对应的 `is_error` 结果 ⇒ N4 的四条断言全被满足，
> 其中 `calls == 0` 是**空断言**。

修法：两个工具名注册成真 spy（各自计数器）+ 加 `ResponseTruncated { dropped_tool_calls: 2 }` 断言。
修复后变异 A 能让 N4 变红（`calls_a: left 1 / right 0`）。

这正是本项目最在意的失败模式（「测试全绿时 bug 依然存在」），由独立 review + 变异复现抓出，而非靠人肉读代码。

## 变化与计划的偏差

| # | 计划写的 | 实际 | 原因 |
|---|---|---|---|
| 1 | 编排方案待定 | **quick** | 用户选择；已预告「无 design-plan/exec-plan 可归档」 |
| 2 | 单元划分含 `crates/ys-runtime/tests/` | 原计划**漏了**该文件 | 首轮只 grep 了 `crates/ys-loop` 与 `apps/`；U6 按 `cargo check` 实际报错扩了范围 |
| 3 | `cargo test -p model-openai-compatible` | 真实包名 **`ys-model-openai-compat`** | 目录名 ≠ 包名；已由 U4 纠正 |
| 4 | U3 的 N4 用 `MockModel` | 改用自定义 `TwoTruncatedCallsModel` | `MockModel` 每次 `push` 是一个**独立响应**，表达不了「同一条响应里两个 tool call」 |
| 5 | e2e 用「一个残缺的 write」 | 批里加了一个**参数完整的 write 当哨兵** | 适配器把非法 JSON 参数降级为 `Value::Null`，单个残缺调用只会报错、**不创建文件**，抓不到回归 |

## 验证方式（全自动）

| 命令 | 期望 | 实测 |
|---|---|---|
| `cargo test --workspace` | 全绿 | 832 passed / 0 failed / 7 ignored |
| `cargo clippy --all-targets` | 0 warning | 0 |
| `cargo test -p ys-coding-agent every_request_variant_is_handled` | 通过 | 通过 |
| `cargo test -p ys-coding-agent --test truncation_guard` | 通过 | 1 passed |

**变异测试**（本仓库硬约定，全部独立复现过）：

| # | 注入 | 应该变红 | 实测 |
|---|---|---|---|
| 1 | `is_truncated()` guard → `if false` | N1 / N2 / **N4** | ✅ 三条全红 |
| 2 | 文本分支 `Truncated` → `Completed` | N2 | ✅ |
| 3 | guard 恒真 | N3 + 12 条既有用例 | ✅ |
| 4 | `for` 里首个结果后 `break` | N4 | ✅ |
| 5 | 删掉 `continue;` | N1（spy 被真执行） | ✅ |
| 6 | `ResponseTruncated` 的 emit 挪到终局之后 | N2 顺序断言 | ✅ |
| 7 | e2e：删掉 guard | `truncation_guard`（文件被创建） | ✅ |
| 8 | 适配器 `None` → `Stop` / `"length"` → `Stop` | 映射表 + 端到端 | ✅ |

所有变异均已还原（`basic.rs` sha256 前后一致）。

## 已知后续项（本切片范围外，未做）

- **模型调用重试**（原清单第 2 条，即「片 2」）：429/5xx/连接失败目前**零重试**，一撞限流整个回合失败。
  它需要先给 `ModelError::Provider(String)` 加上结构化可重试标志（现在状态码只活在字符串里）。
- **provider 截断却不报 `finish_reason`**（如流被 TCP 断掉 ⇒ 现在走 SSE error ⇒ `ModelError`）：
  ADR-0020 明确未覆盖，属片 2 范畴。
- **适配器的 `eprintln!`**（`adapters/model-openai-compatible/src/lib.rs` 的 `ToolCallAccumulator::into_content_block`）：
  违反本项目「库内部不 print / 输出三分」的约定，与本切片相邻但独立，未修。
- **`MockModel` 不支持「一次响应多个 tool call」**：U3 的 N4 因此自建了 model。若后续还有多工具测试，值得补一个 pusher。

## 波及文档

- `docs/CONTEXT.md` — 「终止原因」定义加「被截断」✅
- `docs/design.md` — §5 失败与终止语义表 + §12 测试矩阵 `BasicLoop` 行 ✅
- `docs/adr/0020-truncation-guard.md` ✅
- `docs/design-final/truncation-guard.md` — as-built ✅
- **`docs/design-plans/` / `docs/exec-plans/`** — ❌ 本切片**无**（quick 方案跳过 Phase 2）
