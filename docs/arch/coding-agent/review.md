# Coding Agent 设计 — 架构质量分析报告

## 分析范围

- 对象：Coding Agent 设计（`docs/arch/coding-agent/context.md` + `design.md` + `adr-0005`），对照 v0 设计和 Pi 源码
- 维度：可行性 / 可维护性 / 可理解性 / 性能与可靠性
- 来源：两轮对抗性审查，第二轮聚焦改进后的设计

## 各维度判断

| 维度 | 判断 | 关键发现 |
|------|------|----------|
| 1.1 技术可实现性 | 🟡 | Pi 工具对齐存在 4 个遗漏（fuzzy matching、legacy format、diff output、streaming output）；OpenAI 兼容性声明过于简化 |
| 1.2 依赖成熟度 | 🟡 | Tokio 从 dev-dep 升级为 prod-dep 是 v0 架构原则变更，需要显式决策 |
| 1.3 实现周期 | 🟢 | 25 个任务、6 Phase 结构清晰，增量路径明确 |
| 2.1 模块边界 | 🟡 | ApprovalHandler 归属 crate 有三处不一致描述（context.md/design.md/adr）；v0 "安全外置"原则与内置审批矛盾 |
| 2.2 接口稳定性 | 🟡 | 14 处 breaking change，28 个构造点需更新；ToolSpec #[non_exhaustive] 阻断外部测试 |
| 2.3 错误处理 | 🟢 | ToolError 双通道（PermissionDenied/Timeout）设计合理；截断策略文档化 |
| 2.4 并发安全 | 🟢 | 无新并发模型；ApprovalHandler async trait 不引入锁 |
| 2.5 资源管理 | 🟡 | Bash 工具的进程树管理（SIGTERM→SIGKILL）未详细设计；JsonlSession 单行损坏即全量失败 |
| 3.1 概念一致性 | 🟡 | "截断归 loop"原则与工具自身截断实现矛盾；context.md 的"未解决问题"已全部在 design.md 中回答但未更新 |
| 3.2 上手成本 | 🟢 | Pi 源码可直接参考；6 Phase 实现阶段清晰 |
| 4.1 性能模型 | 🟢 | 截断常量（50KB/2000行）对齐 Pi；ToolRegistry 缓存是正确优化 |
| 4.2 故障模式 | 🟡 | ModelResponse.stop_reason 与 StopReason enum 冗余无协调逻辑；OpenAI 适配器缺少 tool result 格式翻译描述 |

## 风险排序

| # | 风险 | 影响 | 可能性 | 优先级 | 维度 |
|---|------|------|--------|--------|------|
| 1 | **Pi Edit fuzzy matching 遗漏**：Pi 有 fuzzyFindText（Unicode NFKC + smart quote + dash normalization），设计未提及。模型经常产生 slightly different Unicode 字符，无 fuzzy matching 则 edit 频繁失败 | 高 | 高 | **P0** | 可行性 |
| 2 | **Pi Edit legacy format 遗漏**：Pi 处理 oldText/newText 顶层格式、edits 为 JSON string、单对象非数组。设计只描述 edits[] 格式 | 錄 | 高 | **P0** | 可行性 |
| 3 | **截断策略矛盾**：ADR 说"截断归 loop"，但 4 个工具描述都说工具自身做截断（Pi 也是工具内截断）。两套逻辑会同时存在 | 高 | 必现 | **P0** | 可维护性 |
| 4 | **OpenAI tool result 格式翻译缺失**：v0 用 Role::User + ContentBlock::ToolResult，OpenAI API 需要 role:"tool" + tool_call_id。适配器必须做翻译，设计未描述 | 高 | 必现 | **P0** | 可行性 |
| 5 | **ToolSpec #[non_exhaustive] 阻断 8 个测试**：外部 crate 的 Tool impl 需要构造 ToolSpec，加 #[non_exhaustive] 后所有外部构造失败 | 中 | 必现 | **P1** | 可行性 |
| 6 | **RuntimeContext::new() 签名变更影响 10 个调用点**（1 production + 9 tests） | 中 | 必现 | **P1** | 可行性 |
| 7 | **v0 "安全外置"原则矛盾**：v0 design.md 明确说"本项目不定义沙箱、权限、审批"，但设计将 ApprovalHandler 内置到 agent-tool + BasicLoop | 中 | 必现 | **P1** | 可维护性 |
| 8 | **ADR 与 design.md 项目数不一致**：ADR 说 8 项/180 行，design.md 说 9 项/250 行。JsonlSession 未计入 ADR | 低 | 已发生 | **P2** | 可理解性 |
| 9 | **context.md 未更新**：8 个"未澄清问题"已全部在 design.md 中决策，但 context.md 仍列为未澄清 | 低 | 已发生 | **P2** | 可理解性 |
| 10 | **JsonlSession 单行损坏全量失败**：设计说 corrupt line 返回错误，但应该 skip + warn | 低 | 中 | **P2** | 可靠性 |

