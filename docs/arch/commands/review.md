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

**发现 3 个可维护性问题**：

**Issue 1: Command 对 Adapter 的耦合** 🟡
```
builtin.rs 中的 ModelCommand 和 LoginCommand 需要：
  use agent_model_openai_compatible::{OpenAICompatibleModel, OpenAICompatibleConfig};
```
这意味着加一个新适配器（如 Anthropic 原生），需要改 commands/builtin.rs。
**影响范围**：当前可接受（单一适配器），但如果未来支持多适配器会成为问题。

建议：在 Config 中增加 `provider` 字段，或引入 model factory 函数。不阻塞 MVP。

**Issue 2: CommandError 缺乏结构化** 🟡
当前 `CommandError::Message(String)` 无法区分：
- 用户可恢复错误（"No API key configured"）→ 应打印提示
- 系统内部错误（IO 失败）→ 应记录日志

建议：增加 `CommandError` 变体，如 `UserError(String)` 和 `Internal(String)`。MVP 可先用单一 Message，后续迭代。

**Issue 3: /login 的 stdin 阻塞** 🟡
`std::io::stdin().read_line()` 是同步阻塞调用，在 `#[tokio::main]` 的 runtime 上会阻塞线程。当前 TUI 本身就是同步的，所以没问题。但如果未来迁移到 crossterm/ratatui 异步 TUI，这里会成为阻塞点。

建议：MVP 先用同步 stdin，记录为技术债。后续迁 async TUI 时改用 `tokio::task::spawn_blocking` 或 `async-std` 的异步 stdin。

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

| # | 风险 | 影响 | 可能性 | 等级 |
|---|------|------|--------|------|
| 1 | /login stdin 阻塞 Tokio runtime | 低（当前同步 TUI） | 高 | 🟡 中 |
| 2 | Command 直接依赖 adapter 类型 | 中（多适配器时需重构） | 中 | 🟡 中 |
| 3 | CommandError 缺乏结构化 | 低（MVP 足够） | 高 | 🟡 低 |
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
| 可维护性 | 🟡 黄 | 3 个中等问题，均不阻塞 MVP |
| 可理解性 | 🟢 绿 | 模式一致，上手成本低 |
| 性能与可靠性 | 🟢 绿 | 无性能风险，故障隔离良好 |

**整体判定：🟢 可以实现**。3 个黄灯问题均为非阻塞性改进项，可在 MVP 实现后迭代优化。
