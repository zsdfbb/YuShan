# YuShan — 轻量、组件化的 Rust Agent Runtime

用于组装 Agent 的运行时（harness），不是自带全部功能的完整 Agent 产品。项目只负责模型调用、消息状态、工具调用循环和事件流；沙箱、隔离、权限、审批和多租户安全由其他项目独立解决。

> **本文件按渐进式披露组织**：只放**每一轮都需要**的东西 —— 定位 / 目录结构 / 常用命令 / 硬性约束 / 工作约定 / 去哪找。
> 架构、模块职责、数据流、命令分工等**细节一律在 `docs/`**（见下方「目录结构」的 `docs/` 部分）。
>
> **往这里加内容前先问一句：它每一轮都需要吗？** 不是 ⇒ 写进 `docs/`，这里只留一行指针。
> 判据与先例见 `docs/arch/skills/design.md`（skill 的渐进式披露）与 `docs/CONTEXT.md`。

## 目录结构

**本节就是「去哪找什么」**：`crates` / `adapters` / `apps` 止于模块名（一行职责），`docs/` 展开到二级
（所有交付物都在那里）。更细的职责看对应模块的 `src/lib.rs` 模块头 `//!` 注释。

```
YuShan/
├── crates/                       # 运行时核心库：10 个 ys-* crate（分层见 docs/architecture.md §1）
│   ├── ys-core                   # 共享词汇与原语（零 tokio）
│   ├── ys-event                  # 事件枚举与推式出口
│   ├── ys-model                  # LLM 后端抽象
│   ├── ys-tool                   # 工具接口与注册表
│   ├── ys-session                # 会话状态管理
│   ├── ys-component              # 运行时上下文容器
│   ├── ys-loop                   # 执行循环与编排（BasicLoop）
│   ├── ys-runtime                # 组装 facade，锁定外部导入路径
│   ├── ys-protocol               # 能力平面（三模式共用）
│   └── ys-tui-coding             # TUI 渲染层（不依赖 ys-runtime，编译器强制）
├── adapters/                     # 可选实现，各自独立 crate
│   ├── model-openai-compatible   # OpenAI 兼容模型后端
│   ├── tools-basic               # 文件与 shell 工具
│   └── tools-web                 # 联网工具（websearch / webfetch）
├── apps/
│   └── coding-agent              # 可执行入口：TUI / -p / --json 三模式
├── docs/                         # 设计与过程产物；**「去哪找什么」就在这一层**
│   ├── architecture.md           # 架构全貌（15 节：分层 / 信道 / 时序 / 数据流 / 输出三分 / TUI 形态 / 测试分层）
│   ├── design.md                 # 架构规范、核心 trait、Hook/Event 边界、测试矩阵
│   ├── extension-points.md       # 新需求归类（Event / Tool / Follow-up / 还是真需要 Hook）
│   ├── CONTEXT.md                # 术语表（Turn / Round / StopReason / 错误结果 vs 基础设施失败…）
│   ├── TODO.md                   # 未完成工作台账（完成即删条目）
│   ├── adr/                      # 决策记录，**文件名即摘要**
│   ├── arch/                     # 逐需求的 arch-explore → design → review 产出（目录名即主题）
│   ├── design-final/             # as-built：**当前实现的实际形态**（含实现与设计的分歧）
│   ├── design-plans/             # 设计计划（`completed/` 为已归档）
│   ├── exec-plans/               # 执行计划（`completed/` 为已归档）
│   └── reports/                  # 评审与验收报告
├── .claude/                      # Claude Code 本地配置
└── tmp/                          # 临时目录（已被 .gitignore 忽略，勿放正式内容）
```

根文件：`Cargo.toml`（workspace 成员表）、`AGENTS.md`（`CLAUDE.md` 是指向它的软链）、`LICENSE`、`README.md`。

两条补充指针 —— `docs/arch/` 里个别**跨需求**的常引文档值得单独点出：
`prompt-delivery/notes.md`（把项目约定送到模型面前的通道与成本）、`commands/login-model-behavior/`（`/login` & `/model` 行为设计）。
**尚未落地的主题**见 `docs/TODO.md` §C。

## 常用命令

- `cargo build` / `cargo clippy --all-targets` / `cargo fmt`
- **`./scripts/par-test.sh` —— 全量测试的唯一入口，必须用它**
  - `cargo test --workspace` **串行**跑各测试二进制（总耗时 = 各二进制之和），实测 **≈34s**；
    本脚本跨二进制并行（默认 8 路），**≈7.4s**，用例完全相同。
  - 选项：`--jobs N` 改并行度、`-- --ignored` 透传给每个二进制、`--no-build` 跳过构建。
  - 失败时退出码非 0，完整输出落在 `$TMPDIR/yushan-par-test-keep-*.log`。
  - ⚠️ 直接跑 `cargo test --workspace` **不算跑过测试**；只在需要 cargo 原生输出（如 `--ignored` 的 e2e、基准）时单独用它。
