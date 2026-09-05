# 基础设施补齐 — 架构设计

## 概述

为 YuShan v0 Runtime 补齐 9 项基础设施（含 JSONL Session 持久化），为 Coding Agent 层做准备。本文件记录三个候选方案的对比与最终推荐。

## 候选方案概览

| 维度 | A: 最小复杂度 | B: 可扩展优先 | C: 性能优先 |
|------|--------------|--------------|------------|
| 核心理念 | 能加字段就不加 trait | trait 优于 struct，面向接口 | 零拷贝，零堆分配 |
| System prompt | `Option<String>` | `SystemPrompt` enum (Static/Dynamic) | `Option<&'a str>` |
| ToolContext 路径 | `PathBuf` (owned) | `ExecutionEnvironment` struct | `&'a Path` (borrowed) |
| Approval | async trait 单一 | sync + async 双 trait + blanket impl | sync trait + `#[inline(always)]` |
| ModelRequest | owned Vec | owned + `#[non_exhaustive]` | `&'a [Message]` borrowed |
| ToolRegistry::specs() | 每轮重建 Vec | 每轮重建 Vec | **构建时缓存，返回 `&[ToolSpec]`** |

## 逐维度对比

### 1. 实现复杂度

| 指标 | A | B | C |
|------|---|---|---|
| 新增代码行 | ~120 | ~200 | ~180 |
| 新增类型 | 2 (ApprovalDecision, AutoApprove) | 6 (SystemPrompt, ExecutionEnvironment, PermissionKind, ApprovalDecision, AsyncApprovalHandler, PolicyApproval) | 3 (ApprovalDecision, AutoApprove, CachedApproval) |
| 新增 trait | 1 | 2 (+ blanket impl) | 1 |
| 改动文件 | 14 | 16 | 15 |
| lifetime 传播 | 无 | 无 | **高**：ModelRequest<'a> 传播到 Model trait, BasicLoop, 所有测试 |
| 测试改动 | ~15 处构造点 | ~20 处构造点 | **~30 处**（lifetime 注解） |
| 上手成本 | ⭐ 低 | ⭐⭐ 中 | ⭐⭐⭐ 高 |

**C 的 lifetime 传播是最大风险**：`ModelRequest<'a>` 改变了 `Model::complete` 的签名，传播到所有 Model 实现（MockModel、未来的 OpenAI adapter）。Rust 的 async + lifetime 交互在 `async_trait` 下容易出错（`'static` 约束冲突），需要仔细验证。

### 2. 性能特征

| 操作 | A/B (owned) | C (borrowed) | 差异 |
|------|-------------|--------------|------|
| 构造 ModelRequest (per round) | N_tools × ~100B clone + M_msgs × 32B clone | 0 (borrows) | C 胜 |
| ToolContext 构造 | 48B (2 PathBuf = 48B) | 16B (2 &Path) | 差异小 |
| ToolRegistry::specs() | 每轮 N × ~100B 重分配 | 0 (缓存 slice) | **C 胜，最大差异** |
| 审批检查 (Approved 路径) | ~10ns | ~5ns (inline) | 差异微小 |
| **每轮总开销** | **~500B-2KB 堆分配** | **0 堆分配，<200B 栈** | C 胜 |

**关键判断**：每轮的模型 RTT ≥ 100ms。基础设施的 500B-2KB 分配开销在 100ms 延迟面前完全可忽略。性能差异**不构成选择 C 的充分理由**。

### 3. 演进灵活性

| 扩展场景 | A | B | C |
|----------|---|---|---|
| 加 system prompt 动态模板 | 改 Option<String> 为新类型 | **已预留** SystemPrompt.Dynamic | 改 Option<&str> 为新类型 |
| 加 ToolContext 新字段 | 加字段（#[non_exhaustive]） | 加字段（ExecutionEnvironment 独立扩展） | 加字段（#[non_exhaustive]） |
| 加权限类别 | 改 ApprovalHandler | **已预留** PermissionKind | 改 ApprovalHandler |
| 加审批策略 | 改 ApprovalHandler | **已预留** PolicyApproval 枚举 | 加 CachedApproval |
| 加更多请求参数 | 加字段 | **已预留** ModelRequest #[non_exhaustive] | 加字段 |

B 的扩展点最多（15 个），但大部分是「预留」而非「需要」。A 和 C 在需要时也可以演进，只是需要小幅 breaking change。

### 4. 风险评估

| 风险 | A | B | C |
|------|---|---|---|
| lifetime 编译错误 | 无 | 无 | **高** |
| async_trait + lifetime 冲突 | 无 | 无 | **中** |
| 过度设计 | 低 | **中**（PermissionKind, SystemPrompt.Dynamic 在 v0 可能用不上） | 低 |
| 向后兼容破坏 | 低 | 低 | **中**（Model trait 签名变更） |
| 测试维护成本 | 低 | 中 | **高** |

### 5. 与业界参考对齐

| 参考 | 最佳对齐方案 |
|------|-------------|
| Claude Code 的 ToolContext (cwd + cancel) | A/C 的直接字段 vs B 的 ExecutionEnvironment |
| Claude Code 的 deny-first 权限 | B 的 PermissionKind 路由最接近，A/C 需要后补 |
| StrongDM 的 ExecutionEnvironment 抽象 | B 直接对齐 |
| StrongDM 的 head/tail 截断 | A 不做截断，B/C 在 loop 中做 |
| LangChain 的 dynamic_prompt | B 的 SystemPrompt.Dynamic 直接对齐 |
| 业界主流的 sync approval | A 用 async，B 用 sync+async，C 用 sync |

## 推荐方案：A 融合 B 的关键抽象 + C 的 ToolRegistry 缓存

### 核心决策

| 项目 | 决策 | 理由 |
|------|------|------|
| **ModelRequest** | owned，`Option<String>` for system | 避免 lifetime 传播（C 的最大风险），保留未来改为引用的能力 |
| **ToolContext 路径** | `PathBuf` (owned) | 同上，Tool 调用频率低，PathBuf clone 开销可忽略 |
| **ApprovalHandler** | **async trait**（A 的选择） | TUI 需要 async 用户交互，sync + block_on 桥接更复杂 |
| **ToolRegistry::specs()** | **构建时缓存，返回 `&[ToolSpec]`**（C 的贡献） | 这是唯一有实际性能收益的改动 |
| **Head/tail 截断** | **在 BasicLoop 中实现**（C 的贡献） | 工具输出截断是 loop 编排责任，不是工具自身责任 |
| **PermissionKind** | **不做** | v0 只有 AutoApprove，权限类别是 v1 需求 |
| **SystemPrompt.Dynamic** | **不做** | v0 用 Option<String> 够用，动态模板是 v1 需求 |
| **ExecutionEnvironment** | **不做** | PathBuf 直接加到 ToolContext，v0 不需要沙箱抽象 |

