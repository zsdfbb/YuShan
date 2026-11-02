//! 状态行的**现在时间**（本地时区）。
//!
//! 只做一件事：`HH:MM` 字符串。时区偏移取自平台（`localtime_r` 的 `tm_gmtoff`），
//! 因为 `std::time` 只有 UTC 秒；取不到偏移时**退回 UTC**（宁可偏几小时，也不让
//! 状态行报一个假的本地时间）。
//!
//! 纯算术部分（[`hhmm_at`]）与平台调用分开，故时钟行为可在测试里钉死 —— 断言不依赖
//! 「运行机器在哪个时区」「跑测试时几点」。

use std::time::{SystemTime, UNIX_EPOCH};

/// 现在时间（本地 `HH:MM`）：状态行的钟点段。
///
/// 生产路径每次重绘现算（事件循环空闲也每 `POLL_INTERVAL` 重绘）；测试注入
/// [`App::clock_now`] 以免断言依赖机器时区与当前时刻。
pub(crate) fn now_hhmm() -> String {
    now_hhmm_at(SystemTime::now())
}

/// 纯函数入口：把「何时取时间」与「怎么格式化」分开，测试直接钉后者。
pub(crate) fn now_hhmm_at(now: SystemTime) -> String {
    let unix_secs = now
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    hhmm_at(unix_secs, local_utc_offset_secs())
}

/// Unix 秒 + 偏移 ⇒ `HH:MM`。
///
/// `rem_euclid` 而非 `%`：西半球（负偏移）的凌晨会把秒数推到 0 以下，
/// `%` 会给出负余数、进而格式化出 `-5:30` 这种时间。
pub(super) fn hhmm_at(unix_secs: u64, utc_offset_secs: i64) -> String {
    let secs_of_day = (unix_secs as i64 + utc_offset_secs).rem_euclid(86_400);
    format!("{:02}:{:02}", secs_of_day / 3600, (secs_of_day % 3600) / 60)
}

/// 本机时区相对 UTC 的偏移（秒）。取不到时 `0`（= 按 UTC 显示）。
///
/// `unnecessary_cast`：`tm_gmtoff` 在 64 位上是 `c_long = i64`（本平台恒等），但在
/// 32 位目标上是 `i32` —— 不写 `as i64` 就编不过，写了在本机就是恒等转换。
#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
fn local_utc_offset_secs() -> i64 {
    use std::mem::MaybeUninit;

    // SAFETY：`localtime_r` 读写的是本栈上的 `time_t` 与 `tm`，两者指针都有效；
    // 它只写 `tm` 指向的内存，成功时原样返回该指针、失败时返回空（此时不读 `tm`）。
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        if now == -1 {
            return 0;
        }
        let mut tm = MaybeUninit::<libc::tm>::uninit();
        if libc::localtime_r(&now, tm.as_mut_ptr()).is_null() {
            return 0;
        }
        // 32 位目标上 `tm_gmtoff` 是 `i32`，故保留这层转换（见函数文档）。
        tm.assume_init().tm_gmtoff as i64
    }
}

/// 非 unix：`std` 无时区信息，按 UTC 显示。
#[cfg(not(unix))]
fn local_utc_offset_secs() -> i64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 整点与分钟补零：`14:03` 不能是 `14:3`。
    ///
    /// 变异：把 `{:02}` 改回 `{}` ⇒ 第二条断言变红。
    #[test]
    fn hhmm_pads_and_reads_the_clock() {
        assert_eq!(hhmm_at(0, 0), "00:00");
        assert_eq!(hhmm_at(14 * 3600 + 3 * 60, 0), "14:03");
        assert_eq!(hhmm_at(23 * 3600 + 59 * 60 + 59, 0), "23:59");
    }

    /// 东半球偏移（`+08:00`）：UTC 零点即本地 08:00。
    ///
    /// 变异：偏移取反 ⇒ 变红。
    #[test]
    fn hhmm_applies_east_offset() {
        assert_eq!(hhmm_at(0, 8 * 3600), "08:00");
        assert_eq!(hhmm_at(6 * 3600, 8 * 3600), "14:00");
    }

    /// 负偏移跨日：`+00:00` 的 00:30 在 `-05:00` 是**前一天** 19:30。
    ///
    /// 变异：`rem_euclid` 换成 `%` ⇒ 得到负余数、格式化出 `-5:30`，断言变红。
    #[test]
    fn hhmm_wraps_into_previous_day_for_west_offsets() {
        assert_eq!(hhmm_at(30 * 60, -5 * 3600), "19:30");
    }

    /// 跨日边界：23:59:59 + 1 s 回到 00:00。
    #[test]
    fn hhmm_wraps_at_midnight() {
        assert_eq!(hhmm_at(86_400, 0), "00:00");
        assert_eq!(hhmm_at(86_400 - 1, 0), "23:59");
    }

    /// 生产入口不 panic，且形态恒为 `HH:MM`（5 个 ASCII 字节）。
    ///
    /// 注入的已知时刻 + 真实本机偏移 ⇒ 只断言形态，不断言具体钟点（那取决于时区）。
    #[test]
    fn now_hhmm_has_fixed_shape() {
        let text = now_hhmm_at(UNIX_EPOCH + std::time::Duration::from_secs(1_769_000_000));
        assert_eq!(text.len(), 5, "{text}");
        assert_eq!(text.as_bytes()[2], b':', "{text}");
        assert!(
            text.chars().filter(char::is_ascii_digit).count() == 4,
            "{text}"
        );
        // 生产入口本身：只要求不 panic 且形态一致。
        let live = now_hhmm();
        assert_eq!(live.len(), 5, "{live}");
    }
}
