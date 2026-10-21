# LSP 支持（Python / Rust）— 架构上下文

> 产出：`/arch-explore`（2026-10-21）。性质：**现状 + 约束 + 范围**。**不做方案设计** —— 方案交 `arch-design`。
> 缘起（用户原话）：「考虑是否支持 python 和 rust lsp」。
> 证据纪律：本文所有「当前是 X」的断言都指 `code/` 或带标记的 `docs/design-final/`；旧 ADR / 旧
> design plan 只用来解释 **why**（`AGENTS.md`「事实性断言的指针纪律」）。本机实测数据标注采集日期。
>
> **状态：本轮不做（2026-10-21，用户裁定）。** 依据与重开条件见 [`research.md`](./research.md) §4
> 与本文件末尾的 §决策。

---

## 概述

「支持 Python / Rust LSP」是**三件可独立成立的事**，它们的成本差一到两个数量级，必须先拆开：

1. **诊断**（errors / warnings：类型错、语法错、未使用导入）—— 结果小、价值直接；
2. **语义导航**（definition / references / hover / documentSymbol）—— 替代「grep + read 猜」，
   价值在**少几轮往返**，不在工具本身快；
3. **改完自动查诊断**（`edit` / `write` 之后自动附上受影响文件的诊断）—— 形态上最诱人，
   但它是**改动既有工具**，不是新加一个工具。

本仓**零 LSP 代码**（全仓 `rg -i "lsp|language server|rust-analyzer|pyright|pylsp"` 在 `*.rs` 里
只有 `ToolSpec` 的子串命中，无一处真 LSP）。但**两处既有文档已经裁定过归类**，是本次探索的起点：

| 用例 | 既定落桶 | 指针 |
|---|---|---|
| LSP 查询（长驻进程） | **Tool**，且**更适合做成 adapter crate** | `docs/extension-points.md:54` |
| LSP 改完自动查诊断 | **工具内部**（`EditTool` 是我们的 ⇒ 不构成 hook 需求） | `docs/extension-points.md:55`、`docs/arch/hooks-and-plugins/design.md:281` |

> 这两条只定了**归类**（不是 hook、不进循环），**没有定形态、能力面、进程生命周期与配置面** ——
> 那正是本文要摆出来的岔路。

---

## 现有架构

### 模块边界

```text
apps/coding-agent/src/toolset.rs   build_tools()  ← 工具装配的唯一接缝
        │  Vec<Box<dyn Tool>>（有序）
        ▼
crates/ys-tool/src/registry.rs     ToolRegistry（保序 Vec<ToolSpec>）
        │  &[ToolSpec] → 系统提示里的工具清单
        ▼
crates/ys-loop/src/basic.rs        Step 9 逐次构造 ToolContext → tool.call(input, ctx)
        ▲
        └── 结果作为 ToolResult 进 session.messages()（下一轮模型必然看见）
```

- **装配顺序即语义**：`apps/coding-agent/src/toolset.rs:40` 的 `EXPECTED_ORDER` 是**唯一真值**，
  现为 **9 元**（`read/write/edit/bash/grep/find/ls/websearch/webfetch`；`bash_bg`/`bash_poll`/
  `subagent` 由别处追加，见 `toolset.rs:88` 的 `build_tools` 与 `apps/coding-agent/src/assembly.rs`）。
- **工具接口只有四个方法**：`spec()` / `timeout()` / `is_concurrency_safe()` / `call()`，
  见 `crates/ys-tool/src/lib.rs:31`、`:42`、`:53`、`:58`。
- **`ToolContext` 是只读借用**：`crates/ys-tool/src/context.rs:9`，含 `boundary` / `cwd` /
  `workspace_root` 与**三个同形的可选窄能力挂载点** `summarizer:19` / `subagent:25` /
  （新增挂载点见该文件顶部注释）。**没有会话级持有者**，由 `BasicLoop` 逐次构造。
