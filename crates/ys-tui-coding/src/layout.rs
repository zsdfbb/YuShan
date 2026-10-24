//! transcript 的**视窗化布局** —— 逐条目记住折行结果，只渲染视窗覆盖的那些行。
//!
//! # 为什么
//!
//! 旧实现每帧把整段 transcript 拼成一份 `Text`（`build_text`），再交给 `Paragraph`
//! 全量折行 + `line_count` 计数。**滚动一次也要重做一遍** —— 于是「上下翻页」的
//! 成本是 *O(整段 transcript 的字节数)*：实测 2 MB 的会话单次滚动数学就要 ~23 ms，
//! 一次按键走 3～4 遍 ⇒ 明显卡顿。
//!
//! 本模块把折行结果**按条目记住**：每个 transcript 条目的每个渲染行折成几个视觉行
//! （`row_starts`）只算一次，之后只随**宽度**或**该条目内容**变化而重算。于是：
//!
//! - **滚动数学**（总行数、视窗定位）= 查表，O(1)；
//! - **渲染** = 只取视窗覆盖的那些行折行 = O(视窗高度)。
//!
//! # 承重不变量
//!
//! 「整段折行 == 逐行折行之和」（N4）。本模块按行切片依赖它：单独折一行与它在整段里
//! 折出来的行数相同，故按行切片后拼起来与整段折行等价。`draw.rs` 的等价性用例
//! （视窗渲染与整段渲染逐行相同）就是它的守卫。

use std::time::Instant;

use ratatui::text::{Line, Text};
use ratatui::widgets::{Paragraph, Wrap};

use crate::cache::RenderCache;
use crate::transcript::TranscriptLine;

/// 一个 transcript 条目的布局记忆。
struct EntryMemo {
    /// 条目内容签名（**廉价**）—— 变了才需要重渲染 / 重算折行。
    sig: u64,
    /// 已渲染内容的哈希 —— 节流窗口内内容没真变 ⇒ 复用旧的 `row_starts`。
    render_sig: u64,
    /// 条目级版本号：折行真的重算（`render_sig` 变，或首次渲染）时从
    /// [`TranscriptLayout::rev_alloc`] 分配。
    ///
    /// **产物没变就不换号** —— 「流式节流窗口内源变、产物没变」因此不会打红任何
    /// 以它为键的缓存（见 [`TranscriptLayout::revision_range`]）。
    rev: u64,
    /// 该条目的渲染行（**不含**前置留白）。
    lines: Vec<Line<'static>>,
    /// `row_starts[k]` = 第 `k` 行首行在**条目内**的视觉行号；末位 = 该条目总行数。
    row_starts: Vec<usize>,
}

/// 一次滚动状态同步的结果。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ScrollState {
    /// 可滚动上限（距底部的视觉行数）。
    pub max_scroll: usize,
    /// 视窗顶部的**全局视觉行号**。
    pub top_row: usize,
}

/// 视窗内容：**一段**行 + 窗口首行内部要跳过的视觉行数。
pub(crate) struct Window {
    /// 覆盖视窗的那些行（首行可能只显示下半截 ⇒ 用 [`Window::skip_rows`] 跳过）。
    pub lines: Vec<Line<'static>>,
    /// 窗口首行内部要跳过的视觉行数（`Paragraph::scroll` 的纵向分量）。
    pub skip_rows: usize,
}

/// 屏幕第 y 行（0 基，相对内区顶）的来源。
///
/// `line` 是**窗口内第几个 `Line`**（与 [`Window::lines`] 同序、同下标），不是全局行号；
/// `off` 是该 `Line` 内部的第几个视觉行。两者合起来足以把那一行单独重渲染，不必再查
/// 条目表。
///
/// 两个字段同为 `u32`：视觉行号由「该行的显示宽度 / wrap 宽度」决定，**没有窄上界** ——
/// 单条 200 KB 的文本在 `width = 1` 下可折出 20 万视觉行。`off` 若窄化成 `u16` 会静默
/// 截断，让字形缓存在长行上取到**另一行**的字形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowRef {
    pub line: u32,
    pub off: u32,
}

/// 一次同步的结果。
pub(crate) struct SyncResult {
    /// 总视觉行数。
    pub total_rows: usize,
    /// 宽度是否变了（变了 ⇒ 行号空间整个换掉，补偿不适用）。
    pub width_changed: bool,
    /// 本次同步**真正重算了折行**的条目数（= `render_sig` 变了的条目数，含首次渲染）。
    ///
    /// 唯一用途：「流式拖选最多 10 次/秒」由此从推论变成可断言的机制 —— 拖选期该
    /// 计数**不得**因选区而增加。
    ///
    /// 消费方是 `selection.rs` 的拖选限流断言，故非测试构建里暂时无人读它。
    #[allow(dead_code)]
    pub recomputed_entries: usize,
}

/// transcript 的布局记忆。
pub(crate) struct TranscriptLayout {
    /// wrap 宽度（Chat 内区宽）。变了 ⇒ 全部记忆作废。
    width: u16,
    /// thinking 是否可见。变了 ⇒ 可见条目集合变了 ⇒ 全部记忆作废。
    thinking_visible: bool,
    /// 逐条目记忆（与 transcript 等长；不可见条目为 `None`）。
    entries: Vec<Option<EntryMemo>>,
    /// 每个条目前置的留白行数（不可见条目为 0）。
    gaps: Vec<usize>,
    /// 每个条目的**块起始**全局行号（`len = 条目数 + 1`，末位 = 总行数）。
    starts: Vec<usize>,
    /// 总视觉行数。
    total_rows: usize,
    /// **结构版本**：`reset()` / 宽度变 / `thinking_visible` 翻转时 `+1`。
    ///
    /// **不随条目内容变** —— 否则窗口外条目一更新就会连带全量重建视窗字形缓存。
    structural_rev: u64,
    /// 条目版本号**分配器**（单调递增，只增不减）。
    rev_alloc: u64,
}

