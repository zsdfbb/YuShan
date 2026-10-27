# 推理强度（reasoning effort）：同类平台对照与本仓差距

- **日期**：2026-10-27
- **性质**：**调研记录**。§1 的三家做法抄自本仓已有的代码级对照（`docs/arch/explore-speed/research.md` §1，
  2026-10-17 对真实源码逐项核对，带源文件行号）；§2 的 A/B 数字抄自
  `docs/arch/session-latency/design.md` §8.5（真实 provider 实测）；§3 的「本仓现状」于 2026-10-27
  重新核实到代码行。**本文不重复那两份文档的论证**，只把「按调用类型分档」这条差距集中记下来。
- **结论**：本仓只有**一个全局档位**，三家都是**按调用类型分档**。差距在分档，不在档位高低。

---

## 1. 三家怎么处理 DeepSeek 的推理 token

| 平台 | 机制 | 出处（本仓 research.md 转录的源文件位置） |
|---|---|---|
| **pi** | 思考预算**按具体 token 数**分级：`minimal:1024 / low:2048 / medium:8192 / high:16384`，配 `MIN_ANSWER_TOKENS = 1024`（保证答案至少留 1024）；**思考关闭时字段不发**（而非发 `0`） | `packages/ai/src/api/simple-options.ts:55-72`、`:74-94`；`api/anthropic-messages.ts:1130-1145` |
| **pi** | **按角色/场景分档**：子 agent 传 `--thinking`；compaction 尊重 `thinkingLevel !== "off"`；**eval harness 直接 `"off"`** | `examples/extensions/subagent/index.ts:304-305`；`core/compaction/compaction.ts:566`；`evals/src/pi-harness.ts:148` |
| **Codewhale** | 请求体字段 `reasoning_effort: off/low/medium/high/max`，注释直言**由 client 译为 DeepSeek 的 `reasoning_effort` + `thinking`**（两个字段配合） | `crates/core/src/request.rs:28-32`；`crates/tui/src/session.rs:41` |
| **Codewhale** | **router 自身调用只准 `off`/`low`** —— `medium`/`high`/`max` 在**解析期报错**（类型上不可表示），不是运行时静默钳制 | `crates/workflow/src/reasoning_router.rs:19-26`、`:61-73`、`:97-113`；默认值 `fleet_reasoning.rs:285` |
| **deepseek-harness** | `reasoningEffort` 由 adapter 拥有 + `UNSUPPORTED_REASONING_EFFORT` 校验（不支持就明确报错，不静默丢弃） | `packages/llm/llm/src/index.ts:880-898` |
| **deepseek-harness** | **子 agent 可清零 route 继承的 effort** —— 子任务不默认继承父的思考强度 | `subagent/tool-subagent/src/model-selection.ts:92,120-126` |

### 共同模式（四家没有一家用「全局单一档位」）

| 做法 | pi | Codewhale | deepseek-harness |
|---|---|---|---|
| 按调用类型/场景分档 | ✅ | ✅（router 限 off/low） | ✅（子 agent 可清零） |
| 不支持时**报错**而非静默 | — | ✅ 解析期 | ✅ 显式错误码 |
| 关闭时**不发字段**而非发 `0` | ✅ | — | — |
| 评测/基准用最低档 | ✅ `"off"` | — | — |

**要点**：探索、分类、路由这类调用用最低档；正式实现用高档。档位是**每次调用**的属性，不是**进程**的属性。

---

## 2. 本仓的 A/B 实测（真实 provider，n=3/2/3）

出处：`docs/arch/session-latency/design.md` §8.5。

| 配置 | 中位墙钟 | reasoning token | 质量 |
|---|---|---|---|
| **不发送字段** | 199.2 s | 17,938 | — |
| **`high`**（当前默认） | **100.7 s** | **10,054** | **3/3 ×3** |
| `low` | 更快 | 更少 | **掉到 2/3** |

三条可直接用的结论：

1. **发送 `high` 显著优于不发送**（接近减半）⇒ ADR-0042 选 `high` 有实测依据。
2. **`low` 会掉质量**（3/3 → 2/3）⇒ 不能为了提速全局降档。
3. ⇒ **优化空间在「分档」，不在「降档」**。

