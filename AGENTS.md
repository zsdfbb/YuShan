# YuShan — 轻量、组件化的 Rust Agent Runtime

用于组装 Agent 的运行时（harness），不是自带全部功能的完整 Agent 产品。当前已实现 v0 最小闭环（`cargo test` 全部通过），正在向可用适配器阶段推进。

## 目录结构

```
YuShan/
├── crates/                    # 核心库
│   ├── agent-core             # 共享原语：Message、Role、Usage、错误类型，零外部依赖
│   ├── agent-event            # AgentEvent 枚举 + EventSink 推送抽象
│   ├── agent-model            # Model trait（complete）+ MockModel
│   ├── agent-tool             # Tool trait + ToolRegistry + 审批处理
│   ├── agent-session          # Session trait + JsonlSession / MemorySession
│   ├── agent-component        # RuntimeContext 容器 + RunLimits
│   ├── agent-loop             # AgentLoop trait + BasicLoop 执行循环
│   └── agent-runtime          # AgentBuilder 装配门面 + prelude 重导出
├── adapters/                  # 可选适配器
│   ├── model-openai-compatible  # OpenAI 兼容 HTTP 模型后端
│   └── tools-basic            # BashTool / ReadTool / WriteTool / EditTool
├── apps/                      # 产品层应用
│   └── coding-agent           # YuShan Coding Agent 二进制入口
│       ├── src/
│       │   ├── main.rs        # 入口：组装 Agent、启动恢复、model factory
│       │   ├── config.rs      # Config 结构体 + ProviderRegistry 持有
│       │   ├── provider.rs    # ProviderRegistry：provider 目录、auth 持久化、模型获取
│       │   ├── commands/
│       │   │   ├── mod.rs     # Command trait + CommandRegistry
│       │   │   └── builtin.rs # 内置命令：/login /logout /model /help /status 等
│       │   ├── prompt.rs      # 系统提示词构建
│       │   └── tui.rs         # 交互式 REPL 循环
│       └── tests/
│           ├── integration.rs # 工具层集成测试
│           └── e2e_tools.rs   # 端到端测试：真实 API + 四工具协作（#[ignore]）
├── docs/                      # 文档
│   ├── design.md              # 总体设计：原则、crate 划分、核心 trait、Hook/Event 边界、路线图、测试矩阵
│   ├── CONTEXT.md             # 领域术语表
│   ├── adr/                   # 架构决策记录（工具失败双通道、协作取消、会话异步追加、v0 分层骨架）
│   ├── arch/                  # 架构分析
│   │   └── commands/
│   │       └── login-model-behavior/  # /login & /model 行为改进（context/design/ADR/review）
│   ├── design-plans/          # 设计方案
│   ├── exec-plans/            # 执行计划
│   └── reports/               # 报告
└── CLAUDE.md
```

## 常用命令

- `cargo build` — 构建
- `cargo test` — 测试
- `cargo clippy --all-targets` — lint
- `cargo fmt` — 格式化
- `cargo test -p coding-agent e2e -- --ignored --nocapture` — 端到端测试（需 YUSHAN_API_BASE + YUSHAN_API_KEY）

## coding-agent 模块说明

`apps/coding-agent/` 是产品层二进制，组装全部组件。内部模块：

| 模块 | 职责 |
|------|------|
| `config` | Config 结构体，持有 ProviderRegistry，管理运行时配置 |
| `provider` | ProviderRegistry：内置 provider 目录、auth.json 持久化（~/.yushan/）、GET /v1/models 动态模型获取、ProviderCompat 映射 |
| `commands` | Command trait + CommandRegistry + 内置命令（/login /logout /model /help /new /compact /status /copy /export /quit） |
| `prompt` | 系统提示词构建 |
| `tui` | 交互式 REPL 循环（stdin → 命令拦截 → agent turn） |

关键设计：
- `ProviderRegistry` 作为 `Config` 的 pub 字段，命令通过 `ctx.config.registry` 访问
- `CommandContext` 只持有 `&mut Agent` + `&mut Config`，不直接持有 registry
- 凭证持久化到 `~/.yushan/auth.json`（0o600 权限），启动时自动恢复
- 模型列表优先从 API 动态获取，失败 fallback 到静态列表

## 硬性约束

以下是 `docs/design.md` 的不变量摘要，新增代码违反任何一条都算设计偏离，需先改设计文档。

- **依赖方向单向**：`agent-core` ← 组件接口 ← loop/runtime ← 应用适配器；core 不依赖 Tokio、HTTP、数据库、TUI 或具体模型 SDK
- **最小核心**：Runtime 只提供一次 Agent Turn 所需能力；文件系统、Shell、MCP、TUI、记忆、子 Agent 均为可选组件
- **不做安全**：沙箱、隔离、权限、审批、多租户由上层项目解决；`ToolRegistry` 只按名称查找工具
- **四者边界不可混淆**：Event 观察「发生了什么」，Hook 决定「下一步怎么处理」，Component 提供能力，Loop 决定推进规则
- **静态组合优先**：能力通过 crate + Cargo feature 组合；动态插件只做运行时扩展，第一版不支持热插拔
- **不跨动态库边界传递 Rust trait object、Tokio 类型或跨库所有权对象**

## 工作约定

- 实现顺序遵循路线图：最小闭环 → 可用适配器 → 静态组件生态 → 动态插件 → Coding Agent MVP
- 新模块和接口改动须能对应上 `docs/design.md` §12 测试矩阵中的条目
- 文档、讨论、commit message 用中文；代码标识符与注释用英文
- `tmp/` 目录已被 `.gitignore` 忽略，勿将正式内容放入

## 参考资料

需要深入了解时，按需查阅：

| 需要了解... | 阅读... |
|---|---|
| 架构全貌、核心 trait、Hook/Event 边界 | `docs/design.md` |
| 术语定义（Turn、Round、StopReason 等） | `docs/CONTEXT.md` |
| 架构决策的 why | `docs/adr/` |
| 各 crate 的 API 和实现细节 | 对应 crate 的 `src/lib.rs` |
| coding-agent 产品层设计 | `docs/arch/` + `apps/coding-agent/src/main.rs` |
| /login & /model 行为设计 | `docs/arch/commands/login-model-behavior/` |
