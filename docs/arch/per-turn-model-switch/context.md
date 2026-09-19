# 回合粒度切换运行模型 — 架构上下文

> 产出：`/arch-explore`。用途：为「**按回合（Turn）切换当前运行模型**」这一需求界定现状、约束与范围，供后续 `arch-design` 使用。
> 证据均带 `文件:行号`，对应本工作区状态（`master`，HEAD `c2aa299`；`docs/arch/skills-and-compaction/` 为未跟踪文件）。

---

## 概述

用户想引入「每轮对话切换运行模型」。经澄清，**粒度 = 回合（Turn）**，**触发 = 用户指定**（可附带「在上下文中 / 某种 workflow 中描述」的声明式形态，形态未定），**动机 = 成本优化 + 能力差异 + 多 provider 对比/调试**。

**本文件先纠正一个前提，再给现状盘点 —— 因为它决定了这个需求到底还剩多少活要干。**

### ⚠️ 前提纠正：Pi 与 CodeWhale **都有**动态切换模型的能力

用户在澄清时说「似乎 Pi、codewhale 都没有这种动态切换的能力」。**实测两仓源码（`tmp/pi`、`tmp/codewhale`），结论相反**：

| 项目 | 回合粒度切换 | 回合内（轮粒度）切换 | 自动路由 | 证据 |
|---|---|---|---|---|
| **Pi** | ✅ 有（`/model`、Ctrl+P 循环、扩展 `setModel`），**即时生效、无需重启** | ✅ **架构上支持**（模型是可变 agent state，每轮经 `prepareNextTurn` 重读；无 busy guard） | ❌ 无 | `packages/coding-agent/src/modes/interactive/interactive-mode.ts:2980,4849`；`packages/agent/src/agent-loop.ts:177,182` |
| **CodeWhale** | ✅ **有且是核心特性**（每回合经 `plan_turn_route` 定 provider/model/limits/压缩/推理档） | ❌ **显式禁止**（`reject_setting_change_while_busy`） | ✅ **有**（big/cheap 对 + 本地启发式 + 网络分类器） | `crates/tui/src/turn_route_plan.rs:3-6,143,168`；`crates/tui/src/tui/ui/apply.rs:882`；`crates/config/src/auto_model.rs:19,22,219` |

**要点**：
- CodeWhale 的「回合路由」不是简单的 `/model` —— `turn_route_plan` 是**一个函数决定整回合的 provider / model / 路由身份 / client / limits / 压缩策略 / 推理档**（`turn_route_plan.rs:3-6`），每回合派发时冻结进 `Op::SendMessage`（`dispatch.rs:759,820`）。
- CodeWhale 有 **big/cheap 双子模型对**：`PRO_MODEL = "deepseek-v4-pro"` / `FLASH_MODEL = "deepseek-v4-flash"`（`auto_model.rs:19,22`），`/model auto` 开启自动路由（`core.rs:276-282`）。
- 两者**都没有**「提示词内联指令」形态（无 `@model` / `/model:xxx`）。CodeWhale 的 per-step 模型只出现在 **subagent 派生**（`subagent/mod.rs:8687` `model_strength`）与 **JS workflow**（`workflow-js/src/driver.rs:47-50` `pub model: Option<String>`）。
- Pi 的「能力差异」止步于手动：**无自动路由、无 big/cheap 档、压缩也用同一个模型**（`agent-session.ts:1957`）。

> 结论：这个需求**不是无人区**，CodeWhale 是成熟参照（且其「回合路由」概念比 `/model` 重得多）。反过来，**YuShan 的 `/model` 已经覆盖了「回合粒度手动切换」的最小形态** —— 故本需求的真正增量在别处（见「现状盘点」）。

---

## 已定决策（2026-09-19）

| # | 决策 | 含义 |
|---|---|---|
| **D1** | **粒度 = 回合（Turn）** | 每次用户输入前选定模型，整个回合用它。**不做回合内（轮粒度）切换** —— 与 CodeWhale 的取向一致（它显式禁止回合内切换）。 |
| **D2** | **触发 = 用户指定**（非启发式自动路由） | 由用户显式选择；且用户**可能**希望以「在上下文中 / 某种 workflow 中描述」的**声明式**形态表达（**形态未定**，见 Q1）。 |
| **D3** | **动机 = 成本优化 + 能力差异 + 多 provider 对比/调试** | 三条都要服务。其中「多 provider 对比/调试」是**当前架构最不支持的**（见现状盘点 ③）。 |

