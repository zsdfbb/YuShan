//! assistant 渲染产物的记忆 + 流式节流（设计 §2.5）。
//!
//! **纯 memo 契约**：本模块不参与任何业务逻辑；槽位失效只损失 CPU，绝不改变渲染输出
//! （不变量 4）。槽位与 transcript 等长，**仅 Assistant 槽位被填充**（其余变体每帧现算，
//! 是 O(1) 格式化、不解析）。
//!
//! 键 = `(槽下标, src.len(), width)`：槽下标即槽位位置，故槽内只存 `(len, width)`。
//! **必须含 width** —— 库输出对表格依赖宽度（resize 后必须重算）。
//! 「长度相等即内容相同」的前提：transcript **只增不减**、Assistant 只经**尾部 `push_str`**
//! 增长（无编辑历史）；用 `debug_assert!` 钉住这条前提。

use std::time::{Duration, Instant};

use ratatui::text::Line;

use crate::md;
use crate::styles::AssistantStyles;

/// 流式节流窗口：距上次渲染不足此时长时**沿用上次产物**（设计 §2.5）。
pub(crate) const RENDER_THROTTLE: Duration = Duration::from_millis(100);

/// 单条源文本超过此字节数则**不入缓存**（退化每帧现算，避免内存失控，设计 §2.5）。
const MAX_CACHED_SRC: usize = 256 * 1024;

/// 一个已渲染槽位。
struct Slot {
    /// 渲染时源文本的字节长度（键的一部分）。
    len: usize,
    /// 渲染时使用的宽度（键的一部分）。
    width: usize,
    /// 渲染产物。
    lines: Vec<Line<'static>>,
    /// 上次渲染时刻（节流用；由调用方注入）。
    rendered_at: Instant,
    /// 已标记「该条结束」：下次取用时忽略节流、必算（设计 §2.5 边界 ②）。
    forced: bool,
}

/// assistant 渲染产物的记忆缓存 + 流式节流状态。
///
/// 节流状态**存在本结构内**（它记住每条上次算出什么、何时算的），令 `total_rows` 与
/// `draw_transcript` 读到**同一份**上次产物 —— 否则滚动按新内容、画面按旧内容，二者错位
/// （设计 §2.5 边界 ①，风险 R11）。
#[derive(Default)]
pub(crate) struct RenderCache {
    slots: Vec<Option<Slot>>,
    /// 最近一次被 `lines_for` 请求的槽位 —— `sync_len` 增长时据此把「上一条」标记为结束。
    streaming: Option<usize>,
    /// 超长单条（`> MAX_CACHED_SRC`）的**暂存产物**：只服务退化路径，**不写 slot**（内存意图：
    /// 超长单条真的不入缓存，设计 §2.5）。每次退化渲染覆写它。
    scratch: Vec<Line<'static>>,
}

impl RenderCache {
    /// 令槽位数量与 transcript 对齐（transcript 只增不减；缩短按「会话重置」语义截掉尾部）。
    ///
    /// **增长即意味着「上一条」结束**：正在流式增长的那条（`streaming`）不再是尾部，标记其
    /// `forced`，使下一次取用忽略节流、做一次完整渲染 —— 否则该条最后一段增量会永久停留
    /// 不显示（设计 §2.5 边界 ②）。
    pub fn sync_len(&mut self, transcript_len: usize) {
        if transcript_len > self.slots.len() {
            if let Some(prev) = self.streaming
                && let Some(slot) = self.slots.get_mut(prev).and_then(Option::as_mut)
            {
                slot.forced = true;
            }
            self.slots.resize_with(transcript_len, || None);
        } else if transcript_len < self.slots.len() {
            self.slots.truncate(transcript_len);
            if self.streaming.is_some_and(|s| s >= transcript_len) {
                self.streaming = None;
            }
        }
    }

    /// 显式标记某槽位「该条结束」：下次取用忽略节流、必算。
    ///
    /// 与 [`Self::sync_len`] 的增长路径互补 —— 当该条结束**不必**紧跟一条新 transcript 行
    /// （例如 turn 结算而不追加行）时，由调用方显式收尾。
    pub fn finish(&mut self, idx: usize) {
        if let Some(slot) = self.slots.get_mut(idx).and_then(Option::as_mut) {
            slot.forced = true;
        }
    }

