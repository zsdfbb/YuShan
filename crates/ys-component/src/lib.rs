//! ys-component: 运行时上下文容器

mod compact;
mod context;
mod limits;

pub use compact::*;
pub use context::*;
pub use limits::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_limits_default() {
        let limits = RunLimits::default();
        assert_eq!(limits.max_rounds, 10);
        // 压缩策略默认挂上（阈值数据随 limits 走）
        assert_eq!(limits.compaction, CompactPolicy::default());
    }

    #[test]
    fn test_run_limits_custom() {
        let limits = RunLimits::new(5);
        assert_eq!(limits.max_rounds, 5);
    }
}
