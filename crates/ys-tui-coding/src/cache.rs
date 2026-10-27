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
//!
//! **回放（`/resume`）与 `/clear` 都会用更短的内容替换 transcript**（而非追加），
//! 两者都打破「只增不减」这条前提 ⇒ 调用方**都必须显式 [`RenderCache::reset`]**
//! （两者共用 `App::reset_for_replay` 这一个清屏入口）。
//! `/new` **不触**重置（它只换 session 句柄、不清 transcript）；`/resume` 与 `/clear` **触**。
//!
//! ⚠️ **不能指望 `sync_len` 自动兜住**：[`RenderCache::sync_len`] 的「缩短」分支只在长度
//! **变小**时走到。若调用方先 clear、再立刻 extend 到**相同长度**，长度看起来没变，会走
//! 「增长」分支而**绕过**缩短 ⇒ 等长内容假命中（旧槽位的 `len`/`width` 与新内容恰好相同即
//! 直接复用旧产物）。故必须显式 `reset()`。
//!
//! # 主题不在键里
//!
//! 槽位存的是**已上色**的 `Line`，而键只有 `(len, width)` —— 换主题若不作废，屏上就是旧配色。
//! 故换主题必须走显式的 [`RenderCache::set_theme`]（比值 ⇒ 色板变了才全量作废），
//! **不把主题塞进键**：逐槽记一份色板等于给每个槽位多一份状态，而切主题是低频操作，
//! 全量作废的代价可接受。

use std::time::{Duration, Instant};

use ratatui::text::Line;

use crate::md;
use crate::styles::AssistantStyles;
use crate::theme::{Palette, ThemeKind};

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
/// 节流状态**存在本结构内**（它记住每条上次算出什么、何时算的），令布局同步
/// （[`crate::layout`]）与画面读到**同一份**上次产物 —— 否则滚动按新内容、画面按旧内容，
/// 二者错位（设计 §2.5 边界 ①，风险 R11）。
pub(crate) struct RenderCache {
    slots: Vec<Option<Slot>>,
    /// 最近一次被 `lines_for` 请求的槽位 —— `sync_len` 增长时据此把「上一条」标记为结束。
    streaming: Option<usize>,
    /// 超长单条（`> MAX_CACHED_SRC`）的**暂存产物**：只服务退化路径，**不写 slot**（内存意图：
    /// 超长单条真的不入缓存，设计 §2.5）。每次退化渲染覆写它。
    scratch: Vec<Line<'static>>,
    /// 当前色板。**只由 [`RenderCache::set_theme`] 改**，且不参与命中判定（键仍是
    /// `(len, width)`）—— 它唯一的用途是「换主题 ⇒ 全量作废」的比值判据。
    // 消费方是 `App::set_theme`（`app.rs`）→ `RenderCache::set_theme`，两者同步落地；
    // 在接线前这里只有本模块的用例读它。与 `SyncResult::recomputed_entries` 同一处置。
    #[allow(dead_code)]
    theme: Palette,
}

