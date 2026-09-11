//! ratatui UI 渲染共享的 token 格式化辅助。
//!
//! **c phase**: `tui-stdout` 已删除；本文件仅保留 ratatui mode 仍需要的
//! `format_tokens` / `format_scaled_k` / `format_scaled_m`。`print_banner` /
//! `print_footer` / `print_turn_summary` / `render_status` / `status_symbol` /
//! `approx_session_tokens` 等 stdout 专属逻辑连同 `tui-stdout` feature 一起删除。

pub fn format_tokens(n: u32) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format_scaled_k(n)
    } else {
        format_scaled_m(n)
    }
}

fn format_scaled_k(n: u32) -> String {
    let v = n as f64 / 1_000.0;
    // n 在 [1k, 10k) 时始终保留一位小数；n >= 10k 时去掉末尾
    // 的 ".0"，使 10_000 这类整千值渲染为 "10k" 而不是 "10.0k"。
    let with_decimal = format!("{:.1}k", v);
    if n >= 10_000 && with_decimal.ends_with(".0k") {
        format!("{}k", v.round() as u64)
    } else {
        with_decimal
    }
}

fn format_scaled_m(n: u32) -> String {
    let v = n as f64 / 1_000_000.0;
    let with_decimal = format!("{:.1}M", v);
    if n >= 10_000_000 && with_decimal.ends_with(".0M") {
        format!("{}M", v.round() as u64)
    } else {
        with_decimal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_tokens_boundaries() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(1_001), "1.0k");
        assert_eq!(format_tokens(9_999), "10.0k");
        assert_eq!(format_tokens(10_000), "10k");
        assert_eq!(format_tokens(99_999), "100k");
        assert_eq!(format_tokens(100_000), "100k");
        assert_eq!(format_tokens(999_999), "1000k");
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(9_999_999), "10.0M");
        assert_eq!(format_tokens(10_000_000), "10M");
    }
}
