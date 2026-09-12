# TUI 交互与显示可测试性 — 设计文档

> 基于 `docs/arch/tui-interaction-test/context.md`，回答两个问题：
> 1. slash 命令的**决策逻辑**（选了谁、存了什么）能不能单测？
> 2. 模拟输入/返回后，**屏幕显示**对不对？

## 实现状态（2026-09-12）

已按 quick 模式实施（T1/T3/T4/T5/T6）。实现与本文档的偏差：

| 偏差 | 说明 |
|------|------|
| **TuiSurface trait 未实现** | 因 suspend/resume 成对测试（T7）在测试合同中排除，TuiSurface 无消费方，引入会触发 dead_code 警告。故 §4.3/§4.6 的 surface 参数设计暂未落地。若日后要测 suspend/resume 成对性，再补 trait + `dispatch_input` 加参。 |
| **`password` 方法删除** | 本文档臆测 api_key 用 `inquire::Password`，但实际代码用 `inquire::Text`（明文）。实现贴合实际代码，`Prompter` 只保留 `select` + `text`。 |
| **`for_test` helper 未采用** | 测试用 `StubPrompter` 散点构造更简单，helper 成死代码已删除。§7 的 "19 个构造点改 `::for_test()`" 实际是 "17 处统一加 `prompter: &StubPrompter`"。 |

## 1. 设计目标

| 目标 | 度量 |
|------|------|
| 命令决策逻辑可单测 | `/login` `/model` 的 inquire 决策路径 100% 覆盖 |
| suspend/resume 成对可断言 | 任意 slash 命令执行前后，恒 suspend→resume |
| 屏幕渲染正确性 | 给定 transcript 内容，TestBackend 渲染可断言 |
| 最小侵入 | 生产代码行为不变；新增抽象为薄壳 |
| 延续既有范式 | 沿用 draw.rs 的 `#[cfg(test)]` 模块内测试模式 |

## 2. 业界调研

| 项目/来源 | 模式 | 适用性 |
|-----------|------|--------|
| ratatui spawn-vim recipe | `LeaveAlternateScreen` → 外部交互 → `EnterAlternateScreen` + `clear()` | ✅ 确认 suspend/resume 实现正确 |
| ratatui seam-based testing | `handle_key_event(KeyEvent)` 与 `event::read()` 分离 | ✅ 同理：Prompter 与 inquire 分离 |
| ratatui TestBackend | 内存 Buffer 渲染测试 | ✅ 渲染层已有，交互层需新 trait |
| inquire 官方 | 无 mock 机制；建议用 `Term` 抽象或 pty | ❌ pty 太重 |
| aider / Continue | Python 生态，patch stdin/stdout | ⚠️ Rust 不能 patch 全局 stdin |

## 3. 两层测试架构

TUI 行为拆成两层，正交覆盖：

```
┌─────────────────────────────────────────────────────┐
│  第一层：决策逻辑（Prompter trait + TuiSurface trait）  │
│  测：选了哪个 provider / auth 是否持久化 / suspend 成对 │
├─────────────────────────────────────────────────────┤
│  第二层：屏幕渲染（Transcript → TestBackend）          │
│  测：给定对话内容，屏幕显示是否正确                      │
└─────────────────────────────────────────────────────┘
```

| 测什么 | 用什么 | 涉及改造 |
|--------|--------|----------|
| slash 命令决策逻辑 | Prompter trait + FakePrompter | CommandContext 加字段 |
| suspend/resume 成对性 | TuiSurface trait + MockSurface | dispatch_input 加参 |
| 渲染正确性 | Transcript → TestBackend | 零改动，纯测试代码 |
| Working 动画 | App 状态直接构造 | 零改动 |

## 第一层：决策逻辑 — Prompter + TuiSurface trait

### 4.1 候选方案

#### 方案 A：最小注入（推荐）

`CommandContext` 加 `&dyn Prompter` 字段；`TuiSurface` 独立于 `ui/` 模块。

```rust
// commands/mod.rs
pub trait Prompter: Send + Sync {
    fn select(&self, prompt: &str, options: Vec<String>, page_size: usize)
        -> Result<String, PromptError>;
    fn text(&self, prompt: &str, help: Option<&str>) -> Result<String, PromptError>;
    fn password(&self, prompt: &str, help: Option<&str>) -> Result<String, PromptError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptError {
    /// 用户取消或中断（Esc / Ctrl-C）。
    /// 当前 inquire 的 OperationCanceled 和 OperationInterrupted 始终合并处理，
    /// 故统一为一个变体；若未来需区分，可拆为 Canceled / Interrupted。
    Cancelled,
    Other(String),
}

pub struct CommandContext<'a> {
    pub agent: &'a mut Agent,
    pub config: &'a mut Config,
    pub state: &'a mut StateStore,
    pub prompter: &'a dyn Prompter,    // 新增
}
```

