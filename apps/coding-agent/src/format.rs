#[cfg(feature = "tui-stdout")]
use std::io::{self, Write};

use agent_core::StopReason;

#[cfg(feature = "tui-stdout")]
use crate::prompt;
use crate::view::AppView;

/// Startup banner. Called once at the top of the interactive loop.
/// Only shows startup-class information (version, capabilities, help hint).
/// Runtime state (provider / model / cwd / tokens) lives in the footer.
#[cfg(feature = "tui-stdout")]
pub fn print_banner<W: Write>(out: &mut W, view: &AppView) -> io::Result<()> {
    writeln!(
        out,
        "{}",
        crate::ansi::bold(&format!("YuShan Coding Agent v{}", view.version))
    )?;
    if !view.tools.is_empty() {
        writeln!(out, "Tools: {}", view.tools.join(", "))?;
    }
    writeln!(out, "Type /help for commands, 'exit' to quit")?;
    writeln!(out)
}

/// Resident footer line printed before each prompt. Box-drawing style
/// mirrors Pi's interactive footer.
#[cfg(feature = "tui-stdout")]
pub fn print_footer<W: Write>(out: &mut W, view: &AppView) -> io::Result<()> {
    writeln!(
        out,
        "┌─ {} · {} · {} · ↑{} ↓{} · {} ─┐",
        prompt::format_cwd_tilde(&view.cwd),
        view.provider.as_deref().unwrap_or("(not configured)"),
        view.model.as_deref().unwrap_or("(not configured)"),
        format_tokens(view.total_input_tokens),
        format_tokens(view.total_output_tokens),
        view.session_duration_str(),
    )
}

/// Single-line turn summary, printed right after `final_message`.
#[cfg(feature = "tui-stdout")]
pub fn print_turn_summary<W: Write>(
    out: &mut W,
    view: &AppView,
    rounds: u32,
    stop: &StopReason,
    elapsed_secs: f32,
) -> io::Result<()> {
    let context_str = match view.context_window {
        Some(win) if win > 0 => {
            let used = approx_session_tokens(view);
            let pct = (used as f64 / win as f64) * 100.0;
            format!(" · {:.1}%/{}k", pct, win / 1000)
        }
        _ => String::new(),
    };
    writeln!(
        out,
        "{} {} rounds · ↑{} ↓{} tokens · {:.1}s{}",
        status_symbol(stop),
        rounds,
        format_tokens(view.total_input_tokens),
        format_tokens(view.total_output_tokens),
        elapsed_secs,
        context_str,
    )
}

/// v0 token estimation: cumulative input + output tokens.
///
/// We deliberately avoid `token::estimate_session_tokens` because that
/// requires `&[Message]` and `AppView` is intentionally source-agnostic —
/// keeping the estimator off `AppView` lets `format.rs` stay a pure
/// `Writer + AppView` function (no agent dependencies).
///
/// When v0+ needs a real local estimator (see `docs/arch/tui-resident-status/design.md`
/// §5 reserved `session_tokens` field), add a `session_tokens: usize` field to
/// `AppView` populated by `from_sources` and have this fn read it directly.
fn approx_session_tokens(view: &AppView) -> u64 {
    view.total_input_tokens as u64 + view.total_output_tokens as u64
}

/// `/status` command detail output.
#[cfg(feature = "tui-stdout")]
pub fn render_status<W: Write>(out: &mut W, view: &AppView) -> io::Result<()> {
    writeln!(out, "YuShan Coding Agent")?;
    writeln!(out, "Dir:        {}", prompt::format_cwd_tilde(&view.cwd))?;
    writeln!(
        out,
        "Provider:   {}",
        view.provider.as_deref().unwrap_or("(not configured)")
    )?;
    writeln!(
        out,
        "Model:      {}",
        view.model.as_deref().unwrap_or("(not configured)")
    )?;
    writeln!(out, "Turns:      {}", view.turn_count)?;
    writeln!(
        out,
        "Tokens:     ↑{} ↓{}",
        format_tokens(view.total_input_tokens),
        format_tokens(view.total_output_tokens)
    )?;
    Ok(())
}

