# Coding Agent — 架构上下文

## 概述

Coding Agent 是 YuShan 通用 Agent Runtime 之上的独立产品层，提供代码开发体验：文件编辑、命令执行、项目上下文发现和交互式/批处理 CLI。v0 Runtime 已完成，Coding Agent 层代码量为零。

## 现有架构

### v0 Runtime 交付物（已完成）

8 个 crate、43 个测试、严格单向依赖：

```text
agent-core
  ↑
agent-event / agent-model / agent-tool / agent-session
  ↑
agent-component / agent-loop
  ↑
agent-runtime (AgentBuilder + Agent)
```

### 可复用的扩展点

| 扩展点 | 类型 | 用途 |
|--------|------|------|
| `Tool` trait | `spec() + call()` | 实现 read/write/edit/bash |
| `ToolRegistry` | `build(Vec<Box<dyn Tool>>)` | 注册所有 coding tools |
| `ToolContext` | `#[non_exhaustive]` | 可扩展：加入 workspace、permissions |
| `AgentBuilder` | chain API | `.tool()` `.model()` `.session()` `.events()` |
| `AgentLoop` trait | `run_turn()` | 可替换为 CodingLoop（v1） |
| `EventSink` trait | `emit(AgentEvent)` | UI/日志/JSON 输出 |
| `MemorySession` | `Vec<Message>` | 可扩展为持久化 session |
| `ModelRequest` | `{ messages, tools }` | 可扩展：加入 system prompt |

### 关键数据流（BasicLoop）

```text
用户输入 → Session.append → Model.complete → ToolRegistry.get → Tool.call
  → ToolResult 回喂 → 下一轮 Model.complete → 最终回答
```

## 约束

### 技术约束

- **语言**: Rust stable 1.88, edition 2024
- **运行时**: Tokio（dev-dep 已有，生产 dep 待引入）
- **已有框架**: v0 的 8-crate 架构不可破坏，依赖单向性不可违反
- **Tool trait**: `call(input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError>` — 不可更改签名
- **ToolSpec**: 参数是 `serde_json::Value`（JSON Schema），不是强类型

### 性能约束

- **首次响应延迟**: coding tool 调用本身 < 100ms（文件操作），模型 RTT 取决于 provider
- **内存**: 每轮 O(n) 历史克隆（~2KB/消息），上限由 session 管理
- **bash 执行**: 需要超时机制（默认 30s？），避免进程泄漏

### 演进约束

- **向后兼容**: v0 public API 稳定后（已完成），Coding Agent 的新增不应修改已有 trait 签名
- **Hook 依赖**: design.md 设计的 Coding Hooks（`before_model_request` 等）依赖 Step 3 的 Hook Dispatcher，当前不存在。v0 MVP 需要绕过或简化
- **system prompt**: `ModelRequest` 当前无 `system` 字段，`AgentInput` 仅携带 `Message`

### 组织约束

- 单人开发，无外部依赖
- 目标：从零搭建到可交互的 coding agent

## 需求范围

### 范围内（Coding Agent v0 MVP — 完整交付）

**A. 基础设施补齐（9 项，详见 design.md）**

1. ModelRequest 加 system + max_tokens + temperature
2. ModelResponse 加 stop_reason
3. ToolContext 加 cwd + workspace_root
4. ToolError 加 PermissionDenied + Timeout
5. ApprovalHandler 审批机制
6. #[non_exhaustive] 补齐
7. RunLimits 加 tool_timeout + max_tool_output_bytes
8. ToolRegistry::specs() 构建时缓存
9. JsonlSession 持久化

**B. OpenAI-compatible Model Adapter**

10. `agent-model-openai-compatible` crate（adapters/ 目录）
    - OpenAI chat completion API 协议（`/v1/chat/completions`）
    - SSE 流式响应解析（`stream: true`）
    - 非流式响应解析（`stream: false`）
    - 支持 tool_calls 响应格式
    - **优先支持**：DeepSeek、MiniMax、opencode-go/zen
    - 其他 OpenAI-compatible 平台通过相同协议自动兼容
    - 配置：`api_base`（endpoint URL）+ `api_key` + `model`（模型名）
    - 依赖：reqwest + tokio

**C. 4 个 Coding Tools**

11. `read` tool — 读取文件（支持行范围）
12. `write` tool — 创建/覆盖文件
13. `edit` tool — 精确 search-replace 编辑
14. `bash` tool — 执行命令，返回 stdout/stderr/exit_code

**D. System Prompt**

15. Coding agent system prompt 模板（角色定义、工具使用指南）

**E. CLI Binary**

16. `yushan-coding-agent` 二进制入口（apps/coding-agent/）
    - Print 模式：`yushan-coding-agent -p "task"` — 单次任务
    - 简易 TUI：`yushan-coding-agent` — stdin/stdout 交互循环
    - 配置：环境变量或配置文件读取 `api_base`、`api_key`、`model`

### 范围外（明确不做的）

