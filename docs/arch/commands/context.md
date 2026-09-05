# Runtime Commands — 架构上下文

## 概述

为 coding-agent 的交互式 TUI 提供运行时斜杠命令，允许用户在不重启进程的情况下调整配置和行为。命令集参考 Pi 项目，筛选出 MVP 最小闭环所需的 10 个命令。

## 现有架构

### 模块边界

当前交互循环位于 `apps/coding-agent/src/tui.rs`，是一个极简的 stdin 行读取器：

```
stdin → 检查 exit/quit → AgentInput::text(line) → agent.run_turn() → 打印结果
```

**没有任何斜杠命令处理逻辑。** 所有非空、非 exit 的输入都直接转发给 agent。

### 核心抽象（与 commands 相关）

| 抽象 | 位置 | 现状 |
|------|------|------|
| `Config` | `apps/coding-agent/src/config.rs` | 仅从环境变量加载，一次性读取，不可变 |
| `AgentBuilder` | `crates/agent-runtime/src/builder.rs` | 一次性构建，build() 后返回 `Agent`，无法重建 |
| `Agent` | `crates/agent-runtime/src/agent.rs` | 持有 `model`、`registry`、`session` 等所有组件的 owned 值 |
| `OpenAICompatibleModel` | `adapters/model-openai-compatible/src/lib.rs` | `new()` 时创建 `reqwest::Client`，持有 `config: OpenAICompatibleConfig` |
| `MemorySession` | `crates/agent-session/src/memory.rs` | 内存会话，消息存储在 `Vec<Message>` |

### 关键约束

1. **Model 是 `Box<dyn Model>`，不可变**：Agent 构建后，model 字段没有 `mut` 访问路径，也无法替换。
2. **ToolRegistry 也是不可变引用**：构建后无法动态增删工具。
3. **Agent 持有所有组件的 owned 值**，但对外只暴露 `run_turn()` 方法。
4. **Config 是局部变量**：在 `main()` 中创建后传入模型构造，之后无处可访问。

### 当前数据流

```
main()
  ├─ Config::from_env()        ← 环境变量，一次性
  ├─ OpenAICompatibleModel::new(config)  ← 构造时绑定 config
  ├─ AgentBuilder::new()
  │     .model(model)
  │     .tool(ReadTool/WriteTool/EditTool/BashTool)
  │     .system_prompt(...)
  │     .build()               ← 返回 Agent，此后无法修改
  └─ tui::run_interactive(&mut agent)
        └─ loop { stdin → agent.run_turn() → stdout }
```

## Pi 对照

Pi（`tmp/pi/`）有 20+ 个斜杠命令，以下为 MVP 必要子集的对照依据：

| YuShan 命令 | Pi 对应 | Pi 实现方式 |
|-------------|---------|-------------|
| `/help` | `⌘K` 菜单 | UI 弹出命令列表 |
| `/login` | `/login` | 交互式选择提供商 → OAuth 或 API Key → 保存到 `~/.pi/agent/auth.json`（`0o600`） |
| `/logout` | `/logout` | 清除 `auth.json` 中指定提供商凭证 |
| `/model` | `/model` | 无参打开选择器，有参按名称匹配；调用 `session.setModel()`，支持持久化 |
| `/new` | `/new` | 调用 `runtimeHost.newSession()`，创建全新会话 |
| `/compact` | `/compact` | 调用 `session.compact(instructions?)`，支持自定义压缩指令 |
| `/status` | `/session` | 显示会话信息和统计 |
| `/copy` | `/copy` | 复制上一条 agent 消息到系统剪贴板 |
| `/export` | `/export` | 导出会话为 HTML 或 JSONL 文件 |
| `/quit` | `/quit` | 优雅退出进程 |

**MVP 范围外的 Pi 命令**（及原因）：

| Pi 命令 | 不做的原因 |
|---------|-----------|
| `/tree` `/fork` `/clone` | 会话分支，依赖 session 分支架构 |
| `/share` | GitHub gist 集成，偏离核心 |
| `/thinking` | 思维链控制，取决于模型支持 |
| `/trust` | 安全信任机制，设计文档明确不做安全 |
| `/reload` | 热重载，v1 不支持 |
| `/settings` | 完整设置菜单，命令行参数即可覆盖 |
| `/import` `/resume` | 会话持久化依赖 JsonlSession，后续再做 |
| `/scoped-models` `/hotkeys` | UI 增强，非核心 |
| 扩展/技能命令 | 插件系统 v1 不支持 |

## 约束

