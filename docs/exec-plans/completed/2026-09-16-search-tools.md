# 搜索工具（切片一）执行计划

> 每个任务按目录划分，同一目录下的所有变更由一个 agent 完成。按依赖顺序执行。
> 上下文引用：`docs/design-plans/completed/2026-09-16-search-tools.md`

## 任务清单

### Task 1: crates/ys-tool — truncate 模块 + registry 保序 [type=impl]
- **涉及目录**: `crates/ys-tool/src/`
- **涉及文件**: `crates/ys-tool/src/truncate.rs`(新), `crates/ys-tool/src/lib.rs`, `crates/ys-tool/src/registry.rs`
- **描述**: 新增纯函数截断模块（`Limit` / `Truncation` / `head` / `tail` / `line`）；`lib.rs` 加 `mod truncate; pub use truncate::*;`；`registry.rs` 的 `specs` 改为按输入顺序收集（修 B1）
- **关联测试任务**: T2

### Task 2: crates/ys-tool — truncate + registry 顺序测试 [type=test]
- **涉及目录**: `crates/ys-tool/src/`
- **涉及文件**: `crates/ys-tool/src/truncate.rs`(内联 tests), `crates/ys-tool/src/registry.rs`(追加测试)
- **描述**: truncate 边界（空串/单行/超行数/超字节/UTF-8 多字节切分边界）；registry 用 8 个命名工具断言 `specs()` 顺序 == 输入顺序
- **验证方法**:
  - `cargo test -p ys-tool` — 期望全部 passed
- **关联 impl**: T1

### Task 3: adapters/tools-basic — bash/read 改用 truncate + kill_on_drop [type=impl]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/bash.rs`, `adapters/tools-basic/src/read.rs`
- **描述**: 两工具改用 `ys_tool::truncate::{tail,head}`，**输出逐字节不变**（通知文案留调用方）；`bash` 的 spawn 补 `.kill_on_drop(true)`（修 B3）；把两个直调私有 `tail_truncate` 的测试改为经公共 API 断言
- **关联测试任务**: T4
- **依赖**: T1（需要 `truncate` 模块）

### Task 4: adapters/tools-basic — 逐字节不变 + 不泄漏 [type=test]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/bash.rs`, `adapters/tools-basic/src/read.rs`
- **描述**: 保留/补充文案断言（`[Truncated `、`Use offset=`）；新增 N3：`sh -c 'sleep 2; touch <sentinel>'` + `timeout=1`，返回后等 3s 断言 sentinel 不存在
- **验证方法**:
  - `cargo test -p ys-tools-basic` — 期望全部 passed
- **关联 impl**: T3

### Task 5: adapters/tools-basic — exec.rs [type=impl]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/exec.rs`(新), `adapters/tools-basic/src/lib.rs`, `adapters/tools-basic/Cargo.toml`
- **描述**: `SearchBackends`（`detect_from(path)` 纯 PATH 扫描 + 可执行位，`posix_only()`，`degraded_hint()`）+ `run_bounded(prog,args,cwd,byte_cap,deadline,boundary)`（spawn + `kill_on_drop` + 并发排空双管道 + 达上限早杀 + 100ms abort 轮询 + deadline），返回 `{stdout,stderr,code,stop}`。若生产代码需显式 `BoundarySource`，提升 `ys-protocol` 到 `[dependencies]`
- **关联测试任务**: T6
- **依赖**: T1

### Task 6: adapters/tools-basic — exec 四态 + 不死锁 [type=test]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/exec.rs`(内联 tests)
- **描述**: N4（`Eof`/`Cap`/`Aborted`/`TimedOut`；stderr 狂写不挂起，套 5s timeout）+ N10（`detect_from` 空目录/含假 rg 目录）
- **验证方法**:
  - `cargo test -p ys-tools-basic exec` — 期望全部 passed
- **关联 impl**: T5