**D1 的直接推论**：

1. **`BasicLoop` / `RuntimeContext` 不必动** —— 回合内是单一 `&dyn Model`（`context.rs:12`），正好匹配「整回合一个模型」。**这是本需求相对 CodeWhale 最省事的地方**（它要冻结整条 route）。
2. `/model` 已实现「下一回合生效」（`app_loop.rs:144-146` → `capabilities.rs:149-180` → `wiring.rs:134`）—— **D1 的最小形态已存在**。
3. 边界：**回合切换 ≠ 回合内切换** —— 模型在 `run_turn` 期间不可变（`agent.rs:97-114` 一次性构造 `RuntimeContext`）。

**D3 的直接推论**：

4. 「成本优化」若**不要自动路由**（D2），则只能靠**手动 + 可见性**：用户得知道当前是哪档模型、这回合花了多少。
5. 「多 provider 对比/调试」要求**跨 provider 切换** —— 而 `/model` 只在**当前 provider 内**换模型（用 `config.api_base` / `api_key` / `provider`，`capabilities.rs:168` → `config.rs:58-60` → `main.rs:601-615`）。跨 provider 现在**只能走 `/login`**（重认证、改写 provider、落 auth.json）—— 这是对比/调试的最大摩擦。
6. 「多 provider 对比」还要求**知道每个回合实际用了哪个模型** —— 现在**没有任何归因**（见 ④）。

---

## 现有架构

### 模块边界（与本需求相关的部分）

```
TUI(ys-tui-coding)  ──Request──►  app_loop  ──run_turn──►  Agent(ys-runtime)
      ▲                             │  (Wiring: model/session/events)  │
      └──── Outbound ◄── ChannelSink ◄── AgentEvent ◄── BasicLoop(ys-loop)
                                                          │
                                              ctx.model: &dyn Model（整回合固定）
```

- 命令的**用户可见行为**：`crates/ys-tui-coding/src/commands.rs`（表 `COMMANDS`，`45-101`；`/model` 解析 `193-199`）
- 命令的**能力实现**：`apps/coding-agent/src/capabilities.rs`（`set_model` `149-180`）
- 唯一契约：`ys_protocol::Request`（`crates/ys-protocol/src/request.rs:14-42`，**7 个变体**；`Prompt(Message)` `:16`，`SetModel { model }` `:18`）

### 核心抽象

| 抽象 | 位置 | 与本需求的关系 |
|---|---|---|
| `Wiring.model: Option<Box<dyn Model>>` | `apps/coding-agent/src/wiring.rs:31` | **当前模型的唯一持有者**；`set_model` `:134`、`ports()` `:115-122`、`model_id()` `:129-131` |
| `AgentPorts.model` | `crates/ys-runtime/src/agent.rs:20-21` | 每回合经端口借出；`None` → `ConfigError`（`agent.rs:97-101`） |
| `RuntimeContext.model: &'a dyn Model` | `crates/ys-component/src/context.rs:12` | **整回合一个模型**（不可变引用，无 per-round 选型点） |
| `Model` trait | `crates/ys-model/src/trait_def.rs:7-17` | `model_id()` + `complete()` —— **`model_id()` 是归因的唯一现成信息源** |
| `Config` + `model_factory` | `apps/coding-agent/src/config.rs:11-19,58-60` | 重建模型的地方（`build_model`）；工厂读 `cfg.model` / `api_base` / `api_key` / `current_compat()`（`main.rs:601-615`） |
| `Request::SetModel { model: String }` | `crates/ys-protocol/src/request.rs:18` | 只带 **model 名**，**不带 provider** —— 跨 provider 切换的协议缺口 |
| `Envelope { source, turn, event }` | `crates/ys-protocol/src/envelope.rs:47-52` | **无 model 字段** —— 归因缺口 |
| `RunLimits.context_window` | `crates/ys-component/src/limits.rs` | `Agent` 构建期冻结（`main.rs:257-269` 无 `.limits()`）；换模型**不改**窗口（与 `skills-and-compaction` 的 D5 耦合） |

### 关键数据流

**当前 `/model` 路径（回合粒度，已存在）**：

