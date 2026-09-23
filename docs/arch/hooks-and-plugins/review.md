# hooks-and-plugins（落 1/2/3）— 架构质量分析报告

> 日期：2026-09-23 ｜ 基线：`master` @ `6174d37`（工作区干净）。
> 对象：待办清单 ①②③ —— **P0 prefix 指纹测试** / **P1 `ToolPlugin` 装配缝** / **P1.5 `summarizer` 上 `RuntimeContext`**。
> 来源：`design.md` §6、`notes.md` ④⑤、`ADR-0021`，**加直接代码分析**（所有判断带 `文件:行号`）。
> 结论先行：**设计整体健康（零新依赖、零新机制），但「零行为变化」这个承诺被 §6 P1 的 3、4 两步低估了** —— 一次 guideline 重排即可静默废掉所有用户的 prompt cache，而 832 条测试全绿。
>
> **处置（2026-09-23）**：F1–F9 已落入 [`design.md`](./design.md)（§5.1 / §5.2 / §6 P0·P1·P1.5 / §7.2·§7.3）与 [`ADR-0021`](../../adr/0021-plugin-seam-without-hook-system.md) 的补记。本文保留为**分析记录**，不再随设计更新。

---

## 分析范围

- **对象**：设计方案（尚未实现，故评的是**设计**而非代码）。
- **维度**：全量（可行性 / 可维护性 / 可理解性 / 性能与可靠性）。
- **方法**：把 `design.md` §6 的每一步落到真实代码上验一遍，找「文档说零行为变化、代码说会变」的缺口。

---

## 各维度判断

| 维度 | 判断 | 关键发现 |
|------|------|----------|
| 1.1 技术可实现性 | 🟢 | 三项都是**既有模式复用**：`ToolRegistry::build` 已保序（`registry.rs:23-37`）、`approval` 已有「build 期持有 + 每回合借用」先例（`agent.rs:62`/`:109`）、测试基建齐全。无黑科技。 |
| 1.2 依赖成熟度 | 🟢 | **零新依赖**：`ToolPlugin` 放 `ys-tool`（第三方本就要依赖它实现 `Tool`）；`ys-component` 已依赖 `ys-tool`（`Cargo.toml`），加 `Option<&dyn Summarizer>` 不成环。 |
| 1.3 实现周期 | 🟡 | P1 承诺「零行为变化、可独立发布」（`design.md:298`）—— **不成立**，见 F1/F2。须先补一次顺序决策才能开工。 |
| 2.1 模块边界 | 🟢 | 依赖方向不变（`design.md:211-219` 已自证）；无环。 |
| 2.2 接口稳定性 | 🟡 | `RuntimeContext` 是 `#[non_exhaustive]`（`context.rs:10`）⇒ 加字段安全；但 **`ToolPrompt` 形状未定**（F9），而它直接决定前缀字节。 |
| 2.3 错误处理 | 🟡 | 跨插件重名 fail-fast 的错误文案**不点名来源插件**（F8）。 |
| 2.4 并发安全 | 🟢 | 全静态装配、无共享可变状态；前缀在启动期冻结 ⇒ 与并发无关。 |
| 2.5 资源管理 | 🟢 | 无新资源；`ToolPlugin` 是纯数据（`ADR-0021:136` 自述「~30 行数据 + ~50 行合并」）。 |
| 3.1 概念一致性 | 🟡 | 「摘要」一词将同时指 **tools 侧**（新端口）与 **compaction 侧**（`compact.rs:295`）两套，设计未交代关系（F10）。 |
| 3.2 可理解性 / 上手 | 🟡 | 两块手抄表迁走后，**顺序真值**藏进 `order` 常量，须有文档锚点，否则「改一处必须改两处」变成「改一处必须猜到一处」（F7）。 |
| 4.1 性能模型 | 🟢 | 三项本身**每轮 0 token**（P0 是测试、P1 是编译期、P1.5 只改接线）。但**唯一非零的可能就是前缀漂移** ⇒ 这正是 P0 要防的。 |
| 4.2 故障模式 | 🟡 | F1/F2 不修即命中设计自己写下的最坏情形：「一处空格、一次 `trim` 都会改变所有用户的前缀 ⇒ 缓存全废 ⇒ 每轮多付 ~9,000 token，而 832 条测试依然全绿」（`design.md:282`）。 |

---

## 关键发现