### 最终类型设计

#### ModelRequest（agent-model/src/request.rs）

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
}
```

#### ModelResponse（agent-model/src/request.rs）

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub message: Message,
    pub usage: Usage,
    #[serde(default)]
    pub stop_reason: Option<String>,
}
```

#### ToolContext（agent-tool/src/context.rs）

```rust
#[non_exhaustive]
pub struct ToolContext<'a> {
    pub cancel: &'a CancelToken,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
}
```

#### ToolError（agent-tool/src/error.rs）

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ToolError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("execution error: {0}")]
    Execution(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("timeout: {0}")]
    Timeout(String),
}
```

#### ApprovalHandler（agent-tool/src/approval.rs）

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approved,
    Denied { reason: String },
}

#[async_trait::async_trait]
pub trait ApprovalHandler: Send + Sync {
    fn needs_approval(&self, tool_name: &str, input: &serde_json::Value) -> bool;
    async fn request_approval(
        &self,
        tool_name: &str,
        input: &serde_json::Value,
    ) -> ApprovalDecision;
}

pub struct AutoApprove;

#[async_trait::async_trait]
impl ApprovalHandler for AutoApprove {
    fn needs_approval(&self, _tool_name: &str, _input: &serde_json::Value) -> bool {
        false
    }
    async fn request_approval(
        &self,
        _tool_name: &str,
        _input: &serde_json::Value,
    ) -> ApprovalDecision {
        ApprovalDecision::Approved
    }
}
```

#### RunLimits（agent-component/src/limits.rs）

```rust
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RunLimits {
    pub max_rounds: u32,
    /// Bash 工具的默认超时（秒）。None 表示不限时（对齐 Pi）。
    pub bash_timeout: Option<u64>,
    /// 模型 context window 大小（token 数）。用于上下文压缩触发。
    pub context_window: usize,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            max_rounds: 10,
            bash_timeout: None, // 对齐 Pi：无默认超时
            context_window: 128_000, // 默认 128K
        }
    }
}
```

#### ToolRegistry::specs()（agent-tool/src/registry.rs）

#### ToolSpec 构造器（agent-tool/src/spec.rs）

```rust
#[non_exhaustive]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolSpec {
    /// 创建 ToolSpec（对齐 #[non_exhaustive] 后的外部构造需求）
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}
```

#### ToolRegistry 缓存（agent-tool/src/registry.rs）

```rust
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
    specs: Vec<ToolSpec>,  // 构建时缓存
}

impl ToolRegistry {
    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }
}
```

#### JsonlSession（agent-session/src/jsonl.rs）

```rust
use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub struct JsonlSession {
    path: PathBuf,
    messages: Vec<Message>,
}

impl JsonlSession {
    /// 打开或创建 JSONL session 文件
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let messages = if path.exists() {
            let file = File::open(&path).await.map_err(|e| {
                SessionError::Storage(format!("failed to open session: {e}"))
            })?;
            let reader = BufReader::new(file);
            let mut lines = reader.lines();
            let mut msgs = Vec::new();
            while let Some(line) = lines.next_line().await.map_err(|e| {
                SessionError::Storage(format!("failed to read session: {e}"))
            })? {
                let msg: Message = serde_json::from_str(&line).map_err(|e| {
                    SessionError::Storage(format!("corrupt session line: {e}"))
                })?;
                msgs.push(msg);
            }
            msgs
        } else {
            Vec::new()
        };
        Ok(Self { path, messages })
    }
}

#[async_trait::async_trait]
impl Session for JsonlSession {
    fn messages(&self) -> &[Message] {
        &self.messages
    }

    async fn append(&mut self, message: Message) -> Result<(), SessionError> {
        // 1. 追加到内存
        self.messages.push(message.clone());
        // 2. 追加一行 JSON 到文件（原子追加，不重写全量）
        let line = serde_json::to_string(&message).map_err(|e| {
            SessionError::Storage(format!("serialize failed: {e}"))
        })?;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .map_err(|e| {
                SessionError::Storage(format!("failed to open session file: {e}"))
            })?;
        file.write_all(line.as_bytes()).await.map_err(|e| {
            SessionError::Storage(format!("write failed: {e}"))
        })?;
        file.write_all(b"\n").await.map_err(|e| {
            SessionError::Storage(format!("write newline failed: {e}"))
        })?;
        Ok(())
    }
}
```

**设计要点**：
- 启动时读取全量 JSONL 恢复到内存（文件通常 < 1MB，可接受）
- 追加写入，不重写全量（性能友好，crash-safe）
- 单行 JSON 格式，可读可 grep，便于调试
- `SessionError::Storage` 复用现有错误类型

**AgentBuilder 集成**：

```rust
// agent-runtime/src/builder.rs
impl AgentBuilder {
    pub fn session_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.session_path = Some(path.into());
        self
    }
    // build() 中：如果 session_path 存在，自动创建 JsonlSession
    // 否则使用提供的 session 或默认 MemorySession
}
```

#### RuntimeContext（agent-component/src/context.rs）

```rust
#[non_exhaustive]
pub struct RuntimeContext<'a> {
    pub model: &'a dyn Model,
    pub registry: &'a ToolRegistry,
    pub session: &'a mut dyn Session,
    pub events: &'a mut dyn EventSink,
    pub cancel: &'a CancelToken,
    pub limits: RunLimits,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
    pub approval: Option<&'a dyn ApprovalHandler>,
}
```

#### BasicLoop 中的集成（agent-loop/src/basic.rs）

##### 关键改进 1：工具错误恢复（对齐 Pi）

**v0 问题**：工具失败 → `had_tool_error` → `RunFailed` → session 终止。
**Pi 的做法**：工具失败 → `is_error: true` 结果回喂模型 → 模型决定下一步。