补充观测（2026-10-27 本机一次长会话）：`output_tokens` 中 **reasoning 占 69%**
（74,441 / 106,585）。⇒ 推理 token 确实是输出的大头，分档的杠杆是真实的。

---

## 3. 本仓现状（2026-10-27 核实到代码行）

**只有一个全局档位**，所有模型调用共用：

| 调用类型 | 当前档位 | 证据 |
|---|---|---|
| 主模型 | 全局 `effective_reasoning_effort()`（默认 `high`） | `apps/coding-agent/src/main.rs:2592-2593` |
| **子 agent**（按 `model` 名构造） | **继承全局** —— 工厂闭包里直接读 `cfg.effective_reasoning_effort()` | `apps/coding-agent/src/main.rs:2928-2930` |
| **压缩摘要** | **未设置任何档位**（`compact.rs` 全文无 `effort` / `reasoning` 赋值，只有解析 `thinking` 的代码） | `crates/ys-loop/src/compact.rs`（`rg 'effort'` 仅命中 thinking 解析段） |

档位的两层门控与折叠（与本差距**正交**，已落地）：

- provider 级能力位 `supports_reasoning_effort`
- 模型级 id 可识别性（GLM 要求能解析出代际；MiMo 要求是 chat 模型）
- 详见 `docs/design-final/effort.md`

---

## 4. 差距与候选改法

### 差距一句话

本仓的档位是**进程级**的；三家的是**调用级**的。

### 候选改法（按「证据强度」排序，**均未实施**）

| # | 改法 | 依据 | 代价 / 风险 |
|---|---|---|---|
| **E1** | **压缩摘要用最低档**（或直接 off） | 三家都这么做：pi 的 compaction 尊重 `thinkingLevel`；摘要任务不需要深推理。且 `RoundPurpose::Compaction` **已有现成的区分点**，结构上不必新造概念 | 小。但需先量：摘要质量下降会不会导致压缩后模型跑偏（`ADR-0017` 有压缩相关约束） |
| **E2** | **子 agent 默认不继承父档位**，按任务分 | deepseek-harness 明确「子 agent 可清零 route 继承的 effort」 | 中。`SubagentSetup` 是装配期冻结的（缓存相干性要求），加档位字段要确认不动 `ToolSpec` 前缀 |
| **E3** | 探索/检索类调用用低档 | Codewhale 的 router 限 `off`/`low` + 类型强制 | 大。需要先有「调用类型」这个概念，本仓目前没有（`RoundPurpose` 只有 `Turn` / `Compaction`） |

### 落地前必须做的一件事

**先跑 `e2e_bench` 取基线**，再做对照。理由：

- `AGENTS.md` 明确 `fact/1` 是基准里**唯一可判定**的任务（时间与质量同时可测）
- 现有的 A/B（§2）是 2026-10 早前跑的，且只测了「发/不发」与 `low`，**没有按调用类型分档的对照**

拿不到对照就改档位，等于把「掉质量」的风险赌在直觉上 —— 而 §2 已经证明 `low` 会掉质量。

---

## 5. 与其他文档的关系

| 文档 | 关系 |
|---|---|
| `docs/arch/explore-speed/research.md` §1 | §1 内容的**原始出处**，含更多同类平台（Vibe-Trading 等）与完整行号 |
| `docs/arch/session-latency/design.md` §8.5 | §2 的 A/B 数字原始出处 |
| `docs/design-final/effort.md` | 本仓 effort 的 **as-built**（两层门控 + 折叠 + `/effort` 命令）；本文的「现状」以它为准 |
| `docs/adr/0042-default-reasoning-effort-high.md` | 默认值 `high` 的决策记录 |
| `docs/arch/explore-speed/cost-model.md` §0 | 成本模型总纲：墙钟 ≈ 轮数 × 每轮模型耗时；本文属于「每轮模型耗时」这一项的拆解 |

---

## 6. 本文未做的事

- **未改任何代码**。
- **未跑 `e2e_bench`**（§4 要求的基线对照尚未执行）。
- §1 的三家机制**转抄自本仓 research.md**，未在 2026-10-27 重新核对上游源码 —— 上游可能已变化，
  引用时请以行号回查原项目。