fn status_symbol(stop: &StopReason) -> String {
    match stop {
        StopReason::Completed => crate::ansi::green("✓"),
        StopReason::MaxRounds => crate::ansi::yellow("⚠ MaxRounds"),
        StopReason::Cancelled => crate::ansi::red("✗ Cancelled"),
        _ => crate::ansi::dim("?"),
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
    use std::path::PathBuf;
    use std::time::Instant;

    pub(super) fn make_view() -> AppView {
        AppView {
            cwd: PathBuf::from("/tmp"),
            provider: Some("deepseek".into()),
            model: Some("deepseek-chat".into()),
            config_path: PathBuf::from("/tmp/auth.json"),
            logged_in_providers: vec!["deepseek".into()],
            total_known_providers: 3,
            version: "test",
            total_input_tokens: 320,
            total_output_tokens: 1247,
            turn_count: 1,
            session_started: Instant::now(),
            message_count: 0,
            tools: vec!["read".into(), "write".into(), "edit".into(), "bash".into()],
            context_window: None,
            is_first_run: false,
            commands: vec![],
        }
    }

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
        // Hold the shared env lock and ensure NO_COLOR is unset so the color
        // codes are emitted (parallel tests in `ansi` may mutate NO_COLOR).
        let _lock = crate::ansi::tests::env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("NO_COLOR");
        }
        let completed = status_symbol(&StopReason::Completed);
        assert!(completed.contains("✓"));
        assert!(completed.starts_with("\x1b[32m"));
        let max_rounds = status_symbol(&StopReason::MaxRounds);
        assert!(max_rounds.contains("⚠ MaxRounds"));
        assert!(max_rounds.starts_with("\x1b[33m"));
        let cancelled = status_symbol(&StopReason::Cancelled);
        assert!(cancelled.contains("✗ Cancelled"));
        assert!(cancelled.starts_with("\x1b[31m"));
    }

    #[test]
    fn test_approx_session_tokens_sums_cumulative() {
        let mut view = make_view();
        view.total_input_tokens = 100;
        view.total_output_tokens = 50;
        assert_eq!(approx_session_tokens(&view), 150);
        view.total_input_tokens = 0;
        view.total_output_tokens = 0;
        assert_eq!(approx_session_tokens(&view), 0);
    }
}

#[cfg(all(test, feature = "tui-stdout"))]
mod stdout_tests {
    use super::tests::make_view;
    use super::*;

