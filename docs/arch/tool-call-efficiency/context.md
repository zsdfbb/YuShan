# tool-call-efficiency — 架构上下文

> 日期：2026-10-10
> 性质：arch-explore 产出（现状 + 约束 + 范围）。**不做方案设计** —— 方案交 `arch-design`。
> 缘起（用户原话）：「每次 coding-agent 需要频繁的 `cd`、`ls`，有没有改进方法，能够快速获取它需要
> 的信息？比如 tree、ripgrep 等。」→ 归类为「**改进工具，减少无谓的调用与轮次**」。

## 概述

coding agent 的工具面（10 个：`read/write/edit/bash/grep/find/ls/websearch/webfetch/subagent`）已经
够用，但**模型的实际调用形态**在浪费轮次与上下文：它把探索动作塞进 `bash`（`cd … && grep -rn …`、
`ls a b c`、`sed -n a,b f1; sed -n c,d f2`），而不是用现成的结构化工具。

本需求要回答的是：**在不改模型、不加沙箱、不引新依赖的前提下，怎么让「一次调用拿到更多、下一轮
不再重复同一动作」**。

## 现状（问题证据）

### 采样口径

本机 `~/.yushan/sessions/*.jsonl` 共 5 个会话（**本仓的真实开发会话**，非构造语料）。统计脚本按
`content[]` 里的 `ToolUse` / `ToolResult` 计数（纯读，不改任何会话文件）。

| 指标 | 值 |
|---|---|
| 带工具调用的 assistant 消息 | 48 条（批次大小：1 个调用 ×20、2 个 ×26、4 个 ×2） |
| 工具调用总数 | 80（`bash` 69 / `ls` 8 / `read` 2 / `grep` 1；`find`、`edit`、`write` 0） |
| `bash` 调用以 `cd <workspace> && ` 开头的 | **68 / 69** |
| 这些 `cd` 前缀的字符数 | 3128（≈ 780 est token，**纯噪音**） |
| `bash` 的用途分布 | `grep` 20、`echo` 17、`sed` 13、`ls` 9、`head` 5、`git` 3、其它 2 |

> ⚠️ 采样规模小（单仓、单模型、5 个会话），只能作**定性**依据；结论不依赖具体比例，依赖的是
> 「有 68/69 这种量级的前缀」与「探索类命令占多数」这两个形态。

### 三条根因（均可指到代码）

**R1 · `bash` 的 cwd 契约没有告诉模型。** `BashTool` 每次都是 `sh -c` + `current_dir(workspace)`
（`adapters/tools-basic/src/bash.rs:115`），**`cd` 不跨调用保留**；而工具描述只有一句
「Execute shell commands. Supports timeout.」（`bash.rs:94`）。模型因此防御性地给每条命令加
`cd <绝对路径> && `。系统提示末尾那句 `Current working directory: …`（`prompt.rs`）说的是**进程
的 cwd**，没有说「bash 每次都在这里起」。

**R2 · 结构化工具只吃一个 `path`，多目标只能退回 bash。**

| 工具 | 参数面 | 现状后果 |
|---|---|---|
| `ls` | `path` / `limit`（`adapters/tools-basic/src/ls.rs:42`） | 列 3 个目录 = 3 次调用，或 bash `ls a b c` |
| `grep` | `pattern` / `path` / `glob` / … | 搜 5 个文件 = 5 次调用，或 bash `grep -rn pat f1..f5 \| head -80` |
| `find` | `pattern` / `path` / `limit` | 同上 |
| `read` | `path` / `offset` / `limit` | 读多个区间 = N 次调用，或 bash `sed -n a,b f1; sed -n c,d f2; echo ===` |

即：模型想「一次覆盖多目标」时，**唯一能做到的是 bash** —— 代价是丢掉 `rg`/`fd` 的 `.gitignore`
语义、丢掉统一的截断/notice 形态、且命令串越长越容易一次错全错。

