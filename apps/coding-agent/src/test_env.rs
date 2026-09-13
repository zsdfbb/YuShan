//! 测试用的进程级环境变量互斥与恢复。
//!
//! Rust 测试默认并行执行，而进程环境变量是全局共享状态。`set_var` /
//! `remove_var` 与并发的 `getenv` 之间存在数据竞争（Rust 2024 因此把二者
//! 标记为 `unsafe`）。此模块提供**唯一**的进程级 env 锁：所有会改写环境
//! 变量的测试都必须走 [`env_lock`]，并在改写前用 [`EnvRestore`] 记录原值、
//! `Drop` 时恢复。
//!
//! 仅在 `cfg(test)` 下编译，不进入生产二进制。

use std::sync::{Mutex, MutexGuard, OnceLock};

/// 获取进程级 env 锁。
///
/// 所有改写环境变量的测试共享同一把锁，从而彼此串行。跨模块复用必须走
/// 这里，不得各自 `static` 一把新锁——否则两个模块的 env 改写不会互相串行。
///
/// 返回的 guard 在存活期内独占 env 改写权；中毒时取回内部值继续（测试
/// 中断言失败导致的中毒不应让后续测试连锁失败）。
pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 记录若干环境变量的原值，`Drop` 时恢复。
///
/// 用 `Drop` 而非测试末尾手动恢复：断言 panic 时也能恢复，避免污染同
/// binary 内其他读 env 的测试。
///
/// 调用方必须先持有 [`env_lock`]（本结构不自行加锁，以便一个测试跨越
/// 多次改写只加锁一次）。
pub(crate) struct EnvRestore {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvRestore {
    /// 记录 `keys` 在当前进程中的原值。
    pub(crate) fn capture(keys: &[&'static str]) -> Self {
        EnvRestore {
            saved: keys.iter().map(|k| (*k, std::env::var_os(k))).collect(),
        }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        // SAFETY: 调用方持有 env_lock，独占进程环境变量的修改与恢复。
        unsafe {
            for (key, value) in &self.saved {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}
