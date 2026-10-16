# YuShan — 轻量、组件化的 Rust Agent Runtime

用于组装 Agent 的运行时（harness），不是自带全部功能的完整 Agent 产品。项目只负责模型调用、消息状态、工具调用循环和事件流；沙箱、隔离、权限、审批和多租户安全由其他项目独立解决。

> **本文件按渐进式披露组织**：只放**每一轮都需要**的东西 —— 定位 / 目录结构 / 常用命令 / 硬性约束 / 工作约定 / 去哪找。
> 架构、模块职责、数据流、命令分工等**细节一律在 `docs/`**（见文末「去哪找什么」）。
>
> **往这里加内容前先问一句：它每一轮都需要吗？** 不是 ⇒ 写进 `docs/`，这里只留一行指针。
> 判据与先例见 `docs/arch/skills/design.md`（skill 的渐进式披露）与 `docs/CONTEXT.md`。

## 目录结构

```
YuShan/
├── crates/      # 运行时核心库：10 个 ys-* crate（分层见 docs/architecture.md §1）
├── adapters/    # 可选实现：模型（OpenAI 兼容）与工具（basic / web）
├── apps/        # 可执行入口：coding-agent（TUI / -p / --json 三模式）
├── docs/        # 设计、架构、ADR、as-built —— 入口见文末「去哪找什么」
├── .claude/     # Claude Code 本地配置
└── tmp/         # 临时目录（已被 .gitignore 忽略，勿放正式内容）
```

根文件：`Cargo.toml`（workspace 成员表）、`AGENTS.md`（`CLAUDE.md` 是指向它的软链）、`LICENSE`、`README.md`。

## 常用命令

- `cargo build` / `cargo test` / `cargo clippy --all-targets` / `cargo fmt`
- `cargo test --workspace` — 全量测试（1147 passed / 9 ignored）
- `cargo run -p ys-coding-agent` — 交互式 TUI
- `cargo run -p ys-coding-agent -- --resume [<id>]` — 恢复既有会话（**默认新会话**；裸 `--resume` 列表选择，`--resume <id>` 指定 id / 路径；`=` 形态已移除）
- `cargo run -p ys-coding-agent -- -p "任务"` — 单次 Print 模式（边生成边打印文本增量）
- `cargo run -p ys-coding-agent -- --json "任务"` — JSON 事件模式（逐行 `Envelope`）
- `cargo run -p ys-coding-agent -- --json --stats "任务"` — 额外输出背压统计到 stderr
- `cargo test -p ys-coding-agent e2e -- --ignored --nocapture` — 端到端（需 `YUSHAN_API_BASE` + `YUSHAN_API_KEY`）
- `cargo test -p ys-coding-agent --test e2e_skills -- --ignored --nocapture` — **R8 对账**（skill 目录段的真实 input token 成本；`#[ignore]`，**尚未校准**，需同上凭证）
- `cargo test -p ys-coding-agent --test e2e_bench -- --ignored --nocapture` — **基准**（同一任务的轮数 / 墙钟 / `ttft`·`gen`·`tools` 切分；`#[ignore]`，**只测量、不做任何内容判定**，需同上凭证；调参见 `YUSHAN_BENCH_REPEATS` / `YUSHAN_BENCH_ONLY` / `YUSHAN_BENCH_TIMEOUT_SECS` / `YUSHAN_BENCH_CLEAN` / `YUSHAN_BENCH_HEAVY`；**默认保留**逐轮日志等证据供回溯；**长跑项（如开放式提问 t4，实测 5 分钟起）默认跳过**，要跑用 `YUSHAN_BENCH_ONLY` 点名或 `YUSHAN_BENCH_HEAVY=1`）

环境变量：`YUSHAN_API_BASE` / `YUSHAN_API_KEY` / `YUSHAN_MODEL`；`YUSHAN_CHANNEL_CAPACITY`（信道容量，下限 16）；`YUSHAN_SESSIONS_DIR`（会话目录覆盖，测试用）；`YUSHAN_LOG_DIR`（日志目录覆盖，测试/基准用 —— 凭证仍在 `$HOME`，故隔离日志不能靠改 `HOME`）；`YUSHAN_SEARCH_SEARXNG_URL` / `YUSHAN_SEARCH_TAVILY_KEY`（Tavily 亦认生态通用名 `TAVILY_API_KEY`，命名空间名优先）/ `YUSHAN_SEARCH_BOCHA_KEY` / `YUSHAN_SEARCH_FIRECRAWL_KEY`（搜索后端，**各自配置才进链**）；`YUSHAN_FETCHED_DIR`（webfetch 落盘目录覆盖，须为**绝对路径**，测试用）。

