# 工具结果可恢复性 — As-built

> 性质：**实际建成记录**。起因是与 pi 的工具实现逐项对照后发现的三个缺陷。
> 日期：2026-11-06 ｜ 基线 `1793247163`。

## 一句话

三处「模型拿不到它需要的信息」的缺陷：`read` 的越界 offset 静默返回空、`grep` 因为一个
不成立的前提放弃了全部隐藏路径、`bash` 截断后丢掉的那部分**永久不可达**。三处都改掉了，
另把 `read` 行号前缀的 token 成本写进注释（行为不变）。

## 落点

```
adapters/tools-basic/src/read.rs       offset 越界 → 错误结果；render_one 同步
adapters/tools-basic/src/grep.rs       rg_argv 加 --hidden + --glob=!.git/
adapters/tools-basic/src/bash.rs       render_result 增加 full_output 参数；新增 combine_output / spill_full_output
crates/ys-tool/src/truncate.rs         READ_RESULT_BUDGET_BYTES 补行号前缀开销的说明
docs/arch/tools-align-pi/design.md     §1.4 的 --hidden 取舍改判
docs/design-final/search-tools.md      「降级语义」段同步
```

## ① `read` 的越界 `offset` 必须报错

**原行为**：`offset` 超过文件行数时 `min(total_lines)` 钳到末尾 ⇒ `selected` 为空 ⇒
返回**空字符串、`is_error: false`**。

**为什么是缺陷**：模型以为「文件就这么多」，实际是它猜错了行号。空结果**不含任何
纠正信号**，模型只会得出错误结论并继续推理。这是本仓明确反对的「静默的谎」。

**现行为**：`Offset N is beyond end of file (M lines total)`，`is_error: true`。
给出真实行数是**为了可纠正** —— 只说「越界」等于让模型重新猜一次。

**边界**：`offset == total_lines` 合法（读最后一行）；`offset == 0` 按未给处理（与 pi 一致）。

**多文件路径**：`render_one` 返回错误行而不是空串 —— 单目标越界**不拖垮整批**
（与 `read_many` 既有纪律一致），但错误逐文件可见。

## ② `grep` 的 `--hidden`：原取舍建立在半个错误前提上

**原前提**（`docs/arch/tools-align-pi/design.md` §1.4）：「加 `--hidden` 会把 `.git/`
对象也纳入扫描 ⇒ token 灾难 ⇒ 不加」。代价是 `.github/workflows/*.yml`、`.claude/`、
`.cargo/config.toml` 这类隐藏路径**永远搜不到**，而它们恰是配置排查的高频目标。

**前提只对了一半**：搜进 `.git/` 是**可排除的**。ripgrep 对 `.git/` 没有硬编码豁免
（15.1.0 实测：`--hidden` 下 `rg -l 'ref:'` 命中 `.git/HEAD`），所以排除必须显式写：

```
rg --hidden --glob=!.git/ …
```

**为什么用等号形式**：`--glob=!.git/` 是单个位置参数，不跟用户传入的 `--glob <值>`
争参数位（空格形式下两者混在一起，顺序语义脆弱）。

**验证过的四种组合**（均见 `grep.rs` 的 `rg_searches_hidden_paths_but_not_dot_git`）：
挡住 `.git/`、能搜 `.github/`、用户给 `glob` 时排除仍在、普通文件不受影响。

**降级路径不变**：POSIX `grep -r` 仍用既有 `--exclude-dir` 列表。

## ③ `bash` 截断后给出完整输出的落盘路径

**原行为**：`[Truncated N lines.]` + 尾部 2000 行 / 50 KiB。**被丢掉的头部不可达** ——
模型只能重跑命令重新碰运气，而构建日志的**第一个报错**恰在头部。

**现行为**：截断时把**未截断**的完整输出落盘，通知变成
`[Truncated N lines. Full output: <path>]`。模型用已有的 `read` 工具按 offset 取任意片段。

**只在截断时落盘**：未截断时模型已拿到全部内容，写盘是纯浪费。

**落点**：`<bg_root>/bash-full/<pid>-<seq>.log`，其中 `bg_root` 与后台命令输出
共用（`YUSHAN_BG_DIR` 或 `<temp_dir>/yushan-bg`）。理由相同：**不落进用户仓库**。
序号是进程内原子计数器 + `process::id()`，并行调用不撞名。

**目录层次不得深于 `root/<dir>/<file>`**：启动期的 `sweep_bg_root` 只扫两层，多一层
子目录会让这些文件**永远不被清理**。故 `bash-full` 充当「会话目录」的角色，pid 写进
文件名而不是目录名（`spill_path_is_two_levels_below_bg_root` 钉住这一点）。

**落盘失败不影响主结果**：只读文件系统、无临时目录权限等情况下 `spill_full_output`
返回 `None`，通知退化成原来的 `[Truncated N lines.]`。少了全文指针**不该把一次成功的
命令变成错误结果**。

**`combine_output` 抽出来的理由**：渲染与落盘必须是**同一份字节**，否则落盘文件的头部
与模型看到的尾部对不上。

## ④ `read` 行号前缀的成本（仅记录，行为不变）

`read` 每行渲染成 `{:>6}\t{内容}`，前缀恒为 **7 字节**。折算到 16 KiB 预算：
平均行 20 B ⇒ 每行实占 27 B ⇒ 约 600 行；平均行 40 B ⇒ 约 350 行。即前缀吃掉
**15–35%** 的有效预算。

这是**有意换来的**：行号让模型能直接拿行号去 `edit` / 报错定位，不必自己数。
但代价此前没有记录，已补进 `READ_RESULT_BUDGET_BYTES` 的文档注释。

## 变异测试（全部实跑验证会变红）

| 注入的 bug | 抓住它的用例 |
|---|---|
| `read` 越界退回 `.min(total_lines)` | `test_read_offset_beyond_eof_is_an_error` |
| 删掉 `--hidden` | `rg_argv_pairs_hidden_with_git_exclusion` + `rg_searches_hidden_paths_but_not_dot_git` |
| 删掉 `--glob=!.git/` | 同上两条 |
| `spill_full_output` 恒返回 `None` | `test_bash_truncation_spills_full_output` |
| 未截断时也落盘 | `test_bash_no_spill_when_not_truncated` |
| 落盘目录变成三层（sweep 扫不到） | `spill_path_is_two_levels_below_bg_root` |

## 已知缺口

- **`read` 的 `paths` 与 `path` 的必填关系**：schema 里 `required: ["path"]`，但 `paths`
  给了 ≥2 个时 `path` 实际被忽略 —— 模型仍须填一个占位的 `path`。这是既有形态，本次未动。
