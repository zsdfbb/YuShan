# 搜索工具（切片一）设计计划

> 上游设计：`docs/arch/tools-align-pi/design.md`（融合方案）+ `review.md`（R1–R12）+ `docs/adr/0015-search-tools-via-external-binaries.md`。
> 本计划只覆盖**切片一**（`grep`/`find`/`ls`）。切片二（`websearch`/`webfetch`）有两个未决项，不启动。

## 背景

- **当前状态**：coding agent 只有 `read`/`write`/`edit`/`bash` 四个工具。模型搜索只能靠 `bash` 拼 `grep -r`/`find`，会搜进 `target/`、`node_modules/`，且输出非结构化、无 `.gitignore` 感知。
- **为什么做**：给模型三个结构化搜索工具；同时修掉评审独立发现的三个既有问题。
- **既有问题（B1/B2/B3）**：
  - **B1** `ToolRegistry::specs()` 顺序来自 `HashMap::values()`（`crates/ys-tool/src/registry.rs:32`）→ 顺序逐进程抖动。提示词一旦由 registry 驱动，**prompt cache 前缀全废**。
  - **B2** `-p`/`--json` 下 `boundary` 恒为 `None`（`apps/coding-agent/src/wiring.rs`，测试 `ports_carry_no_boundary_source`）→ 一次性模式**没有任何中止通道** → 搜索工具必须自带兜底 timeout。
  - **B3** `bash.rs` 超时只 drop future，`tokio::process::Child` 默认不杀进程（全仓无 `kill_on_drop`）→ 300s 超时后**泄漏子进程**。

## 设计

### 模块划分

| 模块 | 职责 |
|---|---|
| `crates/ys-tool/src/truncate.rs`（新） | 截断的唯一实现：`Limit` / `Truncation` / `head` / `tail` / `line`。纯函数、零依赖、无 IO。**取代** bash/read 各写一遍的实现 |
| `crates/ys-tool/src/registry.rs`（改） | **B1**：`specs` 按**输入顺序**收集 |
| `adapters/tools-basic/src/exec.rs`（新） | `SearchBackends::detect_from(path)` 探测 + `run_bounded` 有界子进程原语 |
| `adapters/tools-basic/src/{grep,find,ls}.rs`（新） | 三个工具。`ls` **不起子进程** |
| `apps/coding-agent/src/toolset.rs`（新） | 唯一装配接缝：探测一次 + 建工具 + 降级提示 |
| `crates/ys-runtime/src/builder.rs`（改） | 新增 `pub fn tools(Vec<Box<dyn Tool>>)` |
| `apps/coding-agent/src/prompt.rs`（改） | `build_system_prompt(cwd, &[ToolSpec])`；清单从 spec 派生 |

### 关键设计约束（实现时必守）

1. **三个上限**：`limit`=100 条（模型可见）/ `byte_cap`=1MiB（内部常量）/ `truncate::head`=2000 行·50KB（输出）。各自 notice 都要报出；`-C N` 会让字节闸先于条数闸触发，两条都要报。
2. **退出码：先看 `stop` 再看 `code`**。`stop != Eof`（我们杀的：Cap/Aborted/TimedOut）→ **忽略 code**，用已有结果渲染；`1` = 无命中（**不是错误**，`is_error=false`）；`Eof + code 0` 但零 match 事件 → **格式漂移 → 报错**；`Eof + code>=2` → `is_error=true` + stderr 前 4KiB。
3. **降级路径**带硬编码排除目录 `.git`/`target`/`node_modules`/`dist`/`build`/`.venv`；**只作用于降级**（rg/fd 自带 `.gitignore` 语义）。
4. **探测注入**：`SearchBackends` 是**值**，不用 `OnceLock`；签名 `detect_from(path: &str)`，`detect()` 是其读进程 env 的包装 → 降级路径可确定性测试、并行安全。
5. **搜索工具自带 60s 兜底 timeout**（因 B2）。
6. **`ls` 扫描上限 20000 条**（防超大目录无界读），再排序取 `limit`(500)；目录项带 `/`；含 dotfile；不起子进程。
7. **逐字节不变**：`bash` 的 `[Truncated {n} lines.]\n\n`（**前插**）与 `read` 的 `\n\n[Showing lines a-b of n. Use offset=N to continue.]`（**后追加**，行格式 `{line:>6}\t`，字节判断为 `>` 严格大于）必须原样保留 —— **通知文案留在调用方**，模块只返回 `Truncation`。
8. **不引入任何新依赖**（无 `regex`/`ignore`/`globset`/`walkdir`）；`tools-basic` 保持无网络。
9. **提示词来源**：`ToolSpec` **不加字段**；清单由 `&[ToolSpec]` 派生，一句话描述/guideline 仍在 `prompt.rs` 按 name 查表 + **漂移测试**。