**skills 没有任何环境变量**（D2′：不开路径配置项）—— 扫描根恒为 `[<cwd>/.yushan/skills, <home>/.yushan/skills]`，由 `Config::default_skill_roots` 唯一给出，**顺序即优先级**（`roots[0]` = 项目级）。

## 硬性约束

以下是 `docs/design.md` 不变量 + 历次 ADR 的摘要。**新增代码违反任何一条都算设计偏离，需先改设计文档。**

- **依赖方向单向**：`ys-core` ← 组件接口 ← loop/runtime ← 应用适配器
  - **`ys-core` / `ys-event` / `ys-protocol` 不含 tokio**（直接与间接皆无）—— 它们只装数据与纯 trait
  - **`ys-tui-coding` 不依赖 `ys-runtime`**（不认识 `Agent`）—— **编译器强制**（crate 边界），不靠自觉。**UI 线程必须无 runtime 上下文**（它用 `blocking_send`，在 async 上下文会 panic）—— 这条编译器管不住。
  - **`ys-component` 不直接依赖 tokio**（自身代码不用 tokio API），但经 `ys-session` **间接**引入（ADR-0012）
  - `ys-session` / `ys-loop` / `ys-runtime` 可直接依赖 tokio（文件 IO、工具超时）
- **最小核心**：Runtime 只提供一次 Agent Turn 所需能力；文件系统、Shell、MCP、TUI、记忆、子 Agent 均为可选组件
- **不做安全**：沙箱、隔离、权限、审批、多租户由上层项目解决；`ToolRegistry` 只按名称查找工具
- **四者边界不可混淆**：Event 观察「发生了什么」，Hook 决定「下一步怎么处理」，Component 提供能力，Loop 决定推进规则
- **静态组合优先**：能力通过 crate + Cargo feature 组合；动态插件只做运行时扩展，第一版不支持热插拔
- **不跨动态库边界传递 Rust trait object、Tokio 类型或跨库所有权对象**
  - 推论：**`ModelEventSink` 必须保持同步 `fn`**（async trait 跨动态库边界困难）—— 这是 try/await 双路径存在的首要理由
- **Agent 无状态**（ADR-0010）：不持有 session / events / model；会话、配置、历史归接线器。**actor 循环已上移到 `app_loop`**（ADR-0010 附注）
- **取消即消息**（ADR-0013）：`CancelToken` / `Agent::cancel()` 已删；`Boundary::Abort` + `BoundarySource` 取代 `Inbox` / `Intent` / `QueueMode`。
  **每回合必须新建 `BoundarySource`** —— `Abort` 的标记**永久置位**，复用会让「上一回合的取消」立刻取消之后每个回合。
- **信道契约**（ADR-0009 / 0011 / 0013）：有界、容量可配；`try_emit` 的 `Err` 仅表示消费者消失；信道满时实现**必须内部缓冲**，不得返回 `Err`。
  - **循环内的自由函数 `emit()` 总是走异步路径** —— 让它先试同步快路径，满时 `try_emit` 会把事件缓冲进 overflow 并返回 `Ok`，此后**冲刷永不发生** ⇒ 终局事件滞留、背压失效。
  - **`-p` / `--json` 的消费必须与 turn 并发**（`tokio::join!`）—— 有界信道若无并发消费者，第一次撞满即死锁。
  - **消费循环以终局事件（`RunFinished` / `RunFailed`）为终止条件**，不是等信道关闭 —— sender 在 agent 侧，等关闭必死锁。
- **事件只观察，Hook 才控制流程**：`AgentEvent` 不可用于改变执行路径
- **`capabilities.rs` 绝不 print**：命令期间的一切输出走 `Outbound::Output` → transcript（输出按**时机**三分，见 `docs/architecture.md` §13）

## 工作约定

