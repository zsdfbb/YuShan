# YuShan v0 — 最小闭环 — 架构上下文

## 概述

YuShan 的第一个可运行版本：一个纯 Rust 的最小 Agent 闭环——模型调用、消息状态、工具调用循环、事件流，用 Mock Model 驱动端到端测试。对应 `docs/design.md` §11 路线图第 1 步。**仓库当前无代码**，本文所有「现有架构」均为设计态（依据 `docs/design.md`）。

## 现有架构（设计态）

### 模块边界

v0 参与的 crate（完整清单见 design.md §9，插件两 crate 完全推迟）：

```text
agent-core        基础数据类型（Message / ToolCall / Event / Error / ID）
agent-component   Component 与 Registry（v0 只需最小 ToolRegistry）
agent-model / agent-tool / agent-session / agent-event   组件接口
agent-loop        AgentLoop trait + BasicLoop
agent-runtime     AgentBuilder + Agent
```

依赖方向单向：`agent-core` ← 组件接口 ← loop/runtime ←（未来）应用适配器。`agent-core` 不依赖 Tokio、HTTP 客户端、数据库、TUI、具体模型 SDK；**serde/serde_json 允许**（`ToolCall.arguments` 即 `serde_json::Value`，且 Event 未来要序列化）。

### 核心抽象（trait 草图见 design.md §4–§6）

- `Model::complete(request, sink) -> ModelResponse` — 请求转响应，增量经 `ModelEventSink` 输出
- `Tool::spec() + Tool::call(input, ctx) -> ToolResult` — 自描述 + 执行；Registry 只按名查找，不做权限
- `Session::messages() -> &[Message]` + `append(Message)` — 会话状态；v0 只有 `MemorySession`
- `EventSink::emit(AgentEvent)` — 事件出口；v0 只有 `NoopEventSink`
- `AgentLoop::run_turn(input, ctx) -> RunResult` — 可替换执行策略；v0 只有 `BasicLoop`
- `AgentBuilder` — 静态组装入口（design.md §7）

### 关键数据流

BasicLoop 一轮（design.md §4 时序图、§5 伪码）：输入写入 Session → 调 Model（事件流出）→ 无 Tool Call 则完成；有则查 Registry、执行、追加 ToolResult、回到 Model，直至无 Tool Call 或触顶。

### 外部依赖（计划引入，仓库尚无 Cargo.toml）

| 依赖 | 用途 | 备注 |
|------|------|------|
| serde + serde_json | 消息/事件序列化、工具参数 | 唯一允许进入 agent-core 的外部依赖 |
| async-trait | dyn 兼容的 async trait（`Box<dyn Tool>`、`&mut dyn ModelEventSink`） | 见未澄清问题 6 |
| tokio（feature `runtime-tokio`，default） | 异步运行时 | 只进 loop/runtime，不进 core |
| tokio-util | 取消原语（若采纳 CancellationToken） | 见未澄清问题 2 |
| thiserror | 各 crate 错误枚举 | |

## 约束

- **技术**：本机 stable 1.88（edition 2024 可用，MSRV 待定，见问题 10）；MIT；crate 名 `agent-*` 已由设计文档固定；AGENTS.md 六条硬性约束全部适用（依赖单向、core 零外部运行时依赖、安全外置、Event/Hook/Component/Loop 边界、静态组合优先、不跨 ABI 传 Rust 类型）。
- **性能**：无量化目标（刻意不设）；延迟由模型主导。真实预算是**依赖面最小化**；事件流注意不要无界缓冲即可。
- **演进**：三个 trait（Model/Tool/Session）自 v0 起要保持稳定——第 2 步适配器、第 4 步动态插件 ABI 都要实现/包装它们，签名里不能埋下「只有内存实现才成立」的假设；`AgentEvent` 从 v0 起就要 serde 可序列化且可向前扩展（后续 JSON 事件模式、回放依赖它）；HookDispatcher 在第 3 步才出现，BasicLoop 须把一轮拆成离散步骤（对应 §6 的 8 个 Hook 点位），避免将来为插 Hook 重写循环。
- **组织**：单人开发（Zhang Shuai）、无 CI、设计阶段仓库；AGENTS.md 约定文档中文、代码英文；v0 应小到能快速合入，再按路线图增量推进。

## 需求范围

### 范围内（v0 交付物）

1. Cargo workspace + 上述 8 个 crate 骨架
2. `agent-core`：Message / ContentBlock / ToolCall / ToolResult / ModelRequest / ModelResponse / Usage / AgentEvent / AgentError / ID 类型，全部带 serde derive
3. 组件接口 trait：Model、Tool、Session、EventSink + `AgentLoop` trait
4. 最小 `ToolRegistry`（注册、按名查找、重复名与缺失的明确错误）
5. `BasicLoop`：文本回合、工具调用循环、最大轮数
6. `MemorySession`、`NoopEventSink`、`MockModel`（可脚本化输出文本或 Tool Call，仅供测试）
7. `AgentBuilder` + `Agent` 最小组装与 `run` 入口
8. 测试：覆盖 design.md §12 矩阵前五行（agent-core、BasicLoop、Registry、Session、EventSink）

