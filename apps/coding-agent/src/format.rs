use std::io::{self, Write};

use agent_core::StopReason;

use crate::config::Config;
use crate::prompt;
use crate::status::TurnStats;

/// 启动 banner。调一次。
pub fn print_banner<W: Write>(out: &mut W, cfg: &Config, model_id: Option<&str>) -> io::Result<()> {
    writeln!(out, "YuShan Coding Agent")?;
    writeln!(
        out,
        "Provider: {} | Model: {} | Dir: {}",
        cfg.provider.as_deref().unwrap_or("(not configured)"),
        model_id.unwrap_or("(not configured)"),
        prompt::format_cwd_tilde(&cfg.cwd),
    )?;
    writeln!(out, "Type /help for commands, 'exit' to quit")?;
    writeln!(out)
}

/// 单行 turn summary，紧跟 final_message 后。
pub fn print_turn_summary<W: Write>(
    out: &mut W,
    stats: &TurnStats,
    rounds: u32,
    stop: &StopReason,
) -> io::Result<()> {
    writeln!(
        out,
        "{} {} rounds · ↑{} ↓{} tokens",
        status_symbol(stop),
        rounds,
        format_tokens(stats.total_input_tokens),
        format_tokens(stats.total_output_tokens),
    )
}

/// `/status` 命令详情：复用同一组格式化逻辑。
pub fn render_status<W: Write>(
    out: &mut W,
    cfg: &Config,
    model_id: Option<&str>,
    stats: &TurnStats,
) -> io::Result<()> {
    writeln!(out, "YuShan Coding Agent")?;
    writeln!(out, "Dir:        {}", prompt::format_cwd_tilde(&cfg.cwd))?;
    writeln!(out, "Provider:   {}", cfg.provider.as_deref().unwrap_or("(not configured)"))?;
    writeln!(out, "Model:      {}", model_id.unwrap_or("(not configured)"))?;
    writeln!(out, "Turns:      {}", stats.turn_count)?;
    writeln!(
        out,
        "Tokens:     ↑{} ↓{}",
        format_tokens(stats.total_input_tokens),
        format_tokens(stats.total_output_tokens)
    )?;
    Ok(())
}

fn status_symbol(stop: &StopReason) -> &'static str {
    match stop {
        StopReason::Completed => "✓",
        StopReason::MaxRounds => "⚠ MaxRounds",
        StopReason::Cancelled => "✗ Cancelled",
        _ => "?",
    }
}

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
    // For n in [1k, 10k) always keep one decimal. For n >= 10k, drop the
    // trailing ".0" so exact-thousand values like 10_000 render as "10k"
    // instead of "10.0k".
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

    #[test]
    fn test_status_symbol_three_variants() {
        assert_eq!(status_symbol(&StopReason::Completed), "✓");
        assert_eq!(status_symbol(&StopReason::MaxRounds), "⚠ MaxRounds");
        assert_eq!(status_symbol(&StopReason::Cancelled), "✗ Cancelled");
    }

    #[test]
    fn test_print_turn_summary_completed() {
        let mut buf = Vec::new();
        let stats = TurnStats { total_input_tokens: 320, total_output_tokens: 1247, turn_count: 1 };
        print_turn_summary(&mut buf, &stats, 1, &StopReason::Completed).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out, "✓ 1 rounds · ↑320 ↓1.2k tokens\n");
    }

    #[test]
    fn test_print_turn_summary_max_rounds() {
        let mut buf = Vec::new();
        let stats = TurnStats { total_input_tokens: 12_400, total_output_tokens: 4_200, turn_count: 1 };
        print_turn_summary(&mut buf, &stats, 10, &StopReason::MaxRounds).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out, "⚠ MaxRounds 10 rounds · ↑12.4k ↓4.2k tokens\n");
    }

    #[test]
    fn test_print_turn_summary_cancelled() {
        let mut buf = Vec::new();
        let stats = TurnStats { total_input_tokens: 1_200, total_output_tokens: 340, turn_count: 1 };
        print_turn_summary(&mut buf, &stats, 2, &StopReason::Cancelled).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out, "✗ Cancelled 2 rounds · ↑1.2k ↓340 tokens\n");
    }
}