**R3 · 只读批次实际上仍是串行。** `BasicLoop` Step 9 有「批次 > 1 **且每一个**工具声明
`is_concurrency_safe()` 才并行」的通道（`crates/ys-loop/src/basic.rs:473` 起，准入判定在
`:507`，含并行/混合/限流测试），但除
`subagent` 外**没有任何工具声明 `true`**（默认 `false`，`crates/ys-tool/src/lib.rs:53`）。采样里
26 条 2 调用批次 + 2 条 4 调用批次（多为 `read`/`grep`/`ls` 组合）**全部按串行执行**，上限
`DEFAULT_MAX_PARALLEL_TOOLS = 4`（`crates/ys-component/src/limits.rs:8`）从未被用到。

### 已澄清的既有事实（避免重复劳动）

- **`rg` / `fd` 已经在工具面里**（`grep` / `find` 的后端；ADR-0015）。缺 `rg`/`fd` 时走 POSIX 降级
  并在启动期给一行 stderr 提示。**本需求不需要「引入 ripgrep」**。
- **`tree` 二进制没有**，也不需要：`ls` 是纯 Rust `read_dir` 实现（不起子进程），加「递归/depth」
  是纯函数改动，不引依赖、不碰 `SearchBackends`。
- `bash`/`read`/`grep`/`find`/`ls` 的输出上限已统一（`limit` 条 / 1 MiB byte_cap / 2000 行·50KB），
  notice 形态由 `ys_tool::append_notices` 统一拼装（`crates/ys-tool/src/notice.rs`）。

## 现有架构

### 模块边界

```text
ys-core ──► ys-tool（Tool trait / ToolSpec / Registry / truncate / notice）
              ▲
              ├── adapters/tools-basic（read / write / edit / bash / grep / find / ls）
              └── adapters/tools-web（websearch / webfetch）
                      ▲
apps/coding-agent（toolset.rs 唯一装配接缝；prompt.rs 派生 system prompt）
```

- **工具实现**：`adapters/tools-basic`（通用适配器，无网络）。
- **装配**：`apps/coding-agent/src/toolset.rs::build_tools` —— 顺序即真值
  （`read, write, edit, bash, grep, find, ls, websearch, webfetch`，父表再追加 `subagent`）。
- **提示词**：`apps/coding-agent/src/prompt.rs::build_system_prompt(cwd, &specs, &catalogue)` ——
  工具清单一句话由 `tool_snippet(name)` 按 **name** 查表（`prompt.rs:127`），行为纪律由
  `tool_guidelines(&names)` 按 name 拼装（`prompt.rs:154`）。

### 核心抽象

```rust
// crates/ys-tool/src/lib.rs
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;                 // 名字 / 描述 / JSON Schema 参数
    fn is_concurrency_safe(&self) -> bool { false }   // ← R3 的闸门，默认关
    fn timeout(&self) -> Option<Duration> { None }
    async fn call(&self, input: Value, ctx: ToolContext<'_>) -> Result<ToolResult, ToolError>;
}
```

### 关键数据流（决定「改哪里会动到什么」）

```text
启动期（每进程一次）
  main → toolset::build_tools → Vec<Box<dyn Tool>> + 降级提示
       → prompt::build_system_prompt(cwd, &specs)        ← 工具清单一句话 + guidelines

每轮（BasicLoop）
  Step 5  registry.specs().to_vec() → ModelRequest.tools  ← ★ 工具描述每轮随请求重发
  Step 9  批次执行：all-safe ⇒ 并行（buffered，保序）；否则串行
```

> **成本含义**（依据 `docs/arch/prompt-delivery/notes.md`）：常驻文本（system prompt / tool 描述）
> 在**稳定前缀**里 ⇒ 缓存命中 ≈ 0.1×；**加一段常量文字进工具描述几乎免费**，而**每轮注入**
> 是它的 10 倍。反过来，**多一个探索轮**是一次完整的模型调用（延迟 + 该轮新 token）。

### 外部依赖