/// 手写而非 `derive`：[`Palette`] 刻意不实现 `Default`（无主题的色板没有合理默认值），
/// 故这里显式取 [`ThemeKind::default`] 的色板作为初始状态。
impl Default for RenderCache {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            streaming: None,
            scratch: Vec::new(),
            theme: ThemeKind::default().palette(),
        }
    }
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

    /// 清空全部槽位 —— **回放（`/resume`）专用**：transcript 将被整体替换，任何按「槽下标」
    /// 记忆的产物都已失效（见模块头的不变量与等长碰撞陷阱）。
    ///
    /// `streaming` 一并归零，避免它继续指向已被丢弃的槽位。`scratch` **不清**：它只服务超长
    /// 单条的退化路径，每次退化渲染都**整体覆写**它，且不参与任何命中判定 —— 留着既不污染
    /// 输出、也不会造成误命中。
    ///
    /// 消费方是 app 侧的 `reset_for_replay`（`/resume`）。
    pub fn reset(&mut self) {
        self.slots.clear();
        self.streaming = None;
    }

    /// 换主题 ⇒ 所有渲染产物作废（槽位里存的是**已上色**的行，而键不含主题）。
    ///
    /// **色板未变时直接返回**：主题浮层逐键预览会反复调用它，同值重入不该白清一屏产物。
    ///
    /// 与 [`Self::reset`] 的区别只有一条：本函数记住新色板，`reset()` 不改它（`/resume`
    /// 的回放清屏与主题无关，两者语义不得混）。
    // 消费方是 `App::set_theme`（`app.rs`，唯一失效入口）。
    #[allow(dead_code)]
    pub fn set_theme(&mut self, palette: Palette) {
        if palette == self.theme {
            return;
        }
        self.theme = palette;
        self.reset();
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
    use crate::theme::ThemeKind;

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
            .lines_for(
                0,
                "# T\n\nbody **x**",
                60,
                AssistantStyles::new(ThemeKind::default().palette()),
                base,
            )
            .to_vec();
        cache.finish(0);
        let second: Vec<Line<'static>> = cache
            .lines_for(
                0,
                "# T\n\nbody **x**",
                60,
                AssistantStyles::new(ThemeKind::default().palette()),
                base,
            )
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
        cache.lines_for(
            0,
            "hello world",
            60,
            AssistantStyles::new(ThemeKind::default().palette()),
            base,
        );
        cache.lines_for(
            0,
            "hello world",
            60,
            AssistantStyles::new(ThemeKind::default().palette()),
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
        cache.lines_for(
            0,
            "aaaa",
            60,
            AssistantStyles::new(ThemeKind::default().palette()),
            base,
        );
        // 同一时刻、同一长度：永久命中，不重算
        cache.lines_for(
            0,
            "aaaa",
            60,
            AssistantStyles::new(ThemeKind::default().palette()),
            later,
        );
        assert_eq!(md::render_calls(), 1, "长度相同应永久命中");
        // 长度 +1：键变 ⇒ 重算
        let out = cache
            .lines_for(
                0,
                "aaaaa",
                60,
                AssistantStyles::new(ThemeKind::default().palette()),
                later,
            )
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
        cache.lines_for(
            0,
            "abcd",
            40,
            AssistantStyles::new(ThemeKind::default().palette()),
            base,
        );
        cache.lines_for(
            0,
            "abcd",
            80,
            AssistantStyles::new(ThemeKind::default().palette()),
            base,
        ); // 同一时刻、同长度，仅宽度不同
        assert_eq!(md::render_calls(), 2, "宽度变化应重算（键必须含 width）");
    }

    /// 键的**槽下标**要素：不同槽位互不干扰。
    #[test]
    fn different_idx_are_independent_slots() {
        md::reset_render_calls();
        let mut cache = RenderCache::default();
        cache.sync_len(2);
        let base = Instant::now();
        cache.lines_for(
            0,
            "one",
            60,
            AssistantStyles::new(ThemeKind::default().palette()),
            base,
        );
        cache.lines_for(
            1,
            "two",
            60,
            AssistantStyles::new(ThemeKind::default().palette()),
            base,
        );
        assert_eq!(md::render_calls(), 2);
        // 再取回 idx 0/1，窗口内均命中
        assert_eq!(
            flat_all(cache.lines_for(
                0,
                "one",
                60,
                AssistantStyles::new(ThemeKind::default().palette()),
                base
            )),
            "one"
        );
        assert_eq!(
            flat_all(cache.lines_for(
                1,
                "two",
                60,
                AssistantStyles::new(ThemeKind::default().palette()),
                base
            )),
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
        cache.lines_for(
            0,
            "grow",
            60,
            AssistantStyles::new(ThemeKind::default().palette()),
            base,
        ); // streaming = 0
        assert_eq!(md::render_calls(), 1);
        // 下一条出现（transcript 增长）→ 上一条结束
        cache.sync_len(2);
        cache.lines_for(
            0,
            "grow",
            60,
            AssistantStyles::new(ThemeKind::default().palette()),
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
        cache.lines_for(
            0,
            "fin",
            60,
            AssistantStyles::new(ThemeKind::default().palette()),
            base,
        );
        cache.finish(0);
        cache.lines_for(
            0,
            "fin",
            60,
            AssistantStyles::new(ThemeKind::default().palette()),
            base,
        ); // 同刻同键，但已 forced
        assert_eq!(md::render_calls(), 2);
    }

    /// **N6（最易假绿）**：回放用等长内容**替换** transcript 时，不得复用旧槽位的产物。
    ///
    /// 构造两条**字节长度相同、width 也相同**的 assistant 文本（`"AAAA"` / `"BBBB"`）：
    /// 键 = `(槽下标, src.len(), width)` 三者全同，若不清槽位就会命中 A 的产物。
    /// 又因 `reset()` 后立刻渲染到**相同长度**，`sync_len` 的「缩短」分支不会走到（长度没变小）
    /// —— 这正是「不能指望 `sync_len` 自动兜住、必须显式 `reset()`」的原因。
    ///
    /// **变异自检**：把 `reset()` 里的 `self.slots.clear();` 注释掉 ⇒ 本用例必须**红**
    /// （`lines_for` 得到 `"AAAA"`；且渲染调用数停在 1）。
    #[test]
    fn n6_reset_prevents_equal_length_collision() {
        md::reset_render_calls();
        let mut cache = RenderCache::default();
        cache.sync_len(1);
        let base = Instant::now();

        let a = cache
            .lines_for(
                0,
                "AAAA",
                60,
                AssistantStyles::new(ThemeKind::default().palette()),
                base,
            )
            .to_vec();
        assert_eq!(flat_all(&a), "AAAA", "先填充 A 的产物");
        assert_eq!(md::render_calls(), 1);

        // 回放：清屏后立刻 extend 到**相同长度** —— 长度没变，键看起来「没变」。
        cache.reset();
        let b = cache
            .lines_for(
                0,
                "BBBB",
                60,
                AssistantStyles::new(ThemeKind::default().palette()),
                base,
            )
            .to_vec();

        assert_eq!(flat_all(&b), "BBBB", "回放后必须重渲染，不得复用等长旧产物");
        assert!(
            !flat_all(&b).contains("AAAA"),
            "屏上不得残留 A 的文本：{:?}",
            flat_all(&b)
        );
        assert_eq!(md::render_calls(), 2, "reset 后即便同键也必须重算");
    }

    /// 单条 > 256 KiB 不入缓存：同键同刻也每次重算，且**槽位保持为空**（真的没进缓存）。
    #[test]
    fn oversized_src_is_not_cached() {
        md::reset_render_calls();
        let mut cache = RenderCache::default();
        let base = Instant::now();
        let big = "a".repeat(MAX_CACHED_SRC + 1);
        let first = cache
            .lines_for(
                0,
                &big,
                60,
                AssistantStyles::new(ThemeKind::default().palette()),
                base,
            )
            .to_vec();
        assert!(!first.is_empty(), "退化路径仍应产出内容");
        assert!(cache.slot_is_none(0), "超长单条不得写入缓存槽");
        cache.lines_for(
            0,
            &big,
            60,
            AssistantStyles::new(ThemeKind::default().palette()),
            base,
        );
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
            cache.lines_for(
                0,
                &src,
                40,
                AssistantStyles::new(ThemeKind::default().palette()),
                now,
            );
        }
        let in_loop = md::render_calls();
        assert!(in_loop <= 10, "1 秒窗口内渲染次数应 ≤ 10（实际 {in_loop}）");
        // 该条结束（例如紧接着追加下一条 / turn 结算）→ 强制完整渲染
        cache.finish(0);
        let final_now = base + Duration::from_millis(1000);
        let lines = cache.lines_for(
            0,
            &src,
            40,
            AssistantStyles::new(ThemeKind::default().palette()),
            final_now,
        );
        let total = md::render_calls();
        assert!(total <= 11, "总渲染次数应 ≤ 11（实际 {total}）");
        assert_eq!(
            flat_len(lines),
            1000,
            "结束时必须完整渲染到最后一段增量（否则尾部增量永久不显示）"
        );
    }

    /// **N13 首帧基准（`#[ignore]`）**：量 `/resume` **新增**的那笔成本 —— `reset()` 之后
    /// **首帧**把整段历史全部重渲染一遍（每条 assistant 重解析 markdown）。先渲染一轮把槽位
    /// 占上再 `reset()`，复刻「已有旧会话 → 回放替换」的真实产品路径（空 cache 上 `reset()`
    /// 是 no-op，量不到等长碰撞面）。
    ///
    /// 只量首帧，**不**量稳态每帧的 `line_count` —— 后者是任何长会话今天就在付的既有成本，
    /// 与本改动无关（设计 I7）。断言不做硬失败，仅 `eprintln!` 出耗时供人读；100 ms 是给人看的
    /// 参照阈值（设计 I7）。
    ///
    /// 之所以是本模块内的**内联**单测而非 `tests/replay_bench.rs`：`RenderCache` 是
    /// `pub(crate)`，集成测试文件访问不到。
    ///
    /// 运行：`cargo test -p ys-tui-coding --release n13_first_frame_baseline -- --ignored --nocapture`
    #[test]
    #[ignore = "基准，需 --release --ignored --nocapture 手动读耗时"]
    fn n13_first_frame_baseline() {
        const N: usize = 500;
        // 每条约 2 KiB 的 assistant 正文（含标题/列表/代码块，尽量贴近真实 markdown 结构）。
        let para = "这是一段用于基准测试的正文，包含中英文混排与 `inline code` 片段。\n\n";
        let docs: Vec<String> = (0..N)
            .map(|i| {
                let mut s = format!("# 条目 {i}\n\n");
                while s.len() < 2000 {
                    s.push_str(para);
                }
                s.push_str("```rust\nfn main() { println!(\"hi\"); }\n```\n");
                s
            })
            .collect();
        let total_bytes: usize = docs.iter().map(|s| s.len()).sum();

        // 复刻 `/resume` 的**真实**失效路径：先有一屏旧产物（槽位已填），再 `reset()` 整体
        // 失效，然后量重渲首帧。空 cache 上 `reset()` 是 no-op，量不到「等长旧槽位」这一
        // 真正会走的碰撞面（cache.rs 模块头 / I6）。
        let mut cache = RenderCache::default();
        let warm = Instant::now();
        for (i, src) in docs.iter().enumerate() {
            let _ = cache.lines_for(
                i,
                src,
                80,
                AssistantStyles::new(ThemeKind::default().palette()),
                warm,
            );
        }
        cache.reset();

        let start = Instant::now();
        for (i, src) in docs.iter().enumerate() {
            let _ = cache.lines_for(
                i,
                src,
                80,
                AssistantStyles::new(ThemeKind::default().palette()),
                start,
            );
        }
        let elapsed = start.elapsed();

        eprintln!(
            "N13 首帧基准：{N} 条 × ~2 KiB（共 {total_bytes} 字节），reset() 后首帧全量渲染耗时 = {elapsed:?}（参照阈值 100 ms）"
        );
        assert_eq!(docs.len(), N, "基准规模自检（不参与性能断言）");
    }

    /// 产物里出现的**前景色集合**（去重后按出现顺序排）—— 比「产物不同」更精确：
    /// 换主题必须真的换成另一套颜色，而不是仅仅多算一次。
    fn fg_colors(lines: &[Line<'static>]) -> Vec<ratatui::style::Color> {
        let mut out = Vec::new();
        for line in lines {
            for color in line
                .spans
                .iter()
                .filter_map(|s| s.style.fg)
                .chain(line.style.fg)
            {
                if !out.contains(&color) {
                    out.push(color);
                }
            }
        }
        out
    }

    /// **主题进失效判据（缓存层）**：换主题后即便 `(槽下标, src.len(), width)` 全同，
    /// 也不得复用旧槽位的产物 —— 槽里存的是**已上色**的行，键里却没有主题。
    ///
    /// **变异自检**：把 `set_theme` 改成空操作（或删掉它的 `self.reset()`）⇒ 本用例必须**红**
    /// （第二次取到的是 A 的旧产物：颜色集合不变、渲染调用数仍为 1）。
    #[test]
    fn set_theme_invalidates_rendered_slots() {
        md::reset_render_calls();
        let mut cache = RenderCache::default();
        cache.sync_len(1);
        let base = Instant::now();
        let src = "# 标题\n\n正文 `code` **粗体**";
        let width = 60usize;
        let a = ThemeKind::default();
        let b = ThemeKind::Nord;
        assert_ne!(a.palette(), b.palette(), "前置：两套色板必须不同");

        let first = cache
            .lines_for(0, src, width, AssistantStyles::new(a.palette()), base)
            .to_vec();
        assert_eq!(md::render_calls(), 1, "前置：首次渲染");
        let first_fg = fg_colors(&first);
        assert!(!first_fg.is_empty(), "前置：产物必须带前景色");

        // 换主题：同一条、同宽度、同长度 —— 键三者全同，只有主题变了。
        cache.set_theme(b.palette());
        let second = cache
            .lines_for(0, src, width, AssistantStyles::new(b.palette()), base)
            .to_vec();

        assert_eq!(md::render_calls(), 2, "换主题后同键也必须重算");
        assert_ne!(first, second, "换主题后产物不得是旧的");
        let second_fg = fg_colors(&second);
        assert_ne!(
            first_fg, second_fg,
            "换主题后前景色集合必须变（{first_fg:?} → {second_fg:?}）"
        );
        // 新产物只用 B 的槽位色 —— 钉住「真的换成了 B」而不只是「换了一份色」。
        let palette_b = b.palette();
        let slots_b = [
            palette_b.fg,
            palette_b.accent,
            palette_b.code,
            palette_b.muted,
            palette_b.user,
            palette_b.ok,
            palette_b.warn,
            palette_b.fail,
            palette_b.info,
        ];
        for color in &second_fg {
            assert!(
                slots_b.contains(color),
                "B 主题的产物只能使用 B 的槽位色，出现 {color:?}"
            );
        }
    }

    /// **同值 `set_theme` 不白清缓存**：主题浮层逐键预览会反复调用它，色板没变时
    /// 不得把已渲染产物丢掉重算。
    ///
    /// **变异自检**：删掉 `set_theme` 开头那句 `if palette == self.theme { return; }`
    /// ⇒ 本用例必须**红**（渲染调用数会涨到 2）。
    #[test]
    fn set_theme_with_same_palette_keeps_cache() {
        md::reset_render_calls();
        let mut cache = RenderCache::default();
        cache.sync_len(1);
        let base = Instant::now();
        let src = "# 标题\n\n正文 `code`";
        let width = 60usize;
        let a = ThemeKind::default();

        let first = cache
            .lines_for(0, src, width, AssistantStyles::new(a.palette()), base)
            .to_vec();
        assert_eq!(md::render_calls(), 1, "前置：首次渲染");

        // 同值重入：色板没变 ⇒ 不清缓存。
        cache.set_theme(a.palette());
        let second = cache
            .lines_for(0, src, width, AssistantStyles::new(a.palette()), base)
            .to_vec();

        assert_eq!(md::render_calls(), 1, "同值 set_theme 不得作废缓存");
        assert_eq!(first, second, "同值 set_theme 后产物应逐字节相同");
    }
}