## 逐维度详细分析

### 1. 可行性

#### Pi 工具对齐差距（P0）

设计声称"严格对齐 Pi 接口"，但遗漏了 4 个关键行为：

| 遗漏项 | Pi 的实际行为 | 设计中的描述 | 影响 |
|--------|-------------|------------|------|
| **Edit fuzzy matching** | `fuzzyFindText`：NFKC 归一化 + smart quote + dash normalization + 尾部空白剥离 | 未提及 | 模型生成的 oldText 与文件内容略有差异时 edit 失败 |
| **Edit legacy format** | `prepareEditArguments`：处理 oldText/newText 顶层格式、edits 为 JSON string、单对象非数组 | 只描述 edits[] 格式 | 部分模型（GLM-5.1）发送 legacy 格式时失败 |
| **Edit diff/patch output** | 返回 `{ diff, patch, firstChangedLine }` 用于 TUI 展示 | 只描述成功消息 | TUI 无法展示编辑详情 |
| **Bash streaming output** | `OutputAccumulator` 流式收集 + 定期 `onUpdate` 回调 | 事后截断 | 长命令期间无输出，TUI 体验差 |

#### OpenAI 兼容性声明（P0）

设计说"所有 OpenAI-compatible 平台使用相同协议"，但实际差异包括：
- **DeepSeek**：`reasoning_content` 字段、`max_tokens` 计数方式不同、SSE 中的 reasoning delta
- **MiniMax**：部分模型将 tool_calls 作为 content 文本返回而非结构化 tool_calls 字段
- **opencode-go/zen**：兼容性级别未知

建议：在 adapter 中增加 provider-specific 的 compat flags（对齐 Pi 的 `OpenAICompletionsCompatSchema`）。

#### Tool result 格式翻译（P0）

v0 内部格式：`Role::User` + `ContentBlock::ToolResult { tool_call_id, content, is_error }`
OpenAI API 期望：`{ role: "tool", tool_call_id: "...", content: "..." }`

这个翻译逻辑是 adapter 的核心职责之一，设计中完全未提及。

### 2. 可维护性

#### 截断策略矛盾（P0）

| 来源 | 说的 | 实际 |
|------|------|------|
| ADR 推荐方案 | "截断是 loop 编排策略，不是工具自身责任" | — |
| 设计 Read 工具 | "head 截断 2000行/50KB" | 工具内截断 |
| 设计 Bash 工具 | "tail 截断 2000行/50KB" | 工具内截断 |
| Pi 源码 | 工具内截断（truncateHead/truncateTail） | — |

**结论**：Pi 的工具自身做截断。设计的工具描述也说工具内截断。但 ADR 的架构原则说 loop 截断。两者矛盾。

**建议**：删除 ADR 中"截断归 loop"的原则，明确"每个工具负责自身的输出截断"（对齐 Pi），`max_tool_output_bytes` 作为 RunLimits 中的默认值传给工具。

#### v0 "安全外置"原则（P1）

v0 design.md 明确说：
> "本项目不定义沙箱、权限、审批和隔离抽象。需要这些能力的上层项目可以包装 Tool 或替换 Runtime。"

但设计将 `ApprovalHandler` 放在 `agent-tool` crate，`BasicLoop` 在执行前调用它。这把安全逻辑内置到了 core runtime。