- `cargo nextest run --workspace` — 已装（`~/.cargo/bin`）。每用例独立进程、隔离更严，但本机 fork 贵 ⇒ **≈14.1s**，比脚本慢；需要重试 / 慢测试报告时用它。
- `./scripts/build-baseline.sh <label>` — 构建耗时基线（冷 / 改底层 crate / 改叶文件 / 无改动），对比构建配置收益用
- `python3 scripts/cancellation-mutation.py` — **取消链路的变异测试**（ADR-0046）：注入 12 处 bug ⇒ 期望用例变红；一次跑完全部并给汇总表（`--list` 只列清单，数字参数只跑指定编号）
- `cargo run -p ys-coding-agent` — 交互式 TUI
- `cargo run -p ys-coding-agent -- --resume [<id>]` — 恢复既有会话（**默认新会话**；裸 `--resume` 列表选择，`--resume <id>` 指定 id / 路径；`=` 形态已移除）
- `cargo run -p ys-coding-agent -- -p "任务"` — 单次 Print 模式（边生成边打印文本增量）
- `cargo run -p ys-coding-agent -- --json "任务"` — JSON 事件模式（逐行 `Envelope`）
- `cargo run -p ys-coding-agent -- --json --stats "任务"` — 额外输出背压统计到 stderr
- `cargo test -p ys-coding-agent e2e -- --ignored --nocapture` — 端到端（需 `YUSHAN_API_BASE` + `YUSHAN_API_KEY`）
- `cargo test -p ys-coding-agent --test e2e_skills -- --ignored --nocapture` — **R8 对账**（skill 目录段的真实 input token 成本；`#[ignore]`，**尚未校准**，需同上凭证）
- `cargo test -p ys-coding-agent --test e2e_bench -- --ignored --nocapture` — **基准**（同一任务的轮数 / 墙钟 / `ttft`·`gen`·`tools` 切分；`#[ignore]`，**只测量、不做任何内容判定**，需同上凭证；调参见 `YUSHAN_BENCH_REPEATS` / `YUSHAN_BENCH_ONLY` / `YUSHAN_BENCH_TIMEOUT_SECS` / `YUSHAN_BENCH_CLEAN` / `YUSHAN_BENCH_HEAVY`；**默认保留**逐轮日志等证据供回溯；长跑项默认跳过（机制保留，当前无条目用它）。默认集 7 条 ≈ **2 分钟**；其中 `fact/1` 是**可判定**任务 —— 时间与质量同时可测，是唯一能做外部对照的提示）

环境变量：`YUSHAN_API_BASE` / `YUSHAN_API_KEY` / `YUSHAN_MODEL`；`YUSHAN_REASONING_EFFORT`（`minimal` / `low` / `medium` / `high` / `xhigh` / `max`，默认 **`high`**；仅 provider 声明支持时随请求发送，当前为 `opencode-go`）；`YUSHAN_CHANNEL_CAPACITY`（信道容量，下限 16）；`YUSHAN_SESSIONS_DIR`（会话目录覆盖，测试用）；`YUSHAN_LOG_DIR`（日志目录覆盖，测试/基准用 —— 凭证仍在 `$HOME`，故隔离日志不能靠改 `HOME`；非空为 Override，只写不清理；未设置或空串为 Default，正常启动清理超过 7 天的 `debug-*.jsonl`）；`YUSHAN_DEBUG`（结构化执行日志：**未设置或空串默认开启**；`1/true/yes/on` 开启，其余非空值关闭；**优先于命令行 `--debug`**，`--debug` 仍兼容）；`YUSHAN_DEBUG_ARGS`（工具参数摘要档位 `none` / `head` / `full`，默认 `head`；脱敏在所有档位都生效）；`YUSHAN_SEARCH_SEARXNG_URL` / `YUSHAN_SEARCH_DEEPSEEK_KEY`（亦认生态通用名 `DEEPSEEK_API_KEY`，命名空间名优先）/ `YUSHAN_SEARCH_TAVILY_KEY`（亦认生态通用名 `TAVILY_API_KEY`，命名空间名优先）/ `YUSHAN_SEARCH_BOCHA_KEY` / `YUSHAN_SEARCH_FIRECRAWL_KEY`（搜索后端，**各自配置才进链**）；`YUSHAN_FETCHED_DIR`（webfetch 落盘目录覆盖，须为**绝对路径**，测试用）；`YUSHAN_BG_DIR`（后台命令落盘根目录，须为**绝对路径**，未设置时默认 `<temp_dir>/yushan-bg`；细节见 `docs/arch/execution-efficiency/design.md` §4.5）。

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
- 实现顺序遵循路线图：最小闭环 → 可用适配器 → 静态组件生态 → 动态插件 → Coding Agent MVP。
  **当前**：前三级与 Coding Agent MVP 已落地；**动态插件有意推迟** —— ADR-0021 只留 `ToolPlugin` 装配缝、不建 hook 系统（ADR-0014 同）。剩余项见 `docs/TODO.md`。
- 新模块和接口改动须能对应上 `docs/design.md` §12 测试矩阵中的条目
- **测试隔离（并行安全，血的教训）**：
  - 临时目录名必须含**进程内原子序号 + `process::id()`**（`SystemTime::as_nanos()` 在 macOS 只到微秒级，并行测试会撞名 → 互删文件）
  - **进程级 env 改写必须走 `crate::test_env::env_lock()` + `EnvRestore`**（RAII 恢复）；直接把 `set_var` 散在测试里会造成 getenv/unsetenv 数据竞争
- **关键逻辑要做变异测试**：注入 bug 验证测试能抓到。本项目多个真 bug（死锁、UTF-8 损坏、`/new` 失效）都是「测试全绿时依然存在」，靠变异测试与端到端复现才暴露
- **事实性断言的指针纪律**：任何关于「X 当前是 Y 状态」的静态断言（已实现/未实现、是/否占位、有/没有 trait），必须引用 `code/` 或带 `[已关闭]`/`[已实现]` 标记的 `docs/design-final/`；旧 ADR / 旧 design plan 只记 **why**，不能用来佐证 **now**。候选清单里出现这类断言而无指针 ⇒ 不写进回答。
- **流式/字节路径注意 UTF-8 边界**：TCP 分片可能切在多字节字符中间；缓冲必须是**字节级**，只在完整行上解码
- `tmp/` 已被 `.gitignore` 忽略，勿将正式内容放入
