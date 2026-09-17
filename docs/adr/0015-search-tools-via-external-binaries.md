# 0015 — 搜索工具外挂外部二进制（`rg`/`fd` → 降级 `grep`/`find`），不内建 `ignore` crate

给 coding agent 补 `grep` / `find` / `ls` 时，**走"外挂环境已有的 `rg`/`fd`，缺失时降级 POSIX
`grep`/`find`"**，**绝不下载、绝不内建**。曾认真考虑过用 `ignore` + `regex` + `globset` 做
**纯 Rust 内建搜索**（`ignore` 正是 ripgrep 的库本体，能完整复刻 `.gitignore` 语义），**否决**。
这是一个"标榜轻量、静态编译的 Rust 项目**主动 shell out**"的反直觉决定，故作记录。

## Status

accepted（2026-09-15）

## 背景

- 需求见 `docs/arch/tools-align-pi/context.md`：补 `grep`/`find`/`ls`，对齐 Pi。
- 业界现状（调研见 design.md）：Cursor / OpenCode / Continue / VS Code **默认 ripgrep 为搜索后端**；
  "有 rg 用 rg，否则 grep 兜底"是编辑器普遍做法；Pi 的做法是**从 GitHub 自动下载** rg/fd。
- `ignore` crate 能提供与 ripgrep **完全相同**的 `.gitignore` 语义（它就是 rg 的那个库）。
- 项目的硬约束：**轻量、静态编译、不引依赖**；ADR-0014 刚立下「**不设计假想未来需求**」。

## 决策

1. **优先环境已有二进制**：探测 `rg`、`fd`（含 Debian 别名 `fdfind`），**启动期一次**（纯 PATH 扫描，不 spawn）。
2. **缺失即降级** POSIX `grep` / `find`，降级路径带硬编码排除目录（`.git`/`target`/`node_modules`/`dist`/`build`/`.venv`）。
3. **绝不下载**（明确否决 Pi 的 `ensureTool`）：只提示用户装 ripgrep，不联网、不写用户环境。
4. **不引入** `ignore` / `regex` / `globset` / `walkdir`。

## Considered Options

- **纯 Rust 内建**（`ignore` + `regex` + `globset`）。**否决**：虽能拿到与 rg 对等的 `.gitignore`
  语义且无运行时二进制依赖，但引入 4 个依赖（含正则引擎），与本项目"不引依赖"的自我定位正面冲突；
  且要自造文件遍历、glob、输出格式——**重造 ripgrep 已在环境里跑着的那部分**。
- **照搬 Pi 的自动下载**。**否决**：把一个 agent runtime 变成包管理器；启动期静默联网 + 供应链面，
  与「不做安装」冲突（用户明确：*不下载，优先用环境自己的*）。
- **只支持 `rg`，缺失即报错**（不降级）。**否决**：用户明确要求降级到 `grep`/`find`；
  且没装 rg 的机器上会**没收模型的结构化搜索能力**。
- **自己做 `.gitignore` 解析**。**否决**：正是 `ignore` crate 为 rg 做掉的事，自造等于重新引入依赖的复杂度而无收益。

## Consequences

- **降级路径与原生路径行为不等价**（**最大代价**）：`grep`/`find` **不读 `.gitignore`**（只有 6 个硬编码目录）、
  `grep -E` 是 POSIX ERE 而非 Rust regex（`\d` 不可用）、含 `/` 的 glob 在 `grep --include` 下匹配不到。
  → 同一提问在装了 rg 与没装 rg 的机器上**产出不同质量的答案**。缓解：启动期 stderr 一行提示 +
  输出尾部 `[degraded: …]` 标注。**这是可接受但永久存在的裂缝。**
- **`tools-basic` 依赖 tokio 的 `process`**（已有），并在 `exec.rs` 里承担**子进程生命周期**
  （`kill_on_drop`、排空双管道防死锁、达上限早杀）。顺带修掉既有隐患：`bash.rs` 超时不杀进程 → 泄漏（B3）。
- **探测结果必须注入而非全局静态**：本项目「关键逻辑必须变异测试」→ 降级路径需在同进程内**确定性复现**，
  故用注入的 `SearchBackends` 值（`posix_only()` 供测试），不用 `OnceLock`。
- **不下载 ⇒ 用户体验由环境决定**：装了 ripgrep 的机器快且准（大仓库 <1s vs POSIX grep ~30s）；
  没装的机器拿到降级体验。**接受**——用户明确选择"用环境已有的，不自己装"。
- **与 ADR-0014 一致**：同样拒绝为"更完整的能力"引入依赖/复杂度；先做能站住的最小切片。
- 参考：`docs/arch/tools-align-pi/design.md`（融合方案）、`docs/extension-points.md`（`ls`/`grep` 属 Tool 桶）。