**两种立场**：
- **内置（当前设计）**：更简单，工具不需要知道审批；loop 统一处理
- **外置（v0 原则）**：core runtime 保持纯净，coding agent 层自己包装 tool 或替换 loop

**建议**：保持内置，但更新 v0 design.md 的原则描述，显式记录这一决策变更。或者将 `ApprovalHandler` 的默认实现设为 `AutoApprove`（允许所有），coding agent 层替换为真正的审批逻辑。

### 3. 可理解性

#### 文档不一致

| 问题 | 位置 | 修复 |
|------|------|------|
| 8 项 vs 9 项 | ADR 说 8 项，context.md/design.md 说 9 项 | ADR 补充 JsonlSession |
| 180 行 vs 250 行 | ADR 说 180 行，design.md 说 250 行 | ADR 更新估算 |
| ApprovalHandler crate 归属 | context.md 说 agent-loop+agent-component，design.md 说 agent-tool | 统一为 agent-tool |
| 未澄清问题已解决但未更新 | context.md 仍列 8 个"未澄清" | 更新 context.md |

#### 任务依赖图缺失

25 个任务有隐含依赖但未声明：

```
T1 (non_exhaustive) ──→ T2 (ToolError) ──→ T9 (ApprovalHandler)
T3 (RunLimits) ──→ T10 (RuntimeContext) ──→ T11 (BasicLoop)
T4 (ModelRequest) ──→ T12-T15 (Adapter)
T6 (ToolContext) ──→ T16-T20 (Tools)
```

建议在 design.md 中添加任务依赖图。

### 4. 性能与可靠性

#### 性能：截断常量双重标准

- `RunLimits::max_tool_output_bytes` 默认 1MB
- 工具内截断常量 50KB（对齐 Pi）
- loop 级截断（1MB）永远不会触发，因为工具已经截断到 50KB

建议：删除 `max_tool_output_bytes`（或将其作为工具截断常量的配置入口），避免混淆。

#### 可靠性：JsonlSession 损坏恢复

当前设计：一行损坏 → 整个 session 加载失败
建议：跳过损坏行 + 警告 + 继续加载剩余行

#### 可靠性：Bash 进程管理

Pi 的 bash 工具有完善的进程管理：
- 进程组跟踪（`trackDetachedChildPid`）
- 超时后 kill 进程树（`killProcessTree`）
- abort 信号处理
- 等待子进程退出避免僵尸进程（`waitForChildProcess`）

设计中的 bash 工具描述较简略，建议在实现时参考 Pi 的进程管理模式。

## 修复建议

### 易修复（本次直接落入文档）

| # | 修复 | 影响文件 |
|---|------|----------|
| 1 | ADR 项目数从 8→9，行数从 180→250 | adr-0005 |
| 2 | context.md "未澄清问题"标记为已解决 | context.md |
| 3 | ApprovalHandler crate 归属统一为 agent-tool | context.md |
| 4 | 添加 25 个任务的依赖图 | design.md |
| 5 | 截断策略原则改为"工具内截断，RunLimits 提供默认值" | adr-0005, design.md |
| 6 | 删除 `max_tool_output_bytes`（或标注为工具截断的配置入口） | design.md |
| 7 | JsonlSession 损坏恢复改为 skip+warn | design.md |

### 需讨论（需要决策后再动）

| # | 问题 | 选项 |
|---|------|------|
| 1 | **Pi fuzzy matching**：是否在 v0 就实现？ | A: 实现基础 fuzzy matching（NFKC + trim） B: v0 只做 exact matching，fuzzy 留 v1 |
| 2 | **Pi edit legacy format**：是否在 v0 兼容？ | A: 实现 prepareEditArguments 兼容 B: v0 只支持 edits[] 格式 |
| 3 | **Bash streaming output**：是否在 v0 实现？ | A: 实现 OutputAccumulator 流式 B: v0 事后截断，streaming 留 v1 |
| 4 | **v0 安全原则更新**：是否显式更新 v0 design.md？ | A: 更新原则描述 B: 保持矛盾（不推荐） |
| 5 | **OpenAI compat flags**：是否需要 provider-specific 配置？ | A: 简单 if/else 按 provider name B: Pi 式 compat schema |

