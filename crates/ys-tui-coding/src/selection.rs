//! 文本选区：拖选状态机 + 视窗字形缓存（ADR-0051）。
//!
//! 本模块只负责「选中了哪一段」与「那一段在屏幕上长什么样」；把结果写进剪贴板、
//! 画高亮、驱动自动滚动分别由 `events.rs` / `draw.rs` / 本模块的自由函数承担。
//!
//! # 三条契约
//!
//! ## 1. 坐标是**全局视觉行**，不是屏幕行
//!
//! `anchor_row` / `cursor_row` 是 [`crate::layout::TranscriptLayout`] 的视觉行号空间
//! 里的行号（按下时 = `top_row` + 内区相对 y）。这带来两个直接后果：
//!
//! - 「拖到边缘自动滚动」（`drag_tick`）能选中**超过一屏**的内容 —— 屏幕只有一屏，
//!   但行号空间没有这个上限；
//! - `anchor_row` 是**逻辑坐标**，不随 `scroll_offset` 改变。滚动只改「哪一行画在
//!   屏幕第 0 行」（`top_row`）与 `cursor_row`，按下点始终指向同一行内容。
//!
//! 因此 `x` 是屏幕列（`u16`），而 `row` 是全局行（`usize`）—— 两者量纲不同，不可互换。
//!
//! ## 2. 字形从 `Buffer` 读回，不自己算
//!
//! [`GlyphGrid`] 缓存的是「用 `Paragraph` 渲染进 `Buffer` 之后」的真实字形。屏幕字形
//! 是唯一真源：折行位置、宽字符续格（CJK / 全角标点 / emoji 的第二格留空）都由 ratatui
//! 决定，自己按 `unicode-width` 复算会与之漂移 —— 漂移的表现是「复制的文字多一个空格
//! 或少一个字」，而高亮看起来完全正常。
//!
//! ## 3. 槽环的等价性前提
//!
//! 字形缓存用「槽环」表示滚动：`order` 把屏幕行映射到槽位，滚动 k 行 = `order` 旋转 k
//! 位 + 只重建换进来的那 k 行，不搬 cell。该做法成立的**唯一前提**是
//! 「逐行渲染 == 整窗渲染」：滚动 k 行后的画面，必须与「整窗重渲后上移 k 行」逐格相同。
//! 这一条由针对该等价性的用例（V2）守住 —— 若哪天 `Paragraph` 的渲染变成依赖上下文的，
//! 槽环会静默错位，只有那条用例能发现。

use std::time::{Duration, Instant};

use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::app::App;
use crate::layout::{RowRef, TranscriptLayout, Window};

/// 一次拖选的生命周期。
///
/// **两态**：没有 `Done`。松手复制后立即 [`Selection::clear`] 回 `Idle`（高亮随之消失，
/// 文字已在剪贴板），故 `Done` 没有观测点。
///
/// 消费方是 `events.rs` 的鼠标分支与 `draw.rs` 的高亮层。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Phase {
    /// 无选区：初始 / 已清 / resize / 用户主动滚动。
    #[default]
    Idle,
    /// 左键已按下（可能尚未移动 ⇒ 两端点相等，松手不复制）。
    Dragging,
}

/// 贴边方向：`1` = 上沿（要看**更早**的行，对应 `scroll` 的 `delta > 0`）；
/// `-1` = 下沿（要看**更新**的行）；`0` = 未贴边。
///
/// 消费方是 `events.rs` 的鼠标分支（写入）与 [`Selection::edge`]（读出）。
type Edge = i8;

#[derive(Debug, Default)]
pub(crate) struct Selection {
    /// 按下点：全局视觉行（`top_row` + 内区相对 y）。**永不因滚动而变**。
    anchor_row: usize,
    /// 按下点的列（屏幕列，相对内区左沿）。
    anchor_x: u16,
    /// 当前点：与 `anchor_*` 同一口径。自动滚动**只改这里**。
    cursor_row: usize,
    /// 当前点的列。
    cursor_x: u16,
    phase: Phase,
    edge: Edge,
    /// 贴边开始时刻 —— `step = clamp(1 + held_ms / 40, 1, MAX_STEP)` 的输入。
    /// 只在**贴边方向变更**时重置，故按住不动会持续加速。
    edge_since: Option<Instant>,
    /// 视窗字形缓存。消费方是 `draw.rs` 的高亮层与 `selection::extract`。
    pub(crate) grid: GlyphGrid,
    /// 高亮矩形复用缓冲：容量复用 ⇒ 稳态帧零分配。消费方是 `draw.rs` 的高亮层。
    rects: Vec<Rect>,
}

/// 选择状态机：按下 / 拖动 / 清理 / 高亮矩形。
///
/// 消费方是 `events.rs` 的鼠标分支、`draw.rs` 的高亮层与 `run.rs` 的自动滚动触发。
impl Selection {
    /// 左键是否仍按着。
    pub(crate) fn is_dragging(&self) -> bool {
        self.phase == Phase::Dragging
    }

    /// 是否存在（或正在进行）选区。
    ///
    /// 当前两态下与 [`Self::is_dragging`] 等价；两个名字都留，是因为调用点语义不同
    /// （「要不要继续自动滚动」问前者，「要不要画高亮 / 清理」问后者）。
    pub(crate) fn is_active(&self) -> bool {
        self.phase != Phase::Idle
    }

    /// 当前贴边方向（`1` 上沿 / `-1` 下沿 / `0` 未贴边）。
    pub(crate) fn edge(&self) -> i8 {
        self.edge
    }

    /// 贴边已持续多久；未贴边 ⇒ [`Duration::ZERO`]。
    pub(crate) fn edge_held(&self, now: Instant) -> Duration {
        match self.edge_since {
            Some(since) => now.saturating_duration_since(since),
            None => Duration::ZERO,
        }
    }

    /// 两端点 `(anchor, cursor)` 的 **Copy 值**，**不**做 min/max 归一化。
    ///
    /// 返回裸值而非 `&self` 的借用，是为了让调用方能同时持 `&mut app` 改写其它字段；
    /// 归一化留给真正需要方向的消费方（提取按「行号小的那端」定首行区间）。
    pub(crate) fn bounds(&self) -> ((usize, u16), (usize, u16)) {
        (
            (self.anchor_row, self.anchor_x),
            (self.cursor_row, self.cursor_x),
        )
    }

    /// 按下：两端点重合、不贴边、计时清零。
    pub(crate) fn begin(&mut self, row: usize, x: u16, now: Instant) {
        let _ = now;
        self.anchor_row = row;
        self.cursor_row = row;
        self.anchor_x = x;
        self.cursor_x = x;
        self.phase = Phase::Dragging;
        self.edge = 0;
        self.edge_since = None;
    }

    /// 拖动：移动当前点，并在**贴边方向变更**时重置计时。
    ///
    /// 不重置的规则是自动滚动加速的前提：按住不动时每一拍都会 `extend` 同一个方向，
    /// 若每次都重置 `edge_since`，`edge_held` 恒为 0，速度永远停在第一档。
    pub(crate) fn extend(&mut self, row: usize, x: u16, edge: i8, now: Instant) {
        self.cursor_row = row;
        self.cursor_x = x;
        if edge == 0 {
            self.edge_since = None;
        } else if edge != self.edge {
            self.edge_since = Some(now);
        }
        self.edge = edge;
    }

    /// 清选区：连字形缓存与矩形缓冲一起回到初始态。
    ///
    /// 丢弃 `rects` 的容量是可接受的 —— 清选区是低频动作，而正确性优先于省一次分配。
    pub(crate) fn clear(&mut self) {
        *self = Selection::default();
    }

    /// 让字形缓存跟上当前帧；无选区时**释放**它（设计 §2.3 第 9 条）。
    ///
    /// 无选区时不单不重建，还要把一屏字形还回去（`GlyphGrid` 与 `layout` 各存一份，
    /// 视窗期约 120 KB）；`release` 保留 `Vec` 容量，下次拖选不必重分配。
    pub(crate) fn ensure_grid(&mut self, layout: &TranscriptLayout, inner: Rect, top_row: usize) {
        if self.is_active() {
            self.grid.ensure(layout, inner, top_row);
        } else {
            self.grid.release();
        }
    }

    /// 把两端点转成**内区局部**矩形（复用缓冲 ⇒ 稳态帧零分配）。
    ///
    /// 调用顺序：必须先 [`Self::ensure_grid`] 再调本函数 —— 宽字符边界要问
    /// [`GlyphGrid::head_col`]，而它只有缓存就位后才准。
    ///
    /// 端点落在宽字符的续格上时，矩形边界**收拢到该字符首格**（右端额外 +1 使整字宽
    /// 入内）：否则高亮只盖半个字，看得见的剩余半字与选中范围不一致。
    pub(crate) fn rects(&mut self, top_row: usize, inner: Rect) -> &[Rect] {
        self.rects.clear();
        if !self.is_active() || inner.width == 0 || inner.height == 0 {
            return &self.rects;
        }
        let (first, last) = self.ordered_endpoints();
        // 两端点都要夹进内区：拖出窗外时端点会超出（行为上是「夹到边界」，设计 §5）。
        let clamp_row =
            |row: usize| -> usize { row.clamp(top_row, top_row + inner.height as usize - 1) };
        let last_x = inner.width - 1;
        let (r_lo, x_lo) = (clamp_row(first.0), first.1.min(last_x));
        let (r_hi, x_hi) = (clamp_row(last.0), last.1.min(last_x));
        let r_lo = r_lo.min(r_hi);
        let (x_lo, x_hi) = if r_lo == r_hi {
            (x_lo.min(x_hi), x_lo.max(x_hi))
        } else {
            (x_lo, x_hi)
        };
        if r_lo == r_hi {
            let y = (r_lo - top_row) as u16;
            let lo = self.grid.head_col(y, x_lo);
            let hi = self.wide_aware_right(y, x_hi, last_x);
            if hi >= lo {
                self.rects.push(Rect::new(lo, y, hi - lo + 1, 1));
            }
            return &self.rects;
        }
        // 首行：从 `x_lo` 到行尾。
        let y = (r_lo - top_row) as u16;
        let lo = self.grid.head_col(y, x_lo);
        self.rects.push(Rect::new(lo, y, inner.width - lo, 1));
        // 中间行：满宽。
        if r_hi > r_lo + 1 {
            self.rects
                .push(Rect::new(0, y + 1, inner.width, (r_hi - r_lo - 1) as u16));
        }
        // 末行：从 0 到 `x_hi`。
        let y = (r_hi - top_row) as u16;
        let hi = self.wide_aware_right(y, x_hi, last_x);
        self.rects.push(Rect::new(0, y, hi + 1, 1));
        &self.rects
    }

    /// 右边界若落在宽字符续格上 ⇒ 退回首格、再 +1（使整字宽进入矩形）。
    fn wide_aware_right(&self, y: u16, x: u16, last_x: u16) -> u16 {
        let head = self.grid.head_col(y, x);
        if head != x {
            // 续格 ⇒ 首格左邻那个字宽 2，右端 = 首格 + 1。
            (head + 1).min(last_x)
        } else if self.grid.is_cont(y, x + 1) {
            (x + 1).min(last_x)
        } else {
            x
        }
    }

    /// 行号小的那端在前。
    fn ordered_endpoints(&self) -> ((usize, u16), (usize, u16)) {
        let (a, c) = self.bounds();
        if (a.0, a.1) <= (c.0, c.1) {
            (a, c)
        } else {
            (c, a)
        }
    }

