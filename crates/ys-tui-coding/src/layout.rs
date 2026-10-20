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

/// 一次同步的结果。
pub(crate) struct SyncResult {
    /// 总视觉行数。
    pub total_rows: usize,
    /// 宽度是否变了（变了 ⇒ 行号空间整个换掉，补偿不适用）。
    pub width_changed: bool,
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
        }
        // 条目数变化（追加 / 回放替换）。
        self.entries.truncate(transcript.len());
        while self.entries.len() < transcript.len() {
            self.entries.push(None);
        }

        // 缓存槽位与 transcript 对齐（增长即「上一条结束」）。
        cache.sync_len(transcript.len());

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
            let row_starts = match previous {
                // 产物没真变（流式节流窗口内）⇒ 折行结果照旧。
                Some(previous) if previous.render_sig == render_sig => previous.row_starts,
                _ => line_row_starts(&lines, width),
            };
            self.entries[idx] = Some(EntryMemo {
                sig,
                render_sig,
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
        }
    }

    /// 取视窗：从 `top_row` 起、覆盖 `height` 个视觉行的那些行。
    ///
    /// `skip_rows` 是**窗口首行内部**要跳过的视觉行数（首行被视窗上沿切开时非零）——
    /// 交给 `Paragraph::scroll` 的纵向分量。
    pub fn window(&self, top_row: usize, height: usize) -> Window {
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
                let rows = memo.row_starts[k + 1] - memo.row_starts[k];
                lines.push(memo.lines[k].clone());
                // 首行可能只显示下半截 ⇒ 它贡献 `rows - line_off` 个可见行。
                let visible = if k == line_idx && first_is_content && lines.len() == 1 {
                    rows.saturating_sub(line_off)
                } else {
                    rows
                };
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
    use crate::app::App;
    use crate::view::CodingView;

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
}