    /// 取该槽位的渲染行：命中则复用，否则调 [`md::render`] 并记住。
    ///
    /// 命中规则（宽度不同、`forced`、超长单条一律不命中）：
    /// - **内容相同**（`src.len()` 与宽度都与上次一致）⇒ **永久命中**，与 `now` 无关；
    /// - **正在增长**（`src.len()` 变大）⇒ 距上次渲染 < [`RENDER_THROTTLE`] 时沿用上次产物，
    ///   到点则重算。节流让"增长中的那条"不被每个 delta 整条重解析（设计 §2.5）。
    ///
    /// `now` **必须由调用方注入** —— 本模块不得调用 `Instant::now()`，否则节流无法确定性单测。
    ///
    /// **借用纪律（设计 §2.3）**：返回的切片生命周期**绑在调用方持有的 `RefMut` 守卫上** ——
    /// 不得带出守卫作用域。唯一正确写法是在守卫内 clone 进目标：
    /// ```ignore
    /// { let mut c = app.md_cache.borrow_mut();
    ///   out.extend(c.lines_for(i, s, w, styles, now).iter().cloned()); }
    /// ```
    /// 缓存省的是**解析**、不是拷贝；每次命中仍会 clone 一遍。**不得重入** `borrow_mut()`。
    pub fn lines_for(
        &mut self,
        idx: usize,
        src: &str,
        width: usize,
        styles: AssistantStyles,
        now: Instant,
    ) -> &[Line<'static>] {
        if self.slots.len() <= idx {
            self.slots.resize_with(idx + 1, || None);
        }
        let oversized = src.len() > MAX_CACHED_SRC;
        let hit = self.slots[idx].as_ref().is_some_and(|slot| {
            !oversized
                && slot.width == width
                && !slot.forced
                && (slot.len == src.len()
                    // 键未变（内容相同）⇒ 永久命中，与时刻无关。
                    || (src.len() > slot.len
                        // 正在增长：键（len）每 delta 都变，整条重解析收益为零 ——
                        // 节流窗口内沿用上次产物（设计 §2.5）。到点则落到下面重算。
                        && now.saturating_duration_since(slot.rendered_at) < RENDER_THROTTLE))
        });
        if oversized {
            // 超长单条**不入缓存**：渲染进 scratch 并返回，不写 slot（内存意图，设计 §2.5）。
            // 不写 slot ⇒ 每次取用都重算（`hit` 已因 oversized 恒 false）。非重入 `borrow_mut`。
            self.scratch = md::render(src, width, styles);
            self.streaming = Some(idx);
            return &self.scratch;
        }
        if !hit {
            // 「长度相等即内容相同」的前提：同宽度下源文本只增不减。
            debug_assert!(
                self.slots[idx]
                    .as_ref()
                    .is_none_or(|slot| slot.width != width || src.len() >= slot.len),
                "assistant 源文本在尾部增长的前提下不应缩短（改历史须升级为内容哈希）",
            );
            let lines = md::render(src, width, styles);
            self.slots[idx] = Some(Slot {
                len: src.len(),
                width,
                lines,
                rendered_at: now,
                forced: false,
            });
        }
        self.streaming = Some(idx);
        self.slots[idx]
            .as_ref()
            .expect("slot 已在上方填充")
            .lines
            .as_slice()
    }