impl Default for TranscriptLayout {
    fn default() -> Self {
        Self {
            width: 0,
            thinking_visible: true,
            entries: Vec::new(),
            gaps: Vec::new(),
            starts: Vec::new(),
            total_rows: 0,
            structural_rev: 0,
            rev_alloc: 0,
        }
    }
}

impl TranscriptLayout {
    /// 清空全部记忆 —— 会话回放（transcript 整体替换）专用。
    pub fn reset(&mut self) {
        self.entries.clear();
        self.gaps.clear();
        self.starts.clear();
        self.total_rows = 0;
        // 行号空间整个作废 ⇒ 结构版本 +1（`rev_alloc` 不清：它只要求单调）。
        self.structural_rev += 1;
    }

    /// 同步布局：把 transcript 折成视觉行，并记住每个条目的折行结果。
    ///
    /// **只重算变化了的条目** —— 条目签名（廉价）没变就直接复用记忆；签名变了才
    /// 重新渲染并重算折行；若重渲染后的内容哈希与上次相同（流式节流窗口内
    /// 「源变、产物没变」），连折行结果一起复用。
    pub fn sync(
        &mut self,
        transcript: &[TranscriptLine],
        cache: &mut RenderCache,
        thinking_visible: bool,
        width: u16,
        now: Instant,
    ) -> SyncResult {
        let width = width.max(1);
        let width_changed = width != self.width;
        if width_changed || thinking_visible != self.thinking_visible {
            // 宽度变了 ⇒ 折行结果全变；thinking 可见性变了 ⇒ 可见条目集合变了。
            self.entries.clear();
            self.gaps.clear();
            self.starts.clear();
            self.width = width;
            self.thinking_visible = thinking_visible;
            // 行号空间整个换掉 ⇒ 结构版本 +1（不随条目内容变，只随结构变）。
            self.structural_rev += 1;
        }
        // 条目数变化（追加 / 回放替换）。
        self.entries.truncate(transcript.len());
        while self.entries.len() < transcript.len() {
            self.entries.push(None);
        }

        // 缓存槽位与 transcript 对齐（增长即「上一条结束」）。
        cache.sync_len(transcript.len());

        let mut recomputed_entries = 0usize;
        for (idx, item) in transcript.iter().enumerate() {
            if !crate::draw::is_visible(item, thinking_visible) {
                self.entries[idx] = None;
                continue;
            }
            let sig = entry_sig(item);
            if let Some(memo) = &self.entries[idx]
                && memo.sig == sig
            {
                continue;
            }
            let lines = render_entry(
                transcript,
                cache,
                idx,
                width as usize,
                thinking_visible,
                now,
            );
            let render_sig = lines_hash(&lines);
            let previous = self.entries[idx].take();
            // 折行真的换了才算一次「重算」—— 产物没变（流式节流窗口内）不计入。
            let reused = previous
                .as_ref()
                .is_some_and(|previous| previous.render_sig == render_sig);
            if !reused {
                recomputed_entries += 1;
            }
            // 版本号：产物没变 ⇒ 沿用旧号；否则从分配器取新号（首次渲染同此路径）。
            let rev = if reused {
                previous.as_ref().map_or(0, |previous| previous.rev)
            } else {
                self.rev_alloc += 1;
                self.rev_alloc
            };
            let row_starts = match previous {
                // 产物没真变（流式节流窗口内）⇒ 折行结果照旧。
                Some(previous) if reused => previous.row_starts,
                _ => line_row_starts(&lines, width),
            };
            self.entries[idx] = Some(EntryMemo {
                sig,
                render_sig,
                rev,
                lines,
                row_starts,
            });
        }

        // 全局行号：逐条目累计（前置留白 + 自身内容）。
        let mut starts = Vec::with_capacity(transcript.len() + 1);
        let mut gaps = Vec::with_capacity(transcript.len());
        let mut acc = 0usize;
        let mut prev_class = None;
        let mut has_visible = false;
        for (idx, item) in transcript.iter().enumerate() {
            starts.push(acc);
            if !crate::draw::is_visible(item, thinking_visible) {
                gaps.push(0);
                continue;
            }
            let gap = crate::draw::gap_rows(item, prev_class, has_visible, thinking_visible);
            gaps.push(gap);
            acc += gap;
            acc += self.entries[idx]
                .as_ref()
                .and_then(|memo| memo.row_starts.last().copied())
                .unwrap_or(0);
            prev_class = Some(crate::draw::LineClass::of(item));
            has_visible = true;
        }
        starts.push(acc);
        self.starts = starts;
        self.gaps = gaps;

        let total = acc;
        self.total_rows = total;
        SyncResult {
            total_rows: total,
            width_changed,
            recomputed_entries,
        }
    }

    /// 取视窗：从 `top_row` 起、覆盖 `height` 个视觉行的那些行。
    ///
    /// `skip_rows` 是**窗口首行内部**要跳过的视觉行数（首行被视窗上沿切开时非零）——
    /// 交给 `Paragraph::scroll` 的纵向分量。
    ///
    /// 本函数是 [`Self::window_impl`] 的薄包装（不建 `RowRef`）—— 定位逻辑只有一份。
    pub fn window(&self, top_row: usize, height: usize) -> Window {
        self.window_impl(top_row, height, None)
    }

    /// 与 [`Self::window`] **共用同一次遍历**，额外给出「屏幕第 y 行来自哪个 `Line` 的
    /// 第几个视觉行」。
    ///
    /// `rows.len() == min(height, 从 `top_row` 起的剩余可见行数)`：宽字符与折行仍由
    /// `Paragraph::line_count` 定（本模块不自己算折行）。
    ///
    /// 消费方是 `selection.rs` 的视窗字形缓存，故非测试构建里暂时无人调用。
    #[allow(dead_code)]
    pub(crate) fn window_with_rows(&self, top_row: usize, height: usize) -> (Window, Vec<RowRef>) {
        let mut rows = Vec::new();
        let window = self.window_impl(top_row, height, Some(&mut rows));
        (window, rows)
    }

