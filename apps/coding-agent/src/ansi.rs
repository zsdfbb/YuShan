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
pub(crate) mod tests {
    use super::*;
    use std::env;
    use std::sync::{Mutex, OnceLock};

    /// 进程级锁，用于修改 `NO_COLOR` 环境变量的测试。
    /// 防止并行测试竞争（Rust 默认并行运行测试）。
    /// 通过 `crate::ansi::tests::env_lock` 跨模块共享——必须是
    /// 单一 static，使所有测试在同一 mutex 上串行。
    pub fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// 辅助：为测试作用域设置 NO_COLOR；结束时恢复。
    /// 在 guard 存活期内持有 `env_lock()`，使并发环境变量修改
    /// 不会与本测试竞争。
    struct NoColorGuard {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl NoColorGuard {
        fn new() -> Self {
            let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
            let previous = env::var_os("NO_COLOR");
            unsafe {
                env::set_var("NO_COLOR", "1");
            }
            NoColorGuard { previous, _lock }
        }
    }
    impl Drop for NoColorGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(v) => unsafe {
                    env::set_var("NO_COLOR", v);
                },
                None => unsafe {
                    env::remove_var("NO_COLOR");
                },
            }
        }
    }

    /// 持有 env lock 且未设置 NO_COLOR 时执行闭包。
    fn with_color_enabled<F: FnOnce()>(f: F) {
        let _lock = env_lock().lock().unwrap_or_else(|e| e.into_inner());
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
        let _g = NoColorGuard::new();
        assert_eq!(green("plain"), "plain");
        assert_eq!(yellow("plain"), "plain");
        assert_eq!(red("plain"), "plain");
        assert_eq!(bold("plain"), "plain");
        assert_eq!(dim("plain"), "plain");
    }
}
