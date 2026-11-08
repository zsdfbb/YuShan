//! 判官输出的**解析与引用校验**（三期验收层，设计见
//! `docs/arch/cross-turn-advance/design.md` §5.7，决策见 ADR-0072 D5 / D9）。
//!
//! 本模块只做一件事：把判官的一段文本变成 [`JudgeParse`]。它**不发请求、不碰 model、
//! 不读 session** —— 调用形态（`model.complete`、不带工具表、关思考）与提示词在别处，
//! 这样解析规则可以被单测逐条钉住，不需要网络。
//!
//! # `MET` 的三个条件缺一不可
//!
//! 1. 判官明确输出 `VERDICT: MET`（**整词**匹配，不做前缀匹配）；
//! 2. 每个被标 `OK` 的项都带一段逐字引用，且引用长度 ≥ [`MIN_QUOTE_CHARS`]；
//! 3. 这些引用能在**证据窗口**里逐字命中（归一化空白后做子串匹配），且至少命中一条。
//!
//! 任一不满足 ⇒ [`JudgeParse::Unparseable`]，即「判官不可靠」，**不是**「未达成」。
//!
//! # 为什么「引用未命中」必须算不可靠而不是未达成
//!
//! 判官编造引用时，它的结论与它拿出的凭据自相矛盾 —— 既不能采信「达成」，也不该记成
//! 「任务还没做完」（那会让人以为工作没做完，而真实情况是判官坏了）。两种错误的代价不对称
//! （ADR-0072 D9）：假阳性（误判达成）远比假阴性严重，故一切可疑情形都往「不可靠」降级，
//! 由调用点的可靠性计数在连续 N 次后停机。
//!
//! # 证据窗口**不含助手自述**
//!
//! `evidence` 的构成归调用点，但契约是硬的：只放**工具结果与 harness 可观测事实**，
//! **不放助手本轮正文**（ADR-0072 D4）。助手自述若能进匹配池，「模型自称完成」就能
//! 靠判官自己引用自己而通关 —— 本模块的引用校验正是为了堵住这条路。
//! 本模块对被给到的 `evidence` 只做归一化与子串匹配，不区分来源。

use ys_tools_todo::TodoItem;

/// 引用长度的下限（字符数，按 `char` 计）。
///
/// 设计 §5.7 的判官输出契约要求每段引用 `≥12 字符`：太短的片段（`ok` / `true` / `-`）
/// 在证据里几乎必然命中，等于没有校验。
pub const MIN_QUOTE_CHARS: usize = 12;

/// 扫 `VERDICT` 的行数上限（非空行计）。判官输出可能带前言，故不只看第一行。
const VERDICT_SCAN_LINES: usize = 8;

/// 判官输出解析结果。
///
/// [`JudgeParse::Unparseable`] 涵盖三种情况，它们**同属判官失效**，调用点一律计一次
/// 可靠性失败：没有 `VERDICT` 标记 / 值不是整词 `MET|NOT_MET|BLOCKED`；`MET` 但没有任何
/// 合格引用；`MET` 但引用在证据里找不到（编造）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JudgeParse {
    /// 三个条件齐备，附命中的那段引用。
    Met { quote: String },
    /// 判官明确判定未达成（含它自称 `BLOCKED` 的情形）。
    NotMet { reason: String },
    /// 判官输出不可用。
    Unparseable,
}

/// 解析判官输出。`evidence` 是本回合的**证据窗口**（工具结果 + harness 事实），
/// `open_items` 是**本回合结束时**仍未完成的项（调用点从 `TodoStore::snapshot()` 筛）。
///
/// 逐字引用的比对是「归一化空白后的子串匹配」：折叠连续空白、去首尾空白，不做大小写折叠
/// —— 引用是**逐字**摘录，大小写差异意味着它不是同一段文本。
///
/// `BLOCKED` 归入 [`JudgeParse::NotMet`]（理由带 `blocked:` 前缀）：本模块的结论只有
/// 「达成 / 未达成 / 不可靠」三态，而 `BLOCKED` 的语义是「条件不可达成 ⇒ 该停了」，
/// 停机决策由调用点按理由前缀区分。**不把它算成 `Unparseable`** —— 它是判官给出的合法判断，
/// 计入可靠性计数会把「任务被阻塞」误记成「判官坏了」。
pub fn parse_judge_output(raw: &str, evidence: &str, open_items: &[TodoItem]) -> JudgeParse {
    let verdict = match find_verdict(raw) {
        Some(verdict) => verdict,
        None => return JudgeParse::Unparseable,
    };

    match verdict {
        Verdict::NotMet => JudgeParse::NotMet {
            reason: find_reason(raw).unwrap_or_else(|| "判官未给出理由".to_string()),
        },
        Verdict::Blocked => JudgeParse::NotMet {
            reason: format!(
                "blocked: {}",
                find_reason(raw).unwrap_or_else(|| "判官未给出理由".to_string())
            ),
        },
        Verdict::Met => verify_met(raw, evidence, open_items),
    }
}

