# Runtime Commands — 设计方案

## 概述

为 coding-agent 的交互式 TUI 设计斜杠命令系统。核心决策：命令注册机制（trait vs 函数匹配）和状态所有权（直接引用 vs AppContext 集中持有）。

## 候选方案

### 方案 A：最小复杂度 — match 分发 + 函数

**核心思路**：命令是 async 函数，通过 match 分发。无 trait、无 registry。

```
tui.rs: if input.starts_with('/') → commands::dispatch(input, &mut agent, &mut config)
commands.rs: match name { "help" => cmd_help(), "model" => cmd_model(args, ctx), ... }
```

| 维度 | 评估 |
|------|------|
| 代码量 | ~180 行 |
| 核心改动 | Agent +2 方法（set_model, reset_session） |
| 扩展性 | 无。加命令 = 加 match arm |
| /help | 硬编码字符串列表 |

### 方案 B：可扩展 — Command trait + Registry

**核心思路**：定义 `Command` trait（name, description, execute），CommandRegistry 存储 `Vec<Box<dyn Command>>` + `HashMap<name, index>` 查找。与现有 Tool/ToolRegistry 模式一致。

```
tui.rs: if starts_with('/') → registry.execute(input, &mut ctx)
command/mod.rs: trait Command { fn name(); fn description(); async fn execute(); }
command/builtin.rs: HelpCommand, ModelCommand, ... (各实现 Command)
CommandRegistry: Vec<Box<dyn Command>> + HashMap<String, usize>
```

| 维度 | 评估 |
|------|------|
| 代码量 | ~475 行 |
| 核心改动 | Agent +4 方法, MemorySession +1 clear() |
| 扩展性 | 强。加命令 = 实现 trait + register()，自动出现在 /help |
| /help | 自描述，从 registry 遍历生成 |

### 方案 C：状态集中 — AppContext 持有所有可变状态

**核心思路**：引入 `AppContext` 拥有 Config、`Option<Agent>`、messages、last_response。命令不修改 Agent 内部，而是从 Config 重建整个 Agent。

```
app_context.rs: struct AppContext { config, agent: Option<Agent>, messages, last_response }
commands.rs: Command enum + parse + dispatch，rebuild_agent() 从 Config 重新构建
```

| 维度 | 评估 |
|------|------|
| 代码量 | ~255 行 |
| 核心改动 | **零**。所有变更在 apps/coding-agent 内 |
| 扩展性 | 中。加命令 = 加 enum variant + match arm |
| /help | 硬编码 |
| 代价 | /model 切换丢失会话；messages 与 Agent 内部 session 重复 |

## 对比矩阵

| 维度 | A: match | B: trait+registry | C: AppContext |
|------|----------|-------------------|---------------|
| 实现复杂度 | ★☆☆ 最低 | ★★★ 最高 | ★★☆ 中等 |
| 代码量 | ~180 行 | ~475 行 | ~255 行 |
| 核心 crate 改动 | 小（+2 方法） | 小（+4 方法 +1 clear） | **零** |
| 扩展性 | 差（if-else） | 强（open-closed） | 中（enum） |
| /help 自描述 | 否 | **是** | 否 |
| 与 Tool 模式一致 | 否 | **是** | 否 |
| 会话保留（/model） | 保留 | 保留 | **丢失** |
| 状态所有权清晰度 | 中（散在参数） | 中（CommandContext 借用） | **高（AppContext 集中）** |
| 测试便利性 | 低（函数耦合） | **高（trait mock）** | 中 |

## 推荐方案：B（Command trait + Registry）

### 推荐理由

1. **与 codebase 一致**：Tool trait + ToolRegistry 是已有模式，Command trait + CommandRegistry 复用同一心智模型。贡献者只需理解一次。

2. **自描述的 /help**：每个 command 自带 name/description/arg_hint，`/help` 遍历 registry 自动生成。方案 A 和 C 需要维护两处（注册 + 帮助文本）。

3. **open-closed 原则**：加新命令不需要修改已有代码（无 match arm、无 enum variant）。这与项目「静态组合优先」的原则一致——组合而不是修改。

