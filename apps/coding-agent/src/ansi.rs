//! Minimal inline ANSI helpers.
//!
//! Honours `NO_COLOR` (https://no-color.org/) — if set, returns plain text.
//! No TTY detection — ANSI escape codes are harmless on non-TTY output.

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
pub(crate) mod tests {
    use super::*;
    use std::env;
    use std::sync::{Mutex, OnceLock};

    /// Process-wide lock for tests that mutate the `NO_COLOR` env var.
    /// Prevents parallel-test races (Rust runs tests in parallel by default).
    /// Shared across modules via `crate::ansi::tests::env_lock` — must be a
    /// single static so all tests serialize on the same mutex.
    pub fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Helper: set NO_COLOR for the test scope; restore at end.
    /// Holds `env_lock()` for the guard's lifetime so concurrent env-var
    /// mutations cannot race with this test.
    struct NoColorGuard {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl NoColorGuard {
        fn new() -> Self {
            let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
            let previous = env::var_os("NO_COLOR");
            unsafe { env::set_var("NO_COLOR", "1"); }
            NoColorGuard { previous, _lock }
        }
    }
    impl Drop for NoColorGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => unsafe { env::set_var("NO_COLOR", v); },
                None => unsafe { env::remove_var("NO_COLOR"); },
            }
        }
    }

    /// Wrap a closure with the env lock held and NO_COLOR unset.
    fn with_color_enabled<F: FnOnce()>(f: F) {
        let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        unsafe { env::remove_var("NO_COLOR"); }
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
        let _g = NoColorGuard::new();
        assert_eq!(green("plain"), "plain");
        assert_eq!(yellow("plain"), "plain");
        assert_eq!(red("plain"), "plain");
        assert_eq!(bold("plain"), "plain");
        assert_eq!(dim("plain"), "plain");
    }
}