```rust
// 工具执行：超时保护
let result = match tokio::time::timeout(
    ctx.limits.bash_timeout,
    tool.call(tool_args.clone(), tool_ctx),
).await {
    Ok(Ok(r)) => r,
    Ok(Err(ToolError::PermissionDenied(msg))) => {
        // 权限拒绝：回喂模型，不终止 run
        CoreToolResult { content: format!("Permission denied: {msg}"), is_error: true }
    }
    Ok(Err(ToolError::Timeout(msg))) => {
        // 超时：回喂模型，不终止 run
        CoreToolResult { content: format!("Tool timed out: {msg}"), is_error: true }
    }
    Ok(Err(ToolError::Execution(msg))) => {
        // 执行错误：回喂模型，不终止 run（对齐 Pi）
        CoreToolResult { content: msg, is_error: true }
    }
    Ok(Err(ToolError::InvalidInput(msg))) => {
        // 输入错误：回喂模型，不终止 run
        CoreToolResult { content: format!("Invalid input: {msg}"), is_error: true }
    }
    Err(_elapsed) => {
        // 超时：回喂模型，不终止 run
        CoreToolResult { content: "Command timed out".into(), is_error: true }
    }
};
// 注意：工具输出截断由工具自身负责（Read=head，Bash=tail），
// BasicLoop 不做二次截断。对齐 Pi 的设计。
```

**防循环安全**（修复 M3）：跟踪连续失败次数，防止模型陷入死循环：

```rust
// 在 BasicLoop run_turn 中维护：
let mut consecutive_errors: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
const MAX_CONSECUTIVE_ERRORS: u32 = 3;

// 工具结果处理后：
if result.is_error {
    let count = consecutive_errors.entry(tool_name.clone()).or_insert(0);
    *count += 1;
    if *count >= MAX_CONSECUTIVE_ERRORS {
        tool_results_for_message.push(ContentBlock::ToolResult {
            tool_call_id: tool_call_id.clone(),
            content: format!("Tool '{tool_name}' failed {MAX_CONSECUTIVE_ERRORS} times consecutively. Try a different approach."),
            is_error: true,
        });
    }
} else {
    consecutive_errors.remove(tool_name);
}
```

**LoopError::Tool 变更**（修复 M4）：保留但标记为 reserved，仅用于 registry miss：

```rust
pub enum LoopError {
    Model(ModelError),
    Event(EventError),
    /// 保留：仅用于 registry miss（tool not found）等结构性错误。
    /// 工具执行错误改为回喂模型，不再通过此路径。
    Tool(String),
}
```

**v0 之后**：所有 `ToolError` 变体都回喂模型，不再终止 run。
模型收到错误后可以决定：重试、换方法、向用户解释。

##### 关键改进 2：Token 估算（新增能力）

```rust
/// 估算消息的 token 数（chars/4 启发式，对齐 Pi estimateTokens）
fn estimate_tokens(message: &Message) -> usize {
    message.content.iter().map(|block| match block {
        ContentBlock::Text { text } => text.len() / 4,
        ContentBlock::ToolUse { arguments, .. } => arguments.to_string().len() / 4,
        ContentBlock::ToolResult { content, .. } => content.len() / 4,
    }).sum()
}

/// 估算整个 session 的 token 数
fn estimate_session_tokens(messages: &[Message]) -> usize {
    messages.iter().map(|m| estimate_tokens(m)).sum()
}
```

##### 关键改进 3：上下文压缩触发（新增能力）

在 BasicLoop 每轮模型调用前检查，防止撞 context window 上限：

```rust
const RESERVE_TOKENS: usize = 16384; // 保留 token 数（对齐 Pi reserveTokens）
const KEEP_RECENT_TOKENS: usize = 20000; // 保留最近 token 数（对齐 Pi keepRecentTokens）

// 在每轮模型调用前：
let total_tokens = estimate_session_tokens(ctx.session.messages());
let context_window = 128_000; // TODO: 从模型配置获取

if total_tokens > context_window - RESERVE_TOKENS {
    // 触发压缩：用模型生成摘要，替换旧消息
    compact_session(ctx, context_window).await?;
}

async fn compact_session(
    ctx: &mut RuntimeContext<'_>,
    context_window: usize,
) -> Result<(), LoopError> {
    let messages = ctx.session.messages();

    // 1. 从最新消息向前扫描，找到保留点
    let mut keep_tokens = 0;
    let mut cut_point = messages.len();
    for (i, msg) in messages.iter().enumerate().rev() {
        keep_tokens += estimate_tokens(msg);
        if keep_tokens >= KEEP_RECENT_TOKENS {
            cut_point = i;
            break;
        }
    }

    // 2. 用模型对 cut_point 之前的消息生成摘要
    let to_summarize = &messages[..cut_point];
    let summary = generate_summary(ctx, to_summarize).await?;

    // 3. 替换 session 内容：摘要 + 保留的最近消息
    let kept = messages[cut_point..].to_vec();
    ctx.session.clear().await; // 需要 Session trait 新增 clear() 方法
    ctx.session.append(Message {
        role: Role::System,
        content: vec![ContentBlock::Text { text: summary }],
    }).await.map_err(|_| LoopError::Event(agent_core::EventError::SendFailed))?;
    for msg in kept {
        ctx.session.append(msg).await
            .map_err(|_| LoopError::Event(agent_core::EventError::SendFailed))?;
    }

    Ok(())
}

async fn generate_summary(
    model: &dyn Model,    // 直接传引用，避免 borrow 冲突（修复 C1）
    messages: &[Message],
) -> Result<String, LoopError> {
    let summary_request = ModelRequest {
        messages: messages.to_vec(),
        tools: vec![], // 摘要不需要工具
        system: Some("Summarize this conversation. Include: Goal, Progress, Decisions made, Next steps. Be concise.".into()),
        max_tokens: Some(2048),
        temperature: None,
    };
    let mut noop = agent_event::NoopEventSink;
    let mut forwarder = Forwarder { sink: &mut noop };
    let response = model.complete(summary_request, &mut forwarder).await
        .map_err(LoopError::Model)?;

    // 提取摘要文本
    let mut summary = String::new();
    for block in &response.message.content {
        if let ContentBlock::Text { text } = block {
            summary.push_str(text);
        }
    }
    Ok(summary)
}
```

**Token 估算改进**（修复 M1：CJK 文本补偿）：

