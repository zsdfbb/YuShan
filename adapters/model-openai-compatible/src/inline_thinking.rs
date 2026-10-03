//! 剥离 `content` 内联思考标签 → `Thinking` 段。
//!
//! 部分 OpenAI 兼容 provider 把思考塞在 `delta.content` 里（如 MiniMax-M3 单条消息内
//! 多段 `<think>…</think>`）。本模块是纯逻辑状态机：跨片扣留「可能是标签真前缀」的尾部，
//! 完整命中的标签不进任何段，标签外的文本原样透传。它不知道 SSE / 网络 / tokio。
//!
//! `reasoning` 标签映射到 `ThinkingDelta`（与领域词 `Thinking` 同义）。

/// D3：内置三对标签（精确、成对；默认开启，不做 per-provider 配置）。
/// 注：`<think>` 与 `<thinking>` 互不为前缀（第 7 字节 `>` vs `i`），精确匹配无歧义。
const TAGS: [(&str, &str); 3] = [
    ("<think>", "</think>"),
    ("<thinking>", "</thinking>"),
    ("<reasoning>", "</reasoning>"),
];
/// 最长分隔符 `</reasoning>` = 12 字节 ⇒ 真前缀上界 = 11。
const MAX_HOLD: usize = 11;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Segment {
    Text(String),
    Thinking(String),
}