- **长驻进程的既有先例**：`adapters/tools-basic/src/background.rs:321` 的 `BackgroundTasks` ——
  **进程级注册表 + `Arc` 共享 + app 层显式清理**，`impl Drop`（`:741`）只作 panic 兜底。

### 核心抽象

| 抽象 | 位置 | 与 LSP 的关系 |
|---|---|---|
| `Tool` trait | `crates/ys-tool/src/lib.rs:31` | LSP 查询若走工具路线，落这里 |
| `ToolSpec` | `crates/ys-tool/src/spec.rs:5` | `{name, description, parameters}`；**刻意无 prompt 字段**，prompt 侧的 snippet/guideline 另在 `prompt.rs` |
| `ToolContext` | `crates/ys-tool/src/context.rs:9` | 只有只读借用；**拿不到 `Model`、拿不到跨次状态** |
| `BackgroundTasks` | `adapters/tools-basic/src/background.rs:321` | **长驻进程注册表的现成范本**（句柄不复用 + 条目带 `session` 双防线） |
| `SearchBackends` | `adapters/tools-basic/src/exec.rs` | **「探测环境已有二进制、缺失即降级、绝不下载」的范本**（ADR-0015） |

### 关键数据流

**每轮固定前缀**（决定 LSP 的 token 成本）：

```text
build_tools() → specs → prompt::build_system_prompt(cwd, &specs, &catalogue)   // apps/coding-agent/src/prompt.rs:33
                       ├─ 工具清单：由 &[ToolSpec] 派生（顺序 = 工具表顺序）
                       ├─ tool_snippet(name)   （prompt.rs:161，按 name 查表，逐个手写）
                       └─ tool_guidelines(names)（prompt.rs:197，条件式追加）
→ limits.fixed_prompt_tokens = fixed_prompt_tokens(&system_prompt, &specs)     // apps/coding-agent/src/main.rs:1085
```

**工具调用**（决定 LSP 的墙钟成本）：

```text
BasicLoop Step 9 → tool_budget(tool, limits) → tool.timeout() ?? limits.bash_timeout ?? 300s
                 （crates/ys-loop/src/basic.rs:43-50）
                 → 并行准入：批次 > 1 且**每一个**都 is_concurrency_safe()（默认 false）
                 → tool.call(input, ctx)
```

### 外部依赖（现状）

| 依赖 | 现状 | LSP 是否需要 |
|---|---|---|
| `serde_json` | 已在 `ys-tool` / 各 adapter / app | ✅ JSON-RPC 必需，无新增 |
| `tokio`（`process` / `io-util` / `time`） | 已在 `ys-tools-basic` | ✅ stdio 帧读写；`tools-basic` 已开 `process` |
| 手写 JSON-RPC（`Content-Length` 帧） | 无 | ⚠️ 可自写（~300–500 行） |
| `lsp-types`（纯数据类型） | 无 | ⚠️ 可选，换类型安全 |
| `async-lsp` / `tower-lsp`（拉 `tower`） | 无 | ⚠️ 与「零新依赖」基线的最大冲突点 |

依赖基线（**§约束**里展开）：ADR-0015 明确否决 `ignore`/`regex`/`globset`/`walkdir`；
`adapters/tools-web/Cargo.toml` 明确禁止第二个 TLS 栈。**新依赖不是自由项。**

---

## 约束

### 技术

- **依赖方向单向**：`ys-core ← 组件接口 ← loop/runtime ← 应用适配器`；`ys-core`/`ys-event`/`ys-protocol`
  不含 tokio。LSP 客户端只能落在 **adapter crate** 或 app 层，且**不得**让 `ys-tool` / `ys-loop` 认识它。
- **zero-new-deps 是本仓的默认立场**，不是偏好：ADR-0015 为「不引 4 个依赖（含正则引擎）」放弃了
  自研 `.gitignore` 解析；`tools-web` 为「只有一个 TLS 栈」禁了 `rustls-tls`。**LSP 客户端若引
  `async-lsp`/`tower-lsp`，是一个需要单独论证的破例。**