```rust
/// 估算消息的 token 数（chars/4 启发式 + CJK 补偿）
fn estimate_tokens(message: &Message) -> usize {
    message.content.iter().map(|block| match block {
        ContentBlock::Text { text } => estimate_text_tokens(text),
        ContentBlock::ToolUse { arguments, .. } => estimate_text_tokens(&arguments.to_string()),
        ContentBlock::ToolResult { content, .. } => estimate_text_tokens(content),
    }).sum()
}

/// 文本 token 估算：英文 chars/4，CJK 字符 chars/1.5
fn estimate_text_tokens(text: &str) -> usize {
    let mut cjk_chars = 0;
    let mut total_chars = text.len();
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk_chars += 1;
            // CJK 字符在 UTF-8 中占 3 bytes，但 chars/4 计算了 bytes
            // 需要减去多算的部分并用 CJK 估算替代
            total_chars -= 2; // 3 bytes UTF-8 - 1 (已在 text.len() 中计为 3, /4 = 0.75)
        }
    }
    let ascii_tokens = (total_chars - cjk_chars * 3) / 4; // 非 CJK 部分
    let cjk_tokens = (cjk_chars as f64 * 1.5) as usize;   // CJK 部分
    ascii_tokens + cjk_tokens
}

fn is_cjk(ch: char) -> bool {
    matches!(ch,
        '\u{4E00}'..='\u{9FFF}' |   // CJK Unified
        '\u{3400}'..='\u{4DBF}' |   // CJK Extension A
        '\u{F900}'..='\u{FAFF}' |   // CJK Compatibility
        '\u{3000}'..='\u{303F}' |   // CJK Symbols
        '\u{FF00}'..='\u{FFEF}'     // Fullwidth
    )
}
```

**上下文溢出恢复**（修复 C2）：

```rust
// 在 BasicLoop 的模型调用处增加 overflow 恢复：
match ctx.model.complete(request, &mut forwarder).await {
    Ok(response) => { /* 正常处理 */ }
    Err(ModelError::Provider(msg)) if msg.contains("context_length_exceeded")
                                    || msg.contains("maximum context") => {
        // 紧急压缩：跳过阈值检查，直接压缩最旧的 50% 消息
        emergency_compact(ctx).await?;
        // 重试一次（不递归，最多一次）
        continue;
    }
    Err(e) => {
        let _ = ctx.events.emit(AgentEvent::RunFailed { error: e.to_string() });
        return Err(LoopError::Model(e));
    }
}
```

**摘要消息角色**（修复 M2：避免 mid-conversation System 消息）：

```rust
// compact_session 中，摘要使用 Role::User 而非 Role::System
// 对齐 Pi 的做法：摘要作为 user message 注入
Message {
    role: Role::User,  // 非 System，兼容所有 OpenAI-compatible API
    content: vec![ContentBlock::Text {
        text: format!("[Context Summary — previous messages have been compressed]\n\n{summary}"),
    }],
}
```

**Session trait 扩展**（新增 `clear()` + 原子写入注记）：

```rust
#[async_trait]
pub trait Session: Send {
    fn messages(&self) -> &[Message];
    async fn append(&mut self, message: Message) -> Result<(), SessionError>;
    /// 清空所有消息（用于上下文压缩后重建）
    async fn clear(&mut self) -> Result<(), SessionError> { Ok(()) }
}

// JsonlSession::clear() 实现注记：
// 使用 tmp file + atomic rename 防止 crash 时数据丢失（修复 C3）
// 1. 写入新内容到 {path}.tmp
// 2. fs::rename({path}.tmp, {path}) — 同文件系统上是原子操作
```

### #[non_exhaustive] 补齐清单

| 类型 | crate | 文件 |
|------|-------|------|
| `StopReason` | agent-core | stop.rs |
| `ToolError` | agent-tool | error.rs |
| `ToolSpec` | agent-tool | spec.rs |
| `RunLimits` | agent-component | limits.rs |

### 模块归属汇总

| 改动 | Crate | 文件 | 类型 |
|------|-------|------|------|
| ModelRequest + ModelResponse | agent-model | request.rs | 修改 |
| ToolContext + ToolError | agent-tool | context.rs, error.rs | 修改 |
| ApprovalHandler | agent-tool | **approval.rs (新)** | 新增 |
| ToolSpec + ToolRegistry | agent-tool | spec.rs, registry.rs | 修改 |
| RunLimits + RuntimeContext | agent-component | limits.rs, context.rs | 修改 |
| BasicLoop 审批/超时/截断 | agent-loop | basic.rs | 修改 |
| AgentBuilder 扩展 | agent-runtime | builder.rs, agent.rs, prelude.rs | 修改 |
| JsonlSession 持久化 | agent-session | **jsonl.rs (新)** | 新增 |

### 实现复杂度估算

| 指标 | 估算 |
|------|------|
| 新增代码行 | ~250 行（approval.rs ~60 行，jsonl.rs ~70 行，其余散布） |
| 修改文件 | ~15 个 |
| 新增类型 | 3 (ApprovalDecision, AutoApprove, JsonlSession) |
| 修改类型 | 9 |
| 新增 trait | 1 (ApprovalHandler) |
| 测试改动 | ~20 处构造点更新 |
| 预估总工作量 | 3-4 小时 |

## 未选方案及理由

### 未选 B（可扩展优先）的原因

- **过度设计风险**：`SystemPrompt` enum 的 Dynamic 变体、`ExecutionEnvironment` struct、`PermissionKind` enum 在 v0 用不上
- **复杂度代价**：6 个新类型 + 2 个 trait + blanket impl，对 ~2k 行项目来说仪式感过重
- **可延后**：这些抽象在需要时可以自然引入（SystemPrompt enum 可以从 Option<String> 演进）

### 未选 C（性能优先）的原因

- **lifetime 传播风险**：`ModelRequest<'a>` 改变 `Model::complete` 签名，传播到所有 Model 实现
- **性能收益不显著**：每轮 500B-2KB 分配 vs 100ms+ 模型 RTT，差异 < 0.001%
- **async_trait 冲突**：Rust 的 async + lifetime 交互在 async_trait 下容易产生 `'static` 约束冲突
- **可延后**：生命周期优化可以在性能 profiling 确认瓶颈后再做

### 采纳 C 的一个贡献

**ToolRegistry::specs() 缓存**：这是唯一一个无风险且有实际收益的性能改进。构建时缓存 specs，返回 `&[ToolSpec]`，消除每轮的 Vec 重建。已纳入推荐方案。

## 所有决策记录