/// `VERDICT` 行的三个合法值。
enum Verdict {
    Met,
    NotMet,
    Blocked,
}

/// 在前 [`VERDICT_SCAN_LINES`] 个非空行里找 `VERDICT: <值>`；值必须**整词**匹配。
///
/// 不做前缀匹配是硬要求：`METRICALLY` 若被读成 `MET`，正是最危险的假阳性方向。
fn find_verdict(raw: &str) -> Option<Verdict> {
    for line in raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(VERDICT_SCAN_LINES)
    {
        let Some(rest) = strip_key(line, "VERDICT") else {
            continue;
        };
        // 只取冒号后的第一个词 —— 整词比较，尾随说明文字不影响判定，
        // 但 `MET.` / `METRICALLY` 这类都不会等于 `MET`。
        let token = rest.split_whitespace().next()?;
        return match token.to_ascii_uppercase().as_str() {
            "MET" => Some(Verdict::Met),
            "NOT_MET" => Some(Verdict::NotMet),
            "BLOCKED" => Some(Verdict::Blocked),
            _ => None,
        };
    }
    None
}

/// `key:` / `key：`（全角冒号）后的剩余文本；键名大小写不敏感。
///
/// 要求键名后紧跟冒号 —— 否则 `EVIDENCE` 会被 `ITEMS` 段落里的正文误命中。
fn strip_key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let trimmed = line.trim();
    let head = trimmed.get(..key.len())?;
    if !head.eq_ignore_ascii_case(key) {
        return None;
    }
    let rest = trimmed[key.len()..].trim_start();
    let rest = rest.strip_prefix(':').or_else(|| rest.strip_prefix('：'))?;
    Some(rest.trim())
}

/// `REASON:` / `EVIDENCE:` 行（任一出现即用），用于给 `NotMet` 带一句人可读的理由。
///
/// 理由只给人看（stderr / 终态摘要），**不进 session**（ADR-0072 D8）。
fn find_reason(raw: &str) -> Option<String> {
    for key in ["REASON", "EVIDENCE"] {
        for line in raw.lines() {
            if let Some(rest) = strip_key(line, key) {
                let reason = unquote(rest);
                if !reason.is_empty() {
                    return Some(reason);
                }
            }
        }
    }
    None
}

