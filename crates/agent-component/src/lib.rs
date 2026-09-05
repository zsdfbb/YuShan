//! agent-component: 运行时上下文容器

mod context;
mod limits;

pub use context::*;
pub use limits::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_limits_default() {
        let limits = RunLimits::default();
        assert_eq!(limits.max_rounds, 10);
    }

    #[test]
    fn test_run_limits_custom() {
        let limits = RunLimits::new(5);
        assert_eq!(limits.max_rounds, 5);
    }
}
