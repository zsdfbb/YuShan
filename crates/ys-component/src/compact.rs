//! 压缩策略：**纯数据 + 纯函数**，无 IO、无 async、无外部依赖。
//!
//! 这里只放「压缩什么时候触发、保留多少」的**策略数据与阈值计算**；真正的压缩**算法**
//! （切消息、调模型做摘要、写回 session）在 `ys-loop`。分居的理由是结构性的：本 crate
//! 零依赖，装不下 async trait。
//!
//! 阈值来源：触发 85%（pi / Codewhale 用 80%，Claude Code 约 83.5%，取中）；保留 10%
//! （三家都用 20k 固定值，÷200k 窗口 = 10%）。

/// 输出侧的开销余量：模型输出之外的 framing / 协议开销（token 计）。
const CONTEXT_HEADROOM: usize = 1024;

/// context 压缩策略（纯数据）。
///
/// `#[non_exhaustive]`：后续加字段是加法式改动，不破坏既有构造。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub struct CompactPolicy {
    /// 触发压缩的窗口占用比例（达到 `window × trigger_ratio` 即压缩）
    pub trigger_ratio: f64,
    /// 目标保留比例（压缩后希望留下的窗口占比）
    pub keep_ratio: f64,
    /// 保留比例下限（窗口极小时防止保留量退化到 0）
    pub keep_min_ratio: f64,
    /// 保留比例上限
    pub keep_max_ratio: f64,
    /// 为模型输出预留的 token 数 —— 触发点据此确保窗口还容得下一轮输出（保守取 4K；
    /// 模型输出不再有人为上限，但压缩触发点在窗口过小时仍需给输出留余量）。
    pub output_reserve: usize,
    /// 生成摘要用的 system prompt（逐字交给摘要模型）
    pub summary_system_prompt: String,
}

impl CompactPolicy {
    /// 触发压缩的 token 占用阈值。
    ///
    /// 同时满足两件事：
    /// 1. **不超过** `window × trigger_ratio`；
    /// 2. 窗口还要**容得下模型输出**（`output_reserve` + framing 开销）。
    ///
    /// 第 2 条在小窗口下会退化：若直接写 `window − output_reserve − headroom`，
    /// 当 `window ≤ 5120` 时 `saturating_sub` 归零 —— 触发点掉到保留预算之下，
    /// 压缩随之空转。故预留量先被钳到「不得吞掉保留下限」，触发点再兜底一个
    /// `keep_budget + 1` 的下界（**P1 承重**：违反它会让 `keep_from` 恒为 0）。
    pub fn trigger_at(&self, window: usize) -> usize {
        let keep = self.keep_budget(window);
        let effective_reserve = (self.output_reserve + CONTEXT_HEADROOM)
            .min(window.saturating_sub(keep.saturating_add(1)));
        let trigger = (window as f64 * self.trigger_ratio) as usize;
        trigger
            .min(window.saturating_sub(effective_reserve))
            .max(keep.saturating_add(1))
    }

    /// 压缩后要保留的 token 预算：
    /// `clamp(window × keep_ratio, window × keep_min_ratio, window × keep_max_ratio)`。
    pub fn keep_budget(&self, window: usize) -> usize {
        let w = window as f64;
        let lo = w * self.keep_min_ratio;
        let hi = w * self.keep_max_ratio;
        // 用 max/min 而非 `f64::clamp`：后者在 min > max（或 NaN）时会 panic，
        // 而纯函数不该因为配置越界就崩。
        (w * self.keep_ratio).max(lo).min(hi) as usize
    }
}

impl Default for CompactPolicy {
    fn default() -> Self {
        Self {
            trigger_ratio: 0.85,
            keep_ratio: 0.10,
            keep_min_ratio: 0.01,
            keep_max_ratio: 0.10,
            output_reserve: 4096,
            summary_system_prompt: DEFAULT_SUMMARY_SYSTEM_PROMPT.to_string(),
        }
    }
}

