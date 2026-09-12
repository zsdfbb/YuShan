/// 消费者消失后的生命周期策略。
///
/// 第三种策略若出现，加变体即可（`#[non_exhaustive]` 已备）。
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecyclePolicy {
    /// 消费者消失 → 干完当前轮收摊（交互式默认）
    StopWhenConsumerGone,
    /// 消费者消失 → 继续跑（事件落盘兜底；后台长任务）
    ContinueWithoutConsumer,
}

impl Default for LifecyclePolicy {
    fn default() -> Self {
        Self::StopWhenConsumerGone
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lifecycle_policy_default() {
        assert_eq!(
            LifecyclePolicy::default(),
            LifecyclePolicy::StopWhenConsumerGone
        );
    }

    #[test]
    fn test_lifecycle_policy_construct_and_compare() {
        let a = LifecyclePolicy::StopWhenConsumerGone;
        let b = LifecyclePolicy::ContinueWithoutConsumer;
        assert_ne!(a, b);
        assert_eq!(a, LifecyclePolicy::StopWhenConsumerGone);
        // Copy：赋值后原值仍可用
        let c = a;
        assert_eq!(c, a);
    }
}