| 问题 | 决策 | 理由 |
|------|------|------|
| system prompt 注入方式 | `ModelRequest.system: Option<String>` | 影响面最小，provider 适配器自行翻译 |
| ToolContext 扩展策略 | 直接加 PathBuf 字段 | `#[non_exhaustive]` 保护 |
| 审批机制设计 | async trait `ApprovalHandler` | TUI 场景必须 async |
| bash 安全边界 | 无默认超时 + 配置化审批 | 对齐 Pi：用户不传 timeout 则不限时 |
| CLI 框架 | 简易 stdin/stdout | 不引入 TUI 框架，v1 可替换 |
| Model adapter | OpenAI-compatible + ProviderCompat | DeepSeek/MiniMax 优先 |
| edit 工具格式 | 批量 `edits[{oldText, newText}]` + fuzzy matching | 对齐 Pi editSchema + fuzzyFindText |
| 工作目录管理 | 固定 cwd（不支持 cd） | 简单安全 |
| 截断策略 | **工具内截断**（非 loop） | 对齐 Pi：Read=head，Bash=tail，50KB/2000行 |
| Edit fuzzy matching | v0 实现基础 fuzzy | NFKC + smart quote + dash normalization + trim |
| Edit legacy format | v0 兼容 | prepareEditArguments 兼容旧格式 |
| Bash streaming | v0 事后截断 | OutputAccumulator 流式留 v1 |
| v0 安全原则 | **更新 v0 design.md** 显式记录内置审批 | 消除矛盾 |
| OpenAI compat flags | **ProviderCompat struct** | 简单结构化，非 Pi 式完整 schema |
| ToolSpec 构造 | 新增 `ToolSpec::new()` 构造器 | 解决 #[non_exhaustive] 阻断外部测试 |
| ModelResponse.stop_reason | 保留 Option<String>，adapter 层做映射 | 不污染 core 的 StopReason enum |
| **工具错误恢复** | **错误回喂模型，不终止 run** | 对齐 Pi：模型决定下一步（重试/换方法/解释） |
| **上下文压缩** | **chars/4 估算 + 阈值触发 + LLM 摘要** | 对齐 Pi reserveTokens=16384, keepRecent=20000 |
| **Token 计数** | **chars/4 启发式**（对齐 Pi estimateTokens） | 无需精确 tokenizer，够用 |

## Coding Agent MVP 层设计

基础设施之上的 Coding Agent 完整交付。

### Agent 结构变更

新增字段需要流经 Agent → RuntimeContext → BasicLoop。Agent 需持有新增的共享状态：

```rust
// agent-runtime/src/agent.rs — 新增字段
pub struct Agent {
    loop_impl: Box<dyn AgentLoop>,
    model: Box<dyn Model>,
    registry: ToolRegistry,
    session: Box<dyn Session>,
    events: Box<dyn EventSink>,
    cancel: CancelToken,
    limits: RunLimits,
    // --- 新增 ---
    cwd: PathBuf,
    workspace_root: PathBuf,
    approval: Option<Box<dyn ApprovalHandler>>,
    system_prompt: Option<String>,
}

// Agent::run_turn() 中构造 RuntimeContext 时传入新字段：
impl Agent {
    pub async fn run_turn(&mut self, input: AgentInput) -> Result<RunResult, LoopError> {
        let mut ctx = RuntimeContext::new(
            &*self.model,
            &self.registry,
            &mut *self.session,
            &mut *self.events,
            &self.cancel,
            self.limits.clone(),
            self.cwd.clone(),           // 新增
            self.workspace_root.clone(), // 新增
            self.approval.as_deref(),    // 新增
            self.system_prompt.clone(),  // 新增
        );
        self.loop_impl.run_turn(input, &mut ctx).await
    }
}
```

### OpenAI-compatible Model Adapter

**位置**：`adapters/model-openai-compatible/`（新 crate）

**核心接口**：

```rust
pub struct OpenAICompatibleConfig {
    pub api_base: String,       // e.g. "https://api.deepseek.com/v1"
    pub api_key: String,
    pub model: String,          // e.g. "deepseek-chat"
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Provider-specific compatibility flags
    pub compat: ProviderCompat,
}

/// Provider-specific 行为差异配置（对齐 Pi OpenAICompletionsCompatSchema）
#[derive(Debug, Clone, Default)]
pub struct ProviderCompat {
    /// max_tokens 字段名: "max_tokens" (default) vs "max_completion_tokens" (OpenAI o1)
    pub max_tokens_field: MaxTokensField,
    /// 是否在 SSE 中解析 usage 字段
    pub supports_usage_in_streaming: bool,
    /// 是否支持 finish_reason 字段
    pub supports_finish_reason: bool,
    /// reasoning_content 的解析格式（DeepSeek 用 "deepseek" 格式）
    pub thinking_format: Option<ThinkingFormat>,
    /// 是否需要 tool result 消息带 name 字段
    pub requires_tool_result_name: bool,
}

pub struct OpenAICompatibleModel {
    config: OpenAICompatibleConfig,
    client: reqwest::Client,
}

impl Model for OpenAICompatibleModel {
    fn model_id(&self) -> &str { &self.config.model }

    async fn complete(
        &self,
        request: ModelRequest,
        sink: &mut dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError> { ... }
}
```

**协议兼容**：基于 OpenAI chat completion 协议，通过 `ProviderCompat` 处理差异。

| 平台 | api_base | model 示例 | compat 注意项 |
|------|----------|-----------|-------------|
| **DeepSeek** | `https://api.deepseek.com/v1` | `deepseek-chat`, `deepseek-reasoner` | reasoning_content 字段、thinking_format="deepseek" |
| **MiniMax** | `https://api.minimax.chat/v1` | `MiniMax-Text-01` | 部分模型 tool_calls 作为 text 返回 |
| **opencode-go/zen** | 用户自定义 | — | 标准兼容 |

**请求格式**（v0 内部 → OpenAI API 格式翻译）：

```text
v0 内部格式:
  Message { role: User, content: [ToolResult { tool_call_id, content, is_error }] }

↓ 适配器翻译 ↓

OpenAI API 格式:
  { role: "tool", tool_call_id: "...", content: "..." }
```

完整请求示例：

```json
{
  "model": "deepseek-chat",
  "messages": [
    {"role": "system", "content": "You are a coding agent..."},
    {"role": "user", "content": "hello"},
    {"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{...}"}}]},
    {"role": "tool", "tool_call_id": "call_1", "content": "file contents..."}
  ],
  "tools": [{"type": "function", "function": {"name": "read", "parameters": {...}}}],
  "stream": true
}
```

**格式翻译清单**（适配器必须处理）：

| v0 内部 | OpenAI API | 翻译方向 |
|---------|-----------|---------|
| `ContentBlock::ToolUse { id, name, arguments }` | `tool_calls[{ id, type:"function", function:{ name, arguments } }]` | response 解析 |
| `ContentBlock::ToolResult { tool_call_id, content, is_error }` | `{ role:"tool", tool_call_id, content }` | request 构造 |
| `ToolSpec { name, description, parameters }` | `{ type:"function", function:{ name, description, parameters } }` | request 构造 |
| `ModelResponse.stop_reason: Option<String>` | `choices[0].finish_reason` | response 解析 |
| `Usage { input_tokens, output_tokens }` | `usage.prompt_tokens` / `usage.completion_tokens` | response 解析 |

