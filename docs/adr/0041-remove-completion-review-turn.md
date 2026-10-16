# 0041 — 撤掉「完工复核」：每次用户输入恰好跑一个回合

撤回 [ADR-0027](0027-completion-check-and-round-budget.md) **决策 2**（模型自称完工后主动追问一次
「是否真的完工」，另起一个回合）。撤回后：**一个 `Request::Prompt` 恰好跑一个回合**，回合结束即请求结束。
ADR-0027 的其余部分（`max_rounds` 可配、wall-clock 预算）**不受影响**。

## Status

accepted（2026-10-17）

## 背景

ADR-0027 决策 2 是为「长程任务撞预算后烂尾」加的兜底：模型自称完工后，系统追问一次，
把「其实没做完」的情况逼出来。当时的形态由 `pending.take()` 循环 + `should_review` 的三道门保证
（`Initial` / `Completed` / `rounds >= REVIEW_MIN_ROUNDS(5)`）。

此后三项实测改变了这个判断：

1. **同类没有这一步。** codex 与 Pi 都只靠「模型不再发工具调用」收场 —— 没有追问回合。
   codex 的提示里只有一句持续性指令（*"stay with the work until the task is handled end to end"*），
   Pi 的循环在 `toolCalls` 为空时结束（外加一个 `terminate` 让工具结果能提前终止整批）。
2. **它只在 TUI 生效，`-p` / `--json` 根本没有。** `main.rs` 的 `run_single_turn` 里没有
   `should_review`（其文档原话：「本路径只有一个回合」）。⇒ **基准度量的流程比真实使用少一环**，
   而我们所有性能结论都出自基准。
3. **它的代价不止一轮。** 文案是「没做完就**现在继续做完**」——所以它能再触发**若干**工具轮，
   而不只是多一次模型往返。而轮数正是墙钟与输入 token 的乘数
   （见 `docs/arch/explore-speed/cost-model.md` §1：每轮 ≈ 2–3 s 首 token 延迟，工具执行只占墙钟 ~1%）。

## 决策

移除完工复核，连同它的缝：

- 删 `apps/coding-agent/src/follow_up.rs`（`should_review` / `TurnKind` / `REVIEW_PROMPT` /
  `REVIEW_MIN_ROUNDS` 及其 4 个用例）。
- `app_loop` 的 `Request::Prompt` 臂由 `pending.take()` 循环收成**单回合**
  （`run_turn_once` 一次，随后写穿 meta）。
- 删 9 个只服务复核的集成用例与随之空转的测试夹具
  （`run_finished_turns` / `forwarded_user_texts` / `collect_until_first_run_finished` /
  `read_meta_turns` / `ProbePlan::TruncateAtCall`）。

**保留**：`run_turn_once` 开头的边界排空。它挡的是**上一个请求**残留的 `Abort`，与回合数无关。

## Consequences

- **每个用户输入恰好一个回合** —— 与同类语义一致，且 `-p` / TUI 两条路径**不再有行为差**。
- **代价（明说）**：模型自称完工即收场，「其实没做完」不再被系统追问。
  兜底责任回到用户（再发一句）与模型自身（提示词里已有持续性纪律）。
  ⇒ 若日后实测「自称完工但并未完工」的比例不可忽略，应重新引入**某个**机制 ——
  但届时应先解决本案暴露的真问题：**它不该只在一条入口上生效**。
- 复核回合原先会写穿 meta（`turns` 计数 +1、标题取自首回合）——现在 `turns` 恒为 1。
- 删掉 13 个用例（4 单元 + 9 集成）；全量 1192 → 由此变化的基线见提交信息。

## 参考

- [ADR-0027](0027-completion-check-and-round-budget.md) —— 被撤回决策 2 的出处（其余决策仍有效）
- `docs/arch/explore-speed/cost-model.md` §1 —— 轮数是墙钟乘数的实测依据