## 涉及文件

- **新增**: `crates/ys-tool/src/truncate.rs`、`adapters/tools-basic/src/{exec,grep,find,ls}.rs`、`apps/coding-agent/src/toolset.rs`、`apps/coding-agent/tests/degraded_search.rs`
- **修改**: `crates/ys-tool/src/{lib,registry}.rs`、`crates/ys-runtime/src/builder.rs`、`adapters/tools-basic/src/{bash,read,lib}.rs`、`adapters/tools-basic/Cargo.toml`、`apps/coding-agent/src/{prompt,main}.rs`、`apps/coding-agent/tests/integration.rs`
- **不动**: `apps/coding-agent/src/view.rs`（工具列表来自 `agent.tool_names()`，registry 派生）、`crates/ys-tui-coding/` 全部

## 测试策略

### 测试合同（与用户签字确认，Phase 3 审查依据）

> 每条需求一条验收条款。Phase 3 review subagent 按本节逐条跑命令 / 核对输出。

#### N1: truncate 下沉 + bash/read 逐字节不变
- **验收方法**: `cargo test -p ys-tool truncate && cargo test -p ys-tools-basic` — 期望全 passed；且 `bash.rs`/`read.rs` 测试里的文案断言（`[Truncated `、`Use offset=`）**未被改动**
- **优先级**: 必测
- **变异**: `tail` 里 `lines[i].len() + 1` 改成 `len()` → bash 尾截断测试变红

#### N2: B1 registry 保序
- **验收方法**: `cargo test -p ys-tool` — 新增测试用 **8 个**命名工具 build，断言 `specs()` 顺序 == 输入顺序
- **优先级**: 必测
- **变异**: `build()` 改回 `map.values()` → 变红（概率 ≈ 1/40320）

#### N3: B3 bash 超时不泄漏
- **验收方法**: `cargo test -p ys-tools-basic` — `sh -c 'sleep 2; touch <sentinel>'` + `timeout=1`，调用返回后再等 3s，断言 sentinel **不存在**（约 4s）
- **优先级**: 必测
- **变异**: 去掉 `kill_on_drop(true)` → 变红

#### N4: exec 四态 + 不死锁
- **验收方法**: `cargo test -p ys-tools-basic exec` — 覆盖 `Eof`/`Cap`/`Aborted`/`TimedOut`；**stderr 狂写不挂起**（套 5s `timeout`，仿 `crates/coding-agent/src/channel.rs` 的超时套路）
- **优先级**: 必测
- **变异**: 删 stderr 排空任务 → 该测试挂起转超时失败

#### N5: grep 语义
- **验收方法**: `cargo test -p ys-tools-basic grep` — 注入 `SearchBackends::posix_only()`：有命中 → `path:line: text`；**无命中 → `is_error == false`**；退出码 2 → `is_error == true`；limit 截断 + notice；排除目录生效
- **优先级**: 必测
- **变异**: 把退出码 1 当错误 → 变红

#### N6: find 语义
- **验收方法**: `cargo test -p ys-tools-basic find` — `posix_only` 注入：命中列表、limit、无命中非错误
- **优先级**: 必测
- **变异**: 无命中返回 `is_error=true` → 变红

#### N7: ls
- **验收方法**: `cargo test -p ys-tools-basic ls` — 排序（大小写不敏感）、目录带 `/`、含 dotfile、超 20000 上限截断
- **优先级**: 必测