```
TUI: /model gpt-x
  commands.rs:193-199  → Action::Request(Request::SetModel{model})
  ──信道①──►
app_loop.rs:144-146    → capabilities::set_model(config, wiring, state, model)
  capabilities.rs:155    config.model = model
  capabilities.rs:158-163 state.save(last_active_model)   ← 每次都写 state.json
  capabilities.rs:168    config.build_model()             ← 用「当前 provider」的 base/key 重建
  capabilities.rs:170    wiring.set_model(Some(built))
  ──► 下一个 Request::Prompt 用 wiring.ports() → 新 model（app_loop.rs:100-109）
```

**回合内模型调用（整回合同一模型）**：

```
BasicLoop::run_turn(ctx)                        basic.rs:44
  loop {                                        basic.rs:89
    Step 5 组装 ModelRequest（messages + tools + system）  basic.rs:185-190
    Step 6 ctx.model.complete(request, forwarder)          basic.rs:194   ← 每轮同一 ctx.model
    Step 9 执行工具，回喂，继续下一轮                        basic.rs:273-395
  }
压缩时 summary 也用同一模型：generate_summary(ctx.model, ..)  basic.rs:425,469-500
```

### 现状盘点：这个需求还剩什么

| # | 能力 | 现状 | 证据 |
|---|---|---|---|
| **①** | **回合粒度手动切模型（同 provider 内）** | ✅ **已实现**（`/model <name>` 下一回合生效） | `app_loop.rs:144-146`、`capabilities.rs:149-180` |
| **②** | **回合内（轮粒度）切模型** | ❌ 不可能（`RuntimeContext.model` 整回合固定） | `context.rs:12`、`agent.rs:97-114` |
| **③** | **跨 provider 切模型（轻量）** | ❌ 只能靠 `/login`（重认证 + 改写 provider + 落 auth.json） | `capabilities.rs:42-117` `/model` 只换同 provider 的 model |
| **④** | **「这回合用了哪个模型」归因** | ❌ **完全没有** —— Envelope 无 model、AgentEvent 无 model、jsonl 只存消息、View 只显示**当前**模型 | `envelope.rs:47-52`、`view.rs:59` |
| **⑤** | **声明式形态（提示词内联 / workflow 文件）** | ❌ 无（`Prompt(Message)` 不带模型；无 workflow 机制） | `request.rs:16` |
| **⑥** | **自动路由（成本/能力启发式）** | ❌ 无（D2 明确暂不做） | — |
| **⑦** | **回合内换模型后 context_window / compat 跟随** | ❌ `limits` 构建期冻结；`compat` 由 `config.provider` 决定 | `main.rs:257-269`、`config.rs:63-66` |

> **一句话**：**① 已有**（用户可能没意识到），**真正的增量是 ③④⑤**（跨 provider + 归因 + 声明式），②⑥ 按 D1/D2 出局，⑦ 是与既有压缩配置工作的接口。

### 参考实现事实（Pi / CodeWhale 的机制摘要，供设计借鉴）

| 机制 | Pi | CodeWhale |
|---|---|---|
| 模型状态 | 可变 agent state，每轮重读（`agent-state.ts` / `agent-loop.ts:177,182`） | 每回合 `plan_turn_route` 重新规划（`turn_route_plan.rs:143`） |
| 用户入口 | `/model`、Ctrl+P 循环、扩展 `setModel` | `/model`（即时、会话内）、`/model save-default`（持久）、`/model auto`（开自动路由） |
| 生效时机 | 下一次 provider 请求（轮边界） | **下一回合**（回合内 `reject_setting_change_while_busy`，`apply.rs:882`） |
| 持久化 | `persist: false` 默认会话内 | 显式 `/model save-default` / `/fleet save` |
| 自动路由 | ❌ 无 | ✅ big/cheap 对 + 本地 `classify(prompt)`（`auto_model.rs:219`）+ 网络分类器（`model_routing.rs:699`） |
| 内联指令 | ❌ 无 | ❌ 无（仅 subagent / JS workflow 可按步指定模型） |

---

## 约束

### 技术