```rust
// ui/mod.rs
pub(crate) trait TuiSurface {
    fn suspend(&mut self) -> Result<(), Box<dyn std::error::Error>>;
    fn resume(&mut self) -> Result<(), Box<dyn std::error::Error>>;
}
```

#### 方案 B：统一注入

`InteractionContext<P, S>` 聚合两个 trait，通过泛型传播。**否决**：泛型传播到 Command trait 导致全仓库改动，生命周期复杂。

#### 方案 C：feature gate 隔离

用 Cargo feature 隔离 inquire。**否决**：只有 2 个命令 4 个调用点，CI 组合爆炸不值得。

### 4.2 方案对比

| 维度 | A 最小注入 | B 统一注入 | C feature gate |
|------|-----------|-----------|----------------|
| 实现复杂度 | ⭐ 最低 | ⭐⭐ 中 | ⭐⭐⭐ 高 |
| 代码改动量 | ~260 行 | ~330 行 | ~400 行 |
| 生产行为不变 | ✅ | ✅ | ✅ |
| 与既有范式兼容 | ✅ | ⚠️ 泛型改动大 | ⚠️ feature 复杂 |

### 4.3 生产 impl

#### InquirePrompter

```rust
pub struct InquirePrompter;

impl Prompter for InquirePrompter {
    fn select(&self, prompt: &str, options: Vec<String>, page_size: usize)
        -> Result<String, PromptError>
    {
        inquire::Select::new(prompt, options)
            .with_page_size(page_size)
            .prompt()
            .map_err(|e| match e {
                inquire::InquireError::OperationCanceled
                | inquire::InquireError::OperationInterrupted => PromptError::Cancelled,
                e => PromptError::Other(e.to_string()),
            })
    }

    fn text(&self, prompt: &str, help: Option<&str>) -> Result<String, PromptError> {
        let mut q = inquire::Text::new(prompt);
        if let Some(h) = help { q = q.with_help_message(h); }
        q.prompt().map_err(|e| match e {
            inquire::InquireError::OperationCanceled
            | inquire::InquireError::OperationInterrupted => PromptError::Cancelled,
            e => PromptError::Other(e.to_string()),
        })
    }

    fn password(&self, prompt: &str, help: Option<&str>) -> Result<String, PromptError> {
        let mut q = inquire::Password::new(prompt);
        if let Some(h) = help { q = q.with_help_message(h); }
        q.prompt().map_err(|e| match e {
            inquire::InquireError::OperationCanceled
            | inquire::InquireError::OperationInterrupted => PromptError::Cancelled,
            e => PromptError::Other(e.to_string()),
        })
    }
}
```

#### TuiSurface：dispatch_input 增加 `surface` 参数

`CrosstermSurface` 不做独立结构体——但 `dispatch_input` 需增加 `surface: &mut dyn TuiSurface` 参数，使 MockSurface 可注入。生产调用传 `&mut CrosstermSurface`（内联实现 `TuiSurface` 的 suspend/resume，委托给现有自由函数）。

### 4.4 测试 fake

```rust
#[cfg(test)]
pub(crate) struct FakePrompter {
    answers: std::collections::VecDeque<Result<String, PromptError>>,
    pub calls: std::sync::Mutex<Vec<PromptCall>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromptCall {
    Select { prompt: String, options: Vec<String>, page_size: usize },
    Text { prompt: String, help: Option<String> },
    Password { prompt: String, help: Option<String> },
}

impl FakePrompter {
    pub fn with_answers(answers: Vec<Result<String, PromptError>>) -> Self {
        Self {
            answers: answers.into(),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl Prompter for FakePrompter {
    fn select(&self, prompt: &str, options: Vec<String>, page_size: usize)
        -> Result<String, PromptError>
    {
        self.calls.lock().unwrap().push(PromptCall::Select {
            prompt: prompt.to_string(), options, page_size,
        });
        self.answers.pop_front().unwrap_or(Err(PromptError::Cancelled))
    }
    // text, password 类似
}
```

