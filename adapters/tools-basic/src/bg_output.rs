//! 后台命令输出的**组装口径** —— 头尾混合截断 + 跳过量报出 + 完整输出路径。
//!
//! **分工**：截断策略（取哪些行、撞上哪个上限、头尾各占多少预算）由
//! [`ys_tool::head_tail`] 承担；本模块**只组装** —— 把截断结果拼成模型可见的内容，
//! 并**报出被跳过的字节数**与**完整输出文件路径**。
//!
//! **唯一真源**：后台命令的输出**只有这一处**拼装 —— 工具侧（`bg_tools`）调用
//! [`assemble`]，不得另写一份同样的拼装逻辑（两份实现必然漂移）。
//!
//! **不静默丢弃**：被跳过的中段一定在结果里**显式报出**（跳了多少字节 / 多少行），
//! 并附上完整输出文件的路径 —— 模型据此 `grep` / `read` 那个文件取回中段，
//! 而不是把命令重跑一遍。

use std::path::{Path, PathBuf};

use ys_tool::{MAX_BYTES, MAX_LINES, head_tail};

/// 头部占用的字节份额 —— 头部拿 1/4，其余留给尾部。
///
/// 份额是**策略常量**：头尾混合截断时两端的字节预算按此划分（余量全给尾部 ——
/// 尾部通常是报错与汇总，信息密度更高）。
pub const HEAD_BYTES: usize = MAX_BYTES / 4;

/// 轮询工具名 —— 「完整输出」提示行的前缀。
const POLL_TOOL: &str = "bash_poll";

/// 一次后台命令输出的组装结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgOutput {
    /// 交给模型的内容：头 + 省略提示 + 尾 + 跳过量报出 + 完整输出路径。
    pub content: String,
    /// 头部内容（源文本的前若干行）。
    pub head: String,
    /// 尾部内容（源文本的后若干行）。
    pub tail: String,
    /// 被跳过的**源文本**字节数 —— `0` 表示未裁剪。
    pub skipped_bytes: usize,
    /// 被跳过的行数 —— `0` 表示未裁剪。
    pub skipped_lines: usize,
    /// 完整输出文件路径（未送达的部分去这里取）。
    pub output_path: PathBuf,
}

impl BgOutput {
    /// 是否发生了裁剪（有任何字节 / 行被跳过）。
    pub fn truncated(&self) -> bool {
        self.skipped_bytes > 0 || self.skipped_lines > 0
    }
}

/// 组装一次后台命令的输出 —— 头尾混合截断 + 跳过量报出 + 完整输出路径。
///
/// `text` 是本次**增量**读到的文本（调用方负责按**完整行**边界截好）；`handle_label`
/// 是模型可见的句柄名（`bg3`）；`output_path` 是完整输出文件路径；
/// `pending_note` 是**待交付字节**的提示行（由调用方按待交付字节的形态拟好）。
///
/// **行的顺序（唯一真源）**：
/// 头 → 省略标记 → 尾 → 头尾裁剪的未送达报出 → 待交付提示 → 完整输出路径。
/// 后两行都是「还有字节没送达」的说明 ⇒ 相邻；最后一行是「没送达的部分去哪取」的指针。
///
/// **预算不放宽**：字节与行数预算取模块常量（[`MAX_BYTES`] / [`MAX_LINES`]），
/// 与既有工具一致 —— 头尾两段之和仍不超过总预算。
pub fn assemble(
    text: &str,
    handle_label: &str,
    output_path: &Path,
    pending_note: Option<&str>,
) -> BgOutput {
    let trimmed = head_tail(text, MAX_LINES, MAX_BYTES, HEAD_BYTES);

    let mut lines: Vec<String> = Vec::new();
    if !trimmed.head.is_empty() {
        lines.push(trimmed.head.clone());
    }
    if trimmed.omitted_lines > 0 {
        lines.push(format!("[... {} lines omitted ...]", trimmed.omitted_lines));
    }
    if !trimmed.tail.is_empty() {
        lines.push(trimmed.tail.clone());
    }
    if trimmed.omitted_bytes > 0 {
        lines.push(format!(
            "[{handle_label}] {} 未送达（头尾裁剪）—— 完整输出见 {}",
            format_bytes(trimmed.omitted_bytes as u64),
            output_path.display()
        ));
    }
    if let Some(note) = pending_note {
        lines.push(note.to_string());
    }
    lines.push(format!(
        "[{POLL_TOOL}] full output: {}",
        output_path.display()
    ));

    BgOutput {
        content: lines.join("\n"),
        head: trimmed.head,
        tail: trimmed.tail,
        skipped_bytes: trimmed.omitted_bytes,
        skipped_lines: trimmed.omitted_lines,
        output_path: output_path.to_path_buf(),
    }
}