/// `MET` 的三个条件里除 `VERDICT` 之外的两条：逐条引用合格 **且** 至少一条命中证据。
///
/// - `ITEMS` 行形如 `- <项> | OK | <引用>` / `- <项> | MISSING | -`；
/// - 任一 `MISSING` ⇒ 判官自己说某项没凭据 ⇒ 降级 [`JudgeParse::NotMet`]（不是不可靠：
///   这是判官**合法**的核对结论）；
/// - 任一 `OK` 行的引用为空 / 过短 / 未命中 ⇒ [`JudgeParse::Unparseable`]（判官编造或没守格式）；
/// - **覆盖面**：`open_items` 里每一项都必须在 `ITEMS` 段里出现且标 `OK`。少一项即降级
///   [`JudgeParse::NotMet`] —— 否则判官只核一项就能放行整份清单，而「模型把项标成
///   `completed` 但没跑出凭据」正是要堵的那条路（设计 §5.7）；
/// - 一行都没解析出来时，退回 `EVIDENCE:` 那段（判官只给整体引用时仍可采信，
///   但覆盖面检查**不豁免**：有 open 项时仍要求逐项 `OK`）。
fn verify_met(raw: &str, evidence: &str, open_items: &[TodoItem]) -> JudgeParse {
    let haystack = normalize(evidence);

    let mut saw_item = false;
    let mut first_hit: Option<String> = None;
    // 判官声称已核过的项（标 `OK` 的那些，原文保留供覆盖面比对）。
    let mut checked: Vec<String> = Vec::new();

    for line in raw.lines() {
        let Some(rest) = line.trim().strip_prefix('-') else {
            continue;
        };
        let parts: Vec<&str> = rest.split('|').map(str::trim).collect();
        if parts.len() < 2 {
            // 解析不出的行忽略（设计 §5.7：宽容解析）。
            continue;
        }
        let status = parts[1].to_ascii_uppercase();
        saw_item = true;
        match status.as_str() {
            "MISSING" => {
                let label = parts[0];
                return JudgeParse::NotMet {
                    reason: format!("判官报告该项缺少凭据：{label}"),
                };
            }
            "OK" => {
                let Some(raw_quote) = parts.get(2) else {
                    return JudgeParse::Unparseable;
                };
                let quote = unquote(raw_quote);
                if !quote_is_valid(&quote) || !haystack.contains(&normalize(&quote)) {
                    // 引用为空 / 过短 / 找不到 —— 判官不可靠，绝不采信「达成」。
                    return JudgeParse::Unparseable;
                }
                if first_hit.is_none() {
                    first_hit = Some(quote);
                }
                checked.push(normalize(parts[0]));
            }
            // 未知状态值：该行忽略，不影响其余判定。
            _ => {}
        }
    }

    // 没有任何 `ITEMS` 行 ⇒ 退回整体引用。
    if !saw_item {
        // 覆盖面检查不豁免：有 open 项却一行都没核，就是没核。
        if !open_items.is_empty() {
            return JudgeParse::NotMet {
                reason: "判官未逐项核对清单".to_string(),
            };
        }
        let Some(quote) = find_evidence_quote(raw) else {
            return JudgeParse::Unparseable;
        };
        if !quote_is_valid(&quote) || !haystack.contains(&normalize(&quote)) {
            return JudgeParse::Unparseable;
        }
        return JudgeParse::Met { quote };
    }

    // 覆盖面：每个非终态项都必须在 `ITEMS` 段里标过 `OK`。
    //
    // 比对用归一化后的内容（与 [`stall_digest`] 同一条归一化链的「折叠空白 + 小写」
    // 子集），因为判官会照拄原文也可能手写编号。任一 open 项找不到对应 `OK` 行
    // ⇒ 降级 `NotMet`，不是 `Unparseable`：判官没编造，只是没核完。
    let missing: Vec<&str> = open_items
        .iter()
        .filter(|item| {
            let needle = normalize(&item.content);
            !checked.iter().any(|c| c == &needle || c.contains(&needle))
        })
        .map(|item| item.content.as_str())
        .collect();
    if !missing.is_empty() {
        return JudgeParse::NotMet {
            reason: format!("判官未核对全部未完成项：{:?}", missing),
        };
    }

    match first_hit {
        Some(quote) => JudgeParse::Met { quote },
        // 有 ITEMS 但一条 `OK` 都没有（全是未知状态 / 空列表）⇒ 无引用可核对。
        None => JudgeParse::Unparseable,
    }
}

/// `EVIDENCE:` 行里的那段引用。
fn find_evidence_quote(raw: &str) -> Option<String> {
    for line in raw.lines() {
        if let Some(rest) = strip_key(line, "EVIDENCE") {
            let quote = unquote(rest);
            if !quote.is_empty() {
                return Some(quote);
            }
        }
    }
    None
}

/// 去掉包裹引号（`"` / `'` / `` ` `` / 全角 `“”`），没有包裹则原样返回。
///
/// 宽容之处只在「有没有引号」；严格之处在「引用必须逐字命中证据」—— 后者才是防线。
fn unquote(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        })
        .or_else(|| {
            trimmed
                .strip_prefix('`')
                .and_then(|rest| rest.strip_suffix('`'))
        })
        .or_else(|| {
            trimmed
                .strip_prefix('“')
                .and_then(|rest| rest.strip_suffix('”'))
        });
    stripped.unwrap_or(trimmed).trim().to_string()
}

/// 引用是否够长（按 `char` 计，避免中文按字节算被误判合格）。
fn quote_is_valid(quote: &str) -> bool {
    quote.chars().count() >= MIN_QUOTE_CHARS
}