4. **会话保留**：通过 `Agent::set_model()` 替换 trait object，session 保留。比方案 C 的「重建整个 Agent」更符合用户预期。

5. **成本可接受**：475 行中 ~175 行是 10 个命令的具体实现（每个 ~17 行），框架本身 ~110 行。多出的 ~300 行 vs 方案 A 换来了真正的扩展性和一致性。

### 方案 B 的取舍

**选择接受的代价：**
- 需要在 agent-runtime 加 4 个小方法（set_model, clear_session, model_id, session_messages）— 跨 product/core 边界，但这些方法是 Agent 自然 API 的一部分
- 需要在 MemorySession 实现 clear() — trait 默认是 no-op，需要真实实现
- async_trait 的堆分配 — 对用户触发的命令系统可忽略

**不选择方案 C 的原因：**
- AppContext 重建 Agent 丢失会话，/model 切换后用户丢失上下文，UX 差
- 消息重复存储（AppContext.messages vs Agent.session）引入一致性问题
- 零核心改动的代价是将组装逻辑从 AgentBuilder 复制到 commands.rs，这本身就是耦合

## 接口设计

### Command trait

```rust
#[async_trait]
pub trait Command: Send + Sync {
    fn name(&self) -> &str;                     // "model"
    fn description(&self) -> &str;              // "Show or switch the current model"
    fn arg_hint(&self) -> Option<&str> { None } // "<model_name>"
    async fn execute(&self, args: &str, ctx: &mut CommandContext<'_>)
        -> Result<CommandResult, CommandError>;
}
```

### CommandContext

```rust
pub struct CommandContext<'a> {
    pub agent: &'a mut Agent,
    pub config: &'a Config,
    pub commands: &'a CommandRegistry,  // /help 遍历用
}
```

### CommandRegistry

```rust
pub struct CommandRegistry {
    commands: Vec<Box<dyn Command>>,        // 有序，/help 展示
    by_name: HashMap<String, usize>,        // O(1) 查找
}

impl CommandRegistry {
    pub fn register(&mut self, cmd: impl Command + 'static);  // 重复名 panic
    pub fn get(&self, name: &str) -> Option<&dyn Command>;
    pub fn all(&self) -> Vec<&dyn Command>;
    pub async fn execute(&self, input: &str, ctx: &mut CommandContext<'_>)
        -> Result<CommandResult, CommandError>;
}
```

### CommandResult / CommandError

```rust
pub enum CommandResult { Continue, Exit }

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// 用户可恢复错误，打印提示即可（如 "No API key configured"）
    #[error("{0}")]
    UserError(String),
    /// 系统内部错误，应记录日志（如 IO 失败、序列化错误）
    #[error("internal error: {0}")]
    Internal(String),
}
```

TUI 层区分处理：`UserError` 打印到 stderr，`Internal` 打印到 stderr 并（未来）记日志。

### Agent 新增方法（agent-runtime）

```rust
impl Agent {
    pub fn model_id(&self) -> Option<&str>;
    pub fn set_model(&mut self, model: Option<Box<dyn Model>>);
    pub async fn clear_session(&mut self) -> Result<(), SessionError>;
    pub fn session_messages(&self) -> &[Message];
}
```

### MemorySession 新增（agent-session）

```rust
async fn clear(&mut self) -> Result<(), SessionError> {
    self.messages.clear();
    Ok(())
}
```

### Config 新增 ModelFactory（coding-agent 产品层）

为解耦 Command 对 adapter 类型的直接依赖，引入 model factory 模式。Config 持有工厂函数，Command 通过 `ctx.config.build_model()` 构造模型，不需要知道具体适配器类型。

```rust
// apps/coding-agent/src/config.rs

use agent_model::Model;

/// Model factory function type. Captures adapter-specific construction logic.
/// Returns None if config is incomplete (missing api_base/api_key).
type ModelFactory = Box<dyn Fn(&Config) -> Option<Box<dyn Model>> + Send + Sync>;

pub struct Config {
    pub api_base: Option<String>,
    pub api_key: Option<String>,
    pub model: String,
    pub cwd: PathBuf,
    model_factory: Option<ModelFactory>,
}

impl Config {
    /// Set the model factory. Called once in main.rs after adapter types are known.
    pub fn set_model_factory(&mut self, factory: impl Fn(&Config) -> Option<Box<dyn Model>> + Send + Sync + 'static) {
        self.model_factory = Some(Box::new(factory));
    }

    /// Build a model from current config. Delegates to the factory.
    pub fn build_model(&self) -> Option<Box<dyn Model>> {
        self.model_factory.as_ref().map(|f| f(self))
    }
}
```