### 范围外（明确不做）

- 真实模型适配器（OpenAI-compatible / DeepSeek）→ 第 2 步
- JsonlSessionStore / SqliteSessionStore → 第 2 步
- RuntimeHook / HookDispatcher → 第 3 步
- 动态插件（plugin-api / plugin-loader / ABI）→ 第 4 步
- Coding Agent 产品层（coding 工具、项目上下文、Prompt、CLI/TUI/Print/JSON/RPC 入口）→ 第 5 步
- MCP、记忆、子 Agent、上下文压缩

### 关键场景（成功路径）

1. **纯文本回合**：输入 → Model 直出文本 → RunResult；Session 内为 [user, assistant]
2. **单次工具回合**：Model 请求工具 → Registry 查找并执行 → 追加 ToolResult → Model 给出最终回答
3. **多轮工具循环**：连续多次 Tool Call 直至模型不再请求工具
4. **带历史继续**：预填充消息的 Session 上发起 run，模型可见全部历史
5. **事件流完整**：以上每一步产生正确顺序的 AgentEvent（UserMessage / ModelTextDelta / ToolCall / ToolResult / RunFinished）

### 异常/边界场景

- 模型错误（网络/协议）→ 经 `LoopError` 传播；此前已发出的事件如何收尾需定义
- 工具未注册 / 工具执行出错 → 终止回合还是作为错误 ToolResult 回喂模型？**设计文档未规定**（问题 3）
- 达到最大轮数 → RunResult 须携带终止原因而非报错
- 运行中取消（§12 矩阵含「取消」）→ 机制待定（问题 2）
- 空输入、空 Registry；`Session::append` 与 `EventSink::emit` 的失败语义待定（问题 4/5）

### 质量目标

- 正确性：§12 矩阵前五行全绿，Mock Model 端到端跑通三个关键场景
- 轻量：agent-core 外部依赖仅 serde/serde_json；全 workspace 默认依赖面 = 上表
- 可演进：不违反 AGENTS.md 硬性约束；三个 trait 通过「第 2 步能直接写适配器」的思想实验

## 已决事项（2026-09-04 grill 会话；用户缺席，按推荐默认收敛，均可推翻）

| # | 问题 | 决策 | 记录 |
|---|------|------|------|
| 1 | v0 是否含真实适配器 | v0 纯 Mock 闭环；OpenAI-compatible 适配器为第 2 步 | 本文「范围内/外」 |
| 2 | 取消机制 | `agent-core` 用 std 原语实现协作式 CancelToken，步骤边界检查；取消产出 RunResult（StopReason=Cancelled）而非 Err | ADR-0002 |
| 3 | 工具错误语义 | 双通道：错误结果（含 Registry miss）回喂模型；ToolError 终止回合 | ADR-0001 |
| 4 | EventSink.emit 失败 | 终止 run；「观察者失败不阻断」留给第 3 步 Hook 语义 | design.md §5 |
| 5 | Session trait 形状 | `append` 定为 async + Result；`messages()` 保持同步切片视图 | ADR-0003 |
| 6 | async trait 实现 | async-trait 宏（dyn 兼容、生态成熟） | 本文「约束」 |
| 7 | ToolContext 字段 | 携带取消句柄，v0 仅此一项 | ADR-0002、design.md §4 |
| 8 | RunResult 内容 | 终止原因 + 累计 Usage + 轮数 + 最终消息（可空） | design.md §5 |
| 9 | Registry 归属 | 最小 ToolRegistry 属于 v0；§11 第 3 步已改为「扩展 Registry」 | design.md §11（已修订） |
| 10 | MSRV / edition | edition 2024、rust-version = 1.88（本机 stable） | 本文「约束」 |
| 11 | workspace 形态 | 虚拟 workspace（无根包），crate 名 `agent-*`，共享 version 0.0.1 | 本文 |

### 按路线图顺延的遗留项

- 强中断取消（runtime-tokio feature 下增强 in-flight 中断）→ 第 3 步
- 观察者失败不阻断、Hook 优先级语义 → 第 3 步
- 大会话懒加载（当前策略：启动全量加载 + 追加写穿）→ 出现真实需求再演进

## 后续建议

- `arch-design` 多方案评审已完成：最终方案与候选对比见同目录 `design.md`，骨架决策见 `docs/adr/0004`；可直接落地 v0 workspace
- v0 骨架落地后，`BasicLoop + MockModel` 本身就是一个活的 prototype，可替代独立原型验证
