use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ys_event::AgentEvent;

/// 事件来源。
///
/// `Arc<str>` 使 `clone` 只做一次原子加（避免每事件一次堆分配），
/// 同时序列化为可读字符串（`--json` 需要）。（设计修订 R7）
///
/// serde 手写而非 derive：`Arc<str>` 的 `Deserialize` 需 serde 的 `rc` feature，
/// 而该 feature 会经 feature 统一扩散到全工作区；手写只影响本类型，
/// 且保证序列化形如 `"agent"`（裸字符串）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source(Arc<str>);

impl Source {
    /// 单 agent 阶段的唯一来源。
    #[inline]
    pub fn agent() -> Self {
        Self("agent".into())
    }
}

impl Default for Source {
    fn default() -> Self {
        Self::agent()
    }
}

impl Serialize for Source {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self(Arc::from(s)))
    }
}

/// 信道传输单位：谁发的、哪一轮、什么事。
///
/// 「信纸」是 `AgentEvent` 不改；信封只加来源与分组（`source` / `turn`）。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub source: Source,
    pub turn: u32,
    pub event: AgentEvent,
}

impl Envelope {
    /// 便捷构造：来源 + 回合号 + 事件。
    #[inline]
    pub fn new(source: Source, turn: u32, event: AgentEvent) -> Self {
        Self {
            source,
            turn,
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_agent_and_default() {
        assert_eq!(Source::agent(), Source::default());
        assert_eq!(Source::default(), Source::agent());
    }

    #[test]
    fn test_envelope_serde_roundtrip() {
        let env = Envelope::new(
            Source::agent(),
            3,
            AgentEvent::ModelTextDelta { text: "hi".into() },
        );
        let json = serde_json::to_string(&env).unwrap();
        // Source 序列化为可读字符串（--json 依赖）
        assert!(json.contains("\"agent\""), "json = {json}");
        let back: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(env, back);
    }

    #[test]
    fn test_envelope_new_sets_fields() {
        let env = Envelope::new(
            Source::agent(),
            7,
            AgentEvent::RunFailed {
                error: "boom".into(),
            },
        );
        assert_eq!(env.turn, 7);
        assert_eq!(env.source, Source::agent());
        assert!(matches!(env.event, AgentEvent::RunFailed { .. }));
    }
}