**流式响应解析**：SSE 格式，每行 `data: {...}`，解析 `choices[0].delta`。对齐 Pi 的 SSE 处理：支持增量 tool_calls 参数拼接（`delta.tool_calls[i].function.arguments` 逐块到达）。

**依赖**：`reqwest`（HTTP）+ `tokio`（runtime）+ `serde_json`（已有）

**workspace 集成**：

```toml
# adapters/model-openai-compatible/Cargo.toml
[dependencies]
agent-core = { path = "../../crates/agent-core" }
agent-model = { path = "../../crates/agent-model" }
agent-event = { path = "../../crates/agent-event" }
reqwest = { version = "0.12", features = ["json", "stream"] }
tokio = { version = "1", features = ["rt", "time", "io-util", "fs"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
async-trait = "0.1"
futures = "0.3"
eventsource-stream = "0.2"
```

### 4 个 Coding Tools（对齐 Pi 接口）

**位置**：`adapters/tools-basic/`（新 crate）

所有工具实现 `agent_tool::Tool` trait，构造时接受 `workspace: PathBuf`。参数 schema、行为和输出格式严格对齐 Pi 源码（`packages/coding-agent/src/core/tools/`）。

#### Read Tool（对齐 Pi `read.ts`）

```rust
pub struct ReadTool { workspace: PathBuf }

// 参数 schema (对齐 Pi readSchema):
// { "path": "string (required)", "offset": "number (optional)", "limit": "number (optional)" }
//
// 行为:
// - 读取文件内容，输出 cat -n 格式（行号从 1 开始）
// - offset: 1-indexed 起始行号
// - limit: 最大读取行数
// - 截断策略: head 截断（保留前面），2000 行 / 50KB 取先触发者
// - 截断后输出续读提示: "[Showing lines X-Y of Z. Use offset=N to continue.]"
// - 首行超 50KB: 提示用 bash sed 读取
// - 路径: 相对路径基于 workspace 解析
//
// 输出格式:
// - 成功: 带行号的文件内容 + 可能的截断提示
// - 文件不存在: 错误信息
// - 权限不足: 错误信息
```

#### Write Tool（对齐 Pi `write.ts`）

```rust
pub struct WriteTool { workspace: PathBuf }

// 参数 schema (对齐 Pi writeSchema):
// { "path": "string (required)", "content": "string (required)" }
//
// 行为:
// - 创建新文件或完全覆盖已有文件
// - 自动创建父目录（recursive mkdir）
// - 路径: 相对路径基于 workspace 解析
//
// 输出格式:
// - 成功: "Successfully wrote to {path}"
// - 错误: 错误信息
```

#### Edit Tool（对齐 Pi `edit.ts`，批量编辑 + fuzzy matching）

```rust
pub struct EditTool { workspace: PathBuf }

// 参数 schema (对齐 Pi editSchema):
// {
//   "path": "string (required)",
//   "edits": [
//     { "oldText": "string (required)", "newText": "string (required)" }
//   ] (required, 至少 1 个)
// }
//
// === Legacy format 兼容（对齐 Pi prepareEditArguments）===
// 模型可能发送非标准格式，需全部兼容：
// - 顶层 oldText/newText 而非 edits[] 数组（Claude 旧版行为）
// - edits 为 JSON string 而非 array（Opus 4.6、GLM-5.1 行为）
// - edits 为单对象而非数组
//
// === 匹配策略 ===
// 1. 精确匹配（exact match）：oldText 与文件内容完全一致
// 2. 模糊匹配（fuzzy match，对齐 Pi fuzzyFindText）：
//    - Unicode NFKC 归一化
//    - smart quote 归一化（" → " , ' → '）
//    - Unicode dash 归一化（– — → -）
//    - 尾部空白剥离
//    - Tab/空格混合归一化
// 3. 模糊匹配仅在精确匹配失败时触发，且仅当恰好一个位置匹配时采用
//
// === 行为约束 ===
// - 所有 edits 基于原始文件匹配（非增量应用）
// - 每个 oldText 在归一化后的文件中必须唯一匹配
// - 不允许重叠或嵌套的 edits
// - BOM 处理: 读取时剥离 BOM，写入时保留
// - 换行符: 检测原始换行风格（LF/CRLF），编辑后恢复
//
// === 输出格式 ===
// - 成功: "Successfully replaced {N} block(s) in {path}."
// - 同时返回 diff 和 patch（对齐 Pi EditToolDetails）：
//   { diff: "unified diff string", patch: "git patch format",
//     firstChangedLine: number }
// - oldText 未找到: 错误信息 + 最近似匹配建议
// - 文件不存在: 错误信息
```

#### Bash Tool（对齐 Pi `bash.ts`，进程组管理 + tail 截断）

```rust
pub struct BashTool { workspace: PathBuf }

// 参数 schema (对齐 Pi bashSchema):
// { "command": "string (required)", "timeout": "number (optional)" }
//
// === 进程管理（对齐 Pi 的 BashOperations）===
// - 使用进程组（detached: true on Unix），确保超时/取消时杀整个进程树
// - 超时后: SIGTERM → 等待 → SIGKILL（对齐 Pi killProcessTree）
// - 取消（CancelToken）: 通过 AbortSignal 机制传播
// - stdout + stderr 合并收集到 OutputAccumulator
//
// === 截断策略（对齐 Pi truncateTail）===
// - tail 截断（保留最后），2000 行 / 50KB 取先触发者
// - 截断后输出: "[Showing lines X-Y of Z. Full output: {path}]"
// - 完整输出保存到临时文件（/tmp/yushan-bash-*.log）
//
// === timeout 语义（对齐 Pi）===
// - timeout 单位：秒（非 ms）
// - 无默认超时（对齐 Pi，用户不传则不限时）
// - 超时上限: 2147483 秒（约 24.8 天，对齐 Pi MAX_TIMEOUT_MS）
//
// === 输出格式 ===
// - 成功 (exit 0): 命令输出
// - 非零退出: 错误 + "\n\nCommand exited with code {N}"
// - 超时: 错误 + "\n\nCommand timed out after {N} seconds"
// - 中止: 错误 + "\n\nCommand aborted"
```

### 截断常量（对齐 Pi `truncate.ts`）

```rust
// 工具内截断常量（每个工具独立使用）
const DEFAULT_MAX_LINES: usize = 2000;
const DEFAULT_MAX_BYTES: usize = 50 * 1024; // 50KB

// Read: head 截断（保留前面）
// Bash: tail 截断（保留最后）
// 截断由工具自身负责，不是 BasicLoop 的职责
```