```rust
#[cfg(test)]
pub(crate) struct MockSurface {
    pub calls: Vec<&'static str>,
}

impl MockSurface {
    pub fn new() -> Self {
        Self { calls: Vec::new() }
    }
}

impl TuiSurface for MockSurface {
    fn suspend(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.calls.push("suspend");
        Ok(())
    }
    fn resume(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.calls.push("resume");
        Ok(())
    }
}
```

> 注：MockSurface 不需要 Mutex——测试中 MockSurface 被 `&mut` 持有，不存在并发访问。
```

### 4.5 builtin.rs 改造

**改造前**（login 第 170 行）：
```rust
let selection = inquire::Select::new("Select a provider:", provider_labels)
    .with_page_size(10).prompt();
```

**改造后**：
```rust
let selection = ctx.prompter.select("Select a provider:", provider_labels, 10);
```

4 个 inquire 调用点（login×3 + model×1）全部同理替换。`match selection` 分支不变。

### 4.6 dispatch_input 改造

dispatch_input 签名增加第 10 个参数 `surface: &mut dyn TuiSurface`：

```rust
async fn dispatch_input<B: Backend + Write>(
    terminal: &mut Terminal<B>,
    input: String,
    events: &mut EventStream,
    app: &mut App,
    agent: &mut Agent,
    config: &mut Config,
    commands: &CommandRegistry,
    stats: &mut TurnStats,
    state_store: &mut StateStore,
    surface: &mut dyn TuiSurface,       // 新增
) -> Result<(), Box<dyn std::error::Error>>
```

slash 分支改造：

```rust
if input.starts_with('/') {
    surface.suspend(terminal)?;                            // 原 suspend_terminal
    let result = {
        let prompter = InquirePrompter;
        let mut ctx = CommandContext {
            agent, config, state: state_store,
            prompter: &prompter,
        };
        commands.execute(&input, &mut ctx).await
    };
    if let Err(e) = surface.resume(terminal) {             // 原 resume_terminal
        return Err(e.into());
    }
    // ... match result 不变
}
```

> 注：`TuiSurface::suspend` / `resume` 的签名需要接受 `terminal` 引用，或改为更宽泛的签名。实际实现时 `CrosstermSurface` 持有 `&mut Terminal<B>` 引用，但 `&mut dyn TuiSurface` 本身不泛型——可通过将 terminal 操作下沉到 `CrosstermSurface` 构造时绑定来解决（详见实施阶段）。

### 4.7 决策逻辑测试骨架

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // /login 无参 → select provider → text api_key → auth 持久化
    #[tokio::test]
    async fn test_login_interactive() {
        let prompter = FakePrompter::with_answers(vec![
            Ok("OpenAI (https://api.openai.com)".into()),
            Ok("sk-test-key".into()),
        ]);
        let mut ctx = mock_command_context(&prompter);
        let result = LoginCommand.execute("", &mut ctx).await.unwrap();
        assert_eq!(result, CommandResult::Continue);
        assert!(ctx.config.registry.auth_for("openai").is_some());
    }

    // /model 无参 → select model → config.model 切换
    #[tokio::test]
    async fn test_model_interactive() {
        let prompter = FakePrompter::with_answers(vec![Ok("gpt-4".into())]);
        let mut ctx = mock_command_context(&prompter);
        let result = ModelCommand.execute("", &mut ctx).await.unwrap();
        assert_eq!(result, CommandResult::Continue);
        assert_eq!(ctx.config.model, "gpt-4");
    }

    // slash 命令 → suspend/resume 成对
    #[tokio::test]
    async fn test_suspend_resume_paired() {
        let mut surface = MockSurface::new();
        let prompter = FakePrompter::with_answers(vec![
            Ok("OpenAI (https://api.openai.com)".into()),
            Ok("sk-test".into()),
        ]);
        // dispatch_input 接收 surface: &mut dyn TuiSurface
        // 断言 surface.calls == ["suspend", "resume"]
    }

    // prompter Cancelled → CommandResult::Continue
    #[tokio::test]
    async fn test_cancel_returns_continue() {
        let prompter = FakePrompter::with_answers(vec![Err(PromptError::Cancelled)]);
        let mut ctx = mock_command_context(&prompter);
        let result = LoginCommand.execute("", &mut ctx).await.unwrap();
        assert_eq!(result, CommandResult::Continue);
    }

    // unknown command → suspend + resume + error 记录
    #[tokio::test]
    async fn test_unknown_command_still_resumes() {
        let surface = MockSurface::new();
        // 断言 resume 被调用 + CommandError::UserError
    }
}
```

## 第二层：屏幕渲染 — Transcript → TestBackend