### Task 7: adapters/tools-basic — grep/find/ls 三工具 [type=impl]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/{grep,find,ls}.rs`(新), `adapters/tools-basic/src/lib.rs`
- **描述**: 三工具实现（参数面照搬 Pi；`grep`/`find` 走 `SearchBackends` rg/fd → POSIX 降级；退出码语义按设计 §1.4；`ls` 纯 `read_dir` + 20000 扫描上限）；`lib.rs` 导出 4 → 7
- **关联测试任务**: T8
- **依赖**: T5

### Task 8: adapters/tools-basic — 三工具语义 [type=test]
- **涉及目录**: `adapters/tools-basic/src/`
- **涉及文件**: `adapters/tools-basic/src/{grep,find,ls}.rs`(内联 tests)
- **描述**: N5/N6/N7，全部用 `SearchBackends::posix_only()` 注入（不依赖开发机 PATH）
- **验证方法**:
  - `cargo test -p ys-tools-basic` — 期望全部 passed
- **关联 impl**: T7

### Task 9: ys-runtime + app — builder.tools 与 toolset 装配模块 [type=impl]
- **涉及目录**: `crates/ys-runtime/src/`, `apps/coding-agent/src/`
- **涉及文件**: `crates/ys-runtime/src/builder.rs`, `apps/coding-agent/src/toolset.rs`(新), `apps/coding-agent/src/main.rs`(加 `mod toolset;`)
- **描述**: `AgentBuilder::tools(Vec<Box<dyn Tool>>)`（`.tool()` 保留）；`toolset.rs` 提供探测一次 + 建工具表 + `degraded_hint()`（**不 print**，提示由 main 打）
- **关联测试任务**: T10
- **依赖**: T7

### Task 10: apps/coding-agent — toolset 探测/hint 单测 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/toolset.rs`(内联 tests)
- **描述**: 装配出的工具表含 7 个工具且顺序确定；`degraded_hint` 在无 rg/fd 时 `Some`
- **验证方法**:
  - `cargo test -p ys-coding-agent toolset` — 期望全部 passed
- **关联 impl**: T9

### Task 11: apps/coding-agent/src — prompt 派生 + main 接线 [type=impl]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/prompt.rs`, `apps/coding-agent/src/main.rs`
- **描述**: `build_system_prompt(cwd: &Path, tools: &[ToolSpec])`，清单从 spec 派生、snippet/guideline 仍按 name 查表；`main` 重排为「build tools → specs → prompt → agent」；`build_agent` 签名同步；3 个入口 + 2 个测试调用点同步
- **关联测试任务**: T12
- **依赖**: T9

### Task 12: apps/coding-agent — 提示词派生 + 漂移测试 [type=test]
- **涉及目录**: `apps/coding-agent/src/`
- **涉及文件**: `apps/coding-agent/src/prompt.rs`
- **描述**: N8（清单 == specs name 序列；漂移测试：真实 toolset 每个工具都有非兜底 snippet）；重写两个受签名影响的既有测试
- **验证方法**:
  - `cargo test -p ys-coding-agent prompt` — 期望全部 passed
- **关联 impl**: T11

### Task 13: apps/coding-agent/tests — 端到端与 CLI 降级 [type=test]
- **涉及目录**: `apps/coding-agent/tests/`
- **涉及文件**: `apps/coding-agent/tests/integration.rs`, `apps/coding-agent/tests/degraded_search.rs`(新)
- **描述**: N12（MockModel 经 `BasicLoop` 走完 grep 工具调用；顺带把该文件两个固定临时目录名改为 `pid+seq`）；N11（`CARGO_BIN_EXE` + 受限 PATH，断言 stderr 含降级提示）
- **验证方法**:
  - `cargo test -p ys-coding-agent --test integration` — 期望全部 passed
  - `cargo test -p ys-coding-agent --test degraded_search` — 期望全部 passed
- **关联 impl**: T7, T11

## 验证清单
- [ ] **所有 (impl, test) 配对均已 completed**（Phase 3 review 放行条件）
- [ ] `cargo test --workspace` — 全绿（≥435）
- [ ] `cargo clippy --all-targets` — 不新增 warning
- [ ] `cargo fmt --check` — 通过