### 架构级

无。对抗性审查未发现结构性缺陷：6 模块分层清晰、依赖方向正确、Pi 工具对齐方向正确。核心问题都是实现细节层面的遗漏，不构成架构风险。

## 总体评价

设计整体健康：11 个维度 5 绿 6 黄 0 红。**最大风险集中在 Pi 工具对齐的 4 个遗漏**（fuzzy matching、legacy format、diff output、streaming output）——这些不是架构问题，而是实现时必须补全的行为细节。其次是文档内部一致性问题（项目数、截断原则、crate 归属），属于易修复项。v0 安全原则矛盾需要一个显式决策。

**建议**：先修复 7 个易修复项，再与用户确认 5 个需讨论项，然后进入实现。

---

## 第二轮验证（改进后设计）

### 第一轮问题修复状态

| 第一轮问题 | 状态 |
|-----------|------|
| P0: Pi fuzzy matching 遗漏 | ✅ 已补 |
| P0: Pi legacy format 遗漏 | ✅ 已补 |
| P0: 截断策略矛盾 | ✅ 已修复（工具内截断） |
| P0: OpenAI tool result 翻译 | ✅ 已补 |
| P1: ToolSpec #[non_exhaustive] | ✅ 已补 ToolSpec::new() |
| P1: RuntimeContext 影响 | ✅ 已记录 10 个调用点 |
| P1: v0 安全原则 | ✅ ADR 记录变更 |
| P2: 文档不一致 | ✅ 已修复 |
| P2: 任务依赖图 | ✅ 已补 |

### 新增 P0 能力验证

| 能力 | 状态 | 发现 |
|------|------|------|
| 工具错误恢复 | ✅ | 所有 ToolError 变体回喂模型 |
| Token 估算 | ✅ | chars/4 + CJK 补偿（修复了纯 chars/4 对中文不准的问题） |
| 上下文压缩 | ✅ | 阈值触发 + 摘要生成 + 溢出恢复 |
| System prompt 多层 | ✅ | 项目上下文 + 自定义覆盖 + 动态 guidelines |

### 第二轮发现的新问题

| # | 严重度 | 问题 | 状态 |
|---|--------|------|------|
| C1 | Critical | compact_session borrow 冲突（ctx 同时被 forwarder 和 model 完全借用） | ✅ 已修复：generate_summary 接收 `&dyn Model` 而非 `&mut RuntimeContext` |
| C2 | Critical | context overflow 无恢复路径 | ✅ 已修复：增加 emergency_compact + 重试 |
| C3 | Critical | 压缩中途 crash 数据丢失 | ✅ 已修复：JsonlSession 用 tmp file + atomic rename |
| M1 | Major | chars/4 对 CJK 文本不准 | ✅ 已修复：CJK 字符用 chars/1.5 估算 |
| M2 | Major | 摘要用 Role::System 可能被 API 拒绝 | ✅ 已修复：改为 Role::User + prefix |
| M3 | Major | 无防循环安全（同一工具连续失败） | ✅ 已修复：连续 3 次同工具失败警告 |
| M4 | Major | LoopError::Tool 成死代码 | ✅ 已修复：标注为 reserved，仅用于 registry miss |
| m1 | Minor | load_project_context 用同步 I/O | ✅ 已加注释说明理由 |
| m2 | Minor | dirs 依赖未在 workspace 中 | ✅ 已修复：改用 std::env |
| m3 | Minor | build_system_prompt 用 &[&str] 而非 &[ToolSpec] | ⏳ 可接受，v1 优化 |
| m5 | Minor | 任务缺少验收标准 | ⏳ 实现时补充 |

### 总体评价

设计经过两轮迭代已达到可实现状态：
- **3 个 Critical 全部修复**（borrow 冲突、overflow 恢复、原子写入）
- **4 个 Major 全部修复**（CJK 估算、System 消息角色、防循环、死代码）
- **11 个第一轮问题已修复**
- **2 个 Minor 保留**（可接受，实现时处理）

**结论：设计可以进入实现阶段。**