- **单工具 spec 预算 ≤ 400 B**（≈100 est tok）是既有设计约束（`docs/arch/hooks-and-plugins/design.md:385`），
  只有 `websearch`/`webfetch` 有显式豁免。「讲清 LSP 怎么用」很容易撞这条线。
- **工具面必须恒定**：`apps/coding-agent/src/toolset.rs` 的工具表**绝不按登录态 / 配置有无增减**，
  降级一律发生在**调用期**（明确错误结果）。⇒ LSP 工具若采用「探测到 server 才注册」，**与本条冲突**（见 D6）。
- **每回合必须新建 `BoundarySource`**；**取消即消息**（ADR-0013）。LSP 查询是**长驻进程上的等待**，
  必须能被 `Boundary::Abort` 探针看见，且中止后不留孤儿。
- **`ToolContext` 没有会话级持有者**（`docs/arch/execution-efficiency/design.md` §4.5 的裁定）：
  会话级状态走**进程级注册表 + app 层显式清理点**，**不走** `ToolContext` 挂载点。LSP server 属会话级资源。
- **超时预算**：`tool.timeout()` → `limits.bash_timeout` → **300s**（`crates/ys-loop/src/basic.rs:43-50`），
  超时会 **drop future**。工具可声明自己的 `timeout()`，但**模型在等**的这段时间是纯墙钟。
- **不做安全**（AGENTS.md 硬约束）：LSP server 是**在工作区里跑的第三方进程**。
  rust-analyzer 会**编译并执行 proc-macro / build script** ⇒ 开 LSP ≈ 信任工作区，
  与 `cargo check` 同级的信任面。本项目**不**提供沙箱/审批，这条只能**声明**，不能**解决**。
- **流式/字节路径的 UTF-8 边界**：JSON-RPC 是**字节级 `Content-Length` 帧**，缓冲必须字节级，
  只在完整帧上解析（AGENTS.md 硬约束的同类风险）。
- **测试隔离**：真 LSP server 不可用于常规测试（慢、需工具链）⇒ 需要一个**假 LSP server 二进制**
  （先例：`grep`/`find` 测试注入假 `rg`/`fd`），真 server 只进 `#[ignore]` 的 e2e。

### 性能

- **每个新工具的常驻成本 ≈ +173 est tok/轮（+8.3%）**（`docs/arch/hooks-and-plugins/design.md:264`）。
  两个工具（如 `lsp` + `diagnostics`）≈ **+350 tok/轮**，且**每轮都付**。
- **墙钟杠杆是「轮数」，不是「工具耗时」**：`docs/arch/explore-speed/cost-model.md` §0 ——
  `一轮墙钟 = 模型往返（秒级） + 工具执行（本地只读毫秒级）`。**LSP 的价值主张必须落在「少几轮 /
  少烧 token」，不能落在「LSP 比 grep 快」**（这句话在本仓的实测数据下是错的）。
- **冷启动是真成本**：rust-analyzer 首次索引本仓是**秒级到分钟级**；`pyright` 首次分析同理。
  一次「冷启动 + 一次查询」可能撞满 300s 预算，而**模型在此期间只能等**。⇒ 惰性起 + 预热 + 超时分层
  （启动握手 / 查询各一档）几乎必然要进设计。
- **结果量可能很大**（references 列表、诊断全文）⇒ 必须走 `ys_tool::truncate` + notice 的统一形态，
  否则会顶穿回合级工具结果预算（`RunLimits::max_turn_tool_result_tokens`，`crates/ys-component/src/limits.rs`）。

### 演进

- **前缀指纹承重**：prompt 前缀字节一变，所有用户的 prompt cache 全废。加工具 = 改前缀，
  是**一次性**成本；但**运行期**改工具面 = **每轮**重付（`hooks-and-plugins/design.md` §4.3）。
- **加工具仍是「6 处手工连锁点」**：`EXPECTED_ORDER` / `build_tools` 的 `vec!` / 三处
  `assert_eq!(len, 9)`（`toolset.rs:226`、`:316`、`:493`）/ `prompt.rs` 的 snippet + guideline
  / 指纹锚。`ToolPlugin` 装配缝**尚未落地**（`docs/TODO.md` §B），所以**没有**「一行登记」的捷径。