#### N8: 提示词派生 + 漂移
- **验收方法**: `cargo test -p ys-coding-agent prompt` — `build_system_prompt(cwd,&specs)` 的清单 == specs 的 name 序列；**漂移测试**：真实 toolset 每个工具都有非兜底 snippet
- **优先级**: 必测
- **变异**: 删清单里的 `grep` → 漂移测试红

#### N9: 装配 + 三模式
- **验收方法**: `cargo test -p ys-coding-agent` 全绿（三模式共用 `build_agent`，编译 + 现有测试即覆盖）
- **优先级**: 必测

#### N10: 探测可注入
- **验收方法**: `cargo test -p ys-tools-basic exec` — `detect_from("<空目录>")` 两后端皆 `None` 且 `degraded_hint().is_some()`；`detect_from("<含假 rg 的目录>")` 命中。不读进程 env
- **优先级**: 必测

#### N11: CLI 降级路径（自动化）
- **验收方法**: `cargo test -p ys-coding-agent --test degraded_search` — `Command::new(env!("CARGO_BIN_EXE_ys-coding-agent"))` + `.env("PATH", <空临时目录>)` + `.env_remove(...)` + `.env("HOME", tmp)` → 断言 **stderr 含降级提示**。仿 `stream_usage_fallback.rs` 的 per-child `.env()` 套路，**不改父进程 env**
- **优先级**: 必测

#### N12: 工具经循环的端到端（自动化）
- **验收方法**: `cargo test -p ys-coding-agent --test integration` — MockModel 发 `grep` tool call → `BasicLoop.run_turn` → 循环完成且工具结果非 error（仿既有 `MockCodingModel`）；顺带把该文件两个固定临时目录名改为 `pid+seq` 约定
- **优先级**: 必测

#### G: 全局
- **验收方法**: `cargo test --workspace`（≥435，全绿）；`cargo clippy --all-targets`（**不新增** warning；实测基线为 0）；`cargo fmt --check`（通过）
- **优先级**: 必测

### 测试框架与命令

- 单元测试: `cargo test -p ys-tool` / `cargo test -p ys-tools-basic` / `cargo test -p ys-coding-agent`
- 集成测试: `cargo test -p ys-coding-agent --test integration` / `--test degraded_search`
- 全量: `cargo test --workspace`
- Lint: `cargo clippy --all-targets`
- 格式: `cargo fmt --check`

> 变异验证的自动化程度：`cargo-mutants` **未安装**，因此每条变异以**断言该属性的测试**落地（注入 bug 必然变红，机械可跑）；注入动作本身需一次人工步骤，本轮不引入外部安装。

## 波及文档

- `AGENTS.md`（`CLAUDE.md`）— 目录结构里 `tools-basic` 的工具清单（现为 `BashTool / ReadTool / WriteTool / EditTool`）、测试计数、常用命令
- `docs/design.md` §10 「默认工具」表 — grep/find/ls 从「第二阶段可增加」改为已实现
- `docs/design-final/` — 若需记录工具面，追加一节（否则标「无」）

## 风险与注意事项

1. **clippy 基线**：实测为 **0 warning**（`cargo clippy --all-targets` 无输出）。验收用「不新增」。
2. **`bash.rs` 两个测试直调私有 `tail_truncate`** → 随函数迁移改为经公共 `truncate::tail` 断言。
3. **N2 变异概率性**（≈1/40320），与「变异必须确定性命中」有半步差距，已接受。
4. **N3 慢测试**（~4s），为证「不泄漏」唯一可行法，已接受。
5. **`ys-protocol` 依赖**：若 `exec.rs` 生产代码需显式命名 `BoundarySource`，把 `ys-protocol` 从 `[dev-dependencies]` 提升到 `[dependencies]`（1 行）。
6. **工具数 4 → 7**：`ModelRequest.tools` 与 system prompt 每轮多约 30–60 token，净负外部性（设计已记录）。
7. **`integration.rs` 现有固定临时目录名**（`yushan_test`/`yushan_agent_test`）违反并行安全约定 → 在 T13 顺带修正。