| 依赖 | 用途 | 备注 |
|---|---|---|
| `rg`（ripgrep） | `grep` 原生后端 | 缺失降级 `grep -r` |
| `fd` / `fdfind` | `find` 原生后端 | 缺失降级 POSIX `find` |
| `sh` | `bash` 工具 | 每次新进程；进程组 + `kill_on_drop` |
| （无 `tree`、无新 crate） | — | 本需求**不引入任何新依赖** |

## 约束

- **技术**
  - `ys-tool` / `ys-protocol` 零 tokio 的约束不涉及本需求；但**不新增依赖**是既定口径（ADR-0015）。
  - 工具参数面是**对模型可见的契约**：改 `ToolSpec`（名字 / 描述 / JSON Schema）必须同步
    `prompt.rs` 的 snippet / guideline 与两侧测试（含 `EXPECTED_ORDER`、漂移断言）。
  - 工具仍是**单次调用、无状态**：不做「跨调用保持 cwd 的持久 shell」—— 与「Agent 无状态」同源，
    且同批多个 `bash` 并发时会互相踩 cwd（`is_concurrency_safe` 的准入规则也依赖「工具无共享状态」）。
- **性能 / 成本**
  - 任何新增的常驻文本都进 `fixed_prompt_tokens`（system prompt + 工具表 JSON），
    而它参与压缩触发比较（R5 启动期警告）。**新增文本要按 token 计价**，不是「反正有缓存」。
  - 一次调用的输出上限（2000 行 / 50KB / limit 条）不得放宽；「一次拿更多」只能靠**更好的形状**，
    不能靠放大输出。
  - 轮次是主要成本（每轮一次模型调用）⇒ 本需求的收益指标是**同任务的调用数 / 轮数**。
- **演进**
  - 工具表顺序与 `prompt.rs` 的 snippet 表由测试钉住（漂移即红）；改一处必须同步另一处。
  - 子 agent 的工具表 = 父表去掉 `subagent`（**字节前缀**命中缓存）——改共享工具的描述会同时改两侧，
    这条不变量不受影响，但**不要给父表单独加描述**。
- **组织**
  - 小团队、无 sandbox：安全边界不在本需求内（`AGENTS.md` 硬约束「不做安全」）。
  - 关键逻辑必须能做**变异测试**；测试隔离用 `test_env::env_lock()` + 唯一临时目录。

## 需求范围

### 范围内（候选，待澄清后取子集）

1. **把 cwd 契约写进 `bash` 的工具描述 + 一条 guideline**（治 R1）：明确「每次调用都在
   `<workspace>` 起、新 shell、`cd` 不跨调用；优先绝对路径或 `cd x && cmd` 单次成句」。
2. **让一次调用覆盖多目标**（治 R2）：`ls` 加 `depth`（树形）/ 多 `path`；`grep` / `find` / `read`
   是否接受多个 path 待定。
3. **让只读批次真正并行**（治 R3）：给 `read` / `grep` / `find` / `ls` 声明
   `is_concurrency_safe() = true`（纯只读、无共享可变状态；混合批次含 `bash`/`write` 自动退回串行），
   并在 guideline 里点明「多目标 ⇒ 一条消息发多个调用」。
4. **（可选）启动期项目布局快照**：把有界的目录/文件清单投进 system prompt（如 `<project_layout>`），
   省掉开局探索轮 —— 代价是每轮固定 token（缓存命中后仍计入 `fixed_prompt_tokens`）。

### 范围外（明确不做）

- 加沙箱 / 权限 / 审批（项目硬约束：安全外置）。
- 引入 `tree` 二进制或新 crate（不新增依赖）。
- **把 `tree` 命令当作改进基座**（它今天已经能走 `bash` 调用，不需要任何代码）。三类代价使它不适合作
  基座：① **机器相关**（本机就没装；缺失时是静默的 `command not found`，不像 `rg`/`fd` 有启动期
  降级提示，ADR-0015）；② **无上限 + 截断方向相反**（`tree` 全量输出经 `bash` 的
  `ys_tool::tail` 保尾丢头 ⇒ 丢的正是顶层骨架）；③ 不读 `.gitignore`、输出是盒线而非可直接回喂的
  路径。采样里模型**从未**用过 `tree`（0 次），探索仍由 `ls`（工具 8 次 + bash 9 次）承担。
  **不禁用**它 —— 只是不把改进押在它身上。