### 5.1 管线分析

```
app.transcript (Vec<TranscriptLine>)
  → compute_visible() → line_to_text() → ratatui widgets → TestBackend Buffer
```

这条链路是**纯函数**：给定 transcript 内容，渲染结果确定。不需要模拟真实 Agent turn——直接构造 `app.transcript` 即可。

| 层 | 当前可测性 |
|----|-----------|
| `line_to_text(TranscriptLine)` | ✅ 已有测试 |
| `compute_visible(App)` | ⚠️ 间接测试 |
| `ui(f, App)` → TestBackend | ✅ 13 个测试 |
| **完整管线**（输入→转录→渲染） | ❌ 本文补齐 |

### 5.2 渲染测试骨架

**零改动，纯测试代码。** 直接构造 `app.transcript`，通过 TestBackend 渲染并断言。

```rust
#[cfg(test)]
mod display_tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use agent_core::StopReason;

    fn make_app_with_transcript(lines: Vec<TranscriptLine>) -> App {
        // 复用 draw.rs 已有的 make_app() 构造基础 App，再覆盖 transcript
        let mut app = make_app();
        app.transcript = lines;
        app.show_status = true;
        app.follow = true;
        app
    }

    // render_to_text 复用 draw.rs 已有实现（draw.rs:334），不重复定义

    // 一问一答
    #[test]
    fn test_single_qa_renders() {
        let mut app = make_app_with_transcript(vec![
            TranscriptLine::User("写个 hello world".into()),
            TranscriptLine::Assistant("fn main() { println!(\"Hello!\"); }".into()),
            TranscriptLine::Summary { rounds: 1, stop: StopReason::EndTurn, elapsed_secs: 2.3 },
        ]);
        let text = render_to_text(&mut app);
        assert!(text.contains("写个 hello world"));
        assert!(text.contains("Hello!"));
        assert!(text.contains("1 rounds"));
    }

    // 多轮对话
    #[test]
    fn test_multi_turn() {
        let mut app = make_app_with_transcript(vec![
            TranscriptLine::User("问题1".into()),
            TranscriptLine::Assistant("回答1".into()),
            TranscriptLine::Summary { rounds: 1, stop: StopReason::EndTurn, elapsed_secs: 1.0 },
            TranscriptLine::User("问题2".into()),
            TranscriptLine::Assistant("回答2".into()),
            TranscriptLine::Summary { rounds: 2, stop: StopReason::EndTurn, elapsed_secs: 3.5 },
        ]);
        let text = render_to_text(&mut app);
        assert!(text.contains("问题1"));
        assert!(text.contains("回答2"));
    }

    // 错误消息
    #[test]
    fn test_error_line() {
        let mut app = make_app_with_transcript(vec![
            TranscriptLine::Error("Login cancelled.".into()),
        ]);
        let text = render_to_text(&mut app);
        assert!(text.contains("Error"));
        assert!(text.contains("Login cancelled."));
    }

    // 长文本换行
    #[test]
    fn test_long_text_wraps() {
        let mut app = make_app_with_transcript(vec![
            TranscriptLine::Assistant("a".repeat(200)),
        ]);
        let text = render_to_text(&mut app);
        assert!(text.contains("aaa"));
    }

    // 空对话
    #[test]
    fn test_empty_transcript() {
        let mut app = make_app_with_transcript(vec![]);
        let text = render_to_text(&mut app);
        assert!(text.contains("no messages yet"));
    }

    // 滚动
    #[test]
    fn test_scroll_offset() {
        let mut app = make_app_with_transcript(vec![
            TranscriptLine::User("msg1".into()),
            TranscriptLine::Assistant("reply1".into()),
            TranscriptLine::User("msg2".into()),
            TranscriptLine::Assistant("reply2".into()),
        ]);
        app.follow = true;
        assert!(render_to_text(&mut app).contains("msg1"));

        app.follow = false;
        app.scroll_offset = 4;
        let text = render_to_text(&mut app);
        assert!(!text.contains("msg1"));
        assert!(text.contains("msg2"));
    }

    // Working 动画
    #[test]
    fn test_working_animation() {
        let mut app = make_app_with_transcript(vec![
            TranscriptLine::User("hello".into()),
        ]);
        app.is_turning = true;

        app.working_dot = 0;
        assert!(render_to_text(&mut app).contains("Working."));
        app.working_dot = 1;
        assert!(render_to_text(&mut app).contains("Working.."));
        app.working_dot = 2;
        assert!(render_to_text(&mut app).contains("Working..."));
    }

    // Status 面板
    #[test]
    fn test_status_panel() {
        let mut app = make_app_with_transcript(vec![]);
        app.show_status = true;
        let text = render_to_text(&mut app);
        assert!(text.contains("Provider:"));
        assert!(text.contains("Model:"));
    }
}
```

