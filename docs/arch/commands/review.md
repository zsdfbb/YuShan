# Runtime Commands — 架构质量分析

## 分析范围

- 对象：`docs/arch/commands/design.md` — Command trait + Registry 方案（方案 B）
- 维度：可行性、可维护性、可理解性、性能与可靠性
- 参照：现有代码（agent.rs、Session trait、ToolRegistry 模式）

## 各维度评估

### 可行性 — 🟢 绿

| 检查项 | 判定 | 说明 |
|--------|------|------|
| 技术可实现 | ✅ | Agent 已有 `model: Option<Box<dyn Model>>`，set_model() 是直接赋值 |
| 依赖成熟度 | ✅ | async_trait、HashMap 均为成熟依赖，无新外部引入 |
| 核心改动风险 | ✅ | Session trait 已有 `clear()` 默认实现，MemorySession 只需覆写 |
| 实现周期 | ✅ | ~475 行，框架 ~110 行 + 10 命令 ~175 行，1-2 天可完成 |

**关键验证**：
- `Session::clear()` 已存在于 trait（line 24，default no-op）— 只需在 MemorySession 实现
- `Agent::model` 已是 `Option` — set_model() 赋值即可
- `Agent::session` 是 `Box<dyn Session>` — clear_session() 委托到 session.clear()
- `Session::messages()` 已存在 — session_messages() 委托即可

### 可维护性 — 🟡 黄

| 检查项 | 判定 | 说明 |
|--------|------|------|
| 模块边界 | 🟡 | Command 实现直接依赖 adapter crate（OpenAICompatibleModel），耦合 |
| 接口稳定性 | 🟢 | Command trait 是纯新增，不修改已有接口 |
| 测试难度 | 🟢 | trait 可 mock，CommandContext 可构造 |
| 错误处理 | 🟡 | CommandError 只有 Message(String)，缺乏结构化 |
| 并发安全 | 🟢 | TUI 单线程，无并发问题 |
| 资源管理 | 🟡 | /login 的 stdin.read_line() 阻塞 Tokio runtime |

**发现 3 个可维护性问题**（已在设计阶段修正）：

~~**Issue 1: Command 对 Adapter 的耦合**~~ ✅ 已修正
- 修正方案：引入 `ModelFactory`（`Config.build_model()`），Command 只调用 `ctx.config.build_model()`，不 import 任何 adapter 类型

~~**Issue 2: CommandError 缺乏结构化**~~ ✅ 已修正
- 修正方案：`CommandError::UserError(String)`（用户可恢复）+ `CommandError::Internal(String)`（系统错误）

~~**Issue 3: /login 的 stdin 阻塞**~~ ✅ 已记录
- 处理方案：记录为已知限制，明确演进路径（Phase 2 async TUI 时改用 spawn_blocking）

### 可理解性 — 🟢 绿

| 检查项 | 判定 | 说明 |
|--------|------|------|
| 概念一致性 | 🟢 | Command/Registry 与 Tool/ToolRegistry 模式一致 |
| 抽象层次 | 🟢 | Command trait 粒度合适，不过度抽象 |
| 文档完整度 | 🟢 | design.md + context.md + ADR 覆盖充分 |
| 新人上手 | 🟢 | 加命令 3 步：实现 trait → register() → 完成 |

### 性能与可靠性 — 🟢 绿

| 检查项 | 判定 | 说明 |
|--------|------|------|
| 延迟 | 🟢 | HashMap O(1) 查找，用户触发命令无延迟要求 |
| 吞吐 | 🟢 | 命令是交互式操作，无吞吐瓶颈 |
| 故障模式 | 🟢 | 命令失败只影响当前轮次，不破坏 agent 状态 |
| 可观测性 | 🟡 | 缺少命令执行日志（MVP 可接受） |

## 风险排序

| # | 风险 | 影响 | 可能性 | 等级 | 状态 |
|---|------|------|--------|------|------|
| 1 | /login stdin 阻塞 Tokio runtime | 低 | 高 | 🟡 中 | ✅ 已记录演进路径 |
| 2 | Command 直接依赖 adapter 类型 | 中 | 中 | 🟡 中 | ✅ 已用 ModelFactory 解耦 |
| 3 | CommandError 缺乏结构化 | 低 | 高 | 🟡 低 | ✅ 已增加 UserError/Internal |
| 4 | async_trait 堆分配 | 无（用户触发） | 确定 | 🟢 忽略 |

## 改进建议

### 易修复（实现时顺手改）

1. **MemorySession::clear() 真实实现**：`self.messages.clear(); Ok(())`
2. **Agent 新增 4 个方法**：set_model, clear_session, model_id, session_messages
3. **/help 使用 registry 遍历**：已在设计中，无需额外改动

### 需讨论（MVP 后迭代）

4. **CommandError 结构化**：是否需要区分 UserError vs InternalError？
   - 建议：MVP 不需要。当前 10 个命令的错误都是用户可理解的。
5. **/login 的 stdin 阻塞**：是否需要在 MVP 中用 `tokio::task::spawn_blocking`？
   - 建议：不需要。当前 TUI 是同步的，spawn_blocking 反而增加复杂度。
6. **session_messages() 返回引用 vs owned**：`&[Message]` 在 /export 序列化时需要 clone。
   - 建议：先用 `&[Message]`，/export 用 `to_string()` 序列化时自然 clone。

### 架构级（当前不适用，记录备忘）

7. **Adapter 解耦**：如果未来支持多适配器，Command 不应直接 import OpenAICompatibleModel。
   - 路径：引入 `ModelFactory` trait 或在 Config 中记录 provider → model 构造映射。
   - 触发条件：第二个适配器接入时。

## 综合判定

| 维度 | 判定 | 说明 |
|------|------|------|
| 可行性 | 🟢 绿 | 所有技术验证通过，无阻塞项 |
| 可维护性 | 🟢 绿 | 3 个问题已在设计阶段修正（ModelFactory 解耦、CommandError 结构化、stdin 限制记录） |
| 可理解性 | 🟢 绿 | 模式一致，上手成本低 |
| 性能与可靠性 | 🟢 绿 | 无性能风险，故障隔离良好 |

**整体判定：🟢 可以实现**。所有审查发现已在设计阶段修正。