### 🔴 F1 — P1 步骤 3「改查 `ToolPlugin`」会静默改变 guideline 顺序 ⇒ 前缀漂移

**证据**：guideline 按**源码 `if` 块的顺序**产出，与工具顺序**不同**：

| 序列 | 顺序 | 出处 |
|---|---|---|
| 工具（`EXPECTED_ORDER`） | `read → write → edit → bash → grep → find → ls → websearch → webfetch` | `toolset.rs:40-50` |
| guideline（`if` 块源码序） | `edit(×3) → write → read → grep → find → ls → websearch → webfetch` | `prompt.rs:143-189` |

`tool_guidelines` 的判据是 `tools.contains(&"edit")` 等，**彼此独立的 `if`**，产出序 = 源码序。故 `edit` 组**永远**在最前，而工具表里 `edit` 排第三。

⇒ `ToolPlugin` 若按 `order` 排序后**依次 emit guidelines**，顺序会变成 `read → write → edit → …`，**每个用户的 system prompt 字节全变**。

**`design.md` §6 P1 步骤 3 只写「两张手抄表删除，改查 `ToolPlugin`」** —— 没有指出这一步会重排。

> 这不是 P0 能替代的：P0 **会**变红，但红的时候重构已写到一半，而「重新生成锚点」恰好是**真实漂移被洗白的唯一窗口**（见 F11）。

### 🔴 F2 — 目标顺序是「保持现状」还是「改成工具序」，设计未选

F1 的决策面。两条路代价不同，**必须选一个并写进设计**，否则实现方会随手选一个：

| 选项 | 代价 |
|---|---|
| **保持现状**（`edit` 组在前） | `ToolPrompt` 须携带**独立于工具 order 的排序**，或「guideline 装配」与「工具装配」**分开** —— 比 §6 P1 描述的多一层结构 |
| **改成工具序**（`read` 组在前） | 一次**显式前缀漂移**，须一次性重生锚点并记录。照 §6 P0 自己的写法：「这是一次**预期的、显式的**前缀漂移」（`design.md:803`） |

### 🟡 F3 — 「照抄 `approval` 的位置」不成立：`summarizer` 依赖 per-turn 的 model

**证据**：`approval` 在 **build 期**由 `AgentBuilder` 持有（`builder.rs:62`），`RuntimeContext.approval` 只是它的**借用**（`agent.rs:109`）。而 `ModelSummarizer { model: ctx.model }`（`basic.rs:463`）需要 `&dyn Model`，**只在 `run_turn` 时刻可得**（`AgentPorts.model`，`agent.rs:21`）。

⇒ 「照抄」会把 `ModelSummarizer` 逼上 `pub`（现为 `pub(crate)`，`summarize.rs:53`）**并改 `AgentPorts` 签名**。`notes.md:111` 的「机械替换，零语义变化」低估了这道缝。

**推荐形状**：`RuntimeContext.summarizer: Option<&dyn Summarizer>` 作为**可选覆盖**，`basic.rs` 在 `None` 时**就地回落**到 `ModelSummarizer { model: ctx.model }`：

```rust
let fallback;
let summarizer: &dyn Summarizer = match ctx.summarizer {
    Some(s) => s,
    None => { fallback = ModelSummarizer { model: ctx.model }; &fallback }
};
```

⇒ **零行为变化、零签名破坏、不必暴露适配器**。与 `boundary`（`context.rs:23`「`None` 时行为逐字节一致」）同一手法，贴合本仓强制约定。

### 🟡 F4 — P0 指纹测试的**环境隔离**没写进骨架

`build_system_prompt` 读 `HOME` / `USERPROFILE`（`prompt.rs:218`、`:266`），并**逐级向上**找 `AGENTS.md`（`prompt.rs:227-237`）。§6 P0 骨架（`design.md:287-293`）只传了 `&tmp`，**未隔离 HOME** ⇒ 开发机上若存在 `~/.yushan/AGENTS.md` 或 `~/.yushan/SYSTEM.md`，锚点随机器而变 —— 测试**红得莫名其妙**，或更糟：**恰好绿灯但锁的不是用户真前缀**。须照既有 `no_global_prompt()`（`prompt.rs:366`）办。

### 🟡 F5 — P0 用空 catalogue ⇒ **skill 段不在指纹覆盖内**