1. **`Request` 是唯一 UI↔app 契约**，且 app 侧 `match` **穷尽**（漏实现是编译错误）。新增字段/变体（如给 `Prompt` 加 `model`）必须：① 改 `request.rs`；② 改 `app_loop.rs` 的分发；③ 跟进 **serde 向后兼容**（照 `Request::Login.api_base` 的 `#[serde(default)]` 范式，`request.rs:27-31`）—— 否则老 JSON 反序列化炸。
2. **依赖方向**：`ys-protocol` 零 tokio；`ys-tui-coding` **不依赖** `ys-runtime`（编译器强制）。任何「UI 侧要知道模型语义」的设计，数据必须经 `CodingView` / `Request` 过，不能引依赖。
3. **`Agent` 无状态（ADR-0010）**：模型归 `Wiring`。**回合粒度切换天然契合** —— app 侧本来就每回合经 `ports` 传模型；无需碰 `Agent`。
4. **`RuntimeContext.model` 是 `&dyn Model`（整回合固定）** —— 若日后要轮粒度切换，需把 `&dyn Model` 换成「可查询的模型提供者」，属**核心改动**（D1 已排除）。
5. **`RunLimits` 构建期冻结**（`agent.rs:105` 一次性构造；`main.rs` 无 `.limits()`）—— 换模型**不跟随** `context_window`；不同窗口的模型混用会算错压缩阈值（与 `skills-and-compaction` D5/D6 咬合）。
6. **`ProviderCompat` 由 `config.provider` 决定**（`config.rs:63-66`）—— 跨 provider 切模型必须同时切 compat，否则 reasoning/tool_calls_as_text 等差异处理错。
7. **prompt 前缀稳定**：工具面恒定（ADR-0016）。**换模型不改工具面/系统提示**（模型不进 system prompt，`main.rs:601-615` 只读 `cfg.model`）——故回合粒度切换**不破坏 prompt cache 前缀**（区别于 skills）。
8. **`/model` 每次都写 `state.json`**（`capabilities.rs:158-163`）—— 若推广为「每回合都设一次模型」，需明确**是否每回合落盘**（写放大 vs 持久化语义）。

### 性能

9. **模型重建是廉价的**：`OpenAICompatibleModel::new(...)` 只是构造一个结构体（`main.rs:601-615`），不含网络 —— 每回合重建可接受。
10. **跨 provider 对比的成本是真实的**：`/login` 走 `save_auth`（落盘）+ `state.save`（落盘）+ 重建（`capabilities.rs:86-113`）——作为「临时对比」太重。
11. **无 token/成本按模型归因**：`TurnStats` 只累计总 input/output（`stats.rs`），不按模型分桶 —— 「对比不同模型成本」当前**统计不出来**。

### 演进

12. **`/model` 的持久化语义已有约定**：`state.json` 的 `last_active_model` 供**重启恢复**（`main.rs:565-598`）—— 若「每回合切模型」与它共用，需定义「临时切换」是否覆盖 `last_active_*`。
13. **压缩配置工作（`skills-and-compaction` D5/D6）尚未落地**：`context_window` 可配 + 阈值比例化正在设计。本需求若涉及「按模型设窗口」，应与那条线**对齐顺序**，避免两套 config 入口。
14. **`docs/extension-points.md` 的归类**：本需求主体是 **改配置（改 `Wiring.model`）**，不产生消息；若要做「workflow 顺序跑多回合」，那属 **Follow-up（回合之间、系统发起）** 邻域 —— 需在 `extension-points.md` 找到归属。

### 组织

15. 文档/注释/commit **用中文**；代码标识符英文。
16. 新模块/接口改动须对应 `docs/design.md` §12 测试矩阵条目。
17. 测试隔离硬约定：临时目录名含**进程内原子序号 + `process::id()`**；进程级 env 改写走 `test_env::env_lock()` + `EnvRestore`。
18. **关键逻辑必须做变异测试**（本项目多个真 bug 是「测试全绿时依然存在」）。

---

## 需求范围

### 范围内（按已定决策框定）

**A. 回合粒度切换的补全（D1/D3）**
- **A1. 跨 provider 的轻量切换**（现状 ③）：让「切到 provider P 的模型 M」不必走完整 `/login`（不重问 key、不落盘，除非用户要持久化）。**这是对比/调试动机的核心缺口。**
- **A2. 回合归因 / 可见性**（现状 ④）：让用户知道**每个回合实际用了哪个模型** —— 形态未定（transcript 行内标？View 加 `last_turn_model`？会话文件记录？），见 Q2。
- **A3. 同 provider 内切换的 UX/语义打磨**：现状 ① 已能切，但需明确「临时切换 vs 持久默认」的语义（现每回合写 `state.json`）。

**B. 声明式形态（D2 的「在上下文中 / workflow 中描述」，形态未定）**
- **B1. 提示词内联指令**（如 `/model:X 任务…`）：TUI 解析 → 该回合用 X，不改全局。
- **B2. workflow / 计划文件**：一个文件描述多回合及各自模型，app 按序跑（**借道 Follow-up 机制**）。
- **B3. session 级配置持久化**：把「当前回合用哪个模型」写进会话/状态，重启恢复。