- **subagent 的工具表是显式决策**：子表 = 父表去掉派发工具（`apps/coding-agent/src/assembly.rs`、
  `apps/coding-agent/src/subagent.rs`）。LSP 工具**要不要给子 agent** 是一个必须回答的问题 ——
  给，则多个子 agent 争用同一台 server 的文档状态；不给，子 agent 就查不了定义。
- **`-p` / `--json` 三模式共用同一套工具**：LSP 不能只在 TUI 下可用。

### 组织

- 单人维护、无专职基础设施：**每多一个常驻进程 + 每多一个依赖，都是长期供养成本**。
- 组织上最关键的约束是**「先证明要它」**：本仓刚落地 `bash_bg`/`bash_poll`，
  「跑 `cargo check` / `pyright` 拿诊断」这条**无需写任何新代码**的路径**已经存在**（见 D2c）。

---

## 现状核查（本次探索的硬事实）

| # | 事实 | 证据 |
|---|---|---|
| F1 | 全仓**零 LSP 代码** | `rg -i "lsp\|language server\|rust-analyzer\|pyright\|pylsp"` 在 `*.rs` 无真命中；命中全在 docs |
| F2 | 归类**已有裁定**（查询=Tool；改后查诊断=工具内部） | `docs/extension-points.md:54-55`、`docs/arch/hooks-and-plugins/design.md:281` |
| F3 | 长驻进程**已有范本**（进程级注册表 + 六个清理点） | `adapters/tools-basic/src/background.rs:321`；`docs/arch/execution-efficiency/design.md` §4.5 |
| F4 | 探测/降级**已有范本**（绝不下载） | ADR-0015；`adapters/tools-basic/src/exec.rs` 的 `SearchBackends` |
| F5 | 本机 **rust-analyzer 不可用**：`~/.cargo/bin/rust-analyzer` 只是 rustup shim，stable toolchain **未装该组件** | `rustup component list --installed` 无 `rust-analyzer-*`；运行报 `Unknown binary 'rust-analyzer'`（2026-10-21） |
| F6 | 本机 **Python LSP 一个都没装**：`pyright` / `pylsp` / `basedpyright` 均不在 `PATH` | `which pyright pylsp basedpyright` 全空；`python3` 3.9、`pip` 25.2 在（2026-10-21） |
| F7 | **异步长命令路径已存在**：`bash_bg` + `bash_poll` 已落地 | `docs/design-final/` 与 `adapters/tools-basic/src/bg_tools.rs` |
| F8 | 工具表**当前 9 元**（`EXPECTED_ORDER`），另有 `bash_bg`/`bash_poll`/`subagent` 由别处追加 | `apps/coding-agent/src/toolset.rs:40`、`:88` |

> F5/F6 的意义：**「支持 LSP」在本机今天是「先装 server」** —— 参照 ADR-0015 的「绝不下载」立场，
> 本项目**不该替用户装 LSP server**，只能「环境有就用、没有就用明确错误结果告知」。

---

## 需求范围

> ⚠️ 本节是**候选**，不是定论 —— 范围由用户在「未澄清问题」里裁定后落定。

### 范围内（候选，按成本从低到高）

1. **诊断（Rust + Python）**：给模型一个「这个文件的编译/类型错误是什么」的能力。
2. **语义导航（Rust + Python）**：definition / references / hover / documentSymbol，
   替代「grep 猜 + read 猜」。
3. **改完自动查诊断**：`edit`/`write` 成功后，把受影响文件的诊断附在工具结果里。

### 范围外（明确不做）