    /// 贴边方向对应的**全局视觉行**（`drag_tick` 据此把 `cursor_row` 推到新边缘）。
    ///
    /// `edge > 0` ⇒ 视窗顶行；`edge < 0` ⇒ 视窗底行；未贴边 ⇒ `cursor_row` 不动。
    pub(crate) fn edge_row(&self, top_row: usize, inner_h: u16) -> usize {
        match self.edge {
            1 => top_row,
            -1 => top_row + inner_h.saturating_sub(1) as usize,
            _ => self.cursor_row,
        }
    }
}

/// 视窗字形缓存的键 —— 「属性全等 ⇒ 画面必然相同」的判据。
///
/// 五个字段合起来覆盖设计 §2.3 的失效集合：`layout_rev` 管结构（宽度 / thinking /
/// 回放替换），`win_rev` 管**窗口内**条目内容，`inner` 管几何，`base_row` 管滚动位置。
///
/// `skip` 是窗口首行内部要跳过的视觉行数。**恒 `0`**：[`GlyphGrid::ensure`] 要在取窗口
/// **之前**算出键（取了窗口才知道 `skip_rows`），故拿不到它；而它随 `base_row` 变化，
/// 已被 `base_row` 覆盖，不需要单独的区分能力。字段保留是为了键的形态固定（下游缓存
/// 按全字段比较），也为了槽环换实现时不必再动结构定义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct GlyphKey {
    /// `TranscriptLayout::structural_rev()`。
    layout_rev: u64,
    /// `TranscriptLayout::rev_alloc()` —— 全布局内容代次（某条目真的重渲才 +1）。
    ///
    /// **不是** `revision_range`：那个把窗口首条目内偏移折进了哈希，而滚动本身就会改这个
    /// 偏移 ⇒ 用它做键的话「纯滚动」永远被误判为内容变化，槽环永远不会生效。
    /// 代价是：窗口**外**条目重渲也会打红缓存（多一次全量重建）。这是有意的取舍——
    /// 正确性优先，且流式结束时本来就要一次全量。
    win_rev: u64,
    /// 内区几何（含 `x` / `y`：内区整体平移也会改变屏幕列的含义）。
    inner: Rect,
    /// 屏幕第 0 行对应的全局视觉行。
    base_row: usize,
    /// 见类型文档：恒 `0`。
    skip: u16,
}

/// 视窗字形缓存：屏幕内区每格的字形。
///
/// **只从 `Paragraph` 渲出来的 `Buffer` 读**（模块头契约 2）：折行位置与宽字符续格由
/// ratatui 决定，自己复算会漂移。
///
/// 滚动走**槽环**：[`Self::shift`] 旋转 `order` 并只重渲换进来的那几行；
/// 其余失效（结构 / 窗口内容 / 几何）走 [`Self::rebuild`] 整窗重渲。
///
/// 坐标口径：`y` / `x` 都是**内区相对**（`y` = 屏幕行、`x` = 屏幕列），与
/// [`Selection::bounds`] 的 `x` 同源。
///
/// 消费方是 `draw.rs` 的高亮层与 [`extract`]。
#[derive(Debug, Default)]
pub(crate) struct GlyphGrid {
    /// `Buffer` 的宽度 = 折行宽度（**必须**等于 `layout.width()`，否则折行不同 ⇒ 字形错位）。
    width: u16,
    /// `Buffer` 的高度 = 内区高度。
    height: u16,
    /// 屏幕第 0 行对应的全局视觉行 —— 槽环 `shift` 据它算位移 `k`。
    ///
    /// 与 `key.base_row` 恒等（同一个值写两处），但两者用途不同：`key` 整体参与
    /// 「键是否全等」，而位移只需要一个数，不必构造一次 `GlyphKey`。
    base_row: usize,
    /// `width × height` 的字形槽，`Paragraph` 直接渲进来。
    slots: Buffer,
    /// 屏幕行 → slot。`shift(k)` 旋转它，故滚动不搬 cell。
    order: Vec<u16>,
    /// 每 slot 一行的宽字符续格位图（`width` 位，按 64 位分字）。
    cont: Vec<u64>,
    /// 位图每行占几个 `u64`（`ceil(width / 64)`）。
    words_per_row: usize,
    key: GlyphKey,
    /// 全量重建次数 —— 只用于「稳态零重建」与「只变 `base_row` 走 `shift`」的断言。
    #[cfg(test)]
    rebuild_count: usize,
}

impl GlyphGrid {
    /// 每帧调用：让缓存与 `(layout, inner, top_row)` 对齐。
    ///
    /// **三档判定**，顺序不可换（设计 §2.3）：
    ///
    /// 1. 键全等 ⇒ **零动作**（画面必然相同）；
    /// 2. 只有 `base_row` 变（结构、窗口内容、几何全等）⇒ [`Self::shift`] **增量移行**；
    /// 3. 其余 ⇒ 全量 [`Self::rebuild`]。
    ///
    /// 先排掉结构/内容/几何变化再判位移：否则滚动（只改 `base_row`）会被当成内容变化
    /// 而每帧全量重建，「滚动」与「重渲」就绑在一起了。
    ///
    /// `inner.height == 0`（尺寸不足）或 `layout.width() == 0`（尚未同步过布局）⇒ 面积为空，
    /// 全部查询返回空白。
    pub(crate) fn ensure(&mut self, layout: &TranscriptLayout, inner: Rect, top_row: usize) {
        let key = GlyphKey {
            layout_rev: layout.structural_rev(),
            win_rev: layout.rev_alloc(),
            inner,
            base_row: top_row,
            skip: 0,
        };
        if key == self.key {
            return;
        }
        // 只有 `base_row` 不同 ⇒ 内容逐行上/下移，槽环旋转即可，不必重渲。
        // 结构（`layout_rev`）/ 窗口内容（`win_rev`）/ 几何（`inner`）任一变化都不能走这条。
        if self.key.layout_rev == key.layout_rev
            && self.key.win_rev == key.win_rev
            && self.key.inner == key.inner
            && self.width > 0
            && self.height > 0
        {
            let k = top_row as isize - self.base_row as isize;
            if k.unsigned_abs() >= self.height as usize {
                // 换进来的行覆盖整屏：`rotate` 会绕回来，把「几乎全新的画面」当成
                // 「原地不动」，故退化为全量重建。
                self.rebuild(layout, inner, top_row, key);
            } else {
                let (window, rows) = layout.window_with_rows(top_row, self.height as usize);
                self.shift(k, &window, &rows);
                self.key = key;
                self.base_row = top_row;
            }
        } else {
            self.rebuild(layout, inner, top_row, key);
        }
    }

    /// 屏幕第 `y` 行第 `x` 列的字形。越界返回 `" "`（不 panic —— 画高亮时 `x` 会探到行尾外）。
    pub(crate) fn symbol(&self, y: u16, x: u16) -> &str {
        match self.slot_of(y) {
            Some(slot) if x < self.width => self.slots[(x, slot)].symbol(),
            _ => " ",
        }
    }

    /// 屏幕第 `y` 行第 `x` 列是否是宽字符的**续格**（`Buffer` 里它就是一个空格）。
    pub(crate) fn is_cont(&self, y: u16, x: u16) -> bool {
        if x >= self.width || self.words_per_row == 0 {
            return false;
        }
        let Some(slot) = self.slot_of(y) else {
            return false;
        };
        let word = slot as usize * self.words_per_row + x as usize / 64;
        self.cont
            .get(word)
            .is_some_and(|word| word & (1u64 << (x % 64)) != 0)
    }

    /// 屏幕第 `y` 行第 `x` 列所属字符的**首格**列号（非续格 ⇒ 自身）。
    ///
    /// 终端的字符宽度不超过 2 列，故续格的首格必在左邻一格。
    pub(crate) fn head_col(&self, y: u16, x: u16) -> u16 {
        if self.is_cont(y, x) {
            x.saturating_sub(1)
        } else {
            x
        }
    }

    /// 屏幕行 → slot 行；越界 ⇒ `None`。
    fn slot_of(&self, y: u16) -> Option<u16> {
        if y >= self.height {
            return None;
        }
        let slot = self.order.get(y as usize).copied().unwrap_or(y);
        (slot < self.height).then_some(slot)
    }

    /// 整窗重渲：按 [`crate::layout::RowRef`] **逐行**把字形渲进各自的 1 行高小矩形。
    ///
    /// 逐行渲染是槽环的前提（模块头契约 3）：只重建换进来的那几行，就必须能单独渲染一行，
    /// 且结果与整窗渲染逐格相同。等价性由测试里的「整窗对帐」用例守住。
    fn rebuild(&mut self, layout: &TranscriptLayout, inner: Rect, top_row: usize, key: GlyphKey) {
        #[cfg(test)]
        {
            self.rebuild_count += 1;
        }
        self.base_row = top_row;
        self.key = key;
        self.layout_slots(layout.width(), inner.height);
        if self.width == 0 || self.height == 0 {
            self.cont.clear();
            self.words_per_row = 0;
            return;
        }
        let (window, rows) = layout.window_with_rows(top_row, self.height as usize);
        for (y, row) in rows.iter().enumerate() {
            self.blit_row(&window, *row, y as u16);
        }
        self.rebuild_cont();
    }

    /// 槽环位移：`k > 0` ⇒ 视窗朝**文档后方**移动（`top_row` 变大）⇒ 内容在屏幕上**上移**。
    ///
    /// `k > 0` 走 `order.rotate_left(k)`（屏幕行 `i` 从此看到 slot `i + k`），
    /// `k < 0` 走 `order.rotate_right(-k)`；随后只把**新换进来的那 `|k|` 行**渲进它们
    /// 各自的槽位（`k > 0` ⇒ 屏幕底部进来的行；`k < 0` ⇒ 屏幕顶部进来的行）。
    ///
    /// **方向不得反写**：`rotate_left` 写反就是「拖到边缘内容反向」，而 `symbol` / `is_cont`
    /// 都经 [`Self::slot_of`] 读 `order` ⇒ 反向会逐格错位，被 V3 的对帐用例抓住。
    ///
    /// 调用方保证 `0 < |k| < height` —— 整屏都换掉的情形由 [`Self::rebuild`] 处理。
    fn shift(&mut self, k: isize, window: &Window, rows: &[RowRef]) {
        debug_assert!(k != 0 && k.unsigned_abs() < self.height as usize);
        let h = self.height as usize;
        let n = k.unsigned_abs();
        if k > 0 {
            self.order.rotate_left(n);
        } else {
            self.order.rotate_right(n);
        }
        // 只重建换进来的那 `n` 行：`k > 0` 是底部 `h-n..h`，`k < 0` 是顶部 `0..n`。
        let start = if k > 0 { h - n } else { 0 };
        for y in start..start + n {
            let slot = self.order[y];
            match rows.get(y) {
                Some(row) => self.blit_row(window, *row, slot),
                // 窗口比内区短（滚到内容尾部）⇒ 这一行**不存在**内容，必须擦掉该槽位
                // 里环转进来的旧字形，否则内容尾之后会露出别的行。
                None => self.clear_row(slot),
            }
        }
        self.rebuild_cont();
    }

    /// 把 `slots` 的第 `slot` 行全部重置成空白格。
    fn clear_row(&mut self, slot: u16) {
        if slot >= self.height {
            return;
        }
        for x in 0..self.width {
            self.slots[(x, slot)].reset();
        }
    }