骨架传 `&empty_catalogue()`，而 skill 段**只在 catalogue 非空时追加**（`prompt.rs:73`）。用户机器上 `~/.yushan/skills` 非空时，那段是真实前缀的一部分，**却没有任何指纹锁**。

`prompt.rs:583` 的「严格前缀」测试锁的是*形状*（空 catalogue 是非空的前缀），**不锁非空段的具体字节**。
⇒ 建议锚**两个** system prompt：空 catalogue + 一个合成 skill 的 catalogue（现成 `one_skill()`，`prompt.rs:295`）。

### 🟡 F6 — `EXPECTED_ORDER` 降为派生量后，prompt 测试的「独立真值」断了一半

`prompt.rs` 测试 `use crate::toolset::EXPECTED_ORDER`（`prompt.rs:279`），`toolset.rs` 测试也引用它（`toolset.rs:142`）。§6 P1 步骤 4 说「测试侧保留一份**独立硬编码**序列」，但没说：**两份测试已引用同一常量**，本身就是同源。降为派生量后若两边都改引派生量，交叉校验归零。⇒ 字面量必须落在**测试侧**，且 **prompt 侧也用它**。

### 🟡 F7 — 跨插件重名 fail-fast 的错误文案不点名来源

`ToolRegistry::build` 已对重名返回 `ToolError::InvalidInput`（`registry.rs:29-33`），但文案是 `duplicate tool name: {name}` —— **不含来源插件**。第三方拿到它不知道是谁撞了谁。插件层再加一道 fail-fast 是对的（早失败），但**错误信息应点名两个插件**。

### 🟡 F8 — 压缩摘要与新端口是**两个独立缝**，设计未交代关系

`compact.rs:295` 的 `generate_summary` 用 `policy.summary_system_prompt`，**不走** `ys_tool::Summarizer`（已核 `compact.rs:33`、`:295-310`）。加 `RuntimeContext.summarizer` 后，同进程将有**两套摘要**。
是「有意分离」（提示词与用途不同）还是「将来统一」？`design.md` / `notes.md` 均未说。**不需现在改，但需现在写一句**（ADR-0017 决策 10 的「片 2」可能与此交汇）。

### 🟢 F9 — `ToolPrompt` 的两个未定义面

1. **全局尾部 guideline**（`"Be concise in your responses"` / `"Show file paths clearly…"`，`prompt.rs:191-192`）**不属于任何工具**，`ToolPlugin.prompts` 无处安放；
2. **`edit` 的 3 条 guideline 是一组**（`prompt.rs:143-153`），`ToolPrompt` 须能表达「多行一组」，否则分组与顺序会散。

⇒ 建议 `ToolPrompt { tool, snippet, guidelines: Vec<String> }`，**全局尾部留在 `build_system_prompt` 内**（它本就不来自工具）。

### 🟢 F10 — 工具表 JSON 的确定性（**已核，绿**）

`serde_json::to_string(&Vec<ToolSpec>)` 的键序由 `ToolSpec` 字段声明序（`spec.rs:5-9`）与 `Value` 的 Map 实现决定。全仓 serde_json 未开 `preserve_order`（已核 14 份 `Cargo.toml`）⇒ `Map` 是 `BTreeMap` ⇒ **键有序 ⇒ 确定性**。`ToolRegistry::build` 亦保序且有守卫（`registry.rs:23`、`:117-131`）。
**残留**：指纹 hash 的是**序列化中间结果**而非最终发给模型的 bytes；serde_json 大版本升级理论上可改序列化。低风险，记录即可。

### 🟢 F11 — 生成锚点常量的**时点**（流程）

P0 的正确姿势是「先量 9 工具锚点 ⇒ 冻结 ⇒ 加第 10 个工具 ⇒ **重新量**」（`design.md:803`）。危险在**「重新量」= 把失败测试的实际值抄进常量** —— 这是真实前缀漂移被洗白的唯一窗口。
⇒ 锚点常量应从**重构前的干净 checkout** 生成（独立 worktree / `git stash`），**不是**从重构后的树里「跑红 → 抄值」。

---

## 风险排序