| 组件 | 原因 |
|------|------|
| Hook Dispatcher | Step 3 范围，v0 MVP 绕过 |
| 完整 TUI（ratatui/crossterm） | 先用简易 stdin/stdout，v1 升级 |
| RPC 模式 | v1 |
| ContextProvider | Hook 不存在，system prompt 直接注入 |
| CodingPrompt Renderer | 简单字符串模板即可 |
| 会话分支 | v1 |
| 上下文压缩/总结 | v1 |
| 动态插件 | Step 4 范围 |
| grep/find/ls/git 工具 | Phase 2 |
| 多 Agent 编排 | v2 |

### 关键场景

1. **单次编码任务**: `yushan-coding-agent -p "在 src/lib.rs 添加 hello world 函数"` → 模型读文件、写代码、执行验证
2. **交互式开发**: `yushan-coding-agent` → 用户输入任务 → agent 执行 → 用户追加指令 → 循环
3. **JSON 事件流**: `yushan-coding-agent --json -p "..."` → 外部程序消费事件流做 UI/日志
4. **文件编辑工作流**: 模型 read 文件 → 分析 → edit 精确修改 → 验证
5. **命令执行验证**: 模型写代码 → bash `cargo test` → 根据输出修复

## 实施决策

**决策（2026-09-05）**: 先补基础设施缺口，再进入 Coding Agent 实现。

### P0 — 必须先补（不补就不能写任何 coding tool）

| # | 改动 | crate | 说明 |
|---|------|-------|------|
| 1 | `ModelRequest` 加 `system: Option<String>` | agent-model | 没有 system prompt，模型不知道自己是 coding agent |
| 2 | `ToolContext` 加 `cwd: PathBuf` + `workspace_root: PathBuf` | agent-tool | read/write/edit/bash 全需要知道工作目录 |
| 3 | `ToolError` 加 `PermissionDenied` + `Timeout` | agent-tool | bash 不能无条件执行，工具不能无限挂起 |
| 4 | 基础审批机制（`ApprovalHandler` trait） | agent-loop + agent-component | bash、write、edit 是危险操作，需要用户确认 |
| 5 | `#[non_exhaustive]` 补齐 | agent-core, agent-tool, agent-component | StopReason、ToolError、ToolSpec、RunLimits 都没加，后面加字段是 breaking change |

### P1 — 实现过程中必须补

| # | 改动 | crate | 说明 |
|---|------|-------|------|
| 6 | `RunLimits` 加 `tool_timeout` + `max_tool_output_bytes` | agent-component, agent-loop | bash 挂起保护 + 大文件截断 |
| 7 | `ModelResponse` 加 `stop_reason` | agent-model | 区分 `max_tokens` 和 `end_turn`，当前 loop 盲区 |
| 8 | `ModelRequest` 加 `max_tokens` / `temperature` | agent-model | 输出长度控制和确定性 |

### Session 持久化判断

**已纳入**。JsonlSession 实现（~70 行），在 Phase 2 与 ToolContext 扩展一起落地。JSONL 格式追加写 + 启动恢复，为调试和交互式体验提供基础。

### Claude Code 基本工具对齐差距

| Claude Code 工具 | YuShan MVP | 额外基础设施需求 |
|------------------|------------|------------------|
| Read | read tool | ToolContext.cwd |
| Write | write tool | 审批机制 |
| Edit (search-replace) | edit tool | 审批机制 |
| Bash | bash tool | 超时 + 进程管理 + 审批 |
| Glob | phase 2 | — |
| Grep | phase 2 | — |

Claude Code 的其余 35+ 工具多为 agent 编排类（Agent/Task/Worktree）和 UI 交互类（AskUserQuestion），属于 v2 范围。

## 已决策问题（详见 design.md + adr-0005）

| 问题 | 决策 | 理由 |
|------|------|------|
| 系统提示注入 | `ModelRequest.system: Option<String>` | 影响面最小 |
| ToolContext 扩展 | 直接加 `PathBuf` 字段 | `#[non_exhaustive]` 保护 |
| 审批机制 | async trait `ApprovalHandler` | TUI 场景必须 async |
| bash 安全边界 | 基础超时 + 配置化审批 | 超时由工具内实现 |
| CLI 框架 | 待定（不在基础设施范围） | — |
| Model adapter | OpenAI-compatible（DeepSeek/MiniMax 优先） | 国内平台兼容 |
| edit 工具格式 | **批量 edits[{oldText, newText}]**（对齐 Pi） | Pi 接口标准 |
| 工作目录管理 | 固定 cwd（不支持 cd） | 简单安全 |
| 截断策略 | **工具内截断**（对齐 Pi） | Read=head，Bash=tail，50KB/2000行 |

## 后续建议

1. **基础设施阶段**：对上述 P0/P1 项做 `arch-design` 详细方案设计（接口、数据流、测试合同）
2. **关键决策**：先确定未澄清问题中的前 3 项（system prompt、ToolContext、审批机制），它们影响面最广
3. **建议拆分为两个独立工作流**：
   - **工作流 A**：基础设施补齐（P0 的 5 项 + P1 的 3 项，~8 个改动点）
   - **工作流 B**：Coding Agent 实现（tools + model adapter + CLI，依赖 A 完成）