impl Segment {
    fn new(kind: SegmentKind, s: String) -> Self {
        match kind {
            SegmentKind::Text => Segment::Text(s),
            SegmentKind::Thinking => Segment::Thinking(s),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SegmentKind {
    Text,
    Thinking,
}

pub(crate) struct InlineThinkingFilter {
    /// Some(i) = 正在 TAGS[i] 包裹的思考内；None = 正文。
    /// 用下标而非 bool：闭标签**严格配对**，`<think>a<reasoning>b</reasoning>c</think>`
    /// 的内层标签原样留在 Thinking 中，不会被 `</reasoning>` 提前关闭。
    inside: Option<usize>,
    /// 跨片扣留：当前模式下某分隔符的**真前缀**。恒为 ASCII，≤ MAX_HOLD 字节。
    hold: String,
}

impl InlineThinkingFilter {
    pub(crate) fn new() -> Self {
        Self {
            inside: None,
            hold: String::new(),
        }
    }

    /// 快路径：`hold` 空且本片不含 `<` ⇒ 整片同属当前段，调用方可直接 move 转发。
    /// （分隔符必含 `<`，故无 `<` 必无标签。）
    pub(crate) fn passthrough(&self, content: &str) -> Option<SegmentKind> {
        if self.hold.is_empty() && !content.contains('<') {
            Some(kind(self.inside))
        } else {
            None
        }
    }

    /// 慢路径：切分本片（含此前扣留的前缀），返回本片可确定的段。
    pub(crate) fn feed(&mut self, content: &str) -> Vec<Segment> {
        let mut out = Vec::new();
        if self.hold.is_empty() {
            let leftover = scan(content, &mut self.inside, &mut out);
            self.hold.push_str(leftover);
            debug_assert!(self.hold.len() <= MAX_HOLD);
        } else {
            // 仅「上片停在半个标签里」走这里
            let buf = format!("{}{}", self.hold, content);
            let leftover = scan(&buf, &mut self.inside, &mut out);
            self.hold.clear();
            self.hold.push_str(leftover);
            debug_assert!(self.hold.len() <= MAX_HOLD);
        }
        out
    }

    /// 流末冲刷 `hold`：思考态 → Thinking（未闭合思考不外泄）；
    /// 正文态 → Text（末尾半截 `<thi` 确属字面文本，不吞字）。幂等。
    pub(crate) fn flush(&mut self) -> Vec<Segment> {
        if self.hold.is_empty() {
            return Vec::new();
        }
        let k = if self.inside.is_some() {
            SegmentKind::Thinking
        } else {
            SegmentKind::Text
        };
        vec![Segment::new(k, std::mem::take(&mut self.hold))] // 非空
    }
}

/// 当前模式对应的段类型：思考态 → Thinking，正文态 → Text。
fn kind(inside: Option<usize>) -> SegmentKind {
    if inside.is_some() {
        SegmentKind::Thinking
    } else {
        SegmentKind::Text
    }
}

/// 扫一片 hay（可能已含此前扣留的前缀），就地切分；返回需扣留的尾部（`""` = 无）。
/// 前置不变量：`pos` 单调、每个切点都在 char 边界、返回值是当前模式某分隔符的真前缀。
fn scan<'a>(hay: &'a str, inside: &mut Option<usize>, out: &mut Vec<Segment>) -> &'a str {
    let mut pos = 0usize;
    while pos < hay.len() {
        let Some(rel) = hay[pos..].find('<') else {
            push_seg(out, kind(*inside), &hay[pos..]); // 无 '<' ⇒ 必无标签
            return "";
        };
        let lt = pos + rel; // '<' 的绝对下标（char 边界）
        match match_delimiter(&hay[lt..], *inside) {
            // ① 先试「完整命中」
            Some((d, idx)) => {
                if lt > pos {
                    push_seg(out, kind(*inside), &hay[pos..lt]);
                }
                *inside = if inside.is_some() { None } else { Some(idx) }; // 开↔闭 翻转
                pos = lt + d.len();
            }
            // ② 再试「真前缀」
            None if is_proper_prefix(&hay[lt..], *inside) => {
                if lt > pos {
                    push_seg(out, kind(*inside), &hay[pos..lt]);
                }
                return &hay[lt..]; // 可能是跨片标签 → 整段扣留
            }
            // ③ 都不是 ⇒ '<' 是字面量
            None => {
                push_seg(out, kind(*inside), &hay[pos..lt + 1]);
                pos = lt + 1;
            }
        }
    }
    ""
}

/// 追加一段，并合并同 kind 的相邻段（否则「字面量 `<`」会把一段正文切成两段）。
/// 跨 feed 不合并 —— 下游 TUI 的 append_stream 本就会合并相邻同 kind 行。
fn push_seg(out: &mut Vec<Segment>, kind: SegmentKind, s: &str) {
    if s.is_empty() {
        return;
    }
    debug_assert!(!s.is_empty());
    match (out.last_mut(), kind) {
        (Some(Segment::Text(t)), SegmentKind::Text) => t.push_str(s),
        (Some(Segment::Thinking(t)), SegmentKind::Thinking) => t.push_str(s),
        _ => out.push(Segment::new(kind, s.to_string())),
    }
}

/// 按**当前模式**的分隔符表取第一个 `d` 满足 `s.starts_with(d)`；
/// 正文模式 = 三开标签，思考模式 = 该标签自己的闭标签。
fn match_delimiter(s: &str, inside: Option<usize>) -> Option<(&'static str, usize)> {
    match inside {
        Some(i) => {
            let d = TAGS[i].1;
            if s.starts_with(d) { Some((d, i)) } else { None }
        }
        None => {
            for (i, (open, _)) in TAGS.iter().enumerate() {
                if s.starts_with(open) {
                    return Some((open, i));
                }
            }
            None
        }
    }
}

/// `s.len() < d.len() && d.starts_with(s)`，`d` 取当前模式的分隔符表。
fn is_proper_prefix(s: &str, inside: Option<usize>) -> bool {
    match inside {
        Some(i) => {
            let d = TAGS[i].1;
            s.len() < d.len() && d.starts_with(s)
        }
        None => TAGS
            .iter()
            .any(|(open, _)| s.len() < open.len() && open.starts_with(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(kind: SegmentKind, s: &str) -> Segment {
        Segment::new(kind, s.to_string())
    }

    fn text(s: &str) -> Segment {
        seg(SegmentKind::Text, s)
    }

    fn thinking(s: &str) -> Segment {
        seg(SegmentKind::Thinking, s)
    }

    /// 一次性喂入 + flush，返回完整段序列。
    fn run(input: &str) -> Vec<Segment> {
        let mut f = InlineThinkingFilter::new();
        let mut out = f.feed(input);
        out.extend(f.flush());
        out
    }

    /// 合并相邻同 kind 段（切分点会天然制造同 kind 相邻段）。
    fn normalize(segs: Vec<Segment>) -> Vec<Segment> {
        let mut out: Vec<Segment> = Vec::new();
        for s in segs {
            match (out.last_mut(), s) {
                (Some(Segment::Text(t)), Segment::Text(s)) => t.push_str(&s),
                (Some(Segment::Thinking(t)), Segment::Thinking(s)) => t.push_str(&s),
                (_, s) => out.push(s),
            }
        }
        out
    }

    fn concat_by_kind(segs: &[Segment]) -> (String, String) {
        let mut t = String::new();
        let mut th = String::new();
        for s in segs {
            match s {
                Segment::Text(x) => t.push_str(x),
                Segment::Thinking(x) => th.push_str(x),
            }
        }
        (t, th)
    }

    /// 独立参考实现：逐字符扫描，按配对语义剥离标签。
    fn reference_split(s: &str) -> (String, String) {
        let mut text = String::new();
        let mut thinking = String::new();
        let mut inside: Option<usize> = None;
        let mut i = 0;
        while i < s.len() {
            if let Some(t) = inside {
                let d = TAGS[t].1;
                if s[i..].starts_with(d) {
                    inside = None;
                    i += d.len();
                    continue;
                }
                let ch = s[i..].chars().next().unwrap();
                thinking.push(ch);
                i += ch.len_utf8();
            } else if let Some(t) = TAGS.iter().position(|(open, _)| s[i..].starts_with(open)) {
                inside = Some(t);
                i += TAGS[t].0.len();
            } else {
                let ch = s[i..].chars().next().unwrap();
                text.push(ch);
                i += ch.len_utf8();
            }
        }
        (text, thinking)
    }

    /// 1. 单块。
    #[test]
    fn single_block() {
        assert_eq!(
            run("a<think>b</think>c"),
            vec![text("a"), thinking("b"), text("c")]
        );
    }

    /// 2. 三标签各一。
    #[test]
    fn all_three_tags() {
        for tag in ["think", "thinking", "reasoning"] {
            let input = format!("x<{tag}>y</{tag}>z");
            assert_eq!(
                run(&input),
                vec![text("x"), thinking("y"), text("z")],
                "tag = {tag}"
            );
        }
    }

    /// 3. 多块：逐块剥离，正文原样拼接。
    #[test]
    fn multiple_blocks() {
        let segs = run("<think>a</think>mid<thinking>b</thinking>tail");
        assert_eq!(
            segs,
            vec![thinking("a"), text("mid"), thinking("b"), text("tail"),]
        );
        let (t, th) = concat_by_kind(&segs);
        assert_eq!(t, "midtail");
        assert_eq!(th, "ab");
    }

    /// 4. 跨片穷举：`<think>b</think>` 每个 char 边界切 2/3 片，归一化后与一次性 feed 等价。
    #[test]
    fn cross_slice_exhaustive() {
        let full = "<think>b</think>";
        let one_shot = normalize(run(full));
        let boundaries: Vec<usize> = full
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(full.len()))
            .collect();

        for &i in &boundaries {
            let mut f = InlineThinkingFilter::new();
            let mut segs = f.feed(&full[..i]);
            segs.extend(f.feed(&full[i..]));
            segs.extend(f.flush());
            assert_eq!(
                normalize(segs),
                one_shot,
                "2 片切点 i = {i} (input = {:?})",
                &full[..i]
            );
        }

        for &i in &boundaries {
            for &j in &boundaries {
                if j < i {
                    continue;
                }
                let mut f = InlineThinkingFilter::new();
                let mut segs = f.feed(&full[..i]);
                segs.extend(f.feed(&full[i..j]));
                segs.extend(f.feed(&full[j..]));
                segs.extend(f.flush());
                assert_eq!(normalize(segs), one_shot, "3 片切点 i = {i}, j = {j}");
            }
        }
    }

    /// 5. 配对闭合：内层异名标签原样留在 Thinking 中。
    #[test]
    fn nested_pairing() {
        assert_eq!(
            run("<think>a<reasoning>b</reasoning>c</think>d"),
            vec![thinking("a<reasoning>b</reasoning>c"), text("d")]
        );
    }

    /// 6. 未闭合思考 / 未闭合闭标签。
    #[test]
    fn unclosed_thinking() {
        let mut f = InlineThinkingFilter::new();
        let segs = f.feed("pre<think>abc");
        assert_eq!(segs, vec![text("pre"), thinking("abc")]);
        assert_eq!(f.hold, "");
        assert_eq!(f.flush(), Vec::new());

        let mut f = InlineThinkingFilter::new();
        let segs = f.feed("pre<think>abc</thi");
        assert_eq!(segs, vec![text("pre"), thinking("abc")]);
        assert_eq!(f.flush(), vec![thinking("</thi")]);
    }

    /// 7. 尾部半截开标签：不丢字。
    #[test]
    fn trailing_partial_open_tag() {
        let mut f = InlineThinkingFilter::new();
        assert_eq!(f.feed("hi <thi"), vec![text("hi ")]);
        assert_eq!(f.hold, "<thi");
        assert_eq!(f.flush(), vec![text("<thi")]);
    }

    /// 8. 实时性：无 `<` 且 hold 空 ⇒ 快路径命中。
    #[test]
    fn passthrough_hits_on_plain_text() {
        let f = InlineThinkingFilter::new();
        assert_eq!(f.passthrough("hello"), Some(SegmentKind::Text));
        assert_eq!(f.hold, "");

        let mut f = InlineThinkingFilter::new();
        let _ = f.feed("<think>");
        assert_eq!(f.passthrough("正文"), Some(SegmentKind::Thinking));
        assert_eq!(f.passthrough("含<字号"), None);

        // hold 非空时即便本片无 '<' 也不得快路径（否则扣留的前缀会被绕过）
        let mut f = InlineThinkingFilter::new();
        let _ = f.feed("a<thi");
        assert!(!f.hold.is_empty());
        assert_eq!(f.passthrough("plain"), None);
    }

    /// 9. 真值表逐行（§5.2 全 11 行）。
    #[test]
    fn truth_table_rows() {
        // (input, feed 段, 结束时 hold, flush 段)
        let rows: Vec<(&str, Vec<Segment>, &str, Vec<Segment>)> = vec![
            (
                "a<think>b</think>c",
                vec![text("a"), thinking("b"), text("c")],
                "",
                vec![],
            ),
            ("<think>a</think>", vec![thinking("a")], "", vec![]),
            ("<think></think>", vec![], "", vec![]),
            ("</think>plain", vec![text("</think>plain")], "", vec![]),
            ("a<thinkx", vec![text("a<thinkx")], "", vec![]),
            (
                "<<think>a</think>",
                vec![text("<"), thinking("a")],
                "",
                vec![],
            ),
            ("a<thi", vec![text("a")], "<thi", vec![text("<thi")]),
            ("a<", vec![text("a")], "<", vec![text("<")]),
            (
                "<think>ab<reasoning>cd</reasoning>ef</think>g",
                vec![thinking("ab<reasoning>cd</reasoning>ef"), text("g")],
                "",
                vec![],
            ),
            ("hi <thi", vec![text("hi ")], "<thi", vec![text("<thi")]),
            (
                "pre<think>abc</thi",
                vec![text("pre"), thinking("abc")],
                "</thi",
                vec![thinking("</thi")],
            ),
        ];

        for (input, expected_feed, expected_hold, expected_flush) in rows {
            let mut f = InlineThinkingFilter::new();
            let got = f.feed(input);
            assert_eq!(got, expected_feed, "feed 段不符：input = {input:?}");
            assert_eq!(f.hold, expected_hold, "hold 不符：input = {input:?}");
            assert_eq!(f.flush(), expected_flush, "flush 不符：input = {input:?}");
        }
    }

    /// 10. 同 kind 合并：字面量 `<` 不得把一段正文切成两段。
    #[test]
    fn adjacent_same_kind_merged() {
        assert_eq!(run("</think>plain"), vec![text("</think>plain")]);
        assert_eq!(run("a<thinkx"), vec![text("a<thinkx")]);
    }

    /// 11. 属性测试：随机文本 × 随机切分。
    #[test]
    fn random_text_random_splits() {
        let tokens = [
            "<think>",
            "</think>",
            "<thinking>",
            "</thinking>",
            "<reasoning>",
            "</reasoning>",
            "<",
            "</",
            "a",
            "b",
            " ",
            "\n",
            "x",
            "think",
            ">",
            "你",
            "好",
        ];
        let mut rng = SplitMix64::new(0x9E3779B97F4A7C15);

        for case in 0..5000 {
            let n = 1 + (rng.next_u64() % 24) as usize;
            let mut input = String::new();
            for _ in 0..n {
                input.push_str(tokens[(rng.next_u64() as usize) % tokens.len()]);
            }

            let (want_text, want_thinking) = reference_split(&input);
            let want = format!("{want_text}{want_thinking}");

            let one_shot_segs = run(&input);
            let one_shot = normalize(one_shot_segs);
            let (t, th) = concat_by_kind(&one_shot);
            assert_eq!(
                format!("{t}{th}"),
                want,
                "C1 不丢字节失败：case = {case}, input = {input:?}"
            );
            assert_eq!(t, want_text, "Text 拼接不符：input = {input:?}");
            assert_eq!(th, want_thinking, "Thinking 拼接不符：input = {input:?}");

            // 随机切成 1–5 片（在 char 边界上）
            let boundaries: Vec<usize> = input
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(input.len()))
                .collect();
            let n_slices = 1 + (rng.next_u64() % 5) as usize;
            let mut cuts: Vec<usize> = Vec::new();
            for _ in 0..n_slices.saturating_sub(1) {
                cuts.push(boundaries[(rng.next_u64() as usize) % boundaries.len()]);
            }
            cuts.sort_unstable();
            cuts.dedup();

            let mut f = InlineThinkingFilter::new();
            let mut segs = Vec::new();
            let mut prev = 0;
            for &c in &cuts {
                segs.extend(f.feed(&input[prev..c]));
                prev = c;
            }
            segs.extend(f.feed(&input[prev..]));
            segs.extend(f.flush());
            assert_eq!(f.hold, "", "flush 后 hold 必须为空：input = {input:?}");
            assert_eq!(
                normalize(segs),
                one_shot,
                "切分归一化后须与一次性 feed 等价：case = {case}, cuts = {cuts:?}, input = {input:?}"
            );
        }
    }

    /// 12. 扣留上界：思考态 11 / 正文态 10。
    #[test]
    fn hold_upper_bound() {
        // 思考态：进入 <reasoning> 后逐字符喂 </reasoning>，峰值 = 11
        let mut f = InlineThinkingFilter::new();
        let _ = f.feed("<reasoning>");
        let mut peak = 0usize;
        for ch in "</reasoning".chars() {
            let _ = f.feed(&ch.to_string());
            peak = peak.max(f.hold.len());
        }
        assert_eq!(peak, 11);
        assert_eq!(MAX_HOLD, 11);

        // 正文态：逐字符喂 <reasoning，峰值 = 10
        let mut f = InlineThinkingFilter::new();
        let mut peak = 0usize;
        for ch in "<reasoning".chars() {
            let _ = f.feed(&ch.to_string());
            peak = peak.max(f.hold.len());
        }
        assert_eq!(peak, 10);
    }

    /// 13. UTF-8：多切点无损坏。
    #[test]
    fn utf8_multi_cut() {
        let input = "你好<think>思考</think>世界";
        let cut_points: Vec<usize> = input.char_indices().map(|(i, _)| i).collect();
        for &i in &cut_points {
            for &j in &cut_points {
                if j < i {
                    continue;
                }
                let mut f = InlineThinkingFilter::new();
                let mut segs = f.feed(&input[..i]);
                segs.extend(f.feed(&input[i..j]));
                segs.extend(f.feed(&input[j..]));
                segs.extend(f.flush());
                assert_eq!(
                    normalize(segs),
                    vec![text("你好"), thinking("思考"), text("世界")],
                    "切点 i = {i}, j = {j} 处 UTF-8 损坏"
                );
            }
        }
    }

    /// 段恒非空。
    #[test]
    fn segments_never_empty() {
        for input in ["<think></think>", "a<thinkx", "<<think>a</think>", ""] {
            for s in run(input) {
                let len = match &s {
                    Segment::Text(t) | Segment::Thinking(t) => t.len(),
                };
                assert!(len > 0, "空段：input = {input:?}");
            }
        }
    }

    /// SplitMix64：确定性伪随机，避免引入外部依赖。
    struct SplitMix64(u64);

    impl SplitMix64 {
        fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
            z ^ (z >> 31)
        }
    }
}
