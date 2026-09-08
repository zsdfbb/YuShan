# TUI 状态展示 — 架构上下文

## 概述

coding-agent 的 TUI 当前只有最简 REPL 循环，需要补充状态信息展示，让用户随时了解 Agent 的运行状态和资源消耗。

## 现有架构

### TUI 现状

`tui.rs` 是一个极简 REPL：启动 banner → `> ` 提取输入 → 命令拦截 → agent turn → 输出文本 → 循环。没有任何状态栏或持续性信息展示。

```
YuShan Coding Agent (type /help for commands, 'exit' to quit)

> _
```

### 模块边界

| 组件 | 提供的信息 | 当前是否可从 TUI 获取 |
|------|-----------|---------------------|
| `Config` | model, provider, api_base, cwd, is_configured | ✅ 通过 `&mut Config` |
| `Agent` | model_id, is_configured, session_messages | ✅ 通过 `&mut Agent` |
| `RunResult` (每次 turn 返回) | stop_reason, usage, rounds | ✅ 返回值已可用 |
| `AgentEvent` (流式事件) | ToolCall, ToolResult, ModelTextDelta, RunFinished | ⚠️ 当前用 `NoopEventSink`，事件未捕获 |
| `RunLimits` | max_rounds, context_window | ❌ 不在 Agent 公开 API 中 |
| `token::estimate_session_tokens` | 会话 token 估算 | ✅ 可调用，但需手动调 `session_messages()` |

### 关键数据流

```
用户输入 → tui.rs → CommandRegistry / Agent.run_turn()
                                    ↓
                            RunResult { stop_reason, usage, rounds }
                                    ↓
                            打印 final_message → 回到 REPL
```

缺失：`RunResult` 的 usage/rounds 信息在打印后即丢弃，没有累计追踪。

## 约束

- **技术**：纯 stdin/stdout TUI，无 TUI 框架（如 ratatui），信息展示需在 CLI 友好格式内
- **演进**：`NoopEventSink` 未来需要替换为有状态 sink 来捕获流式事件；事件捕获可独立于状态展示
- **性能**：token 估算是本地计算，无额外开销；不需要真实 API 调用
- **最小化**：v0 阶段优先展示高价值、低成本的信息，避免过度设计

## 已有信息源分析

### 已有但未展示的数据

| 数据 | 来源 | 获取方式 |
|------|------|---------|
| 当前模型 ID | `Agent::model_id()` | `agent.model_id()` |
| 当前 Provider | `Config::provider` | `config.provider` |
| 当前工作目录 | `Config::cwd` | `config.cwd` |
| API Base | `Config::api_base` | `config.api_base` |
| 会话消息数 | `Agent::session_messages().len()` | 直接调用 |
| 会话 token 估算 | `token::estimate_session_tokens()` | 需 `agent.session_messages()` |
| 上轮 usage | `RunResult::usage` | 每次 turn 返回值 |
| 上轮 rounds | `RunResult::rounds` | 每次 turn 返回值 |
| 上轮 stop_reason | `RunResult::stop_reason` | 每次 turn 返回值 |
| context_window | `RunLimits::context_window` | ⚠️ Agent 不暴露此字段 |
| max_rounds | `RunLimits::max_rounds` | ⚠️ Agent 不暴露此字段 |

### 目前 TUI 丢弃的信息

`RunResult` 在 `tui.rs:54-68` 被消费后，仅提取 `final_message` 打印，其余全部丢弃：
- `usage`（input/output tokens）→ 未累计
- `rounds` → 未记录
- `stop_reason` → 未区分展示

## Pi 对照（`tmp/pi/`）

Pi 的 footer 实现见 `tmp/pi/packages/coding-agent/src/modes/interactive/components/footer.ts`。它展示两行（外加 extension 第三行）：

```
~/path/to/project (main-branch) • my-session
↑1.2k ↓3.4k R256 W12 CH34.5% $0.012 12.5%/128k (auto)        (deepseek) deepseek-chat
```