    /// 视窗遍历的**唯一实现**。`rows` 为 `None` ⇒ 不构造 `RowRef`（稳态帧零分配）。
    fn window_impl(
        &self,
        top_row: usize,
        height: usize,
        mut rows: Option<&mut Vec<RowRef>>,
    ) -> Window {
        // `rows` 是**输出参数**：返回时恰为本次窗口的每行来源（与 `top_row` / `height` 对应）。
        if let Some(rows) = rows.as_deref_mut() {
            rows.clear();
        }
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut skip_rows = 0usize;
        if self.total_rows == 0 || height == 0 {
            return Window { lines, skip_rows };
        }
        let top = top_row.min(self.total_rows - 1);
        let mut idx = self.locate_entry(top);
        let mut rel = top - self.starts[idx];
        let mut remaining = height;
        // 窗口首行是**内容行**还是**留白行**：留白行不折行（1 行 1 视觉行），
        // 内容行可能被视窗上沿切开（`line_off` > 0 ⇒ 渲染时要跳过它的上半截）。
        let mut first_is_content = false;
        let mut first_line_off = 0usize;
        while idx < self.entries.len() && remaining > 0 {
            let Some(memo) = self.entries[idx].as_ref() else {
                idx += 1;
                rel = 0;
                continue;
            };
            let gap = self.gaps[idx];
            if rel < gap {
                // 视窗上沿落在留白行里：先把剩下的留白行补上（每行 1 视觉行）。
                for _ in 0..(gap - rel) {
                    lines.push(Line::from(""));
                    // 留白行也是 1 个视觉行的来源（`off` 恒 0）；窗口装不下的不记。
                    if remaining > 0
                        && let Some(rows) = rows.as_deref_mut()
                    {
                        rows.push(RowRef {
                            line: (lines.len() - 1) as u32,
                            off: 0,
                        });
                    }
                    remaining = remaining.saturating_sub(1);
                }
                rel = 0;
            } else {
                // 视窗上沿在内容里：跳过本条目已过去的留白行 ⇒ `rel` 变成**内容内**行号。
                rel -= gap;
            }
            let content_rows = memo.row_starts.last().copied().unwrap_or(0);
            if rel >= content_rows {
                // 本条目的内容已看完 ⇒ 换下一条目。
                idx += 1;
                rel = 0;
                continue;
            }
            let (line_idx, line_off) = memo.locate(rel);
            if !first_is_content && lines.is_empty() {
                // 窗口的第一行是内容行：它可能被上沿切开。
                first_is_content = true;
                first_line_off = line_off;
            }
            for k in line_idx..memo.lines.len() {
                let rows_n = memo.row_starts[k + 1] - memo.row_starts[k];
                lines.push(memo.lines[k].clone());
                // 首行可能只显示下半截 ⇒ 它贡献 `rows - line_off` 个可见行。
                let cut = k == line_idx && first_is_content && lines.len() == 1;
                let visible = if cut {
                    rows_n.saturating_sub(line_off)
                } else {
                    rows_n
                };
                // 本 `Line` 的可见行：`off` 从 `line_off` 起（被切开时），窗口装不下的不记。
                let first_off = if cut { line_off } else { 0 };
                let take = visible.min(remaining);
                if take > 0
                    && let Some(rows) = rows.as_deref_mut()
                {
                    let line = (lines.len() - 1) as u32;
                    rows.extend((0..take).map(|j| RowRef {
                        line,
                        off: row_off(first_off + j),
                    }));
                }
                remaining = remaining.saturating_sub(visible);
                if remaining == 0 {
                    break;
                }
            }
            idx += 1;
            rel = 0;
        }
        if first_is_content {
            skip_rows = first_line_off;
        }
        Window { lines, skip_rows }
    }

    /// 窗口覆盖条目的版本号 + 窗口顶的**条目内偏移**折叠成一个 `u64`（无分配）——
    /// 视窗字形缓存的**窗口版本键**。
    ///
    /// **只看窗口覆盖的条目**（不遍历整个 `entries`）：窗口外条目更新不得打红缓存，
    /// 否则「尾部增长、视窗原地不动」会退化成全量重建。
    ///
    /// **必含首条目内偏移**：`top_row` 相同不代表窗口内容相同。非贴底时内容在尾部增长
    /// Δ 行，`app` 侧把 `scroll_offset` 也加 Δ —— `top_row` 数值不变；但若增长发生在
    /// 窗口**上方**的条目里，`starts[]` 整体下移，同一个 `top_row` 指向的行内位置就往前
    /// 退了若干视觉行，窗口内容已经换了一批。只折叠 `rev` 会漏掉这次变化，缓存于是拿
    /// 旧字形配新内容。
    ///
    /// 消费方是 `selection.rs` 的视窗字形缓存，故非测试构建里暂时无人调用。
    #[allow(dead_code)]
    pub(crate) fn revision_range(&self, top_row: usize, height: usize) -> u64 {
        if self.total_rows == 0 || height == 0 || self.starts.is_empty() {
            return 0;
        }
        let top = top_row.min(self.total_rows - 1);
        // 窗口覆盖的全局行区间（半开）：`[top, end)`。
        let end = top.saturating_add(height).min(self.total_rows);
        let first = self.locate_entry(top);
        // 最后一个块起始 < `end` 的条目 —— 与 `window_impl` 推进条目的依据同源
        // （`starts[i]` 是第 i 个条目的块起始、`starts[i+1]` 是块结束）。
        let last = self
            .starts
            .partition_point(|&start| start < end)
            .saturating_sub(1);
        // 首条目内偏移（含该条目的前置留白）—— 见函数文档。
        let mut hash = mix(0, 13, (top - self.starts[first]) as u64);
        for idx in first..=last.min(self.entries.len().saturating_sub(1)) {
            // 不可见条目不产出行 ⇒ 无版本可言。
            if let Some(memo) = self.entries[idx].as_ref() {
                hash = mix(hash, 14, memo.rev);
            }
        }
        hash
    }

