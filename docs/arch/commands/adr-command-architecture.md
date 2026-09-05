# ADR: 选择 Command trait + Registry 作为 slash command 架构

## 状态

已提议

## 背景

coding-agent 需要斜杠命令系统（/login、/model、/new 等 10 个 MVP 命令）。当前 TUI 无任何命令处理逻辑，所有输入直达 agent。需要选择命令的定义、注册和分发机制。

三个候选方案：
- **A: match 分发 + 函数** — 最小复杂度（~180 行），无扩展性
- **B: Command trait + Registry** — 可扩展（~475 行），与 Tool 模式一致
- **C: AppContext 集中持有** — 零核心改动（~255 行），但 /model 丢失会话

## 决策

选择方案 B：Command trait + CommandRegistry。

## 理由

1. **与 codebase 一致**：Tool trait + ToolRegistry 是已有模式，Command trait + CommandRegistry 复用同一心智模型。

2. **自描述的 /help**：每个 command 自带 name/description/arg_hint，/help 遍历 registry 自动生成，无需维护独立的帮助文本。

3. **open-closed 原则**：加新命令是纯添加（实现 trait + register），不修改已有代码。与「静态组合优先」原则一致。

4. **会话保留**：通过 Agent::set_model() 替换 trait object，/model 切换保留会话。方案 C 的重建 Agent 会丢失会话。

5. **测试友好**：Command trait 可 mock，每个命令可独立测试，不需要 TUI。

## 否决方案

### 否决 A（match 分发）

否决原因：不可扩展。10 个命令尚可，但代码趋势会增长到 20+。/help 需要维护独立的硬编码列表，容易与实际命令不同步。没有命令自描述能力。

### 否决 C（AppContext 集中持有）

否决原因：/model 和 /login 需要从 Config 重建整个 Agent，丢失会话。消息双重存储（AppContext.messages vs Agent.session）引入一致性问题。零核心改动的代价是将 AgentBuilder 组装逻辑复制到 commands.rs，这本身就是耦合。

## 代价

- agent-runtime 需新增 4 个方法（set_model, clear_session, model_id, session_messages）
- agent-session 的 MemorySession 需实现真实的 clear()
- async_trait 堆分配（对用户触发的命令系统可忽略）
- 框架代码 ~110 行 + 10 个命令实现 ~175 行 = 总计 ~475 行

## 后续

实现时按此顺序：
1. Agent 新增方法 + MemorySession.clear()
2. Command trait + CommandRegistry 框架（command/mod.rs）
3. TUI 集成（修改 tui.rs、main.rs）
4. 逐个实现 10 个内置命令（command/builtin.rs）