/// 字节数的模型可见写法（B / KiB / MiB，一位小数）——
/// 与既有工具的「N KiB」口径一致，只是 ≥ 1 MiB 时换算成 MiB。
///
/// **唯一真源**：所有后台命令输出里的字节数都用这一处渲染。
pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes < KIB {
        format!("{bytes} B")
    } else if bytes < MIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成 `n` 行、每行恰好 `width` 个 ASCII 字符，逐行内容可辨识。
    fn lines_of(n: usize, width: usize) -> Vec<String> {
        (0..n).map(|i| format!("{i:0width$}")).collect()
    }

    /// 未裁剪时：跳过量恒为 0，且结果里**没有**裁剪提示。
    ///
    /// 变异：把 `skipped_bytes` 恒置 0 ⇒ 本用例仍绿（它本来就该是 0）——
    /// 真正的守卫在下面两条「裁剪时报出」的用例。
    #[test]
    fn untruncated_output_reports_zero_skipped_bytes() {
        let text = lines_of(10, 8).join("\n");
        let out = assemble(&text, "bg1", Path::new("/tmp/bg1.log"), None);

        assert_eq!(out.skipped_bytes, 0, "未裁剪时跳过的字节数必须是 0");
        assert_eq!(out.skipped_lines, 0, "未裁剪时跳过的行数必须是 0");
        assert!(!out.truncated(), "未裁剪时不得判为裁剪");
        assert!(!out.content.contains("未送达"), "未裁剪时不得报出跳过量");
        assert!(out.content.contains("bg1.log"), "结果必须给出完整输出路径");
    }

    /// 裁剪时：`skipped_bytes` 必须等于**源文本里被跳过的那一段**的字节数。
    ///
    /// 独立推导（不依赖被测实现自己的口径）：头部末尾到尾部起始之间的那一段
    /// （含中间的换行符）就是被跳过的部分。
    #[test]
    fn skipped_bytes_reports_the_omitted_segment() {
        let text = lines_of(3_000, 40).join("\n");
        let out = assemble(&text, "bg7", Path::new("/tmp/bg7.log"), None);

        assert!(out.truncated(), "超限输出必须判为裁剪");
        assert!(out.skipped_bytes > 0, "裁剪时必须报出跳过的字节数");

        // 独立推导：跳过量 = 源文本里**没出现在展示内容里**的字节。
        // 被跳过的每一行都带自己的行尾换行；此外**头部末行之后**还有一个换行符
        // 没有出现在展示内容里（头与尾是分开拼的）⇒ 也要计入。
        let all: Vec<&str> = text.lines().collect();
        let head_count = out.head.lines().count();
        let tail_count = out.tail.lines().count();
        let skipped: usize = all[head_count..all.len() - tail_count]
            .iter()
            .map(|line| line.len() + 1)
            .sum();
        assert_eq!(
            out.skipped_bytes,
            skipped + 1,
            "报出的跳过量必须等于源文本里没出现在展示内容里的字节"
        );
        assert_eq!(
            out.skipped_lines,
            all.len() - head_count - tail_count,
            "报出的跳过行数必须等于被跳过的行数"
        );

        // 恒等式：源文本字节 = 展示内容字节 + 跳过量。
        assert_eq!(
            text.len(),
            out.head.len() + out.skipped_bytes + out.tail.len(),
            "源文本字节必须被展示内容与跳过量完整瓜分"
        );

        // 结果里必须**显式**报出跳过量与完整路径（未送达的部分不静默丢弃）。
        assert!(
            out.content.contains("未送达"),
            "结果必须报出跳过量：{}",
            out.content
        );
        assert!(
            out.content.contains("/tmp/bg7.log"),
            "结果必须给出完整输出路径：{}",
            out.content
        );

        // 变异：把 `skipped_bytes` 恒置 0 ⇒ 本用例变红。
    }

    /// **待交付提示行**的位置：紧跟在「头尾裁剪的未送达报出」之后、
    /// 「完整输出路径」之前 —— 两行都是「还有字节没送达」的说明。
    #[test]
    fn the_pending_note_is_placed_between_the_omission_report_and_the_path() {
        let text = lines_of(3_000, 40).join("\n");
        let note = "[bg7] 还有 12 B 待交付（末行未换行，命令结束后整行交付）—— 完整输出见 /tmp/bg7.log";
        let out = assemble(&text, "bg7", Path::new("/tmp/bg7.log"), Some(note));

        let lines: Vec<&str> = out.content.lines().collect();
        let omission = lines
            .iter()
            .position(|line| line.contains("未送达（头尾裁剪）"))
            .expect("必须有头尾裁剪的未送达报出");
        let pending = lines
            .iter()
            .position(|line| line.contains("待交付"))
            .expect("待交付提示行必须在结果里");
        let path = lines
            .iter()
            .position(|line| line.contains("full output:"))
            .expect("必须有完整输出路径");

        assert!(
            omission < pending && pending < path,
            "顺序必须是 裁剪报出 → 待交付提示 → 完整输出路径，实际 {omission}/{pending}/{path}"
        );
        assert_eq!(lines[pending], note, "待交付提示行必须逐字出现");

        // 没有待交付字节 ⇒ 不出现该行。
        let plain = assemble(&text, "bg7", Path::new("/tmp/bg7.log"), None);
        assert!(
            !plain.content.contains("待交付"),
            "没有待交付字节时不得出现该行：{}",
            plain.content
        );
    }

    /// 字节与行数预算**不放宽**：头尾两段的字节之和 ≤ [`MAX_BYTES`]，
    /// 行数之和 ≤ [`MAX_LINES`]。
    #[test]
    fn byte_and_line_budgets_are_not_loosened() {
        // 3000 行 × 40 字节 ≈ 120 KB ⇒ 远超 50 KiB 的字节闸门。
        let text = lines_of(3_000, 40).join("\n");
        let out = assemble(&text, "bg2", Path::new("/tmp/bg2.log"), None);

        let shown_bytes = out.head.len() + out.tail.len();
        assert!(
            shown_bytes <= MAX_BYTES,
            "头尾字节之和必须 ≤ {MAX_BYTES}，实得 {shown_bytes}"
        );
        let shown_lines = out.head.lines().count() + out.tail.lines().count();
        assert!(
            shown_lines <= MAX_LINES,
            "头尾行数之和必须 ≤ {MAX_LINES}，实得 {shown_lines}"
        );

        // 行数闸门单独生效：3000 行 × 4 字节（源文本 < 50 KiB）⇒ 只撞行数闸。
        let thin = lines_of(3_000, 4).join("\n");
        let out = assemble(&thin, "bg3", Path::new("/tmp/bg3.log"), None);
        let shown_lines = out.head.lines().count() + out.tail.lines().count();
        assert!(
            shown_lines <= MAX_LINES,
            "行数闸门必须照常生效，实得 {shown_lines}"
        );
        assert!(out.skipped_lines > 0, "被跳过的行必须报出");

        // 变异：把预算常量调大（如 `MAX_BYTES * 2`）⇒ 第一条断言变红。
    }
}