- **补全（completion）/ 重命名（rename）/ code action / 格式化**：对 agent 是低价值高复杂度。
- **语法高亮 / TUI 内联诊断渲染**：那是编辑器的事，不是 harness 的事。
- **替用户下载 / 安装 / 管理 LSP server**（ADR-0015 立场的直接推论）。
- **多根工作区（multi-root）、monorepo 多语言根、Jupyter/notebook**。
- **把 LSP 做成 hook**：既有裁定已排除（`docs/extension-points.md:55`）。
- **改 `ys-*` 核心 crate 去「内建」LSP**：违反「最小核心」与依赖方向。

### 关键场景（成功路径）

| # | 场景 | 期望 |
|---|---|---|
| S1 | 改完 Rust 文件，模型想知道有没有编译错 | **不必**等 `cargo check` 分钟级命令；拿到结构化诊断（文件:行:列 + message + severity） |
| S2 | 模型想知道某个符号在哪定义 / 被谁引用 | 一次调用拿到**定位列表**，而不是 3~5 轮 grep + read |
| S3 | 环境里没有 LSP server（F5/F6） | 工具**不消失**，调用期给**明确错误结果**（模型可换 `grep` 路线），**不崩、不静默** |
| S4 | 用户 Ctrl-C / `/new` / `/resume` / 退出 | LSP server 被清理，**无孤儿进程**（照 `BackgroundTasks` 的六点） |
| S5 | 冷启动（首次索引大仓） | 不撞穿 300s 预算、不把「正在索引」误报成「无诊断」 |

### 异常 / 边界场景

- **server 未安装 / 装了但握手失败 / 版本不兼容** ⇒ 明确错误结果 + 降级到无 LSP 的路径。
- **冷启动长** ⇒ 「索引中就绪」与「索引完无诊断」必须能区分，否则模型得到**假阴性**（最危险的失败态）。
- **push vs pull 诊断模型不同**：rust-analyzer 支持 `textDocument/diagnostic`（pull），
  pyright 主要靠 `publishDiagnostics`（push）⇒ **没有标准的「诊断已就绪」信号**，
  需要启发式等待（超时 / 静默期），这是本需求最硬的技术难点之一。
- **多语言 / 多工作区** ⇒ 一个 server per `(root, language)`，注册表要按这对键索引。
- **文件在 agent 之外被改动**（用户编辑器、其它进程）⇒ 查询前需从磁盘重读并做 `didOpen`/`didChange` 同步。
- **病态 server**（卡死、协议漂移、疯狂刷通知）⇒ 超时 + 杀进程组 + 明确归因文案（照 `bash_poll` 的「措辞纪律」）。
- **子 agent 并发** ⇒ 多个子 agent 同时用同一台 server；只读查询可接受，文档同步写状态不可。
- **`-p`/`--json` 无 TUI 模式**：没有交互提示，server 起不来也只能是**结果里的错误**，不是 stderr 提示。

---

## 关键设计岔路（供 `arch-design` 取舍，本文不选）

### D1 · 能力面：诊断 / 导航 / 两者都要

三件事的成本量级不同：**诊断**的结果小、请求少、与「改完立刻知道对不对」直接挂钩；
**导航**的请求种类多（4+ 个 method）但每次便宜；**改后自动诊断**是**改工具**不是加工具。
必须**逐项**裁定要不要，不能一句「支持 LSP」打包。

### D2 · 形态

| 候选 | 形状 | 代价 | 备注 |
|---|---|---|---|
| **D2a** 新 adapter crate + 工具 | `adapters/tools-lsp`（`ys-tools-lsp`）+ 1~2 个工具 | 6 处连锁点 + 每轮 ~173 tok/工具 + 一台常驻进程 | 与 F2 的裁定一致 |
| **D2b** 改 `EditTool` 自带诊断 | 工具内部做（F2 的另一半裁定） | `tools-basic` 要认识 LSP（**或**新增 `ToolContext` 挂载点，按 `execution-efficiency/design.md` §4.5 的记账 blast radius 大） | 「形似 hook」的那条路 |
| **D2c** 用 `bash_bg` 跑 CLI 检查器 | **零新代码**：`cargo check --message-format=json` / `pyright --outputjson` | 分钟级、无增量、无导航 | **必须先评估**，因为它是现状能力 |