    /// 把 `row` 指的那一行的字形写到 `slots` 的第 `slot` 行。
    ///
    /// `off` 是**该 `Line` 内**的视觉行偏移：首行被视窗上沿切开时非零，交给 `scroll`
    /// 跳过它的上半截 —— 与整窗渲染的 `scroll((skip_rows, 0))` 是同一机制。
    fn blit_row(&mut self, window: &Window, row: RowRef, slot: u16) {
        let Some(line) = window.lines.get(row.line as usize) else {
            return;
        };
        // 先擦干净该 slot 行：旋转后它是另一行内容的旧字形。
        self.clear_row(slot);
        Paragraph::new(Text::from(vec![line.clone()]))
            .wrap(Wrap { trim: false })
            .scroll((row.off.min(u16::MAX as u32) as u16, 0))
            .render(Rect::new(0, slot, self.width, 1), &mut self.slots);
    }

    /// 全量重建次数快照 —— 供跨模块（`draw.rs`）的「稳态零重建」断言读。
    ///
    /// 字段本身只在 `cfg(test)` 下存在，故本访问器也只在测试构建里有定义。
    #[cfg(test)]
    pub(crate) fn rebuild_count_snapshot(&self) -> usize {
        self.rebuild_count
    }

    /// 释放槽位面积但**保留容量**：无选区时不再占一屏字形（设计 §2.3 第 9 条）。
    ///
    /// `slots.resize(Rect::ZERO)` 只把面积置空，底层 `Vec` 容量留着，下次 `ensure` 不必重分配。
    pub(crate) fn release(&mut self) {
        self.slots.resize(Rect::ZERO);
        self.cont.clear();
        self.words_per_row = 0;
        self.width = 0;
        self.height = 0;
        self.order.clear();
        self.key = GlyphKey::default();
    }

    /// 把槽位调到 `width × height` 并清空（复用 `Vec` 容量，不重新分配 content 数组）。
    fn layout_slots(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        let area = Rect::new(0, 0, width, height);
        self.slots.resize(area);
        self.slots.reset();
        self.order.clear();
        self.order.extend(0..height);
    }

    /// 整窗渲染一块（[`extract`] 用）：`top_row` 起的 `height` 个视觉行一目渲进 `slots`。
    ///
    /// 与 [`Self::rebuild`] 的两点差异都是**有意的**：本函数直接交给 `Paragraph` 渲一整块
    /// （提取是一次性动作，不必按行分槽）；也不动 `key`（提取不参与缓存判定）。
    ///
    /// `top_row` 必须 `<= layout.total_rows()`：`window()` 会把越界的顶行夹到最后一行，
    /// 越界调用会渲染出**别的行**的字形。越界块由调用方跳过。
    fn load_window(&mut self, layout: &TranscriptLayout, top_row: usize, height: u16) {
        self.layout_slots(layout.width(), height);
        if self.width == 0 || self.height == 0 {
            self.cont.clear();
            self.words_per_row = 0;
            return;
        }
        let window = layout.window(top_row, height as usize);
        let area = Rect::new(0, 0, self.width, self.height);
        Paragraph::new(Text::from(window.lines))
            .wrap(Wrap { trim: false })
            .scroll((window.skip_rows.min(u16::MAX as usize) as u16, 0))
            .render(area, &mut self.slots);
        self.rebuild_cont();
    }

    /// 重建续格位图：某格是**宽字符**（终端宽度 > 1）⇒ 它右邻那一格是续格。
    ///
    /// 判据用 `Cell::cell_width()`（`ratatui::buffer::CellWidth`）—— 那正是
    /// `Buffer::set_stringn` / `Paragraph::render` 排字时用的同一个函数（`unicode-width`
    /// 外加半角浊点的补偿），故与实际渲染的占格严格一致，且不需要新引依赖。
    /// 不能用 `symbol().chars().count()`：CJK / 全角 / emoji 都是「一个字符、两格」。
    ///
    /// 续格自身在 `Buffer` 里是空 `Cell`（`symbol()` = `" "`），与真正的空格无法区分 ——
    /// 故位图必须由**左邻是宽字符**反推，不能靠读续格的符号。
    fn rebuild_cont(&mut self) {
        self.words_per_row = self.width.div_ceil(64) as usize;
        self.cont.clear();
        self.cont
            .resize(self.words_per_row * self.height as usize, 0);
        if self.words_per_row == 0 {
            return;
        }
        for y in 0..self.height {
            for x in 0..self.width {
                if x + 1 >= self.width || self.slots[(x, y)].cell_width() < 2 {
                    continue;
                }
                let word = y as usize * self.words_per_row + (x as usize + 1) / 64;
                self.cont[word] |= 1u64 << ((x + 1) % 64);
            }
        }
    }
}

// `begin` 的 `now` 目前不参与状态（贴边计时从第一次 `extend` 起算），保留参数是为了
// 与 `extend` 对称、且事件层无需区分两者 —— 后续若加入「按住不动也算贴边」的语义，
// 起点就在这里。

// ---------------------------------------------------------------------------
// 提取
// ---------------------------------------------------------------------------

/// OSC 52 载荷多数终端约 100 KB 封顶。超限**不复制**、不静默截断（ADR-0051「代价」）。
const COPY_MAX_BYTES: usize = 256 * 1024;

/// 提取分块行数：每块用 `layout.window(row, chunk)` 取行，渲进复用 `Buffer` 后读回。
const EXTRACT_CHUNK_ROWS: usize = 64;

/// 「200 拍自动滚动」的**选区增量**上限。
///
/// 实测（debug，2026-10）：增量落在测量噪声内（0 ~ 数 ms），对照组本身约 220 ms。
/// 取 **20 ms** 是有意宽松的：它只为抓「选区成本随行数/会话增长」这类量级回归，
/// 不是微基准 —— 微基准在 debug 构建 + 并行测试下不可靠。
#[cfg(test)]
#[cfg(test)]
const DRAG_TICK_BUDGET: Duration = Duration::from_millis(20);

/// 「提取跨 200 行选区」的耗时上限。
///
/// 实测基线（`cargo test -p ys-tui-coding --lib -- --ignored perf_extract_200rows`，
/// debug 构建，2026-10）：≈ **5.2 ms**，波动 ±0.3 ms。取 **12 ms** 留约 2.3 倍余量 ——
/// 够宽以吸收 CI 机器的差异，又能在「提取退化成 O(n²)」这类真回归时变红。
///
/// 设计计划原先写的 1.5 ms 是按槽环旋转成本估的，漏算了 `Paragraph` 的折行 ——
/// 提取必须重新折行（`layout.window()` 给的是未折行的逻辑行）。
#[cfg(test)]
#[cfg(test)]
const EXTRACT_BUDGET: Duration = Duration::from_millis(12);

/// 一次提取的结局。
///
/// 消费方是 `events.rs` 的松手分支：`Copied` 写进 `app.pending_clipboard`，`Empty` 与
/// `TooLarge` 都不动剪贴板（后者在 transcript 回一行提示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExtractOutcome {
    /// 要写进 `app.pending_clipboard` 的文本。
    Copied(String),
    /// `anchor == cursor` ⇒ 不复制（按下未拖动也算这种）。
    Empty,
    /// 超过 [`COPY_MAX_BYTES`]，带当时的累计字节数 —— 调用方据此回提示。
    TooLarge(usize),
}

/// 把 `anchor..=cursor` 两个端点之间的屏幕内容读成文本。
///
/// **只吃 `&TranscriptLayout` + 两个坐标**，不碰 `App`：既好测，也避开「`&mut app.selection`
/// 与 `&app` 同时借用」。
///
/// 两端点**无方向**（行号小的那端定首行区间，另一端定末行）：反向拖选与正向拖选逐字节相同。
///
/// 字形**重新渲一遍**再读，不复用 [`GlyphGrid`] 的视窗缓存 —— 选区可以超出一屏（拖到边缘
/// 自动滚动选中的就是这种），而缓存只有一屏。
///
/// 消费方是 `events.rs` 的松手分支。
pub(crate) fn extract(
    layout: &TranscriptLayout,
    anchor: (usize, u16),
    cursor: (usize, u16),
) -> ExtractOutcome {
    if anchor == cursor {
        return ExtractOutcome::Empty;
    }
    let width = layout.width();
    let total_rows = layout.total_rows();
    if width == 0 || total_rows == 0 {
        return ExtractOutcome::Empty;
    }
    // 端点归一化：行号小的那端定首行区间，行号大的那端定末行区间。
    let (first, last) = if anchor.0 <= cursor.0 {
        (anchor, cursor)
    } else {
        (cursor, anchor)
    };
    let row_lo = first.0;
    // 拖到视窗底部时行号会越过内容尾（内区高度大于剩余行数）—— 那些行**不存在**，
    // 不产出任何字符（连换行也没有），否则复制会带出一大片空行。
    let row_hi = last.0.min(total_rows - 1);
    if row_lo > row_hi {
        return ExtractOutcome::Empty;
    }
    let single = row_lo == row_hi;
    // 单行 ⇒ 列区间两端都要；跨行 ⇒ 首行从 `first.1` 到行尾、末行从 0 到 `last.1`。
    let (x_lo, x_hi) = if single {
        (first.1.min(last.1), first.1.max(last.1))
    } else {
        (first.1, last.1)
    };
    let row_last = width - 1;

    let mut out = String::new();
    let mut started = false;
    let mut grid = GlyphGrid::default();
    let mut block_row = row_lo;
    while block_row <= row_hi {
        // 分块只为**限制单次 `Buffer` 的分配面积**（一屏以外的选区可能有上万行）。
        // `Paragraph` 的折行成本按实际渲染行数计，故分块不增加总成本，只把峰值内存压住。
        let chunk_len = (row_hi - block_row + 1).min(EXTRACT_CHUNK_ROWS);
        grid.load_window(layout, block_row, chunk_len as u16);
        for y in 0..chunk_len as u16 {
            let row = block_row + y as usize;
            // 本行的选区列区间：跨行时中间行满宽。
            let (xa, xb) = if single {
                (x_lo, x_hi)
            } else if row == row_lo {
                (x_lo, row_last)
            } else if row == row_hi {
                (0, x_hi)
            } else {
                (0, row_last)
            };
            if started {
                out.push('\n');
            }
            started = true;
            // **按内容裁剪**：矩形选区跨出内容边界时，把那一段空白当成内容就会复制出
            // 大片空格；`trim_end` 只救行尾，救不了「选区跨到短行」与「中间的空行」。
            if let Some(line) = row_slice(&grid, y, xa, xb, width) {
                out.push_str(&line);
            }
            if out.len() > COPY_MAX_BYTES {
                return ExtractOutcome::TooLarge(out.len());
            }
        }
        block_row += chunk_len;
    }
    // 选区与内容**全无交集**（拖在块间空白里）⇒ 没有可复制的东西。返回空串会让调用方
    // 把空载荷写进剪贴板，等于**清空**用户的剪贴板 —— 与「按下未拖动不复制」是同一类
    // 防误触，故同样归到 [`ExtractOutcome::Empty`]。
    if out.trim_matches('\n').is_empty() {
        return ExtractOutcome::Empty;
    }
    ExtractOutcome::Copied(out)
}

/// 读回第 `y` 行的 `[xa..=xb]`：把选区区间与该行**实际内容区间**求交后再逐格读。
///
/// `None` = 交集为空（该行在选区里但没有内容）—— 调用方仍保留它的换行，否则前后两段
/// 会被拼成一行。
fn row_slice(grid: &GlyphGrid, y: u16, xa: u16, xb: u16, width: u16) -> Option<String> {
    let (first, last) = content_range(grid, y, width)?;
    let lo = xa.max(first);
    let hi = xb.min(last);
    if lo > hi {
        return None;
    }
    // 端点落在宽字符的续格上 ⇒ 收拢到该字符的首格。从续格起读会**丢掉那个字**
    // （续格在 `Buffer` 里就是一个空格），「逐字等于源文本」随之不成立。
    let lo = grid.head_col(y, lo);
    let hi = grid.head_col(y, hi);
    Some(read_back(grid, y, lo, hi))
}