    /// 结构版本号：`reset()` / 宽度变 / `thinking_visible` 翻转时 +1。**不随条目内容变**。
    #[allow(dead_code)]
    pub(crate) fn structural_rev(&self) -> u64 {
        self.structural_rev
    }

    /// 总视觉行数。
    #[allow(dead_code)]
    pub(crate) fn total_rows(&self) -> usize {
        self.total_rows
    }

    /// 当前 wrap 宽度（折行口径，`>= 1`）—— 造 `Buffer` 渲字形时需要它。
    #[allow(dead_code)]
    pub(crate) fn width(&self) -> u16 {
        self.width
    }

    /// 定位某个全局行号落在哪个条目。
    fn locate_entry(&self, row: usize) -> usize {
        if self.starts.is_empty() {
            return 0;
        }
        // 最后一个 `starts[i] <= row` 的 i。不可见条目与后一条同起点 ⇒ 取到的是**可见**那条。
        self.starts
            .partition_point(|&start| start <= row)
            .saturating_sub(1)
            .min(self.entries.len().saturating_sub(1))
    }
}

impl EntryMemo {
    /// 条目内行号 → （行下标，行内偏移）。
    fn locate(&self, rel: usize) -> (usize, usize) {
        if self.lines.is_empty() {
            return (0, 0);
        }
        let k = self
            .row_starts
            .partition_point(|&start| start <= rel)
            .saturating_sub(1)
            .min(self.lines.len() - 1);
        (k, rel - self.row_starts[k])
    }
}

/// 渲染一个 transcript 条目的行（**不含**前置留白）。
///
/// assistant 走 md 缓存（[`RenderCache`]），其余变体是 O(1) 格式化。
fn render_entry(
    transcript: &[TranscriptLine],
    cache: &mut RenderCache,
    idx: usize,
    width: usize,
    thinking_visible: bool,
    now: Instant,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    crate::draw::push_transcript_line(
        &mut out,
        &transcript[idx],
        width,
        thinking_visible,
        cache,
        idx,
        now,
    );
    out
}

/// 条目内视觉行号 → [`RowRef::off`]。
///
/// **不得截断**：视觉行号没有窄上界（见 [`RowRef`]）。`usize` 超出 `u32` 需要单行折出
/// 40 亿以上视觉行（宽 ≥ 1 ⇒ 单行 40 亿字节以上），`debug_assert` 钉住该前提；
/// 发布构建取饱和值而非回绕 —— 回绕会让第 0 个视觉行与第 2³² 个撞车。
fn row_off(off: usize) -> u32 {
    debug_assert!(
        u32::try_from(off).is_ok(),
        "条目内视觉行号 {off} 超出 u32，RowRef::off 无法表示"
    );
    u32::try_from(off).unwrap_or(u32::MAX)
}

