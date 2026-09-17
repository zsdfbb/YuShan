# 搜索工具（切片一）— 修改报告

> 日期：2026-09-17 ｜ 设计：`docs/design-plans/2026-09-16-search-tools.md` ｜ 执行：`docs/exec-plans/2026-09-16-search-tools.md`
> 决策：`docs/adr/0015-search-tools-via-external-binaries.md` ｜ 归档时两份计划移入各自 `completed/`

## 结果

| 指标 | 基线 | 完成后 |
|---|---|---|
| `cargo test --workspace` | 435 passed | **513 passed / 0 failed**（+78；1 ignored = 需真实凭证的 e2e） |
| `cargo clippy --all-targets` | 0 warning | **0 warning** |
| `cargo fmt --all -- --check` | 通过 | 通过 |
| 工具数 | 4（read/write/edit/bash） | **7**（+ grep/find/ls） |

## 交付内容

**新增**
- `crates/ys-tool/src/truncate.rs` — 截断唯一实现（`Limit`/`Truncation`/`head`/`head_lines`/`tail`/`line`）
- `adapters/tools-basic/src/exec.rs` — `SearchBackends`（纯 PATH 探测）+ `run_bounded`（有界子进程原语）
- `adapters/tools-basic/src/{grep,find,ls}.rs` — 三个工具
- `apps/coding-agent/src/toolset.rs` — 唯一装配接缝
- `apps/coding-agent/tests/degraded_search.rs` — CLI 降级路径测试

**修改**
- `crates/ys-tool/src/{lib,registry}.rs`（挂载 truncate；**B1** specs 保序）
- `crates/ys-runtime/src/builder.rs`（`AgentBuilder::tools(Vec)`）
- `adapters/tools-basic/src/{bash,read,lib}.rs`、`Cargo.toml`（改用 truncate；**B3** `kill_on_drop`；`ys-protocol` 提升为 dependencies）
- `apps/coding-agent/src/{prompt,main}.rs`（清单从 `ToolSpec` 派生；装配重排；降级提示 → stderr）
- `apps/coding-agent/tests/integration.rs`（grep 经循环端到端；临时目录名合规）

## 三个既有问题的修复（评审独立发现，均在本切片顺手解决）

- **B1 · registry 顺序抖动**：`ToolRegistry::build` 的 `specs` 原从 `HashMap::values()` 收集 → 逐进程顺序不定。提示词一旦由 registry 驱动，**prompt cache 前缀会全废**。改为按输入顺序收集。
- **B2 · 一次性模式无中止通道**：`-p`/`--json` 下 `boundary` 恒为 `None`，搜索类工具若挂死无人收场。→ 搜索工具自带 **60s 兜底 deadline**。
- **B3 · 子进程泄漏**：`bash` 超时只 drop future、不杀进程。→ 补 `kill_on_drop(true)`。

## 评审中发现并修掉的真缺陷

| 缺陷 | 发现于 | 处置 |
|---|---|---|
| `read` 改成 `head_lines` 后为**整个文件**物化 `Vec<String>` → 200 万行实测 **161ms → 31ms**（8× 回退） | T3 review（P2-1） | 先按 `MAX_LINES` 裁切片再渲染 |
| 原生 `rg` 下**含 `/` 的 glob 静默返回空**（`src/*.rs` → "No matches found"），而降级路径明确报错 | T7 review（I-1） | 归一为 `**/` 前缀（实测 0 命中 → **125 命中**）；另把「以 `/` 开头」的 glob 改为明确报错 |
| 上限判定若依赖 `stop == Cap`，会在「越限后同窗口自然退出」时**静默漏报** | T5 review（F1） | 统一改用 `stdout.len() >= BYTE_CAP` 判定 |
| 搜索工具若经 `sh -c` spawn，**孙进程**会让 deadline 失去硬上界语义 | T5 review（F2） | 一律直接 spawn 二进制 |
| 提示词清单若用 `hit.is_some()` 判断通知，`offset/limit` 取子区间时会**漏提示** | T1 review（P1） | 沿用文件级 `start + shown_lines < total_lines` |
| `prompt.rs` 注释错引 ADR-0014（那是 follow-up 决策，与保序无关） | T12 review | 改指设计计划 §B1 |

## 验证方式（全自动）

- 每条验收条款都由 `cargo test` 覆盖（N1–N12），命令见设计计划「测试策略」。
- **变异测试手工执行**：13 个任务共注入 **40+ 条变异**（每任务 review 独立注入），全部按预测变红并**逐字节还原**（sha256 校验）。`cargo-mutants` 未安装，故注入动作本身未能自动化。
- 关键路径的独立差分：`truncate` 与原实现 **31,372 例**、`bash/read` **646 例**输入对拍，0 mismatch。
- CLI 降级路径：`CARGO_BIN_EXE` + 受限 PATH，断言 stderr 有提示且 **stdout 为空**。

## 已知后续项（本切片范围外，未做）

1. **`find.rs` 的同族残留**：以 `/` 开头的 glob（如 `/src/*.rs`）经 `fd --full-path` 仍会静默返回空 —— 与 grep 已修的第二处同源，罕见路径。
2. **B3 只修了一半**：`kill_on_drop` 只杀直接子进程；`bash` 无 boundary 的超时路径对 `sh -c "…"` 的**孙进程**仍不覆盖（需复用 `kill_process_group`）。
3. **文档漂移**：`docs/arch/tools-align-pi/design.md` §1.2 的 argv 示例画了 rg 侧排除目录，而 §1.4 与实现是「排除只作用于降级路径」。
4. **切片二未启动**：`websearch`/`webfetch` 有两个未决项（默认搜索 provider、HTML→Markdown crate 需 prototype 验证）。
5. **变异测试未自动化**：`cargo-mutants` 未安装（`.gitignore` 仅预留痕迹）。

## 波及文档

- `AGENTS.md`（`CLAUDE.md`）：工具清单、测试计数、常用命令 —— **待 Phase 4 回写**
- `docs/design.md` §10「默认工具」表：grep/find/ls 从「第二阶段可增加」改为已实现 —— **待 Phase 4 回写**