`main.rs` 中注册工厂（adapter 类型只在这里 import）：

```rust
config.set_model_factory(|cfg| {
    let base = cfg.api_base.as_ref()?;
    let key = cfg.api_key.as_ref()?;
    Some(Box::new(OpenAICompatibleModel::new(OpenAICompatibleConfig {
        api_base: base.clone(),
        api_key: key.clone(),
        model: cfg.model.clone(),
        max_tokens: Some(4096),
        temperature: Some(0.7),
        compat: ProviderCompat::standard(),
    })))
});
```

Command 中使用（不 import 任何 adapter 类型）：

```rust
// 在 ModelCommand::execute() 中：
let model = ctx.config.build_model()
    .ok_or_else(|| CommandError::UserError("No API credentials. Use /login first.".into()))?;
ctx.agent.set_model(Some(model));
```

这样 `commands/builtin.rs` 只依赖 `agent-model`（Model trait）和 `agent-runtime`（Agent），不依赖任何具体 adapter crate。

## 文件结构

```
apps/coding-agent/src/
  main.rs              — 构建 CommandRegistry，注册 ModelFactory，传入 tui
  tui.rs               — 检测 '/' 前缀，调用 registry.execute()
  config.rs            — Config + ModelFactory（模型构造解耦）
  prompt.rs            — 不变
  commands/
    mod.rs             — Command trait, CommandResult, CommandError, CommandContext, CommandRegistry
    builtin.rs         — 10 个内置命令实现（只依赖 agent-model, agent-runtime，不依赖 adapter）
```

## 关键场景

### /login 流程

```
/login deepseek
  → LoginCommand.execute("deepseek", ctx)
  → 提示输入 api_key（stdin）
  → ctx.config.api_key = Some(key)
  → ctx.config.build_model() → Some(OpenAICompatibleModel)
  → ctx.agent.set_model(Some(model))
  → 打印确认
```

### /model 流程

```
/model claude-sonnet-4
  → ModelCommand.execute("claude-sonnet-4", ctx)
  → ctx.config.model = "claude-sonnet-4"
  → ctx.config.build_model() → Some(OpenAICompatibleModel)
  → ctx.agent.set_model(Some(model))
  → 打印确认
  → 会话保留不变
```

Command 不知道具体用了什么适配器，只调用 `config.build_model()`。

### /help 流程

```
/help
  → HelpCommand.execute("", ctx)
  → 遍历 ctx.commands.all()
  → 打印每个命令的 name, arg_hint, description
```

## 未澄清问题

- [ ] `/copy` 是否需要跨平台剪贴板 crate（如 `arboard`）？MVP 可先用 `pbcopy`/`xclip` 命令行调用。
- [ ] `/compact` 的完整实现依赖 session 的 compact 能力，MVP 可先做「清空 + 打印提示」。
- [ ] Command trait 是否需要 `aliases() -> Vec<&str>` 支持别名？MVP 不需要，预留即可（加方法不破坏）。

## 已知限制与演进路径

### /login 的 stdin 阻塞

**现状**：`/login` 交互式提示使用 `std::io::stdin().read_line()`，同步阻塞 Tokio runtime 线程。

**影响**：当前 TUI 本身就是同步的（read_line → run_turn → 打印），所以无实际问题。但如果未来迁移到 crossterm/ratatui 异步 TUI，这里会阻塞 event loop。

**演进路径**：
- **MVP**：同步 stdin，无需改动
- **Phase 2（async TUI）**：改用 `tokio::task::spawn_blocking(|| read_line())` 或 `rustyline-async`
- 触发条件：TUI 从 stdin loop 迁移到 crossterm event loop 时