    #[test]
    fn test_print_banner_omits_runtime_state() {
        // Banner shows startup-class info only (version + tools + hint).
        // Runtime state (provider / model / cwd / config path) belongs in
        // the footer, not here.
        let mut buf = Vec::new();
        let view = make_view();
        print_banner(&mut buf, &view).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("YuShan Coding Agent"));
        assert!(out.contains("Tools: read, write, edit, bash"));
        assert!(out.contains("Type /help"));
        // Must NOT contain runtime state
        assert!(!out.contains("Provider:"));
        assert!(!out.contains("Model:"));
        assert!(!out.contains("Config:"));
        assert!(!out.contains("Dir:"));
    }

    #[test]
    fn test_print_banner_omits_tools_when_empty() {
        let mut buf = Vec::new();
        let mut view = make_view();
        view.tools.clear();
        print_banner(&mut buf, &view).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("YuShan Coding Agent"));
        assert!(!out.contains("Tools:"));
    }

    #[test]
    fn test_print_banner_layout() {
        // Verify exact line layout: title + tools + help + trailing newline
        let mut buf = Vec::new();
        let view = make_view();
        print_banner(&mut buf, &view).unwrap();
        let out = String::from_utf8(buf).unwrap();
        // print_banner emits title \n tools \n help \n \n — split('\n')
        // yields [title, tools, help, "", ""] (the trailing \n splits once
        // into a "" then nothing after; the function ends without trailing \n,
        // so split produces the 4-part expected).
        let parts: Vec<&str> = out.split('\n').collect();
        assert_eq!(
            parts.len(),
            5,
            "banner should be 5 parts (title + tools + help + empty + empty from trailing \\n), got {parts:?}"
        );
        assert!(parts[0].contains("YuShan Coding Agent"));
        assert!(parts[1].starts_with("Tools:"));
        assert!(parts[2].starts_with("Type /help"));
        assert_eq!(parts[3], "");
        assert_eq!(parts[4], "");
    }

    #[test]
    fn test_print_footer() {
        let mut buf = Vec::new();
        let view = make_view();
        print_footer(&mut buf, &view).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.starts_with("┌─"));
        assert!(out.contains("·"));
        assert!(out.ends_with("─┐\n"));
        assert!(out.contains("deepseek"));
        assert!(out.contains("deepseek-chat"));
        assert!(out.contains("↑320"));
        assert!(out.contains("↓1.2k"));
    }

    #[test]
    fn test_print_turn_summary_completed() {
        // Hold the shared env lock and ensure NO_COLOR is unset so the
        // green ANSI codes are emitted (parallel `ansi` tests mutate env).
        let _lock = crate::ansi::tests::env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("NO_COLOR");
        }
        let mut buf = Vec::new();
        let view = make_view();
        print_turn_summary(&mut buf, &view, 1, &StopReason::Completed, 2.3).unwrap();
        let out = String::from_utf8(buf).unwrap();
        // Contains ANSI: "\x1b[32m✓\x1b[0m"
        assert!(out.contains("1 rounds · ↑320 ↓1.2k tokens · 2.3s"));
        assert!(out.contains("✓"));
    }

    #[test]
    fn test_print_turn_summary_max_rounds() {
        let _lock = crate::ansi::tests::env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("NO_COLOR");
        }
        let mut buf = Vec::new();
        let mut view = make_view();
        view.total_input_tokens = 12_400;
        view.total_output_tokens = 4_200;
        print_turn_summary(&mut buf, &view, 10, &StopReason::MaxRounds, 5.1).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("10 rounds · ↑12.4k ↓4.2k tokens · 5.1s"));
        assert!(out.contains("⚠ MaxRounds"));
    }

    #[test]
    fn test_print_turn_summary_cancelled() {
        let _lock = crate::ansi::tests::env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("NO_COLOR");
        }
        let mut buf = Vec::new();
        let mut view = make_view();
        view.total_input_tokens = 1_200;
        view.total_output_tokens = 340;
        print_turn_summary(&mut buf, &view, 2, &StopReason::Cancelled, 5.1).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("2 rounds · ↑1.2k ↓340 tokens · 5.1s"));
        assert!(out.contains("✗ Cancelled"));
    }

    #[test]
    fn test_render_status_uses_view() {
        let mut buf = Vec::new();
        let view = make_view();
        render_status(&mut buf, &view).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("YuShan Coding Agent"));
        assert!(out.contains("Provider:   deepseek"));
        assert!(out.contains("Model:      deepseek-chat"));
        assert!(out.contains("Turns:      1"));
        assert!(out.contains("↑320"));
        assert!(out.contains("↓1.2k"));
    }

    #[test]
    fn test_print_turn_summary_includes_context() {
        let _lock = crate::ansi::tests::env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("NO_COLOR");
        }
        let mut buf = Vec::new();
        let mut view = make_view();
        view.context_window = Some(128_000);
        view.total_input_tokens = 2_560;
        view.total_output_tokens = 1_280;
        print_turn_summary(&mut buf, &view, 1, &StopReason::Completed, 2.3).unwrap();
        let out = String::from_utf8(buf).unwrap();
        // 2560+1280 = 3840; 3840/128000 = 3.0%; format_tokens rounds 2560→2.6k, 1280→1.3k
        // The "✓" symbol is wrapped in ANSI green; assert on the literal-text
        // parts to avoid coupling to the exact escape sequence placement.
        assert!(out.contains("✓"));
        assert!(out.contains("1 rounds"));
        assert!(out.contains("↑2.6k ↓1.3k tokens"));
        assert!(out.contains("2.3s"));
        assert!(out.contains("3.0%/128k"));
    }

    #[test]
    fn test_print_turn_summary_no_context_when_window_unknown() {
        let mut buf = Vec::new();
        let mut view = make_view();
        view.context_window = None;
        print_turn_summary(&mut buf, &view, 1, &StopReason::Completed, 2.3).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("2.3s\n"));
        assert!(!out.contains("%/"));
    }

    #[test]
    fn test_print_turn_summary_no_context_when_window_zero() {
        // A 0 context window is a degenerate value from a buggy upstream —
        // treat it as "unknown" and skip the suffix rather than divide by zero.
        let mut buf = Vec::new();
        let mut view = make_view();
        view.context_window = Some(0);
        print_turn_summary(&mut buf, &view, 1, &StopReason::Completed, 2.3).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(!out.contains("%/"));
    }
}
