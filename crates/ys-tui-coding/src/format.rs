//! 数值与时长的小格式化（纯函数，无状态）。

/// 压缩后的 token 计数：`320` / `1.2k` / `3.4M`。
///
/// 单位按**四舍五入后**的量级选择 —— 否则 `999_999` 会渲染成 `1000.0k`
/// 而不进位到 `M`（v1 曾有的显示瑕疵）。
pub fn format_tokens(n: u32) -> String {
    let n = n as f64;
    // `{:.1}` 会把 >= 999_950 的值进位到 1000.0k，故阈值取在四舍五入点之前
    if n >= 999_950.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 999.5 {
        format!("{:.1}k", n / 1_000.0)
    } else {
        format!("{}", n as u32)
    }
}

/// 会话时长：`Ns` / `Nm Ms` / `Nh Mm`（设计 §2.2 的 `1m23s` 形态）。
///
/// 最小单位是秒 —— 秒以下的精度对「会话开了多久」没有意义。
pub fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_tokens_tiers() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(345), "345", "<1000 原样");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.0k");
        assert_eq!(format_tokens(1_247), "1.2k");
        assert_eq!(format_tokens(1_000_000), "1.0M");
    }

    /// 阶梯**边界**必须钉死（进位点两侧）。
    ///
    /// 单位按四舍五入后的量级选择：`999_999` → `1.0M`（不是 `1000.0k`）。
    #[test]
    fn test_format_tokens_boundaries() {
        assert_eq!(format_tokens(999), "999", "k 阈值前一个数原样");
        assert_eq!(format_tokens(1_000), "1.0k", "k 阈值本身");
        assert_eq!(format_tokens(1_200), "1.2k");
        assert_eq!(format_tokens(9_994), "10.0k", "1 位小数四舍五入进位");
        assert_eq!(format_tokens(10_000), "10.0k");
        assert_eq!(format_tokens(999_949), "999.9k", "M 阈值前的最后一个 k 值");
        assert_eq!(format_tokens(999_950), "1.0M", "M 阈值本身（进位点）");
        assert_eq!(format_tokens(999_999), "1.0M", "四舍五入后应进位到 M");
        assert_eq!(format_tokens(1_000_000), "1.0M", "M 阈值本身");
        assert_eq!(format_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn test_format_duration_tiers() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(59), "59s");
        assert_eq!(format_duration(83), "1m23s");
        assert_eq!(format_duration(3599), "59m59s");
        assert_eq!(format_duration(3661), "1h1m");
    }
}