| 风险 | 影响 | 可能性 | 优先级 | 建议 |
|---|---|---|---|---|
| **F1/F2 guideline 重排 ⇒ 前缀漂移** | 高（每轮 ~9,000 tok × 全部用户） | **高**（按 §6 P1 字面实现必发生） | **P0** | 先选顺序策略，再写代码 |
| **F3 summarizer 形状选错** | 中（返工 + 可能被迫 `pub` 内部适配器） | 中 | **P0** | 采用「可选覆盖 + 就地回落」形状 |
| **F4 指纹测试未隔离 HOME** | 中（测试假红/假绿，锚点不可信） | 高 | P1 | 复用 `no_global_prompt()` |
| **F5 skill 段无指纹** | 中（真实前缀有一段裸奔） | 中 | P1 | 加一个非空 catalogue 锚点 |
| **F11 锚点抄值洗白** | 高（漂移被制度化） | 中 | P1 | 从重构前 checkout 生成 |
| **F6 独立真值同源** | 中（交叉校验名存实亡） | 中 | P2 | 字面量落测试侧 |
| **F7 重名文案无来源** | 低 | 高 | P2 | 错误里带插件名 |
| **F8 两套摘要未说明** | 低（认知债） | 中 | P2 | 写一句关系 |
| **F9 `ToolPrompt` 形状** | 中（影响前缀） | 中 | P1 | 定形状（见建议） |
| F10 serde_json 假设 | 低 | 低 | — | 记录即可 |

---

## 改进建议

### 易修复（低风险快速改进）

- **F4**：P0 测试照 `prompt.rs:366` 的 `no_global_prompt()` 隔离 `HOME`/`USERPROFILE`，并显式用 `env_lock()`。
- **F5**：P0 锚**两**个 system prompt —— 空 catalogue + `one_skill("git-status", "…")` 的 catalogue；后者才是用户实际看到的多数形态。
- **F7**：插件层 fail-fast 的错误文案形如 `tool "todo" declared by both plugin #3 and plugin #7`。
- **F10**：在测试里写一行注释钉住「本 hash 依赖 serde_json Map = BTreeMap」的假设，便于将来 dep 升级时定位。

### 需讨论（需要决策）

- **F1/F2（必须二选一，且写进设计）** —— 这是**唯一阻塞开工**的一项。
  - 若选「保持现状」：`ToolPrompt` 需带独立排序，或 guideline 装配与工具装配分离；
  - 若选「改成工具序」：作为一次**显式、预期**的前缀漂移，一次性重生锚点并记录（照 `design.md:803` 的写法）。
- **F3（已给结论，待确认）**：`RuntimeContext.summarizer` 作**可选覆盖** + `basic.rs` 就地回落；**不**改 `AgentPorts`、**不** `pub ModelSummarizer`。
- **F6**：独立字面量放 `prompt.rs` 的测试模块还是 `toolset.rs` 的测试模块 —— 放哪都行，**但要写明它不引用生产常量**。
- **F8**：写一句「tools 侧摘要 ≠ compaction 摘要，本轮有意分离」进 `design.md`；或明确它归 ADR-0017 片 2。
- **F9**：定 `ToolPrompt { tool, snippet, guidelines: Vec<String> }`，全局尾部**留**在 `build_system_prompt`。

### 架构级

- **无。** 本轮有意不引入新机制（ADR-0014 / 0021 的立场），`Plugin` 泛化按 `ADR-0021` 的触发条件（第 2 个缝的消费者出现）等。这一条**是本设计最强的部分**，不应被 F1–F11 的细节掩盖。

---

## 总体评价

**整体健康度：好。** 三项零新依赖、零新机制、全部复用既有先例（`ToolRegistry` 保序、`approval` 的位置、既有测试基建），符合本仓「不设计假想未来需求」的立场 —— 这是最容易做砸的地方，而设计**没砸**。

**最大风险不在「能不能做」，而在「零行为变化」这个承诺被低估**：`design.md` §6 P1 的步骤 3 与 4 读起来像机械搬家，实际藏着一次**全局指南顺序重排**（F1），一旦按字面实现，就精确命中设计自己在 `:282` 写下的最坏情形 —— *缓存全废、每轮多付 ~9,000 token、832 条测试全绿*。P0 指纹测试的**方向完全正确**（先钉字节再搬家），但需补 **F4/F5** 两处覆盖，且 **F11** 的生成时点要防洗白，否则安全带本身可能成为漂移的洗白通道。

**推荐下一步**：先做两个**不写代码**的决策 —— **F2**（guideline 顺序走哪条）与 **F3**（summarizer 形状）。这两条定下来后，P0 → P1 → P1.5 的路径是清晰的，且 §6 P1 的「可独立发布、零行为变化」承诺才**真正**成立。