/// 归一化：折叠所有空白串成单个空格并去首尾空白。
///
/// 与证据窗口的归一化**同一函数** —— 两边若各写一份，引用命中率会随措辞漂移而漂移。
/// 不做大小写折叠：引用是逐字摘录。
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一段够长的证据（工具结果），测试里反复引用它的片段。
    const EVIDENCE: &str = "\
$ cargo test --workspace
running 41 tests
test result: ok. 41 passed; 0 failed; 0 ignored
$ cargo build --workspace
Finished dev profile in 3.42s";

    /// 三条件齐全：`VERDICT: MET` + 逐条 `OK` + 引用逐字命中 ⇒ `Met`。
    #[test]
    fn met_with_hitting_quotes() {
        let raw = "\
VERDICT: MET
ITEMS:
- 跑通全量测试 | OK | test result: ok. 41 passed; 0 failed; 0 ignored
- 构建通过 | OK | Finished dev profile in 3.42s
EVIDENCE: \"test result: ok. 41 passed; 0 failed; 0 ignored\"";

        match parse_judge_output(raw, EVIDENCE, &[]) {
            JudgeParse::Met { quote } => {
                assert_eq!(
                    quote, "test result: ok. 41 passed; 0 failed; 0 ignored",
                    "返回第一段命中的引用"
                );
            }
            other => panic!("三条件齐全应为 Met，实际 {other:?}"),
        }
    }

    /// **编造引用**：有 `VERDICT: MET`、引用格式也对，但证据里找不到 ⇒ `Unparseable`。
    ///
    /// 这是本模块存在的理由：编造的引用既不能当「达成」，也不能当「没达成」。
    ///
    /// 变异：删掉引用校验（有 `MET` 就算 `Met`）⇒ 本用例变红。
    #[test]
    fn met_with_fabricated_quote_is_unparseable() {
        let raw = "\
VERDICT: MET
ITEMS:
- 跑通全量测试 | OK | test result: ok. 999 passed; 0 failed
EVIDENCE: \"test result: ok. 999 passed; 0 failed\"";

        assert_eq!(
            parse_judge_output(raw, EVIDENCE, &[]),
            JudgeParse::Unparseable,
            "引用在证据里找不到 ⇒ 判官不可靠，不是「达成」"
        );
    }

    /// 引用只在**助手自述**里出现过（不在证据窗口）⇒ 同样 `Unparseable`。
    ///
    /// 本用例把「证据窗口不含助手正文」这条契约钉在解析层：即便判官引用了模型
    /// 自称的完成声明，只要那段不在 `evidence` 里就过不了关。
    #[test]
    fn quote_only_in_assistant_prose_does_not_count() {
        let assistant_prose = "我已经完成了全部改动，所有测试都通过了。";
        let raw = format!(
            "VERDICT: MET\nITEMS:\n- 改完代码 | OK | {assistant_prose}\nEVIDENCE: \"{assistant_prose}\""
        );

        assert_eq!(
            parse_judge_output(&raw, EVIDENCE, &[]),
            JudgeParse::Unparseable,
            "助手自述不在证据窗口 ⇒ 模型自称完成无法通关"
        );
    }

    /// 没有 `VERDICT` 标记 ⇒ `Unparseable`（判官没守格式，计入可靠性）。
    ///
    /// 这里**不**选 `NotMet`：格式错误不是「任务没做完」这一事实判断，记成 `NotMet`
    /// 会让「判官坏了」伪装成「活没干完」。
    #[test]
    fn missing_verdict_marker_is_unparseable() {
        let raw = "看起来差不多了，我认为可以收工。";
        assert_eq!(
            parse_judge_output(raw, EVIDENCE, &[]),
            JudgeParse::Unparseable
        );
        assert_eq!(
            parse_judge_output("", EVIDENCE, &[]),
            JudgeParse::Unparseable
        );
    }

    /// `MET` 只做**整词**比较：`METRICALLY` / `MET.` 都不得被读成 `MET`。
    ///
    /// 变异：把 `find_verdict` 的整词比较改成 `starts_with("MET")` ⇒ 本用例变红
    /// （那是假阳性方向，最危险）。
    #[test]
    fn verdict_requires_whole_word() {
        // 证据**充足**：若值被读成 `MET`，下列断言会得到 `Met`。
        // 不带证据的用例区分不了「值没认出来」与「认出来了但没凭据」—— 两者都是
        // `Unparseable`，前缀匹配的变异会因此蒙混过关。
        let raw_with_evidence = |verdict: &str| {
            format!("VERDICT: {verdict}\nITEMS:\n- 跑通测试 | OK | Finished dev profile in 3.42s")
        };

        for verdict in ["METRICALLY", "MET.", "META"] {
            let raw = raw_with_evidence(verdict);
            assert_eq!(
                parse_judge_output(&raw, EVIDENCE, &[]),
                JudgeParse::Unparseable,
                "{verdict} 不是合法的 VERDICT 值（哪怕证据充足也不得放行）"
            );
        }

        // 对照：同一份证据 + 合法值 ⇒ 必须放行。证明上面那三条失败**只**因为值不合法。
        let ok = raw_with_evidence("MET");
        assert!(
            matches!(
                parse_judge_output(&ok, EVIDENCE, &[]),
                JudgeParse::Met { .. }
            ),
            "对照组：合法值与同一份证据必须放行"
        );
    }

    /// 明确 `NOT_MET` ⇒ `NotMet`，理由取自 `REASON:` 行。
    #[test]
    fn explicit_not_met_is_not_met() {
        let raw = "VERDICT: NOT_MET\nREASON: 测试仍有 3 个失败";
        match parse_judge_output(raw, EVIDENCE, &[]) {
            JudgeParse::NotMet { reason } => {
                assert_eq!(reason, "测试仍有 3 个失败");
            }
            other => panic!("应解析为 NotMet，实际 {other:?}"),
        }
    }

    /// 有 `MET` 但**某一项**标 `MISSING` ⇒ 降级 `NotMet`（逐条核对生效）。
    ///
    /// 降级而非不可靠：判官给了合法的核对结论，只是结论是「没全做完」。
    #[test]
    fn met_with_missing_item_downgrades_to_not_met() {
        let raw = "\
VERDICT: MET
ITEMS:
- 跑通全量测试 | OK | test result: ok. 41 passed; 0 failed; 0 ignored
- 更新文档 | MISSING | -";

        match parse_judge_output(raw, EVIDENCE, &[]) {
            JudgeParse::NotMet { reason } => {
                assert!(
                    reason.contains("更新文档"),
                    "理由应点名缺凭据的项：{reason}"
                );
            }
            other => panic!("有 MISSING 项应降级为 NotMet，实际 {other:?}"),
        }
    }

    /// 有 `MET`、没有任何 `ITEMS` 行、只有整体 `EVIDENCE` 引用且逐字命中 ⇒ `Met`。
    #[test]
    fn met_with_only_overall_evidence_hits() {
        let raw = "VERDICT: MET\nEVIDENCE: \"Finished dev profile in 3.42s\"";
        match parse_judge_output(raw, EVIDENCE, &[]) {
            JudgeParse::Met { quote } => assert_eq!(quote, "Finished dev profile in 3.42s"),
            other => panic!("整体引用命中应为 Met，实际 {other:?}"),
        }
    }

    /// 有 `MET` 但**一条引用都没有** ⇒ `Unparseable`（条件 3 无从满足）。
    ///
    /// 变异：把「无引用」放行成 `Met` ⇒ 本用例变红。
    #[test]
    fn met_without_any_quote_is_unparseable() {
        assert_eq!(
            parse_judge_output("VERDICT: MET", EVIDENCE, &[]),
            JudgeParse::Unparseable,
            "没有逐字引用的 MET 不可采信"
        );
        assert_eq!(
            parse_judge_output("VERDICT: MET\nITEMS:\n- 干完了 | OK | ", EVIDENCE, &[]),
            JudgeParse::Unparseable,
            "空引用同样不可采信"
        );
    }

    /// 过短的引用（< [`MIN_QUOTE_CHARS`] 字符）⇒ `Unparseable`：`ok` 这类片段几乎必然命中。
    ///
    /// 变异：删掉 `quote_is_valid` 检查 ⇒ 本用例变红。
    #[test]
    fn too_short_quote_is_unparseable() {
        let raw = "VERDICT: MET\nITEMS:\n- 跑通测试 | OK | ok. 41";
        assert_eq!(
            parse_judge_output(raw, EVIDENCE, &[]),
            JudgeParse::Unparseable
        );
    }

    /// 引用的空白与证据不同（换行 / 多空格）⇒ 归一化后仍命中 ⇒ `Met`。
    ///
    /// 变异：把 `normalize` 从比对链上删掉（裸 `contains`）⇒ 本用例变红。
    #[test]
    fn quote_matching_ignores_whitespace() {
        let raw = "VERDICT: MET\nITEMS:\n- 跑通测试 | OK | test result: ok.   41 passed;\n0 failed; 0 ignored";
        assert!(
            matches!(
                parse_judge_output(raw, EVIDENCE, &[]),
                JudgeParse::Met { .. }
            ),
            "空白差异不该让逐字引用失配"
        );
    }

    /// `BLOCKED` ⇒ `NotMet`，理由带 `blocked:` 前缀（停机决策由调用点按前缀区分）。
    ///
    /// 不计入可靠性：这是判官给出的合法判断，不是格式错误。
    #[test]
    fn blocked_is_not_met_with_prefix() {
        match parse_judge_output(
            "VERDICT: BLOCKED\nREASON: 缺少凭证，无法跑集成测试",
            EVIDENCE,
            &[],
        ) {
            JudgeParse::NotMet { reason } => {
                assert!(reason.starts_with("blocked:"), "理由应带前缀：{reason}");
                assert!(reason.contains("缺少凭证"));
            }
            other => panic!("BLOCKED 应归入 NotMet，实际 {other:?}"),
        }
    }

    /// 键名与值都宽容大小写：`verdict: met` 同样成立。
    #[test]
    fn verdict_key_and_value_are_case_insensitive() {
        let raw = "verdict: met\nitems:\n- 跑通测试 | ok | Finished dev profile in 3.42s";
        assert!(matches!(
            parse_judge_output(raw, EVIDENCE, &[]),
            JudgeParse::Met { .. }
        ));
    }

    /// 判官输出带前言：`VERDICT` 在前 8 个非空行内仍能找到。
    #[test]
    fn verdict_found_after_preamble() {
        let raw = "我核对了一下清单与证据。\n结论如下：\nVERDICT: NOT_MET";
        assert!(matches!(
            parse_judge_output(raw, EVIDENCE, &[]),
            JudgeParse::NotMet { .. }
        ));
    }

    /// 无法解析的 `ITEMS` 行被忽略，不影响其余合格行。
    #[test]
    fn unparsable_item_lines_are_ignored() {
        let raw = "\
VERDICT: MET
ITEMS:
这是一行没有竖线的说明
- 跑通测试 | OK | Finished dev profile in 3.42s";
        assert!(matches!(
            parse_judge_output(raw, EVIDENCE, &[]),
            JudgeParse::Met { .. }
        ));
    }

    /// **覆盖面**：清单里每个未完成项都必须被逐项标 `OK`。
    ///
    /// 判官只核一项就放行整份清单 ⇒ 降级 `NotMet`。这是「模型把项标成 `completed`
    /// 但没跑出凭据」那条路的唯一结构防线（设计 §5.7 的充分条件 2）。
    ///
    /// 变异：去掉 `verify_met` 里的覆盖面比对 ⇒ 本用例变红（得到 `Met`）。
    #[test]
    fn met_must_cover_every_open_item() {
        use ys_tools_todo::TodoStatus;

        let raw = "\
VERDICT: MET
ITEMS:
- 跑通测试 | OK | Finished dev profile in 3.42s";
        let open = vec![
            TodoItem::new("跑通测试", TodoStatus::InProgress),
            TodoItem::new("补写文档", TodoStatus::Pending),
        ];

        match parse_judge_output(raw, EVIDENCE, &open) {
            JudgeParse::NotMet { reason } => {
                assert!(reason.contains("补写文档"), "理由应点名漏核的项：{reason}");
            }
            other => panic!("漏核一项必须降级 NotMet，实际 {other:?}"),
        }

        // 两项都核过 ⇒ 放行。
        let raw_all = "\
VERDICT: MET
ITEMS:
- 跑通测试 | OK | Finished dev profile in 3.42s
- 补写文档 | OK | running 41 tests";
        assert!(
            matches!(
                parse_judge_output(raw_all, EVIDENCE, &open),
                JudgeParse::Met { .. }
            ),
            "全部项都核过应放行"
        );
    }

    /// 有 open 项但判官一行 `ITEMS` 都没给 ⇒ 不退回整体引用，直接降级 `NotMet`。
    ///
    /// 变异：去掉 `!saw_item` 分支里的 `open_items` 检查 ⇒ 本用例变红（得到 `Met`）。
    #[test]
    fn overall_evidence_does_not_skip_coverage() {
        use ys_tools_todo::TodoStatus;

        let raw = "\
VERDICT: MET
EVIDENCE: \"Finished dev profile in 3.42s\"";
        let open = vec![TodoItem::new("补写文档", TodoStatus::Pending)];

        assert!(
            matches!(
                parse_judge_output(raw, EVIDENCE, &open),
                JudgeParse::NotMet { .. }
            ),
            "有未完成项时，整体引用不得代替逐项核对"
        );
    }
}