/// 第 `y` 行的**实际内容区间** `[首个非空白格, 末个非空白格]`；整行空白 ⇒ `None`。
///
/// 空白判据是 `" "`：`Buffer` 里未写过的格与宽字符的续格都是这个符号。
fn content_range(grid: &GlyphGrid, y: u16, width: u16) -> Option<(u16, u16)> {
    let mut first = None;
    let mut last = 0u16;
    for x in 0..width {
        if grid.symbol(y, x) != " " {
            first.get_or_insert(x);
            last = x;
        }
    }
    first.map(|first| (first, last))
}

/// 读回单行的 `[xa..=xb]`，续格按 `skip = max(skip, cell_width) - 1` 跳过，行尾去空格。
///
/// 这条规则与 ratatui 写屏时的 diff-writer 同形（`ratatui-core-0.1.2/src/buffer/buffer.rs:589-598`）：
/// 宽字符只写首格，它**右侧那一格保持空白**。故读回时必须把续格一并吃掉，否则每个 CJK
/// 字后面会多出一个空格。
///
/// `cell_width` 由 [`GlyphGrid::is_cont`] 判定 —— 下一格是续格 ⇒ 本格宽 2。**不看**
/// `Cell::skip`（0.30 已废弃且恒 `false`），也不看 `symbol().is_empty()`（续格是空格，不是空串）。
fn read_back(grid: &GlyphGrid, y: u16, xa: u16, xb: u16) -> String {
    let mut out = String::new();
    if xa > xb {
        return out;
    }
    let mut skip = 0u16;
    for x in xa..=xb {
        if skip == 0 {
            out.push_str(grid.symbol(y, x));
        }
        let cell_width = if grid.is_cont(y, x + 1) { 2 } else { 1 };
        skip = skip.max(cell_width) - 1;
    }
    let trimmed = out.trim_end_matches(' ');
    if trimmed.len() == out.len() {
        out
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// 高亮覆盖层
// ---------------------------------------------------------------------------

/// 选区高亮：把每格**样式**改成反显，**不动 symbol**。
///
/// 三个约束都是承重的：
/// - **不 clobber symbol** —— 宽字符不能被拆成两半；
/// - **不用 `Clear`** —— `Clear` 会清 symbol，正是要避免的；
/// - **只写样式** —— `Buffer::diff_iter` 只下发真变了的格，加高亮不增加 diff 量。
///
/// 用 `REVERSED` 而不猜主题：对已着色的错误行 / 代码块同样可见，且不引依赖。
///
/// 矩形先与 `area` 求交再遍历 —— `rects` 可能含超出内区的成分。
pub(crate) struct Highlight<'a> {
    pub(crate) rects: &'a [Rect],
}

impl Widget for Highlight<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let style = Style::default().add_modifier(Modifier::REVERSED);
        for r in self.rects {
            // `rects` 是**内区相对**坐标（与 `Selection::bounds` 的 `x` 同源），
            // 而本 widget 画在 `area` 上（通常是 Chat 内区）：必须把相对坐标平移成
            // 屏幕绝对坐标，否则内区不在原点时高亮会整体跳动一个偏移。
            let abs = Rect::new(
                r.x.saturating_add(area.x),
                r.y.saturating_add(area.y),
                r.width,
                r.height,
            );
            let abs = abs.intersection(area);
            if abs.width == 0 || abs.height == 0 {
                continue;
            }
            for y in abs.top()..abs.bottom() {
                for x in abs.left()..abs.right() {
                    buf[(x, y)].set_style(style);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 自动滚动
// ---------------------------------------------------------------------------

/// 自动滚动单拍的最大步长（视觉行）。
const MAX_STEP: usize = 8;

/// 按住越久滚得越快：`1` 行/拍起步，每多 40 ms 加一行，封顶 [`MAX_STEP`]。
///
/// 起步就得是 `1`（`held < 40 ms` 时不能是 `0`，否则刚贴边的那几拍完全不动，手感像卡死）。
fn drag_step(held: Duration) -> usize {
    (1 + held.as_millis() as usize / 40).clamp(1, MAX_STEP)
}

/// 拖到边缘时推进自动滚动，并把 `cursor_row` 跟到新边缘。
///
/// **六步顺序不可换**（设计 §5.5 / review.md R6），每一步都有具体后果：
///
/// 1. 未拖 / 未贴边 ⇒ 什么都不做；
/// 2. **先**置 `follow = false` —— `compute_max_scroll` 会同步布局，而 `follow == true` 时
///    它强制 `scroll_offset = 0`，锚点会与实际画面脱节；
/// 3. 内容不足一屏（`max == 0`）⇒ 无可滚，直接返回（也不能让 `scroll()` 把 `follow` 置回）；
/// 4. 复用 [`crate::events::scroll`] 的夹取逻辑（`delta > 0` = 向上，与 `edge > 0` 同向）；
/// 5. **再**置一次 `follow = false` —— 第 4 步在滚回底（`next == 0`）时会把它设真；
/// 6. `cursor_row` 推到新边缘行（`anchor_row` **不动** —— 锚点是按下点，不因滚动而变）。
///
/// **不得**调 [`Selection::clear`]：自动滚动 ≠ 用户主动滚动，混了就会「拖到边缘自我取消」。
pub(crate) fn drag_tick(app: &mut App, width: u16, height: u16, now: Instant) {
    if !app.selection.is_dragging() || app.selection.edge() == 0 {
        return;
    }
    app.follow = false;
    let max = app.compute_max_scroll(width, height);
    if max == 0 {
        return;
    }
    let step = drag_step(app.selection.edge_held(now));
    let delta = app.selection.edge() as isize * step as isize;
    crate::events::scroll_with_max(app, max, delta);
    // 第 4 步滚回 `next == 0` 时 `scroll()` 会把 `follow` 置真 —— 必须压回，
    // 否则下一帧 `sync_scroll_state` 会强制 `scroll_offset = 0`。
    app.follow = false;
    debug_assert!(!app.follow, "自动滚动期间必须脱离底部跟随");
    // `top_row` 用算术求，**不再调 `sync_scroll_state`**：那是一次 O(条目数) 的全量
    // 布局同步，而 `scroll()` 刚做过一次。拖动期间内容不变 ⇒ `max` 不变，
    // 而 `top_row = max - scroll_offset` 是 `sync_scroll_state` 的同一个式子。
    let top_row = max.saturating_sub(app.scroll_offset);
    let inner_h = crate::draw::chat_viewport(app, width, height).1;
    // 先取值再赋值：RHS 借 `app.selection`，LHS 写 `app.selection`。
    let cursor = app.selection.edge_row(top_row, inner_h);
    app.selection.cursor_row = cursor;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::TranscriptLine;

    /// 显式时钟基准：所有用例只用 `t0() + 偏移` 造 `Instant`，不读真实时钟。
    ///
    /// 基准必须是**进程内唯一且只取一次**的：每次调用都 `Instant::now()` 会让两次
    /// `at(100)` 相差数微秒，`edge_held` 的精确 `Duration` 断言随之失败。
    fn t0() -> Instant {
        static BASE: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *BASE.get_or_init(Instant::now)
    }

    fn at(ms: u64) -> Instant {
        t0() + Duration::from_millis(ms)
    }

    fn norm(s: &Selection) -> ((usize, u16), (usize, u16)) {
        let (a, c) = s.bounds();
        // 逐分量 min/max：行号小的那端为首，同行为列小的那端为首。
        if (a.0, a.1) <= (c.0, c.1) {
            (a, c)
        } else {
            (c, a)
        }
    }

    #[test]
    fn begin_gives_degenerate_selection_and_clear_resets() {
        let mut s = Selection::default();
        assert!(!s.is_active(), "默认态无选区");
        assert!(!s.is_dragging());

        s.begin(5, 3, at(0));
        let (a, c) = s.bounds();
        assert_eq!(a, (5, 3), "按下点即两端点");
        assert_eq!(c, (5, 3), "未拖动 ⇒ 两端点相等（松手不复制）");
        assert!(s.is_active(), "按下后存在选区");
        assert!(s.is_dragging(), "按下后正在拖");
        assert_eq!(s.edge(), 0, "按下不贴边");
        assert_eq!(s.edge_held(at(500)), Duration::ZERO, "未贴边 ⇒ 不计时");

        s.clear();
        assert!(!s.is_active());
        assert!(!s.is_dragging());
        assert_eq!(s.bounds(), ((0, 0), (0, 0)));
        assert_eq!(s.edge(), 0);
    }

    #[test]
    fn reverse_drag_yields_same_normalized_endpoints() {
        let mut down_right = Selection::default();
        down_right.begin(9, 20, at(0));
        down_right.extend(2, 4, 0, at(10));

        let mut up_left = Selection::default();
        up_left.begin(2, 4, at(0));
        up_left.extend(9, 20, 0, at(10));

        // `bounds()` 不归一化：两个方向各自保留按下/当前的身份。
        assert_eq!(down_right.bounds(), ((9, 20), (2, 4)));
        assert_eq!(up_left.bounds(), ((2, 4), (9, 20)));
        // 归一化之后必须一致 —— 反向拖选选中的是同一段。
        assert_eq!(norm(&down_right), ((2, 4), (9, 20)));
        assert_eq!(norm(&down_left()), ((2, 4), (9, 20)));
        assert_eq!(norm(&down_right), norm(&up_left));
    }

    /// 辅助构造：与 `down_right` 反向的等价形态，用于三处断言互相对照。
    fn down_left() -> Selection {
        let mut s = Selection::default();
        s.begin(2, 4, at(0));
        s.extend(9, 20, 0, at(10));
        s
    }

    #[test]
    fn edge_starts_timer_on_first_contact() {
        let mut s = Selection::default();
        s.begin(5, 3, at(0));
        s.extend(4, 3, 1, at(100));
        assert_eq!(s.edge(), 1);
        assert_eq!(s.edge_held(at(100)), Duration::ZERO, "刚贴边 ⇒ 计时为 0");
        assert_eq!(s.edge_held(at(140)), Duration::from_millis(40));
    }

    #[test]
    fn repeated_same_edge_does_not_reset_timer() {
        let mut s = Selection::default();
        s.begin(5, 3, at(0));
        s.extend(4, 3, 1, at(100));
        // 贴边期间每 16ms 一拍，方向不变 ⇒ 起点仍是第一次贴边的 100ms。
        s.extend(4, 3, 1, at(116));
        s.extend(4, 3, 1, at(132));
        s.extend(4, 3, 1, at(148));
        assert_eq!(s.edge(), 1);
        assert_eq!(
            s.edge_held(at(148)),
            Duration::from_millis(48),
            "同一方向重复 extend 不得重置 edge_since —— 否则加速永远停在第一档"
        );
    }

    #[test]
    fn edge_direction_flip_resets_timer() {
        let mut s = Selection::default();
        s.begin(5, 3, at(0));
        s.extend(4, 3, 1, at(100));
        assert_eq!(s.edge_held(at(400)), Duration::from_millis(300));

        s.extend(30, 3, -1, at(400));
        assert_eq!(s.edge(), -1, "方向翻转");
        assert_eq!(
            s.edge_held(at(400)),
            Duration::ZERO,
            "方向变更 ⇒ 计时从这一刻重新起算"
        );
        assert_eq!(s.edge_held(at(440)), Duration::from_millis(40));
    }

    #[test]
    fn leaving_edge_clears_timer() {
        let mut s = Selection::default();
        s.begin(5, 3, at(0));
        s.extend(4, 3, 1, at(100));
        s.extend(10, 3, 0, at(200));
        assert_eq!(s.edge(), 0, "回到区内 ⇒ 未贴边");
        assert_eq!(s.edge_held(at(200)), Duration::ZERO);
        assert_eq!(
            s.edge_held(at(9_999)),
            Duration::ZERO,
            "未贴边 ⇒ 任意 now 都为 0"
        );

        // 再次贴同一方向也算「变更」（上一次是 0）⇒ 计时重置。
        s.extend(4, 3, 1, at(300));
        assert_eq!(s.edge(), 1);
        assert_eq!(s.edge_held(at(300)), Duration::ZERO);
    }

    // -----------------------------------------------------------------------
    // T5 / T6：字形缓存与提取（A1，整窗重渲）
    // -----------------------------------------------------------------------

    /// 用真实的 `TranscriptLayout` 行号空间造样本（**不**直接摆字串）——
    /// 提取读的是「`Paragraph` 渲出来的字形」，只有经过 `sync` 才有那份产物。
    fn layout_of(lines: Vec<TranscriptLine>, width: u16) -> TranscriptLayout {
        let mut cache = crate::cache::RenderCache::default();
        let mut layout = TranscriptLayout::default();
        layout.sync(
            &lines,
            &mut cache,
            true,
            crate::theme::ThemeKind::default().palette(),
            width,
            Instant::now(),
        );
        layout
    }

    /// `Copied` 的文本；其它结局 ⇒ 失败（`Debug` 里带文本，窄样本可读）。
    fn copied(outcome: ExtractOutcome) -> String {
        match outcome {
            ExtractOutcome::Copied(text) => text,
            other => panic!("期望 Copied，实际 {other:?}"),
        }
    }

    /// 不打印整段文本的结局描述 —— 超限用例的 `Copied` 有数百 KB。
    fn describe(outcome: &ExtractOutcome) -> String {
        match outcome {
            ExtractOutcome::Copied(text) => format!("Copied({} 字节)", text.len()),
            ExtractOutcome::Empty => "Empty".into(),
            ExtractOutcome::TooLarge(bytes) => format!("TooLarge({bytes})"),
        }
    }

    /// **V4 续格无损**：整行作为选区 ⇒ 读回文本逐字等于源文本。
    ///
    /// 宽字符的第二格在 `Buffer` 里是一个空格，若读回时不按续格跳过，每个 CJK 字后面
    /// 都会多出一个空格 —— 高亮看起来完全正常，只有这条用例能发现。
    #[test]
    fn v4_wide_chars_round_trip_without_extra_spaces() {
        let sources = [
            "中文测试",
            "，。！",
            "🙂🙃",
            "ab中c🙂d",
            "混合 mixed 文字",
            "全角（）【】——",
        ];
        let width = 40u16;
        let lines: Vec<TranscriptLine> = sources
            .iter()
            .map(|source| TranscriptLine::System((*source).to_string()))
            .collect();
        let layout = layout_of(lines, width);
        assert_eq!(
            layout.total_rows(),
            sources.len(),
            "前置：每条只占 1 个视觉行"
        );

        for (row, source) in sources.iter().enumerate() {
            let whole = copied(extract(&layout, (row, 0), (row, width - 1)));
            assert_eq!(&whole, source, "第 {row} 行：续格读回必须无损");
            // 逐字对照：`assert_eq!` 已能报差异，这里额外钉住「字符序列本身相同」
            // （两者不同 ⇒ 差异不在空白上，而在字形丢失）。
            assert_eq!(
                whole.chars().collect::<Vec<_>>(),
                source.chars().collect::<Vec<_>>(),
                "第 {row} 行：字符序列必须逐位相同"
            );
        }
    }

    /// **V5 ①**：一行只有 3 个字，选区拖到远超行宽 ⇒ 只复制内容，无尾部空格。
    #[test]
    fn v5_selection_beyond_content_end_is_trimmed() {
        let layout = layout_of(vec![TranscriptLine::System("abc".into())], 40);
        assert_eq!(copied(extract(&layout, (0, 0), (0, 99))), "abc");
        // 行内起步同样裁到真实内容边界。
        assert_eq!(copied(extract(&layout, (0, 1), (0, 99))), "bc");
        // 两端都在行宽之外 ⇒ 与内容无交集 ⇒ 没有可复制的文本（不能拿空串去清剪贴板）。
        assert_eq!(extract(&layout, (0, 50), (0, 99)), ExtractOutcome::Empty);
    }

    /// **V5 ②**：选区纵跨到视窗底部的空白填充行（行号越过内容尾）⇒ 不含空行。
    #[test]
    fn v5_selection_running_past_content_end_has_no_blank_lines() {
        let layout = layout_of(
            vec![
                TranscriptLine::System("first".into()),
                TranscriptLine::System("second".into()),
            ],
            40,
        );
        assert_eq!(layout.total_rows(), 2, "前置：内容只有两行");
        // 内区高度远大于剩余行数：`top` 靠底、拖到屏幕底部就会越过内容。
        let out = copied(extract(&layout, (0, 0), (20, 10)));
        assert_eq!(out, "first\nsecond", "内容尾之后的行不得产出空行");
        // 两端都在内容尾之后 ⇒ 空。
        assert_eq!(extract(&layout, (5, 0), (9, 3)), ExtractOutcome::Empty);
    }

    /// **V5 ③**：选区跨「长折行段 + 其后短行」⇒ 短行不带尾随空白，折行不丢字。
    #[test]
    fn v5_short_line_after_wrapped_paragraph_has_no_trailing_blanks() {
        let width = 20u16;
        let long = "aaaa bbbb cccc dddd eeee ffff";
        let layout = layout_of(
            vec![
                TranscriptLine::System(long.into()),
                TranscriptLine::System("short".into()),
            ],
            width,
        );
        let total = layout.total_rows();
        assert!(total > 2, "前置：首条折成了多行（total={total}）");

        let out = copied(extract(&layout, (0, 0), (total - 1, width - 1)));
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), total, "每个视觉行一行，不多不少");
        assert_eq!(lines.last().copied(), Some("short"), "末行就是短行本身");
        for line in &lines {
            assert!(!line.ends_with(' '), "行尾不得带尾随空白：{line:?}");
        }
        let head = lines[..lines.len() - 1].concat().replace(' ', "");
        assert_eq!(head, long.replace(' ', ""), "折行只是排版，不得丢字");
    }

    /// **V10 上限**：超过 `COPY_MAX_BYTES` ⇒ `TooLarge`，**不得**返回半截文本。
    #[test]
    fn v10_oversized_selection_is_reported_not_truncated() {
        // 宽度取大一些是为了让块数变少（提取按块重渲，块越多总耗时越高），与判据无关。
        let width = 200u16;
        let big = "x".repeat(300 * 1024);
        let layout = layout_of(vec![TranscriptLine::User(big)], width);
        let total = layout.total_rows();
        assert!(total > 1, "前置：超长行折成了多行");

        let outcome = extract(&layout, (0, 0), (total - 1, width - 1));
        match &outcome {
            ExtractOutcome::TooLarge(bytes) => {
                assert!(
                    *bytes > COPY_MAX_BYTES,
                    "报出的字节数必须真超上限：{bytes} <= {COPY_MAX_BYTES}"
                );
            }
            other => panic!("期望 TooLarge，实际 {}", describe(other)),
        }
    }

    /// **V13 反向等价**：正向与反向拖选复制的文本逐字节相同。
    #[test]
    fn v13_reverse_drag_copies_identical_bytes() {
        let width = 24u16;
        let layout = layout_of(
            vec![
                TranscriptLine::System("第一行 中文".into()),
                TranscriptLine::System("second line that wraps a bit".into()),
                TranscriptLine::System("第三行 🙂".into()),
            ],
            width,
        );
        let total = layout.total_rows();
        assert!(total >= 3, "前置：样本跨多行");

        let forward = copied(extract(&layout, (0, 6), (total - 1, 5)));
        let backward = copied(extract(&layout, (total - 1, 5), (0, 6)));
        assert!(!forward.is_empty(), "前置：选区里有内容");
        assert_eq!(
            forward.as_bytes(),
            backward.as_bytes(),
            "端点顺序不得影响结果"
        );
    }

    /// **V6 半边**：两端点相等 ⇒ `Empty`（松手不复制）。
    #[test]
    fn v6_degenerate_selection_is_empty() {
        let layout = layout_of(vec![TranscriptLine::System("abc".into())], 20);
        assert_eq!(extract(&layout, (0, 3), (0, 3)), ExtractOutcome::Empty);
        assert_eq!(extract(&layout, (0, 0), (0, 0)), ExtractOutcome::Empty);
        // 尚未同步过布局（宽度 / 总行数都是 0）⇒ 空，且不 panic。
        let fresh = TranscriptLayout::default();
        assert_eq!(extract(&fresh, (5, 0), (9, 2)), ExtractOutcome::Empty);
    }

    /// **界面等价（V2 的雏形）**：`GlyphGrid` 的字形 / 续格位图与「整窗渲一次」逐格相同。
    ///
    /// 这是槽环的成立前提（模块头契约 3）：只重建换进来的那几行，结果必须与整窗重渲后
    /// 上移 k 行逐格相同。`rebuild` 走的是**逐行**渲染，故本用例同时守住两条路径不漂移。
    #[test]
    fn glyph_grid_matches_whole_window_render() {
        let width = 18u16;
        let layout = layout_of(
            vec![
                TranscriptLine::System("short".into()),
                TranscriptLine::System("this is a long line that will wrap around".into()),
                TranscriptLine::System("中文测试 混合 mixed 文字".into()),
                TranscriptLine::System("a\n\nb".into()),
                TranscriptLine::System("🙂🙃 emoji".into()),
            ],
            width,
        );
        let total = layout.total_rows();
        assert!(total > 6, "前置：样本足够铺满小视窗（total={total}）");

        for height in [1u16, 3, 7, 20] {
            for top in [0usize, 1, total / 2, total - 1] {
                let inner = Rect::new(0, 0, width, height);
                let mut grid = GlyphGrid::default();
                grid.ensure(&layout, inner, top);

                let window = layout.window(top, height as usize);
                let mut expected = Buffer::empty(inner);
                Paragraph::new(Text::from(window.lines))
                    .wrap(Wrap { trim: false })
                    .scroll((window.skip_rows.min(u16::MAX as usize) as u16, 0))
                    .render(inner, &mut expected);

                for y in 0..height {
                    for x in 0..width {
                        let case = format!("top={top} h={height} ({x},{y})");
                        assert_eq!(
                            grid.symbol(y, x),
                            expected[(x, y)].symbol(),
                            "字形不同：{case}"
                        );
                        let cont = x > 0 && expected[(x - 1, y)].cell_width() > 1;
                        assert_eq!(grid.is_cont(y, x), cont, "续格判定不同：{case}");
                        let head = if cont { x - 1 } else { x };
                        assert_eq!(grid.head_col(y, x), head, "首格列号不同：{case}");
                    }
                }
            }
        }
    }

    /// 越界查询不 panic：高亮层会把 `x` 探到行尾之外，`y` 也可能在空视窗上。
    #[test]
    fn glyph_grid_queries_out_of_bounds_are_blank() {
        let layout = layout_of(vec![TranscriptLine::System("abc".into())], 10);
        let mut grid = GlyphGrid::default();
        grid.ensure(&layout, Rect::new(0, 0, 10, 2), 0);
        assert_eq!(grid.symbol(0, 0), "a");
        assert_eq!(grid.symbol(0, 99), " ", "列越界 ⇒ 空格");
        assert_eq!(grid.symbol(99, 0), " ", "行越界 ⇒ 空格");
        assert!(!grid.is_cont(0, 99));
        assert!(!grid.is_cont(99, 0));
        assert_eq!(grid.head_col(99, 99), 99, "越界不算续格 ⇒ 返回自身");

        // 空视窗（尺寸不足）⇒ 所有查询都是空白，且不 panic。
        let mut empty = GlyphGrid::default();
        empty.ensure(&layout, Rect::new(0, 0, 10, 0), 0);
        assert_eq!(empty.symbol(0, 0), " ");
        assert!(!empty.is_cont(0, 0));
    }

    // -----------------------------------------------------------------------
    // T7 / T8：槽环（A2）
    // -----------------------------------------------------------------------

    /// 四类样本：短行 / 折行 / CJK / 空行 —— 覆盖 `Paragraph` 折行与续格的两条路径。
    fn ring_lines() -> Vec<TranscriptLine> {
        vec![
            TranscriptLine::User("short".into()),
            TranscriptLine::Assistant("this is a long line that will wrap around".into()),
            TranscriptLine::User("中文测试文字".into()),
            TranscriptLine::Assistant(String::new()),
            TranscriptLine::User("tail".into()),
            TranscriptLine::Assistant("收尾行 also wraps a bit here".into()),
        ]
    }

    /// 逐格比较两份 `GlyphGrid`（字形 + 续格位图），差异时报出坐标。
    fn assert_grids_equal(a: &GlyphGrid, b: &GlyphGrid, ctx: &str) {
        assert_eq!(a.width, b.width, "{ctx}: 宽不同");
        assert_eq!(a.height, b.height, "{ctx}: 高不同");
        for y in 0..a.height {
            for x in 0..a.width {
                assert_eq!(a.symbol(y, x), b.symbol(y, x), "{ctx}: ({x},{y}) 字形不同");
                assert_eq!(
                    a.is_cont(y, x),
                    b.is_cont(y, x),
                    "{ctx}: ({x},{y}) 续格位不同"
                );
            }
        }
    }

    /// **行尾不产出空格**（`trim_end` 的守卫）。
    ///
    /// `content_range` 已把区间收窄到末个非空白格，故正常路径不会产出尾部空格；
    /// 本用例直接调 `read_back` 传一个**故意超出内容**的右端点，钉住「即使如此也不带
    /// 尾部空白」这条输出契约（变异：删掉 `trim_end_matches` ⇒ 本用例红）。
    #[test]
    fn read_back_never_leaves_trailing_spaces() {
        let width = 20u16;
        let layout = layout_of(vec![TranscriptLine::User("abc".into())], width);
        let mut grid = GlyphGrid::default();
        grid.load_window(&layout, 0, 1);
        // 右端点故意超出内容尾：`abc` 之后都是空白。
        let out = read_back(&grid, 0, 0, width - 1);
        assert!(!out.ends_with(' '), "读回结果不得带尾部空格：{out:?}");
        assert!(out.contains("abc"), "内容应完整读回：{out:?}");
    }

    /// **V3 `shift(k)` == 全量重建**（槽环正确性守卫，等价于 ADR-0045 的 N4）。
    ///
    /// `k` 逐个取值（含负向）与「直接在新 `top_row` 上重建」逐格对帐（**含 `cont` 位图**）。
    /// 方向写反就是「拖到边缘内容反向」，只有这条用例能发现。
    #[test]
    fn v3_shift_matches_full_rebuild() {
        let width = 24u16;
        let layout = layout_of(ring_lines(), width);
        let inner = Rect::new(0, 0, width, 8);
        let total = layout.total_rows();
        assert!(total > 8, "样本需超过一屏（实际 {total} 行）");

        // 必须从中间出发：从边界出发时 `k` 会被夹回，大部分组合根本测不到位移。
        let max_top = total - 8;
        let base = max_top / 2;
        let mut tested = 0usize;
        for k in -3isize..=3 {
            if k == 0 {
                continue;
            }
            let target = (base as isize + k).clamp(0, max_top as isize) as usize;
            if target == base {
                continue;
            }
            tested += 1;
            let mut shifted = GlyphGrid::default();
            shifted.ensure(&layout, inner, base);
            shifted.ensure(&layout, inner, target);

            let mut rebuilt = GlyphGrid::default();
            rebuilt.ensure(&layout, inner, target);

            assert_grids_equal(
                &shifted,
                &rebuilt,
                &format!("shift 从 base={base} 到 target={target} (k={k})"),
            );
        }
        assert!(tested >= 5, "应实际测到多个方向与幅度（实际 {tested}）");
    }

    /// **V3 的尾部边界**：从满屏窗口滚到尾段（窗口比内区短）与反向滚回来，都要逐格一致。
    ///
    /// `v3_shift_matches_full_rebuild` 只从**中段**出发（两头都满屏），测不到
    /// 「`rows.len() < height`」这条分支 —— 那里换进来的行有一部分是空的。
    #[test]
    fn v3_shift_matches_rebuild_at_both_ends() {
        let width = 20u16;
        let lines: Vec<TranscriptLine> = (0..24)
            .map(|i| TranscriptLine::User(format!("row {i}")))
            .collect();
        let layout = layout_of(lines, width);
        let total = layout.total_rows();
        let h = 8u16;
        let hh = h as usize;
        assert!(total > hh + 4, "样本需比一屏多几行（total={total}）");
        let inner = Rect::new(0, 0, width, h);
        let max_top = total - 1;

        for step in 1..=hh {
            for from in [0usize, total - hh, total.saturating_sub(hh / 2)] {
                for target in [from as isize + step as isize, from as isize - step as isize] {
                    let target = target.clamp(0, max_top as isize) as usize;
                    if target == from {
                        continue;
                    }
                    let mut shifted = GlyphGrid::default();
                    shifted.ensure(&layout, inner, from);
                    shifted.ensure(&layout, inner, target);
                    let mut rebuilt = GlyphGrid::default();
                    rebuilt.ensure(&layout, inner, target);
                    assert_grids_equal(
                        &shifted,
                        &rebuilt,
                        &format!("from={from} target={target} step={step} total={total}"),
                    );
                }
            }
        }
    }

    /// **滚到内容尾部时 `shift` 必须擦掉环转进来的旧字形**。
    ///
    /// 窗口比内区短（`rows.len() < height`）时，换进来的行里有一部分**不存在内容**
    /// （`rows` 里没有对应项）：若只是「有就渲、没有就跳过」，那些槽位会保留环转进来的
    /// 别的行 —— 表现是「内容尾之后露出上面几行的字」。
    ///
    /// 变异：把 `shift` 里 `None => self.clear_row(slot)` 改回跳过 ⇒ 本用例红。
    #[test]
    fn shift_clears_rows_past_the_end_of_content() {
        let width = 24u16;
        let lines: Vec<TranscriptLine> = (0..20)
            .map(|i| TranscriptLine::User(format!("row{i}")))
            .collect();
        let layout = layout_of(lines, width);
        let total = layout.total_rows();
        let h = 8u16;
        let inner = Rect::new(0, 0, width, h);
        let hh = h as usize;
        assert!(total > hh + 4, "样本需比一屏多几行（total={total} h={hh}）");
        // `|k| < h` 且 from 窗口满、to 窗口不足（走到尾部）：必须先滚到 from。
        let from = total - hh - 2; // 满屏
        let to = total - 3; // 只剩 3 行 ⇒ rows.len() = 3 < h
        assert!(
            (to as isize - from as isize).unsigned_abs() < hh,
            "k 必须小于 height"
        );
        let mut shifted = GlyphGrid::default();
        shifted.ensure(&layout, inner, from);
        shifted.ensure(&layout, inner, to);
        let mut rebuilt = GlyphGrid::default();
        rebuilt.ensure(&layout, inner, to);
        assert_grids_equal(
            &shifted,
            &rebuilt,
            &format!("tail from={from} to={to} total={total}"),
        );
    }

    /// **只变 `base_row` ⇒ 走 `shift`，不重建**（设计 §2.3 第 6 条）。
    #[test]
    fn only_base_row_change_avoids_rebuild() {
        let width = 24u16;
        let layout = layout_of(ring_lines(), width);
        let inner = Rect::new(0, 0, width, 6);
        let mut grid = GlyphGrid::default();
        grid.ensure(&layout, inner, 2);
        let after_first = grid.rebuild_count;
        grid.ensure(&layout, inner, 3);
        assert_eq!(
            grid.rebuild_count, after_first,
            "仅 base_row 变 ⇒ 必须走 shift，不得重建"
        );
        // 键全等 ⇒ 同样零动作。
        grid.ensure(&layout, inner, 3);
        assert_eq!(grid.rebuild_count, after_first, "键全等 ⇒ 零动作");
    }

    /// **缓存键必须含 `inner` 的宽与高**（变异注入 #1 的锚点）。
    ///
    /// 漏掉 `width` ⇒ resize 后仍用旧字形（折行宽度变了，字形却还是旧列宽的）。
    #[test]
    fn inner_geometry_change_forces_rebuild() {
        let width = 24u16;
        let layout = layout_of(ring_lines(), width);
        let mut grid = GlyphGrid::default();
        grid.ensure(&layout, Rect::new(0, 0, width, 6), 2);
        let after_first = grid.rebuild_count;

        // 只变高。
        grid.ensure(&layout, Rect::new(0, 0, width, 5), 2);
        assert!(
            grid.rebuild_count > after_first,
            "只变 height ⇒ 必须重建（不能走 shift）"
        );

        // 只变宽。
        let mut grid = GlyphGrid::default();
        grid.ensure(&layout, Rect::new(0, 0, width, 6), 2);
        let after_first = grid.rebuild_count;
        grid.ensure(&layout, Rect::new(0, 0, width - 2, 6), 2);
        assert!(
            grid.rebuild_count > after_first,
            "只变 width ⇒ 必须重建（变异 #1：漏 width 会静默复用旧字形）"
        );
    }

    /// **`release()` 释放面积但保留容量**，且再 `ensure` 能正常重建。
    #[test]
    fn release_frees_area_and_recovers() {
        let width = 24u16;
        let layout = layout_of(ring_lines(), width);
        let inner = Rect::new(0, 0, width, 6);
        let mut grid = GlyphGrid::default();
        grid.ensure(&layout, inner, 2);
        let capacity_after_use = grid.slots.content.capacity();
        assert!(grid.slots.area().width > 0, "前置：应有面积");

        grid.release();
        assert_eq!(*grid.slots.area(), Rect::ZERO, "release ⇒ 面积为空");
        assert_eq!(grid.symbol(0, 0), " ", "release 后查询为空白");
        assert_eq!(
            grid.slots.content.capacity(),
            capacity_after_use,
            "release 应保留容量（低频动作也不能每次重分配）"
        );

        grid.ensure(&layout, inner, 2);
        assert!(grid.slots.area().width > 0, "release 后可重建");
    }

    // -----------------------------------------------------------------------
    // T9 / T10：高亮与矩形（A2）
    // -----------------------------------------------------------------------

    /// 造一份 `width × height` 的缓冲区，交 `Highlight` 上样式。
    fn render_highlight(rects: &[Rect], width: u16, height: u16) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        Highlight { rects }.render(Rect::new(0, 0, width, height), &mut buf);
        buf
    }

    /// **V11 只改 style 不动 symbol**：宽字符不能被拆。
    #[test]
    fn v11_highlight_only_changes_style() {
        let width = 12u16;
        let height = 4u16;
        let mut before = Buffer::empty(Rect::new(0, 0, width, height));
        // 写一行 CJK + 普通字，制造续格。
        before.set_stringn(0, 1, "中文ab", width as usize, Style::default());
        let symbols_before: Vec<String> = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .map(|(x, y)| before[(x, y)].symbol().to_string())
            .collect();

        let rects = [Rect::new(1, 1, width - 2, 1)];
        let mut after = before.clone();
        Highlight { rects: &rects }.render(Rect::new(0, 0, width, height), &mut after);

        // 1) symbol 逐格不变。
        let symbols_after: Vec<String> = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .map(|(x, y)| after[(x, y)].symbol().to_string())
            .collect();
        assert_eq!(symbols_before, symbols_after, "高亮不得改动任何 symbol");

        // 2) 矩形内 REVERSED 置位，矩形外未置位。
        for y in 0..height {
            for x in 0..width {
                let inside = y == 1 && (1..width - 1).contains(&x);
                let reversed = after[(x, y)].modifier.contains(Modifier::REVERSED);
                assert_eq!(reversed, inside, "({x},{y}) REVERSED 与矩形不符");
            }
        }
    }

    /// **与 `area` 求交**：矩形超出内区时不得 panic，且只染交集中的格。
    #[test]
    fn highlight_clips_to_area_without_panic() {
        let rects = [Rect::new(5, 5, 100, 100), Rect::new(0, 0, 4, 1)];
        let buf = render_highlight(&rects, 8, 6);
        assert!(
            buf[(7, 5)].modifier.contains(Modifier::REVERSED),
            "交集内的格应被染色"
        );
        assert!(
            buf[(0, 0)].modifier.contains(Modifier::REVERSED),
            "第二个矩形应生效"
        );
    }

    /// **`rects` 是内区相对坐标**：渲在偏移的内区上时，必须平移成绝对坐标。
    ///
    /// 漏了平移 ⇒ 高亮整体跳一个偏移（`inner.y == 1` 时看不出，因为只差一行；
    /// 故这里直接违背一个明显偏移的内区）。
    #[test]
    fn highlight_translates_relative_rects_by_area_origin() {
        let area = Rect::new(3, 4, 10, 5);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 12));
        // 内区相对坐标 (1,1) ⇒ 绝对 (4,5)。
        let rects = [Rect::new(1, 1, 2, 1)];
        Highlight { rects: &rects }.render(area, &mut buf);
        for y in 0..12u16 {
            for x in 0..20u16 {
                let expected = y == 5 && (4..6).contains(&x);
                assert_eq!(
                    buf[(x, y)].modifier.contains(Modifier::REVERSED),
                    expected,
                    "({x},{y}) 高亮位置错 —— 内区相对坐标必须按 area 原点平移"
                );
            }
        }
    }

    /// **宽字符边界（右端）**：选区右端落在 CJK 续格上 ⇒ 矩形右边界是首格 + 1（整字宽）。
    ///
    /// User 行带 `"> "` 前缀 ⇒ 内容从列 2 起：「中」占 2-3、续格在 3。
    ///
    /// 变异：`wide_aware_right` 改成直接返回 `x` ⇒ 右端停在续格上，矩形宽变 1 ⇒ 本用例红。
    #[test]
    fn rects_widen_wide_char_right_edge() {
        let width = 20u16;
        let layout = layout_of(vec![TranscriptLine::User("中文测试".into())], width);
        let inner = Rect::new(0, 0, width, 4);
        let mut sel = Selection::default();
        sel.begin(0, 2, at(0)); // 「中」的首格
        sel.extend(0, 3, 0, at(10)); // 列 3 是「中」的续格
        sel.ensure_grid(&layout, inner, 0);
        let rects = sel.rects(0, inner).to_vec();
        assert_eq!(rects.len(), 1, "单行选区 ⇒ 一个矩形");
        assert_eq!(rects[0].x, 2, "左端就是首格");
        assert_eq!(
            rects[0].width, 2,
            "右端在续格上 ⇒ 矩形宽应为 2（整字），不得只盖半个字"
        );
    }

    /// **跨行选区的三块形态**：首行 `[x_lo..行尾]`、中间行满宽、末行 `[0..=x_hi]`。
    ///
    /// 三个矩形都必须是**内区相对**坐标（`y` 相对 `top_row`），且中间行是连续一整块 ——
    /// 逐行各推一个会退化成「每行一个矩形」，在高亮与差集上都更贵。
    #[test]
    fn rects_multi_line_shape() {
        let width = 16u16;
        let lines: Vec<TranscriptLine> = (0..10)
            .map(|i| TranscriptLine::User(format!("row{i:02}-----------")))
            .collect();
        let layout = layout_of(lines, width);
        let total = layout.total_rows();
        assert!(total >= 10, "每行不折行（total={total}）");
        let inner = Rect::new(0, 0, width, 6);
        let top = 1usize;
        let mut sel = Selection::default();
        sel.begin(2, 4, at(0)); // top+1 行，列 4
        sel.extend(4, 3, 0, at(10)); // top+3 行，列 3
        sel.ensure_grid(&layout, inner, top);
        let rects = sel.rects(top, inner).to_vec();
        assert_eq!(rects.len(), 3, "三行 ⇒ 首/中/末三块");
        assert_eq!((rects[0].x, rects[0].y), (4, 1), "首行：从 x_lo 到行尾");
        assert_eq!(rects[0].width, width - 4, "首行宽度到行尾");
        assert_eq!(
            (rects[1].x, rects[1].y, rects[1].width, rects[1].height),
            (0, 2, width, 1),
            "中间行：满宽一整块"
        );
        assert_eq!((rects[2].x, rects[2].y), (0, 3), "末行：从 0 到 x_hi");
        assert_eq!(rects[2].width, 4, "末行宽度 = x_hi + 1");
    }

    /// **宽字符边界（左端）**：选区左端落在 CJK 续格上 ⇒ 矩形左边界是**首格**。
    ///
    /// 只测右端会漏掉这一半：左端不收拢时高亮从续格开始，`中` 的左半格露在选区外
    /// —— 视觉上是「选中的字少了一半」，而右端的断言依然全绿。
    ///
    /// 变异：`rects` 里左端的 `grid.head_col(y, x_lo)` 改成直接用 `x_lo` ⇒ 本用例红。
    #[test]
    fn rects_collapse_wide_char_left_edge_to_head() {
        let width = 20u16;
        let layout = layout_of(vec![TranscriptLine::User("中文测试".into())], width);
        let inner = Rect::new(0, 0, width, 4);
        // User 行带 `"> "` 前缀 ⇒ 内容从列 2 起；「中」占 2-3，「文」4-5，「测」6-7。
        let mut sel = Selection::default();
        sel.begin(0, 3, at(0)); // 列 3 是「中」的续格
        sel.extend(0, 7, 0, at(10)); // 列 7 是「测」的续格
        sel.ensure_grid(&layout, inner, 0);
        let rects = sel.rects(0, inner).to_vec();
        assert_eq!(rects.len(), 1, "单行选区 ⇒ 一个矩形");
        assert_eq!(
            rects[0].x, 2,
            "左端在续格上 ⇒ 必须收拢到首格（否则「中」露半格）"
        );
        assert_eq!(
            rects[0].width, 6,
            "右端在续格上 ⇒ 收拢到首格再 +1：2..=7 共 6 格（三个整字）"
        );
    }

    /// **`ensure_grid` 的 Idle 分支**：无选区 ⇒ `grid` 被 `release`。
    #[test]
    fn ensure_grid_releases_when_idle() {
        let width = 20u16;
        let layout = layout_of(ring_lines(), width);
        let inner = Rect::new(0, 0, width, 6);
        let mut sel = Selection::default();
        sel.begin(0, 0, at(0));
        sel.ensure_grid(&layout, inner, 0);
        assert!(sel.grid.height > 0, "拖动期缓存就位");

        sel.clear();
        sel.ensure_grid(&layout, inner, 0);
        assert_eq!(sel.grid.height, 0, "Idle ⇒ release（面积为空）");
        assert_eq!(sel.grid.width, 0);
    }

    /// `edge_row` 三档。
    #[test]
    fn edge_row_three_branches() {
        let mut sel = Selection::default();
        sel.begin(10, 3, at(0));
        assert_eq!(sel.edge_row(4, 6), 10, "未贴边 ⇒ cursor_row");

        sel.begin(10, 3, at(0));
        sel.extend(0, 3, 1, at(10));
        assert_eq!(sel.edge_row(4, 6), 4, "上沿 ⇒ top_row");

        sel.begin(10, 3, at(0));
        sel.extend(0, 3, -1, at(10));
        assert_eq!(sel.edge_row(4, 6), 9, "下沿 ⇒ top_row + inner_h - 1");
    }

    // -----------------------------------------------------------------------
    // T11 / T12：自动滚动（A2）
    // -----------------------------------------------------------------------

    /// 造一个装得下 200+ 视觉行的 `App`（每行都折行，凑够行数）。
    fn app_with_many_rows() -> App {
        let mut app = App::new(crate::view::CodingView::for_test());
        let text = "word ".repeat(30);
        for _ in 0..120 {
            app.transcript.push(TranscriptLine::Assistant(text.clone()));
        }
        app
    }

    /// 步长边界：`1` 行/拍起步，每多 40 ms 加一行，封顶 8 行。
    #[test]
    fn drag_step_boundaries() {
        assert_eq!(drag_step(Duration::ZERO), 1);
        assert_eq!(drag_step(Duration::from_millis(39)), 1);
        assert_eq!(drag_step(Duration::from_millis(40)), 2);
        assert_eq!(drag_step(Duration::from_millis(280)), 8);
        assert_eq!(drag_step(Duration::from_secs(10)), 8, "封顶 MAX_STEP");
    }

    /// **V8 自动滚动全程 `follow == false`**（变异注入 #7 的锚点）。
    ///
    /// `scroll()` 在滚回底（`next == 0`）时会把 `follow` 置真，若 `drag_tick` 不压回，
    /// 下一帧 `sync_scroll_state` 会强制 `scroll_offset = 0`，`anchor_row` 与实际画面脱节。
    /// 故刻意从**远离底部**出发、向下沿贴边，一直滚到 `scroll_offset == 0`（回底那一刻）。
    #[test]
    fn v8_drag_tick_never_restores_follow() {
        let (w, h) = (40u16, 12u16);
        let mut app = app_with_many_rows();
        let max = app.compute_max_scroll(w, h);
        assert!(max > 20, "前置：内容应可滚（max={max}）");
        // 从接近顶部出发（offset 大），向下沿贴边才能真的滚回底部。
        app.scroll_offset = max;
        app.follow = false;
        let top_row = app.sync_scroll_state(w, h, at(0)).top_row;
        let inner_h = crate::draw::chat_viewport(&app, w, h).1;

        app.selection.begin(top_row, 0, at(0));
        // 拖到下沿（贴边 -1）⇒ 看更新的内容 ⇒ `scroll_offset` 递减。
        app.selection
            .extend(top_row + inner_h as usize, 1, -1, at(0));
        app.follow = false;

        let anchor_before = app.selection.bounds().0;
        let start = app.scroll_offset;
        let mut reached_bottom = false;
        for i in 1..=80u64 {
            drag_tick(&mut app, w, h, at(i * 16));
            assert!(
                !app.follow,
                "第 {i} 拍（offset={}）：自动滚动期间不得恢复底部跟随",
                app.scroll_offset
            );
            assert_eq!(
                app.selection.bounds().0,
                anchor_before,
                "第 {i} 拍：anchor 不得因滚动而变"
            );
            assert!(app.selection.is_dragging(), "拖动态应保持");
            if app.scroll_offset == 0 {
                reached_bottom = true;
                break;
            }
        }
        assert!(reached_bottom, "应一直滚到回底（start={start}）");
        // 回底那一拍后 `scroll()` 本会把 `follow` 置真 —— 必须已被压回。
        assert!(
            !app.follow,
            "滚回底部时 `scroll()` 会置 follow=true，drag_tick 必须压回"
        );
    }

    /// 方向：`edge = 1`（上沿，看更早的内容）⇒ `scroll_offset` 递增；
    /// `edge = -1`（下沿，看更新的内容）⇒ 递减。
    ///
    /// `scroll_offset` 是「距底部的视觉行数」，故与 `top_row` 反向。
    #[test]
    fn drag_tick_direction_follows_edge_sign() {
        let (w, h) = (40u16, 12u16);

        // edge = -1（下沿）⇒ offset 减。从中间出发，两个方向都有余地。
        let mut app = app_with_many_rows();
        let max = app.compute_max_scroll(w, h);
        assert!(max > 10, "前置：内容应可滚（max={max}）");
        app.scroll_offset = max / 2;
        app.follow = false;
        let top_row = app.sync_scroll_state(w, h, at(0)).top_row;
        let inner_h = crate::draw::chat_viewport(&app, w, h).1;

        app.selection.begin(top_row, 0, at(0));
        app.selection
            .extend(top_row + inner_h as usize, 1, -1, at(0));
        let before = app.scroll_offset;
        drag_tick(&mut app, w, h, at(16));
        assert!(
            app.scroll_offset < before,
            "下沿贴边 ⇒ scroll_offset 应递减（{before} → {}）",
            app.scroll_offset
        );

        // edge = 1（上沿）⇒ offset 增。
        let mut app = app_with_many_rows();
        let max = app.compute_max_scroll(w, h);
        app.scroll_offset = max / 2;
        app.follow = false;
        let top_row = app.sync_scroll_state(w, h, at(0)).top_row;
        let before = app.scroll_offset;
        app.selection.begin(top_row + 1, 0, at(0));
        app.selection.extend(top_row, 1, 1, at(0));
        drag_tick(&mut app, w, h, at(16));
        assert!(
            app.scroll_offset > before,
            "上沿贴边 ⇒ scroll_offset 应递增（{before} → {}）",
            app.scroll_offset
        );
    }

    /// 多拍**单调**：`edge = -1` ⇒ `scroll_offset` 逐拍不增（且真的在推进）；
    /// `edge = 1` ⇒ 逐拍不减。`cursor_row` 每拍都跟到新的边缘行。
    ///
    /// 单拍用例只看了一次差值，抓不到「第一拍对、第二拍被夹回」这类回归。
    #[test]
    fn drag_tick_multi_tick_is_monotonic_and_moves_cursor() {
        let (w, h) = (40u16, 12u16);
        let mut app = app_with_many_rows();
        let max = app.compute_max_scroll(w, h);
        assert!(max > 30, "前置：内容应可滚（max={max}）");
        app.scroll_offset = max / 2;
        app.follow = false;
        let top_row = app.sync_scroll_state(w, h, at(0)).top_row;
        let inner_h = crate::draw::chat_viewport(&app, w, h).1;

        app.selection.begin(top_row, 0, at(0));
        app.selection
            .extend(top_row + inner_h as usize, 1, -1, at(0));

        let mut prev = app.scroll_offset;
        for i in 1..=6u64 {
            drag_tick(&mut app, w, h, at(i * 16));
            assert!(
                app.scroll_offset <= prev,
                "第 {i} 拍：下沿贴边必须逐拍不增（{prev} → {}）",
                app.scroll_offset
            );
            assert!(
                app.scroll_offset < prev || app.scroll_offset == 0,
                "第 {i} 拍：应真的在推进（{prev} → {}）",
                app.scroll_offset
            );
            // `cursor_row` 跟到新的底行：`top_row + inner_h - 1`。
            let expect_cursor = max.saturating_sub(app.scroll_offset) + inner_h as usize - 1;
            assert_eq!(
                app.selection.bounds().1.0,
                expect_cursor,
                "第 {i} 拍：cursor_row 必须跟到新的边缘行"
            );
            prev = app.scroll_offset;
        }

        // 反向：上沿逐拍不减。
        let mut app = app_with_many_rows();
        let max = app.compute_max_scroll(w, h);
        app.scroll_offset = max / 2;
        app.follow = false;
        let top_row = app.sync_scroll_state(w, h, at(0)).top_row;
        app.selection.begin(top_row + 1, 0, at(0));
        app.selection.extend(top_row, 1, 1, at(0));
        let mut prev = app.scroll_offset;
        for i in 1..=6u64 {
            drag_tick(&mut app, w, h, at(i * 16));
            assert!(
                app.scroll_offset >= prev && app.selection.bounds().1.0 == max - app.scroll_offset,
                "第 {i} 拍：上沿贴边必须逐拍不减且 cursor 跟到顶行（{prev} → {}）",
                app.scroll_offset
            );
            prev = app.scroll_offset;
        }
    }

    /// **`max == 0`**（内容不足一屏）⇒ 不动滚动位，但 `follow` 仍被置 `false`。
    #[test]
    fn drag_tick_without_scrollable_content_keeps_offset() {
        let (w, h) = (80u16, 40u16);
        let mut app = App::new(crate::view::CodingView::for_test());
        app.transcript.push(TranscriptLine::User("one line".into()));
        assert_eq!(app.compute_max_scroll(w, h), 0, "前置：内容不足一屏");
        app.follow = true;
        app.scroll_offset = 0;

        app.selection.begin(0, 0, at(0));
        app.selection.extend(5, 1, -1, at(0));
        drag_tick(&mut app, w, h, at(16));
        assert_eq!(app.scroll_offset, 0, "无可滚内容 ⇒ 位置不动");
        assert!(!app.follow, "即使无可滚也要置 false（防 sync 归零）");
    }

    // -----------------------------------------------------------------------
    // T21：性能基准（`#[ignore]`，只测量内容，不做内容判定）
    // -----------------------------------------------------------------------

    /// 造 500 个视觉行左右的 transcript。
    fn perf_app() -> App {
        let mut app = App::new(crate::view::CodingView::for_test());
        for i in 0..500 {
            app.transcript.push(TranscriptLine::Assistant(format!(
                "line {i} of the benchmark"
            )));
        }
        app
    }

    /// **稳态帧**：渲 100 次同一选区，取 p99 与重建次数。
    ///
    /// 门槛（设计计划的临时值）：p99 ≤ 基线 + 10 µs（基线在首个测量里打印）；
    /// 且 `rebuild_count == 1`（无流式输出时「每次拖选 1 次」）。
    #[test]
    #[ignore = "性能基准：cargo test -p ys-tui-coding --lib -- --ignored perf_ --nocapture"]
    fn perf_drag_steady() {
        let width = 100u16;
        let layout = layout_of(perf_app().transcript.clone(), width);
        let inner = Rect::new(0, 0, width, 30);
        let mut sel = Selection::default();
        sel.begin(10, 0, at(0));
        sel.extend(40, 20, 0, at(10));

        let mut samples = Vec::with_capacity(100);
        for _ in 0..100 {
            let t = Instant::now();
            sel.ensure_grid(&layout, inner, 5);
            let _ = sel.rects(5, inner);
            samples.push(t.elapsed());
        }
        samples.sort();
        let p99 = samples[samples.len() * 99 / 100];
        let median = samples[samples.len() / 2];
        println!(
            "perf_drag_steady: median={median:?} p99={p99:?} rebuilds={}",
            sel.grid.rebuild_count_snapshot()
        );
        // 只到「不重建」这一条硬指标；耗时只打印不判定（机器差异）。
        assert_eq!(
            sel.grid.rebuild_count_snapshot(),
            1,
            "稳态渲 100 次只允许 1 次重建"
        );
    }

    /// **200 行自动滚动**：`drag_tick` 的**选区自身**开销。
    ///
    /// 设计计划的 5 ms 是按「槽环 8–12 µs/步 × 200」估的，**只算选区增量**。而 `drag_tick`
    /// 每拍还必然做两件与选区无关的既有工作：`compute_max_scroll`（全量 `layout.sync`）
    /// 与 `chat_viewport`（输入框软折行）。二者合计约 1 ms/拍，远大于选区成本，
    /// 故本基准用**同频次对照**把它们减掉，只断言选区增量的上限。
    ///
    /// 结论：选区增量应为 **0**（槽环搬 cell 的成本低于测量噪声），这正是设计
    /// §2.5「单步成本与 `inner_h`、会话长度、已滚行数均无关」要的结果。
    #[test]
    #[ignore = "性能基准：cargo test -p ys-tui-coding --lib -- --ignored perf_ --nocapture"]
    fn perf_drag_scroll200() {
        let (w, h) = (100u16, 30u16);
        let mut app = perf_app();
        let max = app.compute_max_scroll(w, h);
        app.scroll_offset = max;
        app.follow = false;
        let top_row = app.sync_scroll_state(w, h, at(0)).top_row;
        let inner_h = crate::draw::chat_viewport(&app, w, h).1;
        app.selection.begin(top_row, 0, at(0));
        app.selection
            .extend(top_row + inner_h as usize, 1, -1, at(0));
        app.follow = false;

        let t = Instant::now();
        for i in 1..=200u64 {
            drag_tick(&mut app, w, h, at(i * 16));
        }
        let total = t.elapsed();

        // 对照：只做同样的两项固定 UI 工作，不含任何选区逻辑。
        let mut control = perf_app();
        control.scroll_offset = max;
        control.follow = false;
        let t2 = Instant::now();
        for _ in 0..200u64 {
            control.compute_max_scroll(w, h);
            crate::draw::chat_viewport(&control, w, h);
        }
        let fixed = t2.elapsed();

        let own = total.saturating_sub(fixed);
        println!(
            "perf_drag_scroll200: 200 拍总 {total:?}；固定 UI 对照 {fixed:?}；\
             选区增量 {own:?}（阈值 {DRAG_TICK_BUDGET:?}）"
        );
        assert!(
            own < DRAG_TICK_BUDGET,
            "200 行自动滚动的**选区增量**应 ≤ {DRAG_TICK_BUDGET:?}，实际 {own:?}"
        );
    }

    /// **提取跨 200 行的选区**。
    ///
    /// 设计计划给的临时阈值是 1.5 ms —— 那是按「槽环旋转成本」估的量级，**没有算**
    /// `Paragraph::render` 的折行开销（提取必须重新折行：`layout.window()` 给的是
    /// 未折行的逻辑行）。实测 debug 构建 ≈ 5.2 ms、release ≈ 见下方实测值。
    ///
    /// 阈值按**实际基线留 2 倍余量**定，而不是照抄估算值 —— 一个永远红的基准等于没有基准。
    /// 收紧后的值与测量日期见常量 [`EXTRACT_BUDGET`]。
    #[test]
    #[ignore = "性能基准：cargo test -p ys-tui-coding --lib -- --ignored perf_ --nocapture"]
    fn perf_extract_200rows() {
        let width = 100u16;
        let layout = layout_of(perf_app().transcript, width);
        assert!(layout.total_rows() > 200, "样本应超过 200 行");
        let t = Instant::now();
        let outcome = extract(&layout, (0, 0), (199, width - 1));
        let elapsed = t.elapsed();
        println!(
            "perf_extract_200rows: {elapsed:?}（阈值 {EXTRACT_BUDGET:?}），结局 {}",
            describe(&outcome)
        );
        assert!(
            elapsed < EXTRACT_BUDGET,
            "提取 200 行应 ≤ {EXTRACT_BUDGET:?}，实际 {elapsed:?}"
        );
    }
}