- **文档、注释、讨论、commit message、PR 描述用中文**；代码标识符用英文（技术术语/专名可夹英文）
- **注释只写当前设计 —— 不写历史，也不写竞品**：
  - **要写**：这段代码**现在**的行为契约、不变量、约束；以及**属于本设计自身**的取舍理由（依赖方向、性能、安全、数据格式）。用现在时或条件句（「若 X 则 Y」）。
  - **不写**：**变更叙事**（「曾经在这里…」「以前是…」「修过…」「长期未被发现」）；**拿别的产品的做法当本设计的理由**（「X 是这么做的」「X 用 80%，我们取中」）。
  - **例外，要留**：外部**格式 / 协议 / 标准**的定义来源。「`SKILL.md` 是 Claude Code 定义的格式」是**兼容性契约**，属设计本身，不是竞品对照。
  - **去处**：设计的 why → `docs/adr/`；跨产品取舍对照 → `docs/arch/**/research.md`；变更过程 → `git log` 与 commit message。
- 实现顺序遵循路线图：最小闭环 → 可用适配器 → 静态组件生态 → 动态插件 → Coding Agent MVP
- 新模块和接口改动须能对应上 `docs/design.md` §12 测试矩阵中的条目
- **测试隔离（并行安全，血的教训）**：
  - 临时目录名必须含**进程内原子序号 + `process::id()`**（`SystemTime::as_nanos()` 在 macOS 只到微秒级，并行测试会撞名 → 互删文件）
  - **进程级 env 改写必须走 `crate::test_env::env_lock()` + `EnvRestore`**（RAII 恢复）；直接把 `set_var` 散在测试里会造成 getenv/unsetenv 数据竞争
- **关键逻辑要做变异测试**：注入 bug 验证测试能抓到。本项目多个真 bug（死锁、UTF-8 损坏、`/new` 失效）都是「测试全绿时依然存在」，靠变异测试与端到端复现才暴露
- **事实性断言的指针纪律**：任何关于「X 当前是 Y 状态」的静态断言（已实现/未实现、是/否占位、有/没有 trait），必须引用 `code/` 或带 `[已关闭]`/`[已实现]` 标记的 `docs/design-final/`；旧 ADR / 旧 design plan 只记 **why**，不能用来佐证 **now**。候选清单里出现这类断言而无指针 ⇒ 不写进回答。
- **流式/字节路径注意 UTF-8 边界**：TCP 分片可能切在多字节字符中间；缓冲必须是**字节级**，只在完整行上解码
- `tmp/` 已被 `.gitignore` 忽略，勿将正式内容放入

## 去哪找什么

| 需要了解… | 读… |
|---|---|
| **架构全貌** | **`docs/architecture.md`**（14 节：分层与依赖 / 三信道与两线程 / 组件关系 / 一次 run 的时序 / 出站·入站数据流 / 事件信道两条路径 / 流式 / 生命周期 / 三种入口 / `/new` 换 Wiring / 会话落盘 / **输出与诊断三分** / `ys-tui-coding` 形态与命令分工 / 测试分层） |
| 架构规范、核心 trait、Hook/Event 边界、测试矩阵 | `docs/design.md` |
| 术语（Turn / Round / StopReason / 错误结果 vs 基础设施失败…） | `docs/CONTEXT.md` |
| **给新需求归类**（Event / Tool / Follow-up / 还是真需要 Hook） | `docs/extension-points.md` |
| 某个决策的 why | `docs/adr/` —— **文件名即摘要**（如 `0021-plugin-seam-without-hook-system.md`） |
| **当前实现的实际形态**（含实现与设计的分歧） | `docs/design-final/coding-agent-tui.md`（路线 B as-built，最新）；其余 as-built：`session-default-new` / `search-tools` / `tools-web` / `compaction` / `skills` / `truncation-guard` / `opencode-go` |
| 某个 crate / 文件干什么 | 对应 crate 的 `src/lib.rs`（模块头 `//!` 注释）；目录与分层见 `docs/architecture.md` §1、组件关系见 §3 |
| `/login` & `/model` 的行为设计 | `docs/arch/commands/login-model-behavior/` |
| 把项目约定送到模型面前的通道与成本 | `docs/arch/prompt-delivery/notes.md` |