/// 逐行折行，返回每行的起始行号（`len = 行数 + 1`，末位 = 总行数）。
///
/// **逐行折行 == 整段折行**（N4 不变式）—— 视窗切片依赖它。
fn line_row_starts(lines: &[Line<'static>], width: u16) -> Vec<usize> {
    let mut starts = Vec::with_capacity(lines.len() + 1);
    let mut acc = 0usize;
    starts.push(0);
    for line in lines {
        let rows = Paragraph::new(Text::from(vec![line.clone()]))
            .wrap(Wrap { trim: false })
            .line_count(width);
        debug_assert!(rows >= 1, "宽 ≥ 1 时每行至少占 1 个视觉行");
        acc += rows.max(1);
        starts.push(acc);
    }
    starts
}

/// 条目内容签名（**廉价**）—— 变了才需要重渲染 / 重算折行。
///
/// 只看**长度**不看内容：内容变了但长度没变（极少见，且只在原地改写时发生）
/// 会漏检 —— 那种情况下折行结果多半也不变，故代价只是「可能没跟上」，
/// 不是「算错」。真要严谨可换成内容哈希，代价是每次同步都 O(全部字节)。
fn entry_sig(item: &TranscriptLine) -> u64 {
    let mut hash = FNV_OFFSET;
    match item {
        TranscriptLine::User(s) => hash = mix(hash, 1, s.len() as u64),
        TranscriptLine::Assistant(s) => hash = mix(hash, 2, s.len() as u64),
        TranscriptLine::Thinking(s) => hash = mix(hash, 3, s.len() as u64),
        TranscriptLine::Tool {
            name,
            summary,
            result,
            success,
            ..
        } => {
            hash = mix(hash, 4, name.len() as u64);
            hash = mix(hash, 5, summary.len() as u64);
            hash = mix(hash, 6, result.as_ref().map_or(0, |r| r.len() as u64 + 1));
            hash = mix(hash, 7, u64::from(*success));
        }
        TranscriptLine::Summary {
            rounds,
            stop,
            elapsed_secs,
        } => {
            hash = mix(hash, 8, *rounds as u64);
            hash = mix(hash, 9, crate::draw::stop_key(stop));
            hash = mix(hash, 10, u64::from(elapsed_secs.to_bits()));
        }
        TranscriptLine::Error(s) => hash = mix(hash, 11, s.len() as u64),
        TranscriptLine::System(s) => hash = mix(hash, 12, s.len() as u64),
    }
    hash
}

/// 已渲染内容的哈希 —— 判「源变了但产物没变」（流式节流窗口内）。
fn lines_hash(lines: &[Line<'static>]) -> u64 {
    let mut hash = FNV_OFFSET;
    for line in lines {
        for span in &line.spans {
            for byte in span.content.as_bytes() {
                hash = (hash ^ *byte as u64).wrapping_mul(FNV_PRIME);
            }
            // 分隔符：防止相邻 span 的内容拼接出歧义。
            hash = (hash ^ 0xff).wrapping_mul(FNV_PRIME);
        }
        hash = (hash ^ 0xfe).wrapping_mul(FNV_PRIME);
    }
    hash
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 混入一个数（`hash = (hash ^ value) * PRIME`）。
fn mix(hash: u64, tag: u64, value: u64) -> u64 {
    let hash = (hash ^ tag).wrapping_mul(FNV_PRIME);
    (hash ^ value).wrapping_mul(FNV_PRIME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::app::App;
    use crate::view::CodingView;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn app_with(lines: Vec<TranscriptLine>) -> App {
        let mut app = App::new(CodingView::for_test());
        app.transcript = lines;
        app
    }

    fn sample() -> App {
        app_with(vec![
            TranscriptLine::User("问题：这段代码怎么读？".into()),
            TranscriptLine::Assistant(
                "# 标题\n\n这是 **粗体** 与 `code` 的一段中文正文。\n\n- 要点一\n- 要点二\n".into(),
            ),
            TranscriptLine::Thinking("先想想再答。".into()),
            TranscriptLine::User("继续".into()),
            TranscriptLine::Assistant("| 名称 | 值 |\n| -- | -- |\n| 中文 | 数字 |\n".into()),
        ])
    }

    /// **N4（layout 层）**：逐条目的折行行数之和 == 整段折行的行数。
    ///
    /// 视窗切片依赖它 —— 若不成立，滚动位置与画面会错位。
    #[test]
    fn test_total_rows_equals_whole_line_count() {
        for w in [20u16, 40, 80, 120] {
            for thinking_visible in [false, true] {
                let mut app = sample();
                app.thinking_visible = thinking_visible;
                let total = {
                    let mut cache = app.md_cache.borrow_mut();
                    let mut layout = TranscriptLayout::default();
                    layout
                        .sync(
                            &app.transcript,
                            &mut cache,
                            thinking_visible,
                            w,
                            Instant::now(),
                        )
                        .total_rows
                };
                // 重新取整段文本（此时 md 缓存已命中 ⇒ 与 layout 读的是同一份产物）。
                let text = crate::draw::build_text(&app, w as usize, Instant::now());
                let whole = Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .line_count(w);
                assert_eq!(
                    total, whole,
                    "w={w} thinking={thinking_visible}：逐条目折行之和必须等于整段折行"
                );
            }
        }
    }

    /// 视窗的两端：**顶行**取到内容的第一行，**末行**取到内容的最后一行。
    #[test]
    fn test_window_anchors_at_both_ends() {
        let app = sample();
        let w = 40u16;
        // 同步与取整段文本**分两段**做 —— `build_text` 自己也要借 md 缓存
        // （`RefCell` 不可重入）。
        let total = {
            let mut cache = app.md_cache.borrow_mut();
            let mut layout = TranscriptLayout::default();
            layout
                .sync(&app.transcript, &mut cache, true, w, Instant::now())
                .total_rows
        };
        assert!(total > 3, "前置：内容不止几行");
        let mut layout = TranscriptLayout::default();
        {
            let mut cache = app.md_cache.borrow_mut();
            layout.sync(&app.transcript, &mut cache, true, w, Instant::now());
        }

        let head = layout.window(0, 1);
        assert_eq!(head.skip_rows, 0, "顶行不必跳过行内偏移");
        assert_eq!(head.lines.len(), 1, "只要一行");
        assert!(
            head.lines[0].to_string().contains("问题"),
            "顶行必须是第一条内容：{:?}",
            head.lines[0].to_string()
        );

        let tail = layout.window(total - 1, 1);
        assert_eq!(tail.lines.len(), 1, "末行只要一行");
        // 末行 = 整段文本的最后一行（表格的底边框也算内容的一部分）。
        let whole = crate::draw::build_text(&app, w as usize, Instant::now());
        assert_eq!(
            tail.lines[0].to_string(),
            whole.lines.last().expect("整段非空").to_string(),
            "末行必须是整段的最后一行"
        );
    }

    /// **不重复渲染**：条目没变 ⇒ 再次同步**不重新解析** markdown。
    ///
    /// 这是「滚动不随会话长度变慢」的机制保证：折行结果按条目记住。
    #[test]
    fn test_unchanged_entries_are_not_re_rendered() {
        let app = sample();
        let w = 40u16;
        crate::md::reset_render_calls();
        {
            let mut cache = app.md_cache.borrow_mut();
            let mut layout = TranscriptLayout::default();
            layout.sync(&app.transcript, &mut cache, true, w, Instant::now());
        }
        let first = crate::md::render_calls();
        assert!(first > 0, "前置：首轮确实渲染了");
        {
            let mut cache = app.md_cache.borrow_mut();
            let mut layout = TranscriptLayout::default();
            layout.sync(&app.transcript, &mut cache, true, w, Instant::now());
        }
        assert_eq!(
            crate::md::render_calls(),
            first,
            "内容未变 ⇒ 不得重新解析（折行结果按条目记住）"
        );
    }

    /// 同步一次（把 `md_cache` 的 `RefCell` 守卫封在内部 —— 用例里不重入借用）。
    fn sync_once(
        app: &App,
        layout: &mut TranscriptLayout,
        width: u16,
        thinking_visible: bool,
        now: Instant,
    ) -> SyncResult {
        let mut cache = app.md_cache.borrow_mut();
        layout.sync(&app.transcript, &mut cache, thinking_visible, width, now)
    }

    fn synced(app: &App, width: u16, thinking_visible: bool, now: Instant) -> TranscriptLayout {
        let mut layout = TranscriptLayout::default();
        sync_once(app, &mut layout, width, thinking_visible, now);
        layout
    }

    /// 整段渲染的**逐视觉行**结果（每行拼成字符串）—— 语义对照的参考答案。
    ///
    /// 一次性把整份 transcript 渲进「高度 = 总视觉行数」的缓冲区。思考可见性由 `app`
    /// 自身决定（`build_text` 从 `app` 读）—— 调用方必须先把它调成与 `layout.sync`
    /// 传入的一致。
    fn whole_render_rows(app: &App, width: u16, total: usize) -> Vec<String> {
        let text = crate::draw::build_text(app, width as usize, Instant::now());
        let backend = TestBackend::new(width.max(1), total.min(u16::MAX as usize) as u16);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let para = Paragraph::new(text).wrap(Wrap { trim: false });
                f.render_widget(para, f.area());
            })
            .unwrap();
        buffer_rows(terminal.backend().buffer())
    }

    /// 单个 `Line` 的**第 `off` 个视觉行**（1 行高的缓冲区）—— 解码 [`RowRef`] 的参考路径。
    fn rendered_row(line: &Line<'static>, width: u16, off: usize) -> String {
        let backend = TestBackend::new(width.max(1), 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let para = Paragraph::new(Text::from(vec![line.clone()]))
                    .wrap(Wrap { trim: false })
                    .scroll((off.min(u16::MAX as usize) as u16, 0));
                f.render_widget(para, f.area());
            })
            .unwrap();
        buffer_rows(terminal.backend().buffer()).concat()
    }

    fn buffer_rows(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
        let width = buffer.area().width as usize;
        buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect()
    }

    /// **V1**：`RowRef` 与视窗遍历的一致性，且**与真实折行逐行对上**。
    ///
    /// 样本含 CJK / 折行 / 表格 / thinking，宽度取窄（12）—— 必然折行，`off` 才有非零的机会。
    /// `top` 扫遍**每一个**行号（而非只采样几个）：折行只在特定行上把 `off` 推开，采样会
    /// 恰好落在「行首」那些行上，变异才不会被抓到。
    ///
    /// 两类断言：
    /// 1. **内部自洽**：`line` 单调、同 `Line` 内 `off` 逐行递增、首行 `off` == `skip_rows`；
    /// 2. **语义对照**：`rows[y]` 解码出的那一视觉行，必须与「整段渲染的第 `top + y` 行」
    ///    逐字节相同 —— 这条把 `rows` 钉在真实折行上（少记行 / `off` 起点错都会红）。
    #[test]
    fn test_window_rows_invariants() {
        // `whole_render_rows` 从 `app` 读思考可见性 ⇒ 必须先调成与 `synced` 传入的一致。
        let mut app = sample();
        app.thinking_visible = true;
        let width = 12u16;
        let layout = synced(&app, width, true, Instant::now());
        let total = layout.total_rows();
        assert!(total > 6, "前置：样本需多于几行（实际 {total}）");
        assert!(total < u16::MAX as usize, "前置：参考渲染要一次放进缓冲区");
        let reference = whole_render_rows(&app, width, total);
        assert_eq!(
            reference.len(),
            total,
            "前置：整段渲染的行数 == 布局总行数（N4）"
        );
        for top in 0..total {
            for h in [1usize, 3, 7, total] {
                let (window, rows) = layout.window_with_rows(top, h);
                let ctx = format!("top={top} h={h} total={total}");
                // 窗口恰有 `min(h, 剩余可见行)` 行 —— **等号**：少记一行（每个 `Line`
                // 只记 1 行）或漏掉首行的剩余部分都会红。
                assert_eq!(
                    rows.len(),
                    h.min(total - top),
                    "{ctx}：行映射必须恰为窗口覆盖的视觉行数"
                );
                assert!(!rows.is_empty(), "{ctx}：窗口非空 ⇒ 行映射非空");
                assert_eq!(
                    rows[0].off as usize, window.skip_rows,
                    "{ctx}：首行偏移必须与 skip_rows 一致"
                );
                for (y, r) in rows.iter().enumerate() {
                    assert!(
                        (r.line as usize) < window.lines.len(),
                        "{ctx}：rows[{y}].line 越界（窗口 {} 行）",
                        window.lines.len()
                    );
                    if y > 0 {
                        let prev = rows[y - 1];
                        assert!(prev.line <= r.line, "{ctx}：rows[{y}].line 必须单调不减");
                        if prev.line == r.line {
                            assert_eq!(r.off, prev.off + 1, "{ctx}：同一 Line 内 off 必须逐行递增");
                        }
                    }
                    // 语义对照：解码出的那一行 == 整段渲染的第 `top + y` 行。
                    let decoded =
                        rendered_row(&window.lines[r.line as usize], width, r.off as usize);
                    assert_eq!(
                        decoded,
                        reference[top + y],
                        "{ctx}：第 {y} 行（line={} off={}）与整段渲染的第 {} 行不一致",
                        r.line,
                        r.off,
                        top + y
                    );
                }
            }
        }
    }

    /// **`RowRef.off` 不得截断**：条目内视觉行号可以远超 `u16`。
    ///
    /// 宽 1 + 7 万字符的单条 `User` 行 ⇒ 7 万余视觉行。窗口顶落在其中 ⇒ `rows[0].off`
    /// 必须是那个行号本身。（`off` 写成 `u16` 时 70000 会静默变成 4464。）
    #[test]
    fn test_row_ref_off_keeps_visual_row_beyond_u16() {
        let width = 1u16;
        // 宽 1 时每个字符占一个视觉行；`> ` 前缀占 2 行，故第 `n` 个视觉行 == 正文第 `n-2` 格。
        let target = 70_000usize;
        let app = app_with(vec![TranscriptLine::User("x".repeat(target + 2))]);
        let layout = synced(&app, width, true, Instant::now());
        let total = layout.total_rows();
        assert!(
            total > target,
            "前置：单条就能折出 {target} 行以上（实际 {total}）"
        );
        let (window, rows) = layout.window_with_rows(layout.starts[0] + target, 2);
        assert_eq!(rows.len(), 2, "窗口高 2 ⇒ 两行映射");
        assert_eq!(rows[0].line, 0, "整条只有 1 个 `Line`");
        assert_eq!(
            rows[0].off as usize, target,
            "条目内视觉行号必须原样给出（不得截断到 u16）"
        );
        assert_eq!(rows[1].off as usize, target + 1, "后续行逐行递增");
        assert_eq!(window.skip_rows, target, "首行偏移与 rows[0].off 同源");
    }

    /// **同一遍历**：`window()` 与 `window_with_rows().0` 逐字段相同。
    ///
    /// 两者共用 `window_impl` —— 若哪天有人复制了第二份定位逻辑，这条用例会红。
    #[test]
    fn test_window_with_rows_shares_traversal() {
        let app = sample();
        let width = 32u16;
        let layout = synced(&app, width, true, Instant::now());
        let total = layout.total_rows();
        for top in [0usize, 3, total / 2, total - 1] {
            for h in [1usize, 3, 12, total] {
                let plain = layout.window(top, h);
                let (with_rows, _rows) = layout.window_with_rows(top, h);
                assert_eq!(
                    plain.skip_rows, with_rows.skip_rows,
                    "top={top} h={h}：skip_rows"
                );
                assert_eq!(plain.lines, with_rows.lines, "top={top} h={h}：lines");
            }
        }
    }

    /// 4 行版 `User` 条目 —— 宽 40 下每行恰 1 个视觉行，**改内容不改几何**。
    fn user_block(tag: &str) -> TranscriptLine {
        TranscriptLine::User(format!("{tag}1\n{tag}2\n{tag}3\n{tag}4"))
    }

    /// 六个等高的 `User` 块：每个块 = 2 行留白 + 4 行内容（首块无留白）。
    fn revision_sample() -> App {
        app_with((1..=6).map(|n| user_block(&"p".repeat(n))).collect())
    }

    /// `revision_range` 只折叠**窗口内**条目 —— 窗口上方 / 下方的条目更新不得打红缓存。
    ///
    /// 窗口取**中部**且恰好盖住一个条目（窗口上沿 = 该条目块起点、下沿 = 下一条目块起点）：
    /// 这样「上一条目」与「下一条目」都真实存在，且下沿**恰好压在块边界上**。
    #[test]
    fn test_revision_range_only_covers_window() {
        let mut app = revision_sample();
        let width = 40u16;
        let base = Instant::now();
        let mut layout = TranscriptLayout::default();
        let result = sync_once(&app, &mut layout, width, true, base);
        assert_eq!(
            layout.total_rows(),
            result.total_rows,
            "只读取值器与同步结果同源"
        );
        assert_eq!(layout.width(), width, "只读取值器回的是折行口径宽度");

        // 窗口位于中部、恰好覆盖第 `k` 个条目的块（留白 + 内容）。
        let k = 3usize;
        let top = layout.starts[k] + 3;
        let h = layout.starts[k + 1] - top;
        let total = layout.total_rows();
        assert!(k > 0 && k + 1 < layout.entries.len(), "前置：上下都有邻居");
        assert!(
            top > total / 4 && top + h < total * 3 / 4,
            "前置：窗口在中部"
        );

        let before = layout.revision_range(top, h);
        assert_ne!(before, 0, "前置：窗口内有条目 ⇒ 版本键非零");
        let starts_k = layout.starts[k];
        let starts_k1 = layout.starts[k + 1];

        // 窗口**上方**的条目：换成等高的另一段（`starts` 不动 ⇒ 窗口内容不变）。
        app.transcript[1] = user_block("zzzz");
        sync_once(
            &app,
            &mut layout,
            width,
            true,
            base + Duration::from_secs(1),
        );
        assert_eq!(
            layout.starts[k], starts_k,
            "前置：上方换成等高的 ⇒ 行号不变"
        );
        assert_eq!(
            layout.revision_range(top, h),
            before,
            "窗口上方条目更新 ⇒ 窗口版本键不得变（否则退化成全量折叠）"
        );

        // 窗口**下方**的条目：窗口下沿恰好压在块边界上。
        assert_eq!(starts_k1, top + h, "前置：下沿压在块边界上");
        app.transcript[k + 1] = user_block("wwwwww");
        sync_once(
            &app,
            &mut layout,
            width,
            true,
            base + Duration::from_secs(2),
        );
        assert_eq!(
            layout.revision_range(top, h),
            before,
            "窗口下方条目更新 ⇒ 窗口版本键不得变（不得多算一行）"
        );

        // 改窗口**内**条目。
        app.transcript[k] = user_block("qq");
        sync_once(
            &app,
            &mut layout,
            width,
            true,
            base + Duration::from_secs(3),
        );
        assert_ne!(
            layout.revision_range(top, h),
            before,
            "窗口内条目更新 ⇒ 窗口版本键必须变"
        );
    }

    /// **窗口键必含窗口顶的条目内偏移**：`top_row` 不变不等于窗口内容不变。
    ///
    /// 场景（`app::sync_scroll_state` 的增量补偿）：非贴底时内容在**上方**增长 Δ 行，
    /// `scroll_offset` 也跟着加 Δ ⇒ `top_row` 数值不变；但 `starts[]` 整体下移，
    /// 同一个 `top_row` 在条目内的相对位置退了 Δ 行 ⇒ 窗口已换了一批行。
    ///
    /// 只折叠 `rev` 会漏掉这次变化（覆盖条目集合与各自的 `rev` 都没变）⇒ 字形缓存不刷新。
    #[test]
    fn test_revision_range_folds_window_top_inner_offset() {
        let mut app = revision_sample();
        let width = 40u16;
        let base = Instant::now();
        let mut layout = TranscriptLayout::default();
        sync_once(&app, &mut layout, width, true, base);

        let k = 3usize;
        let top = layout.starts[k] + 3;
        let h = layout.starts[k + 1] - top;
        let before_key = layout.revision_range(top, h);
        let starts_k = layout.starts[k];
        let before_lines: Vec<String> = layout
            .window(top, h)
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect();
        let rev_of = |layout: &TranscriptLayout, idx: usize| {
            layout.entries[idx].as_ref().map(|memo| memo.rev)
        };
        let before_rev = rev_of(&layout, k);

        // 只长**窗口上方**的条目：块高 +1 行 ⇒ 其下所有 `starts` 下移 1。
        app.transcript[1] = TranscriptLine::User("zz1\nzz2\nzz3\nzz4\nzz5".into());
        sync_once(
            &app,
            &mut layout,
            width,
            true,
            base + Duration::from_secs(1),
        );

        // 前置：窗口顶**行号不变**，上方条目真的变高了，窗口内条目原封不动。
        assert_eq!(
            layout.starts[k],
            starts_k + 1,
            "前置：上方条目长高 1 行（`starts` 下移）"
        );
        assert_eq!(
            rev_of(&layout, k),
            before_rev,
            "前置：窗口内条目未被重算 ⇒ 覆盖条目的版本号没变"
        );
        let after_lines: Vec<String> = layout
            .window(top, h)
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect();
        assert_ne!(before_lines, after_lines, "前置：窗口内容确实换了");

        // 行号与覆盖条目的 `rev` 都没变 —— 唯一的区别是条目内偏移。
        assert_ne!(
            layout.revision_range(top, h),
            before_key,
            "窗口顶在条目内的偏移变了 ⇒ 窗口版本键必须变"
        );
    }

    /// `recomputed_entries` 数的是**真正重算**的条目。
    #[test]
    fn test_recomputed_entries_counts_real_rerenders() {
        let mut app = sample();
        let width = 40u16;
        let base = Instant::now();
        let mut layout = TranscriptLayout::default();
        let first = sync_once(&app, &mut layout, width, true, base).recomputed_entries;
        assert!(first > 0, "首轮每条都要算");
        assert_eq!(
            sync_once(&app, &mut layout, width, true, base).recomputed_entries,
            0,
            "内容未变 ⇒ 一次重算都不该有"
        );
        // 改**非 assistant** 条目：`RenderCache` 的前提是 assistant 源文本只增不减
        // （改历史须升级为内容哈希），换成 User 才能自由改写内容。
        app.transcript[0] = TranscriptLine::User("换一个问题：这次写得长一些。".into());
        assert_eq!(
            sync_once(
                &app,
                &mut layout,
                width,
                true,
                base + Duration::from_secs(1)
            )
            .recomputed_entries,
            1,
            "改一条 ⇒ 只重算那一条"
        );
    }

    /// `structural_rev` 只随**结构**（reset / 宽度 / thinking）变，不随条目内容变。
    #[test]
    fn test_structural_rev_only_tracks_structure() {
        let mut app = sample();
        let now = Instant::now();
        let mut layout = TranscriptLayout::default();

        sync_once(&app, &mut layout, 40, true, now);
        let base = layout.structural_rev();
        assert!(base > 0, "首次同步换掉了整个行号空间（宽度 0 → 40）");

        sync_once(&app, &mut layout, 40, true, now);
        assert_eq!(
            layout.structural_rev(),
            base,
            "同宽同 thinking ⇒ 结构版本不动"
        );

        // 只改内容（且是**非 assistant** 条目 —— 缓存前提是 assistant 源只增不减）。
        app.transcript[3] = TranscriptLine::User("继续（改写过的同一问）".into());
        sync_once(&app, &mut layout, 40, true, now + Duration::from_secs(1));
        assert_eq!(layout.structural_rev(), base, "只内容变 ⇒ 结构版本不动");

        sync_once(&app, &mut layout, 30, true, now + Duration::from_secs(2));
        assert_eq!(layout.structural_rev(), base + 1, "宽度变 ⇒ +1");

        sync_once(&app, &mut layout, 30, false, now + Duration::from_secs(3));
        assert_eq!(layout.structural_rev(), base + 2, "thinking 翻转 ⇒ +1");

        layout.reset();
        assert_eq!(layout.structural_rev(), base + 3, "reset ⇒ +1");
    }

    /// `rev_alloc` 单调递增；产物没变的那次**不**消耗新号。
    #[test]
    fn test_rev_alloc_monotonic_and_no_waste() {
        let mut app = app_with(vec![TranscriptLine::Assistant("abc".into())]);
        let width = 40u16;
        let base = Instant::now();
        let mut layout = TranscriptLayout::default();

        sync_once(&app, &mut layout, width, true, base);
        let rev0 = layout.entries[0].as_ref().expect("已渲染").rev;
        assert_eq!(layout.rev_alloc, rev0, "首次渲染从分配器取新号");

        // 节流窗口内增长：**源**变了、**产物**没变 ⇒ 不换号、不消耗新号。
        app.transcript[0] = TranscriptLine::Assistant("abcd".into());
        let result = sync_once(
            &app,
            &mut layout,
            width,
            true,
            base + Duration::from_millis(10),
        );
        assert_eq!(result.recomputed_entries, 0, "产物没变 ⇒ 不算重算");
        assert_eq!(
            layout.entries[0].as_ref().expect("已渲染").rev,
            rev0,
            "不换号"
        );
        assert_eq!(layout.rev_alloc, rev0, "不消耗新号");

        // 过了节流窗口、且源再变一次（只改长度 ⇒ 布局签名也会变）⇒ 真重算 ⇒ 换号。
        app.transcript[0] = TranscriptLine::Assistant("abcde".into());
        sync_once(
            &app,
            &mut layout,
            width,
            true,
            base + Duration::from_secs(1),
        );
        let rev1 = layout.entries[0].as_ref().expect("已渲染").rev;
        assert!(rev1 > rev0, "重算 ⇒ 必须换新号（{rev1} > {rev0}）");
        assert_eq!(layout.rev_alloc, rev1, "分配器与最新分配号一致");
    }
}