### System Prompt 多层组合

对齐 Pi 的多层组合架构（`buildSystemPrompt` + `loadProjectContextFiles`），简化为 v0 三层：

```text
┌─────────────────────────────────────────┐
│  Layer 5: 项目上下文（AGENTS.md）        │  ← 自动发现，<project_context> 包裹
├─────────────────────────────────────────┤
│  Layer 4: 追加指令（APPEND_SYSTEM.md）   │  ← 用户自定义追加
├─────────────────────────────────────────┤
│  Layer 3: 基础 prompt                    │  ← 默认模板 or 用户覆盖
├─────────────────────────────────────────┤
│  Layer 2: Tool guidelines               │  ← per-tool snippet + rules
├─────────────────────────────────────────┤
│  Layer 1: Tool 列表                      │  ← 根据注册工具动态生成
└─────────────────────────────────────────┘
```

#### 项目上下文加载（对齐 Pi `loadProjectContextFiles`）

```rust
use std::path::{Path, PathBuf};

/// 加载的上下文文件
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

/// 从工作目录向上查找 AGENTS.md / YUSHAN.md（对齐 Pi loadProjectContextFiles）
/// 对齐 Pi: AGENTS.override.md > AGENTS.md > CLAUDE.md
/// 注意：同步 I/O（文件 <10KB，每 turn 一次）。如需 async 可用 spawn_blocking。
pub fn load_project_context(cwd: &Path) -> Vec<ContextFile> {
    let mut context_files = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // 1. 全局上下文: ~/.yushan/AGENTS.md（用 std::env 避免 dirs 依赖）
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        try_load_context_file(&PathBuf::from(home).join(".yushan/AGENTS.md"), &mut context_files, &mut seen);
    }

    // 2. 从 cwd 向上查找（对齐 Pi 的 ancestorContextFiles）
    let mut current = cwd.to_path_buf();
    let mut ancestor_files = Vec::new();
    loop {
        // 优先级: AGENTS.override.md > AGENTS.md > YUSHAN.md > CLAUDE.md
        for name in &["AGENTS.override.md", "AGENTS.md", "YUSHAN.md", "CLAUDE.md"] {
            let path = current.join(name);
            if path.exists() && !seen.contains(&path) {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    ancestor_files.push(ContextFile { path: path.clone(), content });
                    seen.insert(path);
                }
            }
        }
        let parent = current.parent();
        if parent.is_none() || parent == current.as_path() { break; }
        current = parent.unwrap().to_path_buf();
    }
    // 祖先文件按从远到近排列（Pi 的 unshift 行为）
    ancestor_files.reverse();
    context_files.extend(ancestor_files);

    context_files
}

fn try_load_context_file(path: &Path, files: &mut Vec<ContextFile>, seen: &mut std::collections::HashSet<PathBuf>) {
    if path.exists() && !seen.contains(path) {
        if let Ok(content) = std::fs::read_to_string(path) {
            files.push(ContextFile { path: path.to_path_buf(), content });
            seen.insert(path.to_path_buf());
        }
    }
}
```

#### Prompt 来源优先级

```rust
/// 确定基础 system prompt 的来源（对齐 Pi discoverSystemPromptFile）
pub fn resolve_system_prompt(cwd: &Path) -> Option<String> {
    // 1. CLI 参数 --system-prompt（最高优先）
    // 2. 环境变量 YUSHAN_SYSTEM_PROMPT
    // 3. {cwd}/.yushan/SYSTEM.md（对齐 Pi projectPath）
    // 4. ~/.yushan/SYSTEM.md（对齐 Pi globalPath）
    // 5. None → 使用内置默认 prompt
    todo!()
}

/// 追加指令来源
pub fn resolve_append_prompt(cwd: &Path) -> String {
    // 1. CLI 参数 --append-system-prompt
    // 2. {cwd}/.yushan/APPEND_SYSTEM.md
    // 3. ~/.yushan/APPEND_SYSTEM.md
    // 4. 空字符串
    todo!()
}
```

#### Prompt 组装

```rust
pub fn build_system_prompt(
    cwd: &Path,
    registered_tools: &[&str],   // 已注册的工具名列表
    custom_prompt: Option<&str>,  // 用户覆盖的 prompt
    append_prompt: &str,          // 追加指令
    context_files: &[ContextFile], // 项目上下文
) -> String {
    let base = match custom_prompt {
        Some(p) => p.to_string(),
        None => default_coding_prompt(registered_tools, cwd),
    };

    let mut prompt = base;

    if !append_prompt.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(append_prompt);
    }

    // 项目上下文包裹在 <project_context> 标签中（对齐 Pi）
    if !context_files.is_empty() {
        prompt.push_str("\n\n<project_context>\n\n");
        prompt.push_str("Project-specific instructions and guidelines:\n\n");
        for ctx in context_files {
            prompt.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                ctx.path.display(), ctx.content
            ));
        }
        prompt.push_str("</project_context>");
    }

    prompt.push_str(&format!("\n\nCurrent working directory: {}", cwd.display()));
    prompt
}

/// 内置默认 prompt（对齐 Pi 的默认 system prompt 结构）
fn default_coding_prompt(tools: &[&str], cwd: &Path) -> String {
    let tool_list = tools.iter()
        .map(|name| format!("- {}: {}", name, tool_snippet(name)))
        .collect::<Vec<_>>()
        .join("\n");

    let guidelines = tool_guidelines(tools);

    format!(r#"You are a coding agent. You help users with software engineering tasks.

Available tools:
{tool_list}

Guidelines:
{guidelines}

Current working directory: {cwd_display}"#,
        tool_list = tool_list,
        guidelines = guidelines,
        cwd_display = cwd.display(),
    )
}

/// 每个工具的一句话描述（对齐 Pi toolSnippets）
fn tool_snippet(name: &str) -> &'static str {
    match name {
        "read" => "Read file contents",
        "write" => "Create or overwrite files",
        "edit" => "Make precise file edits with exact text replacement",
        "bash" => "Execute bash commands",
        _ => "Custom tool",
    }
}

/// 每个工具的行为规则（对齐 Pi toolGuidelines）
fn tool_guidelines(tools: &[&str]) -> String {
    let mut guidelines = Vec::new();

    if tools.contains(&"edit") {
        guidelines.push("Use edit for precise changes (edits[].oldText must match exactly)".into());
        guidelines.push("When changing multiple separate locations in one file, use one edit call with multiple entries".into());
        guidelines.push("Keep edits[].oldText as small as possible while still being unique in the file".into());
    }
    if tools.contains(&"write") {
        guidelines.push("Use write only for new files or complete rewrites".into());
    }
    if tools.contains(&"read") {
        guidelines.push("Use read to examine files instead of cat or sed".into());
    }
    if tools.contains(&"bash") && !tools.contains(&"grep") {
        guidelines.push("Use bash for file operations like grep, find, ls".into());
    }

    guidelines.push("Be concise in your responses".into());
    guidelines.push("Show file paths clearly when working with files".into());

    guidelines.iter().map(|g| format!("- {g}")).collect::<Vec<_>>().join("\n")
}
```