> D2c 是本轮最有价值的对照：**诊断**这件事，`bash_bg` + CLI 已经能做（F7），
> 差别在「分钟级 + 全仓」vs「亚秒级 + 增量 + 单文件」。**导航**则 CLI 基本做不到，这才是 LSP 的独有领地。

### D3 · 进程生命周期

- **进程级注册表 + app 层显式清理点**（照 `BackgroundTasks` 六点）：与既有裁定一致（`ToolContext`
  不做会话级持有者）。
- 待定：**惰性启动**（首次查询时才起，付出冷启动）vs **启动期预热**（装配期起 + 后台索引，
  不阻塞工具调用，但**没用到 LSP 的用户也养着一台进程**）。
- 待定：是否设**空闲回收**（多久不用杀掉）。

### D4 · 协议实现：手写 vs 引依赖

手写 JSON-RPC 是 ~300–500 行（`Content-Length` 帧、请求 ID 关联、通知分发、能力子集），
**零新依赖**且完全可控；`lsp-types` 只给类型（纯数据，便宜）；`async-lsp`/`tower-lsp` 给整套
（最省事，但引 `tower` 生态）。**这条要与 ADR-0015 的依赖基线正面摊牌**，不能默认选「省事那个」。

### D5 · 配置面

- 自动探测 `PATH`（照 `SearchBackends`）+ 环境变量覆盖（照 `YUSHAN_SEARCH_*` 链）？
- 还是进 `~/.yushan/config.json`（server 路径、启动参数、启用开关）？
- 还是**零配置**（只用固定的默认命令名）？

注意 skills 的 `D2′`（`docs/arch/skills/context.md`）刚刚裁过「默认路径即最终路径、不引入路径配置项」——
但 LSP 的处境不同（server 位置因机器而异），**这条分歧要显式记录理由**。

### D6 · 工具面恒定 vs 探测到才注册

既有规则：工具恒注册、降级发生在调用期（`toolset.rs` 注释）。但 LSP server **可能根本不存在**
（本机 F5/F6 就是这种状态）⇒ 恒注册意味着**给用不上的用户白付每轮 token**。
需要一次显式裁定：是守「工具面恒定」（调用期错误），还是开一个「启动期探测决定注册」的例外。

### D7 · 与 subagent 的关系

LSP 工具是否进子表？给 ⇒ 子 agent 也能查定义，但多进程/多连接争用；不给 ⇒ 子 agent 能力受限。
（子表是显式决策：`apps/coding-agent/src/assembly.rs` / `subagent.rs`。）

---

## 未澄清问题

> **本节随 2026-10-21 的「本轮不做」裁定一并冻结** —— 十问全部未答，保留原样供再次启动时使用。
> 重开时应先读 §决策 的三条触发条件，判断是否值得重新评审。

- [ ] **Q1（范围）**：要的是**诊断**、**语义导航**、还是**两者**？还是先只做一件？
- [ ] **Q2（触发形态）**：诊断是「模型主动调工具」还是「`edit`/`write` 后**自动附上**」？
      后者会改既有 `EditTool`（D2b），成本与 blast radius 明显更大。
- [ ] **Q3（与现有能力的关系）**：`bash_bg` + `cargo check`/`pyright`（D2c）**已经能拿诊断** ——
      它是「够用」还是「太慢、必须上 LSP」？这条决定整个需求的优先级。
- [ ] **Q4（server 来源）**：接受「环境有才用、没有就明确报错」（ADR-0015 立场），
      还是希望 harness 帮忙装 / 引导装？（本机 rust-analyzer 组件缺、Python LSP 全无，F5/F6）
- [ ] **Q5（配置面）**：server 路径与启动参数走**环境变量**、`config.json`、还是**零配置**？（D5）
- [ ] **Q6（启动策略）**：**惰性启动**（首次查询付冷启动）还是**启动期预热**（所有用户养一台进程）？（D3）
- [ ] **Q7（依赖立场）**：是否接受为 LSP 引**新依赖**（`lsp-types` / `async-lsp`）？
      若否，则确定走手写 JSON-RPC 子集。（D4）
