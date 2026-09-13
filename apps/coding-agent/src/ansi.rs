//! 最小化的内联 ANSI 辅助。
//!
//! 遵循 `NO_COLOR`（https://no-color.org/）约定——设置了该变量时返回纯文本。
//! 不做 TTY 检测——ANSI escape codes 在非 TTY 输出上无害。

fn colorize(code: &str, s: &str) -> String {
    if std::env::var_os("NO_COLOR").is_some() {
        return s.to_string();
    }
    format!("{code}{s}\x1b[0m")
}

pub fn green(s: &str) -> String {
    colorize("\x1b[32m", s)
}

pub fn yellow(s: &str) -> String {
    colorize("\x1b[33m", s)
}

pub fn red(s: &str) -> String {
    colorize("\x1b[31m", s)
}

pub fn bold(s: &str) -> String {
    colorize("\x1b[1m", s)
}

pub fn dim(s: &str) -> String {
    colorize("\x1b[2m", s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::{EnvRestore, env_lock};
    use std::env;

    /// 持有统一 env 锁且未设置 NO_COLOR 时执行闭包。
    fn with_color_enabled<F: FnOnce()>(f: F) {
        let _guard = env_lock();
        let _env = EnvRestore::capture(&["NO_COLOR"]);
        unsafe {
            env::remove_var("NO_COLOR");
        }
        f();
    }

    #[test]
    fn test_green_wraps_with_codes() {
        with_color_enabled(|| {
            let s = green("hello");
            assert!(s.starts_with("\x1b[32m"));
            assert!(s.ends_with("\x1b[0m"));
            assert!(s.contains("hello"));
        });
    }

    #[test]
    fn test_yellow_wraps_with_codes() {
        with_color_enabled(|| {
            let s = yellow("warn");
            assert!(s.starts_with("\x1b[33m"));
            assert!(s.contains("warn"));
        });
    }

    #[test]
    fn test_red_wraps_with_codes() {
        with_color_enabled(|| {
            let s = red("err");
            assert!(s.starts_with("\x1b[31m"));
        });
    }

    #[test]
    fn test_bold_wraps_with_codes() {
        with_color_enabled(|| {
            let s = bold("title");
            assert!(s.starts_with("\x1b[1m"));
        });
    }

    #[test]
    fn test_dim_wraps_with_codes() {
        with_color_enabled(|| {
            let s = dim("subtle");
            assert!(s.starts_with("\x1b[2m"));
        });
    }

    #[test]
    fn test_no_color_env_disables_codes() {
        let _guard = env_lock();
        let _env = EnvRestore::capture(&["NO_COLOR"]);
        unsafe {
            env::set_var("NO_COLOR", "1");
        }
        assert_eq!(green("plain"), "plain");
        assert_eq!(yellow("plain"), "plain");
        assert_eq!(red("plain"), "plain");
        assert_eq!(bold("plain"), "plain");
        assert_eq!(dim("plain"), "plain");
    }
}
