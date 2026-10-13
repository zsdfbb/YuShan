# 0038 — 恢复会话改用空格形态 `--resume <id>`（移除 `=` 形态）

把 coding-agent 的恢复会话入口从 `--resume=<id>` 改为 **`--resume <id>`**（空格形态）；`=` 形态
**移除**（命中即报可操作的错误，不再静默降级）。裸 `--resume`（列表选择）不变。

## Status

accepted（2026-10-13）

## 背景

- **需求**（原话）：「调整 coding-agent 的 cli 命令 resume 后面直接跟空白符 + session id，不要使用 `=`。」
- **原设计刻意用 `=`**：`docs/design-final/session-default-new.md` 明写「`--resume <id>`（空格）**不支持**
  （`=` 消除与任务文本的歧义）」—— 因为 `-p` / `--json` 的**任务文本是位置参数**，`--resume` 若按空格
  取值会与任务文本混淆。
- **现状**：`parse_args` / `split_leading_flags` / `parse_mode_tail` 三处都靠 `strip_prefix("--resume=")` 取 id；
  非 TTY picker 的提示、`resolve_resume` 的报错、文档与测试也都写 `--resume=<id>`。

## 决策

**1. 空格形态 `--resume <id>`；`=` 形态移除**

- `--resume` 取**紧随其后**的 token 为 id；命中 `--resume=` ⇒ 返回明确的迁移错误
  （「改为空格形态：`--resume <id>`」），**绝不当成陌生 flag 或任务文本**。
- 三处解析点（顶层 `parse_args`、`split_leading_flags`、`parse_mode_tail`）一致。

**2. `-p` / `--json` 的歧义用「任务文本必需」消解**

- 这两个模式下**任务文本是位置参数、必需** ⇒ `--resume` 只在**其后还留着任务文本**（至少还有一个
  token）时才把紧跟的 token 当 id：`--resume <id> <任务>` 取 id，`--resume <任务>`（其后无 token）
  是裸 flag + 任务文本。
- 下一 token 不存在或以 `-` 开头同样 ⇒ 裸 `--resume`（列表选择）。
- **顶层（TUI）无任务文本** ⇒ `--resume <id>` 直接取 id；`--resume` 为列表选择。

**3. 非 TTY picker 提示与 `resolve_resume` 报错同步改为空格形态**

（`sessions.rs::pick_with` 的「请用 `--resume <id>`」、`main.rs::resolve_resume` 的「`--resume` 需要
一个会话 id（例如 `--resume 2026-10-07-abc`）」）。

## 方案对比

| 方案 | 描述 | 优点 | 代价 |
|---|---|---|---|
| **A 空格形态 + 移除 `=`（采纳）** | `--resume <id>`；`=` 报错 | 符合用户直觉与其它 CLI 惯例；需求原话 | 破坏既有 `--resume=<id>` 调用；`-p` 下需「任务文本必需」规则消除歧义 |
| **B 只加空格，保留 `=`** | 两种形态都收 | 零破坏 | 与「不要使用 `=`」相悖；两套语法 |
| **C 保持 `=`** | 不动 | 零改动 | 正是被要求改掉的 |

**采纳 = A**。

## Consequences

- **正面**：恢复会话的 CLI 与主流惯例一致；`--resume=<id>` 给出可操作的迁移错误而非静默误解。
- **负面 / 取舍**：`--resume=<id>` 由「可用」变为「报错」（**破坏性变更**）；`-p` / `--json` 下
  `--resume <单 token>` 的语义是「裸 flag + 任务文本」（因为任务必需），与顶层 TUI 的
  `--resume <id>` 语义不同 —— 这是「任务文本必需」的直接推论，已在 `split_leading_flags` 文档写明。
- **行为变更（须写进 PR 描述）**：`--resume=<id>` 报错；`--resume <id>` 生效。

## 关联

- `docs/design-final/session-default-new.md` §CLI 契约 / §恢复的三条分支 — as-built 同步
- `docs/architecture.md` §`/new` 与恢复流程 — 同步
- `apps/coding-agent/src/{main,sessions}.rs` — 解析与提示
- [ADR-0026](./0026-session-default-new-and-resume.md) / [ADR-0033](./0033-runtime-resume-command.md) — 恢复会话的既有决策（形态由本 ADR 修订）