> B1/B2/B3 **三者是否都做、做哪一个**尚未定（Q1）。

### 范围外（明确不做）

- ❌ **回合内（轮粒度）切换模型**（D1）—— 与 CodeWhale 的取向一致；不碰 `RuntimeContext` / `BasicLoop`。
- ❌ **自动路由**（启发式 / 分类器 / big-cheap 对）（D2）—— 用户未选；**CodeWhale 的 `turn_route_plan` 那套整回合规划不在本轮**。
- ❌ **Subagent 的 per-role 模型**（CodeWhale `subagent_model_overrides`）—— YuShan 无子 agent 能力。
- ❌ 模型热插拔 / 动态增删工具面（工具面恒定，ADR-0016）。
- ❌ 权限 / 沙箱（本项目「不做安全」）。

### 关键场景

- **S1（回合切换·同 provider）**：`/model deepseek-reasoner` → 下一回合用 reasoner，历史不清空。（**现状已支持**）
- **S2（回合切换·跨 provider）**：用户想让「这一回合用 minimax，下一回合用 deepseek」做对比 → 现在必须 `/login` 两次；目标形态是一次轻量切换。
- **S3（提示词内联）**：`/model:gpt-4o 帮我重构` → **仅该回合**用 gpt-4o，下一回合回到默认。
- **S4（归因）**：跑完几回合后，用户能看出「第 1 回合 deepseek-pro，第 2 回合 deepseek-flash」，并据此判断成本/质量。
- **S5（workflow 顺序多回合）**：一个文件让同一 prompt 依次用 3 个模型跑 3 个回合，产出对比。
- **S6（重启恢复）**：切了模型后重启，回到**上次选择的**模型（现状：`state.json` 的 `last_active_model`）。

### 边界 / 异常场景

- **E1**：切到**未配置凭证**的 provider → 必须**明确提示**（照 `capabilities.rs:173-177`「Cannot build model」的既有范式），且**不得**留下骗人的旧模型（现状 `set_model` 在构建失败时会 `set_model(None)`，`capabilities.rs:173-176`）—— 跨 provider 版需保持这条不变量。
- **E2**：内联指令指向未知模型 / 未知 provider → 明确提示，不静默回退。
- **E3**：切换后 `context_window` 与真实模型不符（约束 5）—— 大窗口模型被按 128k 压缩。
- **E4**：跨 provider 切换后 `ProviderCompat` 未跟随（约束 6）→ reasoning/工具调用格式错。
- **E5**：`Prompt` 加 `model` 字段后，**老 JSON（无该字段）**必须仍能反序列化（约束 1）。
- **E6**：归因与会话文件的交互 —— 若每回合模型记进 jsonl，**恢复时**是否要重建「回合 → 模型」映射？

### 质量目标

- **可用性优先**：跨 provider 对比要「一次操作完成」，不能退化成两次 `/login`。
- **不破坏既有不变量**：工具面恒定 / prompt 前缀稳定 / 依赖方向 / `Agent` 无状态 —— 尤其 **S3 的内联指令不得改动系统提示**（否则 prompt cache 前缀抖动）。
- **归因要可靠**：用户据以判断成本，错了比没有更糟。

---

## 未澄清问题

- [ ] **Q1 声明式形态到底做哪个？**（B1 内联指令 / B2 workflow 文件 / B3 session 持久化）—— 用户答「还没想好」。Pi/CodeWhale **都没有内联指令**，CodeWhale 的 per-step 模型只在 subagent / JS workflow；可作为「值不值得做 B1」的参照。
- [ ] **Q2 归因的承载形态？**（transcript 行内标注 / `CodingView` 加 `last_turn_model` / 新 `AgentEvent` 变体 / 写进会话 jsonl）—— 用户答「没考虑好」。取决于 Q3 的范围。
- [ ] **Q3 本轮最小垂直切片是什么？** 现状 ①（同 provider 手动切换）已可用 —— 是「先补 ③ 跨 provider」还是「先补 ④ 归因」还是「先做 ⑤ 声明式」？
- [ ] **Q4 跨 provider 切换的协议形态**：给 `Request::SetModel` 加 `provider: Option<String>`？还是新增变体（如 `Request::SwitchRoute { provider, model }`）？—— 影响 serde 兼容（约束 1）。
- [ ] **Q5 「临时切换 vs 持久默认」语义**：现有 `/model` **每回合写 `state.json`**（`capabilities.rs:158-163`）。跨 provider 轻量切换若也写盘，是否要区分「本次对比（不写）」与「改默认（写）」？（CodeWhale 用 `/model save-default` 显式区分，`core.rs:416`）
- [ ] **Q6 与压缩配置工作（`skills-and-compaction` D5/D6）的顺序**：`context_window` 可配尚未落地；按模型设窗口是「本需求的 ⑦」还是「归压缩那条线」？（约束 5/13）
- [ ] **Q7 是否需要按模型的 token/成本统计**（约束 11）？「多 provider 对比」若含成本对比，`TurnStats` 需按模型分桶。
- [ ] **Q8 workflow 路线（B2）是否本轮做？** 它属 `extension-points.md` 的 **Follow-up（回合之间、系统发起）** 邻域，可能需要先有 follow-up 的承载（`docs/design-plans/2026-09-15-follow-up.md`）。