## 6. 两层如何互补

```
用户输入 "帮我登录"
        │
        ▼
┌─ 第一层 ──────────────────────────┐
│  Prompter.select("Select provider")│  ← FakePrompter 返回 "OpenAI"
│  → auth 存入 config               │  ← 断言 auth_for("openai").is_some()
│  → ModelCommand.select()           │  ← FakePrompter 返回 "gpt-4"
│  → agent 切模型                    │  ← 断言 model_id == "gpt-4"
├────────────────────────────────────┤
│  suspend → resume 成对             │  ← MockSurface 断言 ["suspend","resume"]
└────────────────────────────────────┘
        │
        ▼
┌─ 第二层 ──────────────────────────┐
│  app.transcript = [               │
│    User("帮我登录"),               │  ← 构造
│    Assistant("已切换到 gpt-4"),    │  ← 构造
│    Summary { rounds: 1, ... },    │  ← 构造
│  ]                                │
│  → TestBackend 渲染               │  ← 断言屏幕包含 "gpt-4" / checkmark
└────────────────────────────────────┘
```

**第一层测"脑子对不对"，第二层测"画面对不对"。** 两者独立、正交、可并行推进。

## 7. 改动清单

### 生产代码

| 文件 | 改动 | 行数 |
|------|------|------|
| `commands/mod.rs` | Prompter trait + PromptError + CommandContext 加 prompter 字段 + `for_test` helper | +50 |
| `commands/builtin.rs` | 4 个 inquire 调用点 → `ctx.prompter.*` | ~-10 ~+20 |
| `ui/mod.rs` | TuiSurface trait + dispatch_input 加 `surface` 参数 + 更新调用点 | +30 |

### 测试代码

| 文件 | 改动 | 行数 |
|------|------|------|
| `commands/builtin.rs` (tests) | 5 个决策逻辑测试 + FakePrompter + 17 个 CommandContext 构造点改为 `::for_test()` | +160 |
| `commands/mod.rs` (tests) | 1 个 CommandContext 构造点改为 `::for_test()` | ~0 |
| `ui/draw.rs` (tests) | 8 个渲染测试（复用 `make_app()` + `render_to_text()`） | +100 |

### 总计

~+350 行（~130 生产 + ~220 测试）

### CommandContext 改造量

生产 1 个构造点（`ui/mod.rs:245`）+ 测试 18 个构造点（`builtin.rs` 17 + `mod.rs` 1）= **19 个**。
通过 `CommandContext::for_test()` helper，测试构造点单行替换：

```rust
// commands/mod.rs #[cfg(test)]
impl<'a> CommandContext<'a> {
    pub fn for_test(
        agent: &'a mut Agent,
        config: &'a mut Config,
        state: &'a mut StateStore,
        prompter: &'a dyn Prompter,
    ) -> Self {
        Self { agent, config, state, prompter }
    }
}
```

测试改造前后对比：
```rust
// before（17 处相同模式）
let mut ctx = CommandContext { agent: &mut agent, config: &mut config, state: &mut state };

// after
let prompter = FakePrompter::with_answers(vec![...]);
let mut ctx = CommandContext::for_test(&mut agent, &mut config, &mut state, &prompter);
```

## 8. 风险与缓解

| 风险 | 缓解 |
|------|------|
| Prompter trait 方法不够用 | 有 default impl，新方法不破坏现有 fake |
| FakePrompter 队列耗尽 | 默认返回 Cancelled，测试自然失败 |
| `&dyn Prompter` async lifetime | CommandContext 已是 `'_`，可跟随 |
| TuiSurface 的 suspend/resume 签名需适配 terminal 引用 | 实施时将 terminal 绑定到 `CrosstermSurface`，trait 方法签名调整为不泛型 |
| dispatch_input 已有 9 参数，加 surface 后 10 个 | 后续可重构为参数 struct，本次不改 |

## 9. 实施顺序

1. **先做第二层**（渲染测试）：零改动，立刻写 8 个测试，验证渲染管线
2. **再做第一层**（Prompter trait）：引入 trait + fake，写 5 个决策测试
3. **合流**：两层测试一起跑 `cargo test`，确认完整覆盖