#### v1 演进预留

| 机制 | v0 | v1 |
|------|----|----|
| 项目上下文 | AGENTS.md / YUSHAN.md / CLAUDE.md | 加入 Git 状态、项目类型检测 |
| 自定义 prompt | 文件/环境变量覆盖 | Hook 注入、动态模板 |
| 追加 prompt | 文件追加 | per-turn 追加 |
| 动态 guidelines | 根据注册工具生成 | 根据模型能力、权限策略生成 |
| Skills | 不支持 | 插件式 skill 系统 |

### CLI Binary

**位置**：`apps/coding-agent/`（新 binary crate）

**依赖**：`agent-runtime` + `agent-model-openai-compatible` + `tools-basic`

**Print 模式**：

```bash
yushan-coding-agent -p "在 src/lib.rs 添加 hello world 函数"
# 输出: 模型的最终回答（文本）
# 退出码: 0 成功, 1 失败
```

**简易 TUI 模式**：

```bash
yushan-coding-agent
# 交互式 stdin/stdout 循环
# > 用户输入
# 模型回答（带工具调用过程）
# > 下一条输入
# ...
# Ctrl+C 退出
```

**配置来源**（优先级从高到低）：

1. 环境变量：`YUSHAN_API_BASE`、`YUSHAN_API_KEY`、`YUSHAN_MODEL`
2. 配置文件：`~/.yushan/config.json` 或 `./.yushan.json`
3. CLI 参数：`--api-base`、`--api-key`、`--model`

**简易 TUI 实现**：不引入 ratatui/crossterm 等 TUI 框架，使用简单的 stdin readline + stdout print。模型工具调用过程以文本形式输出（`[Tool: read] src/lib.rs` 等）。后续 v1 可替换为完整 TUI。

### 完整组装

```rust
// apps/coding-agent/src/main.rs

fn build_agent(config: &Config) -> Agent {
    let model = OpenAICompatibleModel::new(OpenAICompatibleConfig {
        api_base: config.api_base.clone(),
        api_key: config.api_key.clone(),
        model: config.model.clone(),
        max_tokens: Some(4096),
        temperature: Some(0.7),
    });

    let workspace = std::env::current_dir().unwrap();

    AgentBuilder::new()
        .model(model)
        .tool(ReadTool::new(workspace.clone()))
        .tool(WriteTool::new(workspace.clone()))
        .tool(EditTool::new(workspace.clone()))
        .tool(BashTool::new(workspace.clone()))
        .system_prompt(format_coding_prompt(&workspace))
        .working_dir(workspace.clone(), workspace.clone())
        .approval(AutoApprove)  // v0: 允许所有操作
        .session(JsonlSession::open(".yushan/session.jsonl").await?)
        .events(StderrEventSink)  // 事件输出到 stderr
        .build()
}
```

## 后续建议

1. 本方案设计完成，建议用 `/sequential-workflow` 进入实现
2. 实现分为 6 个 phase（见下方），每 phase 可独立测试
3. 所有现有 43 个测试必须继续通过

### 实现 Phase

```
Phase 1: 基础设施 — 纯类型扩展
  T1: #[non_exhaustive] 补齐
  T2: ToolError 加 PermissionDenied + Timeout
  T3: RunLimits 加 bash_timeout + context_window
  T4: ModelRequest 加 system + max_tokens + temperature
  T5: ModelResponse 加 stop_reason

Phase 2: 基础设施 — 工具上下文 + 持久化
  T6: ToolContext 加 cwd + workspace_root
  T7: ToolRegistry::specs() 缓存 + ToolSpec::new() 构造器
  T8: JsonlSession 实现（损坏行 skip+warn + clear() 方法）

Phase 3: 基础设施 — 审批 + 组装
  T9: ApprovalHandler trait + AutoApprove
  T10: RuntimeContext + AgentBuilder 扩展
  T11: BasicLoop 改造
    T11a: 审批检查集成
    T11b: 超时保护（tokio::time::timeout）
    T11c: 工具错误恢复（错误回喂模型，不终止 run）
    T11d: Token 估算（chars/4 启发式）
    T11e: 上下文压缩触发（阈值检查 + 摘要生成）

Phase 4: Model Adapter
  T12: agent-model-openai-compatible crate 骨架
  T13: OpenAI chat completion 请求/响应类型
  T14: 流式 SSE 解析
  T15: Model trait 实现 + 测试（mock server）
  T16: Tool result 格式翻译（Role::User → role:"tool"）
  T17: Provider-specific compat flags（DeepSeek reasoning_content 等）

Phase 5: Coding Tools（对齐 Pi 行为）
  T18: tools-basic crate 骨架
  T19: ReadTool（head 截断 50KB/2000行 + 图片支持）
  T20: WriteTool（自动创建父目录）
  T21: EditTool（批量 edits + fuzzy matching + legacy format 兼容 + diff 输出）
  T22: BashTool（进程组管理 + tail 截断 + 流式输出预留）

Phase 6: CLI + 集成
  T23: coding-agent binary 骨架 + 配置
  T24: Print 模式
  T25: 简易 TUI 模式
  T26: System prompt 多层组合（项目上下文加载 + 自定义覆盖 + 动态 guidelines）
  T27: 端到端集成测试
```

### 任务依赖图

```text
T1 ──→ T2 ──→ T9 ──→ T10 ──→ T11
 │                       │         │
 ├─→ T5 ─────────────────┤         │
 │                       │         │
 ├─→ T3 ──→ T10         │         │
 │                       │         │
 └─→ T4 ──→ T10 ──→ T11 │         │
                         │         │
T6 ──→ T10 ──→ T11      │         │
T7 (独立)               │         │
T8 (独立)               │         │
                         │         │
T4 ──→ T12-T17 (Adapter) │         │
T6 ──→ T18-T22 (Tools)   │         │
T2 ──→ T18-T22           │         │
                         │         │
T10+T15+T22 ──→ T23-T27 (CLI)
```