---

## 后续建议

**先把「增量」与「已有」分清**，避免重复造轮子：

### 建议的第一刀：跨 provider 轻量切换 + 回合归因（现状 ③ + ④）

- **③ 是本需求最实的一块**：`/model` 已能同 provider 换模型（①），但「多 provider 对比」是 D3 明列的动机，而现状只能 `/login` —— 边际收益最大、风险可控（app 层为主，`Wiring` / `capabilities` / `Request`）。
- **④ 归因紧随**：没有归因，「对比/调试」就无从对照；且归因形态（Q2）会**反向约束** ③ 的数据通路 —— 两者应**同一份设计**。
- **两处结构改动**：① `Request` 加 provider 维度（Q4，注意 serde 兼容）；② 模型 id 从 `Model::model_id()`（`trait_def.rs:8`）冒泡到 UI（Q2）—— 后者若走 `Envelope` / `AgentEvent` 会碰 `ys-protocol` / `ys-event`（零 tokio 边界，无碍但需 ADR）。
- **`context_window` / `compat` 跟随**（E3/E4）是本刀的**隐藏坑**：跨 provider 后 `limits`（构建期冻结）与 `compat`（随 `config.provider`）必须一起理，否则「换了模型但压缩阈值/差异处理没换」—— 建议**与 `skills-and-compaction` 的 D5/D6 对齐**（Q6）。

### 建议的第二刀：声明式形态（B1/B2/B3，先答 Q1）

用户未定形态。给两条互斥路线供裁决：

- **轻（B1 内联指令）**：只在 TUI 解析层加一个前缀 → 产出「带 model 的 Prompt」。**代价小、不碰 workflow 机制**；但要新增 `Prompt` 字段（serde 兼容）+ 归因（否则「这回合用了啥」看不见）。**Pi/CodeWhale 都无此形态** —— 属 YuShan 的差异化，需想清用户心智。
- **重（B2 workflow 文件）**：借道 **Follow-up**（回合之间、系统发起）顺序跑多回合。要新增文件格式 + app 侧驱动 —— 与 `skills-and-compaction` 的 skills 机制（也是文件驱动）有潜在协同（**同一次「文件发现 + 解析」**）。

### 需要先裁决的既有记录冲突

| 冲突 | 出处 | 处置 |
|---|---|---|
| `state.json` 被 app `save()` 覆盖，用户配置不能放进去 | `skills-and-compaction/context.md:32` | 若 B3（session 级持久化）要落盘，**不能借 `state.json`** —— 需与 `~/.yushan/config.json`（D5 新文件）对齐 |
| 「改配置类命令」无消息、agent 回合开始自取 | `docs/arch/coding-agent-tui/tui-repl-context.md:266-296`（已作废前身，但结论仍适用） | 回合粒度切换**正是这一类** —— 归类结论**不**被推翻，可复用 |
| 「context 占比」字段曾被否决（理由：需扩 `RunLimits` API） | `docs/arch/tui-status-display/context.md:111-117` | 若归因要在状态行显示模型/窗口，需先解此否决；且 `skills-and-compaction` 已认定该否决**部分被现实推翻** |

### 建议用 `prototype` 验证的假设

- 「内联指令（B1）」是否真比「先 `/model` 再输入」更顺手 —— 这是**交互假设**，静态分析给不出。
- 跨 provider 轻量切换**是否真能不落盘**（凭证从 `auth.json` 现取即可）—— 快速验证 `capabilities` 是否可拆出「只重建不落盘」的路径。