| Pi 字段 | 数据源 | YuShan 是否可获得 |
|--------|--------|------------------|
| `~/cwd (branch) • session` | 文件系统 + git + session name | ✅ cwd 可；❌ git/branch、❌ session name |
| `↑input ↓output` 累计 tokens | session entries usage 累加 | ⚠️ 需 TUI 层累加 `RunResult.usage` |
| `R cache-read W cache-write` | model usage.cache_read/write | ❌ YuShan `Usage` 只有 input/output |
| `CH cache hit %` | 同上派生 | ❌ 无 |
| `$ cost` | 各 provider pricing | ❌ 无定价数据 |
| `12.5%/128k` 上下文占比 | session context usage vs window | ⚠️ 需暴露 `RunLimits::context_window` |
| `(auto)` auto-compact 开关 | 配置 | ⚠️ 有 compact 命令但开关状态未追踪 |
| `modelName • thinkingLevel` | model + reasoning level | ⚠️ 模型 ID 可；❌ 无 reasoning level 概念 |
| `(provider) model` | 多个 provider 时显示 | ⚠️ 当前总是一个 provider |
| Extension 状态行 | `ctx.ui.setStatus()` | ❌ 无 extension API |

Pi 的全功能 footer 需要：cache/cost/git/thinking-level/extension 5 块数据基建。**YuShan v0 没有这些基建**——盲目照搬等于造 5 个新特性。

## 聚焦后提议（v0 高价值子集）

按"展示成本 / 信息价值"排序，砍掉 v0 无基建支撑的项：

### 必须展示（成本低、价值高）

| 字段 | 例子 | 数据源 | 展示场景 |
|------|------|--------|---------|
| 工作目录（`~` 缩写） | `~/Develop/YuShan` | `Config.cwd` + `HOME` | 启动 banner + footer |
| Provider + Model | `deepseek deepseek-chat` | `Config.provider` + `Agent.model_id()` | 启动 banner + footer 右侧 |
| 输入/输出 tokens | `↑1.2k ↓3.4k` | TUI 层累加 `RunResult.usage` | footer |
| Rounds（最近 turn） | `3 rounds` | `RunResult.rounds` | turn 结束后提示行 |

### 不展示（v0 无基建，跳过）

- ❌ 缓存命中、cost 美元、context 占比（需要 `RunLimits` API 扩展 + pricing 数据）
- ❌ Git branch（v0 不涉及 Git 操作）
- ❌ Thinking level / reasoning indicator（YuShan 无 reasoning level 切换）
- ❌ Extension status（无 extension API）
- ❌ Auto-compact 状态指示（compact 是手动命令）

### 推荐效果

启动时：
```
YuShan Coding Agent
Provider: deepseek | Model: deepseek-chat | Dir: ~/Develop/YuShan
Type /help for commands, 'exit' to quit

> _
```

Turn 结束反馈（单行，紧跟回复后）：
```
> 解释 main.rs 的结构

[AI response...]

✓ 1 round · ↑320 ↓1,247 tokens
> _
```

Multi-round / 异常：
```
✓ 3 rounds · ↑2.1k ↓856 tokens          # 工具调用后正常完成
⚠ MaxRounds · 10 rounds · ↑12.4k ↓4.2k  # 超限
✗ Cancelled · 2 rounds · ↑1.2k ↓340     # Ctrl-C 中断
```

## 数据缺口

实现聚焦方案**不需要 API 扩展**：

- `RunResult.usage` / `RunResult.rounds` / `RunResult.stop_reason` 已有
- `Config.cwd` / `Config.provider` 已有
- `Agent.model_id()` 已有
- TUI 层自行维护累计 `TurnStats { total_input_tokens, total_output_tokens }`

之前提到的 `RunLimits` 缺口**对聚焦方案无影响**——v0 不展示 context 占比。

## 未澄清问题

- [ ] 启动 banner 是固定展示还是可以 toggle？
- [ ] turn 后的 summary 用哪个符号？（✓ / ⚠ / ✗ / 不同 stop_reason 用不同符号）
- [ ] `cwd` 是否要 `~` 缩写？（Pi 风格）还是绝对路径？

## 后续建议

聚焦方案无需新基建，可直接进入实现：

1. **优先级 1** — Turn 结束 summary（累计 tokens、rounds、stop_reason）→ 改 `tui.rs` 的 `run_turn` 消费 `RunResult` 路径
2. **优先级 2** — 启动 banner 增强（provider + model + cwd）→ 改 `tui.rs` 入口
3. **未来** — context 占比、cache 命中、成本估算等需要扩展 `Usage` / 暴露 `RunLimits` / pricing 数据，单独议程

不需要先做 `arch-design` —— 聚焦方案的范围和格式已经很明确，直接实现即可。如果未来要扩展到 Pi 级别的 footer（cache/cost/git/thinking/extension），那时再启动 `arch-design` + `arch-validate` 评估扩展成本。