- **持久 shell / 跨调用 cwd**（破坏无状态与并行安全）。
- 放宽任何输出上限（2000 行 / 50KB / limit 条）。
- 改模型侧：不做「每轮注入」（缓存经济学，`docs/arch/prompt-delivery/notes.md` §2）。
- 新增工具（如 `tree` 工具）—— 先看能否用 `ls` 的参数面解决；新增工具要过 ADR 与 `EXPECTED_ORDER`。

### 关键场景（成功路径）

- **S1 · 开局定位**：用户要求「改一下 X 模块」。模型第一轮就能拿到足够信息（布局快照或一次
  `ls depth=2`），而不是 `ls .` → `ls docs …` → 再 grep 三轮。
- **S2 · 多目标搜索**：`grep` 一次覆盖多个目录/文件，或一条消息里并发若干调用，**不再**用
  `cd … && grep -rn … | head` 拼串。
- **S3 · 多区间阅读**：读若干文件区间时，用 `read offset/limit` 的多个并发调用，而不是
  `sed -n a,b f1; sed -n c,d f2; echo ===`。
- **S4 · bash 里跑真命令**：`cargo test` / `git log` 之类**本来就该走 bash**的调用，不再被
  `cd <abs> &&` 前缀污染（前缀对模型可读性、对 token 都是纯损耗）。

### 异常 / 边界场景

- 模型**仍然**用 bash 兜底：改动只降低概率，不保证 —— 所以验收要看**调用形态的比例**，不能
  假设某条路径必然发生。
- 并发只读调用与「同一批里既有读又有写」：必须仍是串行（既有准入规则），不得为了提速放宽。
- 布局快照与真实工作区**不一致**（agent 自己创建/删除了文件）⇒ 快照必须自带「启动快照、
  可能过期，用 `ls`/`find` 取当前状态」的时效声明。
- 仓库很大（如 monorepo）：快照与 `ls depth` 都必须有硬上限，不能因新参数面而放大输出。

## 未澄清问题

- [ ] **Q1 · 本轮范围**：只做「零/低成本」的 1 + 3（描述/guideline + 并行声明），还是把 2
      （多目标参数面）一起纳入？（1、3 是小改；2 改的是对模型可见的工具契约。） 待用户确认
- [ ] **Q2 · 多目标形态**：`ls` 用 `depth`（树形）还是 `paths: [..]`，还是两者都要？`grep` / `find` /
      `read` 是否也接受多 path（Pi 对齐口径是单 `path`，`docs/arch/tools-align-pi/context.md`）？ 待用户确认
- [ ] **Q3 · 布局快照**：要不要做？形态（顶层 + 深度 2 目录树 / `rg --files` 前 N 行 / 两者）与预算
      （例如 ≤1k est token 固定开销）？ 待用户确认
- [ ] **Q4 · 验收口径**：用「同任务的平均工具调用数 / bash 占比」验收？还是接受
      「人工对照若干 prompt 的调用轨迹」这种定性验收？ 待用户确认
- [ ] **Q5 · 措辞归属**：新增 guideline 是**产品内置**（对所有项目生效），还是走项目可配？
      （`docs/arch/prompt-delivery/notes.md` §5 的未决项 a/b 在此复现。） 待用户确认

## 后续建议

- 建议用 `arch-design` 做方案设计：把 1/2/3/4 拆成可独立落地的小切片，各自给出「改哪里 + 测试 + 变异点」。
- 建议先落地 1 + 3（无契约风险、可单测/变异测），把 2/4 作为第二批 —— 它们的收益需要**同一批
  prompt 的前后对照**才能证明（本仓尚无此基线，可借 `app_loop` / e2e 的既有 mock 脚手架）。
- 若要做 4（布局快照），建议先用 `prototype` 量一版真实 token 与「探索轮数下降多少」再定预算。