    /// 该槽位当前是否**空**（未缓存）—— 供用例钉住「超长单条没进缓存」。
    #[cfg(test)]
    fn slot_is_none(&self, idx: usize) -> bool {
        self.slots.get(idx).is_none_or(Option::is_none)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_all(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn flat_len(lines: &[Line<'static>]) -> usize {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.len()).sum::<usize>())
            .sum()
    }

    /// 纯 memo：同一输入重算得到**逐字节相同**的产物（清空/重算只损失 CPU，不改输出）。
    #[test]
    fn pure_memo_rerender_is_identical() {
        let mut cache = RenderCache::default();
        let base = Instant::now();
        let first: Vec<Line<'static>> = cache
            .lines_for(0, "# T\n\nbody **x**", 60, AssistantStyles, base)
            .to_vec();
        cache.finish(0);
        let second: Vec<Line<'static>> = cache
            .lines_for(0, "# T\n\nbody **x**", 60, AssistantStyles, base)
            .to_vec();
        assert_eq!(first, second, "重算产物应与首次逐字节一致");
        assert_eq!(flat_all(&first), flat_all(&second));
    }

    /// 节流窗口内同键复用（不重算）。
    #[test]
    fn same_key_within_window_reuses() {
        md::reset_render_calls();
        let mut cache = RenderCache::default();
        let base = Instant::now();
        cache.lines_for(0, "hello world", 60, AssistantStyles, base);
        cache.lines_for(
            0,
            "hello world",
            60,
            AssistantStyles,
            base + Duration::from_millis(50),
        );
        assert_eq!(md::render_calls(), 1, "窗口内应复用、不重算");
    }

    /// 键的**长度**要素独立失效：同宽度、同内容长度 ⇒ 永久命中；长度一变（且过了节流窗口）
    /// ⇒ 必重算。（节流窗口内增长沿用上次产物，故这里把时刻推到窗口之外以隔离「长度」因素。）
    #[test]
    fn len_change_forces_rerender_after_window() {
        md::reset_render_calls();
        let mut cache = RenderCache::default();
        let base = Instant::now();
        let later = base + Duration::from_millis(200); // 已过 RENDER_THROTTLE 窗口
        cache.lines_for(0, "aaaa", 60, AssistantStyles, base);
        // 同一时刻、同一长度：永久命中，不重算
        cache.lines_for(0, "aaaa", 60, AssistantStyles, later);
        assert_eq!(md::render_calls(), 1, "长度相同应永久命中");
        // 长度 +1：键变 ⇒ 重算
        let out = cache
            .lines_for(0, "aaaaa", 60, AssistantStyles, later)
            .to_vec();
        assert_eq!(md::render_calls(), 2, "长度变化应重算");
        assert_eq!(flat_all(&out), "aaaaa");
    }

    /// 键的**宽度**要素独立失效（M2 的对应用例）：即便长度与时刻都不变，宽度变化也必须重算。
    #[test]
    fn width_change_forces_rerender() {
        md::reset_render_calls();
        let mut cache = RenderCache::default();
        let base = Instant::now();
        cache.lines_for(0, "abcd", 40, AssistantStyles, base);
        cache.lines_for(0, "abcd", 80, AssistantStyles, base); // 同一时刻、同长度，仅宽度不同
        assert_eq!(md::render_calls(), 2, "宽度变化应重算（键必须含 width）");
    }

    /// 键的**槽下标**要素：不同槽位互不干扰。
    #[test]
    fn different_idx_are_independent_slots() {
        md::reset_render_calls();
        let mut cache = RenderCache::default();
        cache.sync_len(2);
        let base = Instant::now();
        cache.lines_for(0, "one", 60, AssistantStyles, base);
        cache.lines_for(1, "two", 60, AssistantStyles, base);
        assert_eq!(md::render_calls(), 2);
        // 再取回 idx 0/1，窗口内均命中
        assert_eq!(
            flat_all(cache.lines_for(0, "one", 60, AssistantStyles, base)),
            "one"
        );
        assert_eq!(
            flat_all(cache.lines_for(1, "two", 60, AssistantStyles, base)),
            "two"
        );
        assert_eq!(md::render_calls(), 2);
    }

    /// `sync_len` 增长 ⇒ 上一条被标记结束，下次取用忽略节流、必算。
    #[test]
    fn sync_len_growth_finishes_previous_line() {
        md::reset_render_calls();
        let mut cache = RenderCache::default();
        cache.sync_len(1);
        let base = Instant::now();
        cache.lines_for(0, "grow", 60, AssistantStyles, base); // streaming = 0
        assert_eq!(md::render_calls(), 1);
        // 下一条出现（transcript 增长）→ 上一条结束
        cache.sync_len(2);
        cache.lines_for(
            0,
            "grow",
            60,
            AssistantStyles,
            base + Duration::from_millis(1),
        );
        assert_eq!(md::render_calls(), 2, "上一条结束后必须重算一次");
    }

    /// 显式 `finish` 亦强制重算。
    #[test]
    fn explicit_finish_forces_rerender() {
        md::reset_render_calls();
        let mut cache = RenderCache::default();
        let base = Instant::now();
        cache.lines_for(0, "fin", 60, AssistantStyles, base);
        cache.finish(0);
        cache.lines_for(0, "fin", 60, AssistantStyles, base); // 同刻同键，但已 forced
        assert_eq!(md::render_calls(), 2);
    }

    /// 单条 > 256 KiB 不入缓存：同键同刻也每次重算，且**槽位保持为空**（真的没进缓存）。
    #[test]
    fn oversized_src_is_not_cached() {
        md::reset_render_calls();
        let mut cache = RenderCache::default();
        let base = Instant::now();
        let big = "a".repeat(MAX_CACHED_SRC + 1);
        let first = cache.lines_for(0, &big, 60, AssistantStyles, base).to_vec();
        assert!(!first.is_empty(), "退化路径仍应产出内容");
        assert!(cache.slot_is_none(0), "超长单条不得写入缓存槽");
        cache.lines_for(0, &big, 60, AssistantStyles, base);
        assert_eq!(md::render_calls(), 2, "超长单条应每帧现算");
        assert!(cache.slot_is_none(0), "第二次取用后槽位仍应为空");
    }

    /// N7：1 秒内 1000 次 delta（src 持续增长）⇒ 渲染 ≤ 11 次，且结束时必有一次完整渲染。
    #[test]
    fn n7_streaming_throttle_bounds_calls_and_finishes_complete() {
        md::reset_render_calls();
        let mut cache = RenderCache::default();
        cache.sync_len(1);
        let base = Instant::now();
        let mut src = String::new();
        for i in 0..1000usize {
            src.push('x');
            let now = base + Duration::from_millis(i as u64); // t = 0..=999 ms
            cache.lines_for(0, &src, 40, AssistantStyles, now);
        }
        let in_loop = md::render_calls();
        assert!(in_loop <= 10, "1 秒窗口内渲染次数应 ≤ 10（实际 {in_loop}）");
        // 该条结束（例如紧接着追加下一条 / turn 结算）→ 强制完整渲染
        cache.finish(0);
        let final_now = base + Duration::from_millis(1000);
        let lines = cache.lines_for(0, &src, 40, AssistantStyles, final_now);
        let total = md::render_calls();
        assert!(total <= 11, "总渲染次数应 ≤ 11（实际 {total}）");
        assert_eq!(
            flat_len(lines),
            1000,
            "结束时必须完整渲染到最后一段增量（否则尾部增量永久不显示）"
        );
    }
}
