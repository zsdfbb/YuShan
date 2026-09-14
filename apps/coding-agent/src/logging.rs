//! 极简日志文件（设计 §5「输出与诊断的去处」）。
//!
//! 去处三分里，**库内部诊断**（`auth.json` / `state.json` 解析失败）没有
//! 「历史输出」可看：TUI 起来之后 stderr 会冲掉整屏。故给它们一个落处 ——
//! `~/.yushan/logs/yushan.log`，追加写。
//!
//! ```text
//! [1757841234] Warning: failed to parse auth.json: expected value at line 1 column 1
//! ```
//!
//! **best-effort**：目录创建 / 打开 / 写入的**任何** IO 错误一律静默忽略 ——
//! 日志失败绝不能影响调用方（返回值 `()`，不 panic、不传播错误）。
//!
//! **不引新依赖**：只有两个调用点，用 `tracing` 是杀鸡用牛刀。时间戳用
//! `SystemTime` 的 unix 秒（不引 `chrono`）—— 定位「什么时候出的问题」够用。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 默认日志文件：`~/.yushan/logs/yushan.log`（无 HOME 时为 `None`）。
pub fn default_log_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(".yushan")
            .join("logs")
            .join("yushan.log"),
    )
}

/// 追加一条日志到默认路径。
///
/// 路径不可得（无 HOME）时静默返回 —— 日志永远不该成为失败的来源。
pub fn log(msg: &str) {
    if let Some(path) = default_log_path() {
        log_to(&path, msg);
    }
}

/// 追加一条日志到**指定**路径。`log()` 调它 + 默认路径；测试直接调它。
pub fn log_to(path: &Path, msg: &str) {
    // 目录不存在则自建（父目录即 `~/.yushan`）。失败即放弃本次日志。
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    // 写失败同样静默：调用方在读 auth.json / state.json 的中途，不该被日志打断。
    let _ = writeln!(file, "{} {msg}", timestamp());
}

/// 时间戳前缀 `[unix秒]`。
///
/// 系统时钟早于 epoch（不可能，但类型上存在）时退化为 `[?]`，不 panic。
fn timestamp() -> String {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => format!("[{}]", d.as_secs()),
        Err(_) => "[?]".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 测试目录名含**进程内原子序号 + `process::id()`**：并行测试不得撞名。
    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "yushan_logging_{name}_{}_{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 追加语义：两次调用 → 两行（第二次不得覆盖第一次）。
    #[test]
    fn test_log_appends_two_lines() {
        let dir = temp_dir("append");
        let path = dir.join("yushan.log");

        log_to(&path, "first");
        log_to(&path, "second");

        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "两次追加应为两行：{content:?}");
        assert!(lines[0].ends_with("first"), "{}", lines[0]);
        assert!(lines[1].ends_with("second"), "{}", lines[1]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目录不存在则自建（`~/.yushan/logs/` 那条路径）。
    #[test]
    fn test_log_creates_missing_parent_dirs() {
        let dir = temp_dir("mkdir");
        let path = dir.join("nested").join("deeper").join("yushan.log");
        assert!(!path.parent().unwrap().exists());

        log_to(&path, "hello");

        assert!(path.exists(), "应自建父目录并落盘");
        assert!(std::fs::read_to_string(&path).unwrap().contains("hello"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目标不可写（此处指向一个**目录**）→ 不 panic、不返回错误、调用方照常继续。
    ///
    /// 变异：把 `let Ok(mut file) = … else { return }` 改成 `.unwrap()` → 本测试
    /// panic（打开目录写入在 Unix/macOS 上必然失败）。
    #[test]
    fn test_log_to_unwritable_target_is_silent() {
        let dir = temp_dir("unwritable");
        // 打开目录作为文件写入在 Unix/macOS 上必然失败。
        log_to(&dir, "should be swallowed");
        // 走到这里没 panic 就是通过；目录内容不受影响。
        assert!(dir.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 父路径是个**普通文件**（`create_dir_all` 必然失败）→ 同样静默吞掉，
    /// 不 panic、不留下半截文件。
    ///
    /// 变异：把 `create_dir_all(parent).is_err()` 的早退改成 `unwrap()` → 本测试
    /// 变红。
    #[test]
    fn test_log_silent_when_parent_is_a_file() {
        let dir = temp_dir("parent_is_file");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"not a dir").unwrap();
        let path = blocker.join("yushan.log");

        log_to(&path, "should be swallowed");

        assert!(blocker.is_file(), "阻塞文件本身不受影响");
        assert!(!path.exists(), "不得在文件下造出日志");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 无 HOME 的默认路径：返回 `None` 而不是 panic。
    #[test]
    fn test_default_log_path_without_home() {
        let _guard = crate::test_env::env_lock();
        let _env = crate::test_env::EnvRestore::capture(&["HOME", "USERPROFILE"]);
        // SAFETY: 持有 env_lock，独占这两个变量的修改与恢复。
        unsafe {
            std::env::remove_var("HOME");
            std::env::remove_var("USERPROFILE");
        }
        assert_eq!(default_log_path(), None);
        log("no-op"); // 不得 panic
    }

    /// 每行以 `[unix秒] ` 开头，且秒数可解析、量级正确（不是 0、不是纳秒）。
    #[test]
    fn test_log_lines_carry_unix_second_prefix() {
        let dir = temp_dir("timestamp");
        let path = dir.join("yushan.log");
        log_to(&path, "msg");

        let content = std::fs::read_to_string(&path).unwrap();
        let line = content.lines().next().unwrap();
        let rest = line
            .strip_prefix('[')
            .expect("应以 `[` 开头")
            .split_once(']')
            .expect("应有闭合 `]`")
            .0;
        let secs: u64 = rest.parse().expect("前缀应为 unix 秒");
        // 2020-01-01 之后、且远小于纳秒级（纳秒约 1.7e18）。
        assert!(secs > 1_577_836_800, "秒数应大于 2020 年：{secs}");
        assert!(secs < 10_000_000_000, "应是秒而非纳秒：{secs}");
        assert_eq!(line, format!("[{secs}] msg"), "{line}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