/// 默认摘要 system prompt（逐字固定，勿改写）。
///
/// **来源是两家之合并**：**节骨架取 pi**（固定 Markdown 节），**内容规则取 Codewhale**
/// （逐字引用路径/命令/错误文本）。最初的落地版是**逐字 Codewhale 的 `COMPACT_PROMPT`**
/// —— bullet 式 `Include:` 列表，内容要求对，但**没有结构**：接手的模型拿到的是一坨
/// bullet，无从预期「哪段在哪」。改成固定节骨架后，接手模型有了**可预期的解析结构**，
/// 不必自己猜摘要的组织方式。
///
/// 单独说 `## Files` 这一节：它是**避免重复劳动与误覆盖文件的唯一直接入口**。接手模型
/// 最危险的动作是「凭印象重读/重写一个其实已经改过的文件」——只有把**已读/已改的精确
/// 路径**与「为什么重要」列出来，接手方才可能直接续写而不是从头再来。故这一节承重，
/// 勿在「美化」时合并或删除。
const DEFAULT_SUMMARY_SYSTEM_PROMPT: &str = "You are performing a context checkpoint compaction. Create a handoff summary for another LLM that will resume the task. Produce only the summary — do not continue the conversation.

Use exactly these sections:

## Goal
The user's objective and what is currently being worked on.

## Constraints & Preferences
Constraints, project conventions, and user preferences that must be respected.

## Progress
- Done: completed work
- In Progress: current work
- Blocked: anything blocked, and why

## Key Decisions
Decisions taken and the reason, so they are not re-litigated.

## Files
Exact paths that were read or modified, one line each on why they matter.

## Next Steps
What remains to be done, in order.

## Critical Context
Exact commands, error text, identifiers, and any other data needed to continue.

Rules:
- Be concise and specific. No preamble, no meta-commentary.
- Quote paths, commands, and error text verbatim — the resuming model must not need to re-derive them.
- Summarize the task, not the checkpoint machinery: never mention compaction, checkpoints, or context management.";

#[cfg(test)]
mod tests {
    use super::*;

    /// P1 的采样点（含小窗口陷阱区：4096 / 5120 / 8192）
    const P1_WINDOWS: [usize; 7] = [1, 100, 4096, 5120, 8192, 128_000, 1_000_000];

    #[test]
    fn test_default_values() {
        let p = CompactPolicy::default();
        assert_eq!(p.trigger_ratio, 0.85);
        assert_eq!(p.keep_ratio, 0.10);
        assert_eq!(p.keep_min_ratio, 0.01);
        assert_eq!(p.keep_max_ratio, 0.10);
        assert_eq!(p.output_reserve, 4096);
        assert!(p.summary_system_prompt.starts_with(
            "You are performing a context checkpoint compaction. Create a handoff summary"
        ));
        assert!(
            p.summary_system_prompt
                .ends_with("never mention compaction, checkpoints, or context management.")
        );
    }

    /// 摘要 prompt 的**承重语义**（防后人「美化」时删掉结构或规则）。
    ///
    /// 钉三件事：
    /// 1. 固定 Markdown 节骨架存在（pi 的血统 —— 接手模型要能预期解析结构）；
    /// 2. `verbatim` 引用要求还在（Codewhale 的血统 —— 精确性靠它）；
    /// 3. 排除 checkpoint 元叙述的禁令还在（摘要要说任务，不说压缩机器）。
    ///
    /// 尤其 `## Files`：它是避免「凭印象重读/重写已改文件」的唯一直接入口。
    #[test]
    fn test_summary_prompt_pins_structure_and_rules() {
        let p = CompactPolicy::default();
        let s = p.summary_system_prompt.as_str();

        // 节骨架：Goal / Files / Critical Context 等，逐节必须在场
        for section in [
            "## Goal",
            "## Constraints & Preferences",
            "## Progress",
            "## Key Decisions",
            "## Files",
            "## Next Steps",
            "## Critical Context",
        ] {
            assert!(s.contains(section), "摘要 prompt 缺少节标题：{section}");
        }

        // 规则：逐字引用 + 排除 checkpoint 元叙述
        assert!(s.contains("verbatim"), "摘要 prompt 缺少逐字引用要求");
        assert!(
            s.contains("never mention compaction, checkpoints, or context management"),
            "摘要 prompt 缺少排除 checkpoint 元叙述的禁令"
        );
    }