- **技术**：Rust ownership 模型 — Agent 持有 Box<dyn Model>，需要 unsafe 或重构才能在运行时替换。不引入 unsafe。
- **技术**：OpenAICompatibleModel 内部持有 reqwest::Client，重建成本不高（~ms 级），但不应每轮重建。
- **技术**：slash 命令在 agent 之外执行（不应发给模型），需要在 TUI 层拦截。
- **演进**：命令可能随版本增多，需要可扩展的注册机制，而非 if-else 硬编码。
- **组织**：这是 coding-agent 产品层功能，不影响核心 crates 的 trait 定义。

## 需求范围

### 范围内 — 10 个命令

| 命令 | 功能 | 难度 | 持久化 |
|------|------|------|--------|
| `/help` | 列出所有命令及说明 | 低 | 无 |
| `/login [provider]` | 交互式配置 API 提供商凭证（api_base、api_key） | 中 | `~/.yushan/auth.json` |
| `/logout [provider]` | 清除指定提供商凭证 | 低 | `~/.yushan/auth.json` |
| `/model [name]` | 无参显示当前模型，有参切换（需重建 Agent） | 高 | `~/.yushan/models.json` |
| `/new` | 清空会话，从零开始 | 低 | 无 |
| `/compact [instructions]` | 手动触发上下文压缩，可选自定义指令 | 中 | 无 |
| `/status` | 显示 model、api_base（脱敏）、session 消息数、token 用量 | 低 | 无 |
| `/copy` | 复制上一条 agent 回复到剪贴板 | 低 | 无 |
| `/export [path]` | 导出会话为 JSONL 文件（默认 stdout） | 中 | 文件输出 |
| `/quit` | 优雅退出 | 低 | 可选：保存会话 |

### 范围外（明确不做的）

| 功能 | 原因 |
|------|------|
| `/approve` — 工具审批 | 当前使用 AutoApprove，审批是独立 feature |
| `/undo` — 撤销操作 | 需要 git 集成，Phase 2 |
| `/plugin` — 动态插件管理 | 设计文档明确 v1 不支持热插拔 |
| 命令别名 / alias | 过早优化 |
| 命令自动补全 | 需要 readline 库，后续增强 |
| `/tree` `/fork` `/clone` | 会话分支是高级功能 |
| `/share` | GitHub gist 集成偏离核心 |
| `/thinking` | 思维链控制取决于模型 |
| `/trust` | 安全信任机制不在范围内 |
| `/reload` | 热重载 v1 不支持 |
| `/settings` | 完整设置菜单，CLI 参数覆盖 |
| `/import` `/resume` | 会话持久化依赖 JsonlSession |

### 关键场景

1. **首次使用**：启动 → 环境变量未设置 → `/login deepseek` → 交互式输入 api_key → 保存到 `~/.yushan/auth.json` → 后续启动自动读取
2. **切换模型**：`/model claude-sonnet-4` → 匹配到 anthropic 提供商 → 重建 Agent 模型实例 → 确认切换
3. **清空重来**：`/new` → 会话清空 → 从零开始对话
4. **手动压缩**：上下文快满 → `/compact 保留最近的代码修改` → 指令式压缩 → 确认完成
5. **查看状态**：`/status` → 显示当前 model、api_base（脱敏）、消息数、token 用量
6. **复制代码**：agent 回复了一段代码 → `/copy` → 粘贴到编辑器
7. **导出调试**：agent 行为异常 → `/export session.jsonl` → 保存完整对话记录供分析
8. **切换提供商**：`/login openai` → 输入新 API key → `/model gpt-4o` → 切换到新提供商的模型

### 配置文件格式

```json
// ~/.yushan/auth.json — 凭证存储（权限 0o600）
{
  "deepseek": { "type": "api_key", "key": "sk-..." },
  "openai": { "type": "api_key", "key": "sk-proj-..." }
}

// ~/.yushan/models.json — 模型偏好（可选）
{
  "default_provider": "deepseek",
  "default_model": "deepseek-chat"
}
```

## 未澄清问题

- [ ] `/login` 持久化目标：建议默认持久化到 `~/.yushan/auth.json`，当前进程立即生效。无需 `--save` 标志。
- [ ] `/model` 切换是否需要重建 Agent？如果 model name 变了但 api_base/api_key 未变，只需改 config；如果提供商变了，需要重建。建议统一走重建路径。
- [ ] 命令注册机制的抽象层级：建议 coding-agent 层独立定义 `Command` trait，命令签名（接收 agent 引用 + 配置）和 tool（接收 JSON 参数）本质不同。
- [ ] `/login` 的交互方式：建议逐字段提示输入（api_base → api_key），对用户更友好。
- [ ] `/export` 默认输出到 stdout 还是文件？建议默认写入当前目录下 `session-{timestamp}.jsonl`。

## 后续建议

- 建议用 `arch-design` 做命令框架的具体 trait 设计和 Agent 可变性方案
- 建议先 prototype `/login` + `/model` 的可行性，验证 Agent 重建方案
