//! 「杀进程组」的共享内核 —— 本 crate 内**只有这一份**实现。
//!
//! 三个调用方都需要「连孙进程一起杀掉」这一动作：工具内超时/中止
//! （[`crate::bash`]）、有界子进程（[`crate::exec`]）与后台命令
//! （[`crate::background`]）。三份实现会漂移，故内核落在本模块，
//! 调用方各自包一层外壳。
//!
//! 调用方拿到的内核有两种形态：
//!
//! - [`kill_group`] —— **同步**内核（零 tokio 依赖），后台命令模块用。
//! - [`kill_group_async`] —— 同一内核的**异步外壳**，把同步内核放进阻塞线程池，
//!   使 async 调用方不必在自己的 worker 线程上同步等 `sh` 退出。

#[cfg(unix)]
use std::process::{Command, Stdio};

/// 尽力终止整个进程组（spawn 时已 `process_group(0)`，故进程组 id == 子进程 pid）。
///
/// **调用方必须在 `wait()` 收尸之前调用**：leader 一旦被收尸，
/// 它的 pid 就可能被复用，`kill -9 -<pid>` 便可能打到无关的进程组。
///
/// 不引入 `libc` 依赖：借助本工具已依赖的 `sh` 内建 `kill`，向负 pid（= 进程组）
/// 发 `SIGKILL`。仅覆盖 `sh` fork 出的孙进程（如 `sh -c "npm test"` 的 node 子进程）；
/// 失败时静默 —— 调用方仍有 `Child::start_kill()` 兜底。
#[cfg(unix)]
pub(crate) fn kill_group(pid: u32) {
    let _ = Command::new("sh")
        .arg("-c")
        .arg(format!("kill -9 -{pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// [`kill_group`] 的异步外壳 —— 同一份内核，只是把同步的 `sh` 等待挪进阻塞线程池。
///
/// 语义与内核一致（失败静默）；**先杀后收**的告诫同样适用（见 [`kill_group`]）。
#[cfg(unix)]
pub(crate) async fn kill_group_async(pid: u32) {
    let _ = tokio::task::spawn_blocking(move || kill_group(pid)).await;
}