    /// P1 — 永续进展：触发点严格大于保留预算，**任意** `window ≥ 1`。
    ///
    /// 这是承重性质：一旦违反，`keep_from` 恒为 0 —— 压缩每轮空转、永不推进。
    #[test]
    fn test_p1_trigger_always_above_keep_budget() {
        let p = CompactPolicy::default();
        for window in P1_WINDOWS {
            let keep = p.keep_budget(window);
            let trigger = p.trigger_at(window);
            assert!(
                trigger > keep,
                "window={window}: trigger_at={trigger} 未超过 keep_budget={keep}"
            );
        }
    }

    /// P1 的密集区：小窗口陷阱区逐点扫描，别只赌采样点。
    #[test]
    fn test_p1_dense_small_windows() {
        let p = CompactPolicy::default();
        for window in 1..=20_000usize {
            let keep = p.keep_budget(window);
            let trigger = p.trigger_at(window);
            assert!(
                trigger > keep,
                "window={window}: trigger_at={trigger} 未超过 keep_budget={keep}"
            );
            assert!(trigger <= window, "window={window}: trigger_at 超出窗口");
        }
    }

    /// P2 — 触发点上界：`trigger_at(w) ≤ w × trigger_ratio`。
    ///
    /// 注：`window = 1` 时整数下取整使上界归零（`floor(0.85) = 0`），而 P1 要求
    /// 触发点 ≥ 1 —— 两条性质在 `w = 1` 上不可同时成立，此时以 P1 为准（见实现注释）。
    /// 从 `w = 2` 起上界成立。
    #[test]
    fn test_p2_trigger_bounded_by_ratio() {
        let p = CompactPolicy::default();
        for window in [2, 100, 4096, 5120, 8192, 128_000, 1_000_000] {
            let bound = (window as f64 * p.trigger_ratio) as usize;
            assert!(
                p.trigger_at(window) <= bound,
                "window={window}: trigger_at={} 超过上界 {bound}",
                p.trigger_at(window)
            );
        }
    }

    /// P3 — 大窗口行为（阈值语义的锚点）。
    #[test]
    fn test_p3_large_windows() {
        let p = CompactPolicy::default();
        // 1M 窗口：触发 850k、保留 100k
        assert_eq!(p.trigger_at(1_000_000), 850_000);
        assert_eq!(p.keep_budget(1_000_000), 100_000);
        // 128k 窗口：min(108_800, 128_000 − 5120) = 108_800（预留量不构成约束）
        let reserve = p.output_reserve + CONTEXT_HEADROOM;
        assert_eq!(p.trigger_at(128_000), 108_800);
        assert_eq!(p.trigger_at(128_000), 108_800usize.min(128_000 - reserve));
    }

    /// 输出预留确实生效：触发点给模型输出留出了 `output_reserve + headroom`。
    #[test]
    fn test_trigger_leaves_room_for_output() {
        let p = CompactPolicy::default();
        let reserve = p.output_reserve + CONTEXT_HEADROOM;
        // 128k 窗口下触发点 + 预留不超过窗口
        assert!(p.trigger_at(128_000) + reserve <= 128_000);
        // 小窗口下预留被钳到「不吞掉保留下限」：触发点仍严格大于保留预算
        let keep = p.keep_budget(4096);
        assert!(p.trigger_at(4096) > keep);
    }

    /// 保留预算的 clamp 语义：默认参数下 `keep_ratio == keep_max_ratio`，取 `window × keep_ratio`。
    #[test]
    fn test_keep_budget_clamp() {
        let p = CompactPolicy::default();
        assert_eq!(p.keep_budget(200_000), 20_000);
        assert_eq!(p.keep_budget(0), 0);

        // 上下界真的参与 clamp
        let narrow = CompactPolicy {
            keep_ratio: 0.10,
            keep_min_ratio: 0.05,
            keep_max_ratio: 0.30,
            ..Default::default()
        };
        // 1M × 0.10 = 100k，落在 [50k, 300k] 内 → 100k
        assert_eq!(narrow.keep_budget(1_000_000), 100_000);
        // 下界抬高：keep_ratio 0.10 → 0.05 的下界不生效（0.10 > 0.05）
        let low = CompactPolicy {
            keep_ratio: 0.001,
            keep_min_ratio: 0.01,
            keep_max_ratio: 0.10,
            ..Default::default()
        };
        assert_eq!(low.keep_budget(1_000_000), 10_000);
    }
}