- [ ] **Q8（注册策略）**：server 不存在时，工具是**恒注册 + 调用期报错**，还是**探测不到就不注册**？（D6）
- [ ] **Q9（子 agent）**：LSP 工具是否进子表？（D7）
- [ ] **Q10（TUI 可见性）**：LSP 的活动（索引中 / 诊断数）要不要进 `CodingView` 给用户看？
      还是完全静默（只在工具结果里）？

---

## 决策（2026-10-21）：本轮不做

**裁定**：不实现 LSP 支持；本目录两份文档（本文 + [`research.md`](./research.md)）保留为
**再次启动时的输入**，不删除、不改写。

**依据**（本轮探索得出的三条，按分量排序）：

1. **导航才是 LSP 的独有领地，而它还没有实测痛点**。诊断那一半已被现成的 `bash_bg` + CLI 检查器
   （`cargo check --message-format=json` / `pyright --outputjson`）覆盖大半，**零新代码**；
   `definition` / `references` 确实无替代，但本仓尚无「为一个符号绕好几轮」的采样证据
   （对照：`tool-call-efficiency/context.md` 与 `explore-speed/cost-model.md` 那类
   **带事件流数据的**痛点记录）。
2. **参照系是 1:2，且没做的是用户日常在用的两家**。Pi 与 Codex CLI 均**零 LSP**
   （证据见 `research.md` §1 / §3），都靠模型自己跑构建；CodeWhale 做了的是
   **~3.6k 行 + 一台常驻 server + 配置面**的完整版，不是「半天的事」。
3. **本机环境当前跑不通**：`rust-analyzer` 未装 toolchain 组件、`pyright`/`pylsp` 均不在
   `PATH`（本文 §现状核查 F5/F6）⇒ 任何 e2e 验证都要先改用户环境。

**重开条件**（满足任一条再启动，且启动时应重采基线而不是复用本文的数字）：

- 反复出现**具体案例**：「改完不知道错在哪」或「追一个符号的定义/引用要绕 3 轮以上」，
  且有 `~/.yushan/sessions/*.jsonl` 事件流证据（采样口径照 `explore-speed/research.md`）。
- 用 `bash_bg` + CLI 检查器跑一次**带计时的基线**，证明它在**小步改**场景下确实不够用
  —— 这是 `context.md` 的 D2c 对照，成本极低，应作为重开的第一步。
- 环境里 `rust-analyzer` / `pyright` 稳定可用（重开时先核 F5/F6 是否仍成立）。

**本轮不动的东西**（记账，避免下次误以为漏了）：

- **不改任何代码**（`code/` 零改动）。
- **不写 `docs/TODO.md`**：该清单的收录判据是「ADR 已 accepted 或设计已写完，但代码里确实还没有」，
  本需求**没有设计文档**，不符判据（`docs/TODO.md` 开头的维护约定）。
- **不立 ADR**：本次是**优先级裁定**（先不做），不是「论证后否掉某方案」的设计否定。
  若后续需要正式归档否决理由，再补 `docs/adr/0046-*.md`。

---

## 后续建议

- **先跑一次 D2c 对照**再谈设计：用现成的 `bash_bg` 跑 `cargo check --message-format=json`，
  记下**轮数与墙钟**，作为「LSP 值不值」的基线。`docs/arch/explore-speed/design.md` 的方法论
  （先采基线、再改）适用。
- 若要做，建议 `arch-design` 按 **D2a（新 adapter crate）** 起手，把 **D3/D4/D5/D6** 作为设计的
  主要取舍点，并把 **D2b（改 `EditTool`）** 单列为一期或明确推迟。
- 需要一次**真实 server 可用性**确认：本机需先 `rustup component add rust-analyzer`、
  并安装 `pyright`（或 `basedpyright`）才能跑通任何 e2e。
- 依赖与配置的裁定建议进 ADR（照 ADR-0015 / ADR-0016 的先例），因为二者都会成为长期约束。
