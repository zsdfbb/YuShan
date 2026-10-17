//! 思考档位（`/effort` 的规范取值）。
//!
//! 这是请求侧 `reasoning_effort` / `thinking` 的**唯一拼写来源**；
//! 各 provider 的 wire 方言由模型适配器翻译，本类型不感知 provider。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// 模型思考档位（`/effort` 的规范取值）。
///
/// 这是请求侧 `reasoning_effort` / `thinking` 的唯一拼写来源；
/// 各 provider 的 wire 方言由模型适配器翻译，本类型不感知 provider。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl EffortLevel {
    /// 规范线拼写。serde 的 `rename_all = "lowercase"` 与本函数必须保持一致——
    /// 二者是同一个对外契约的两种呈现（JSONL / 协议 vs. provider 参数）。
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for EffortLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// `FromStr` 的解析错误（输入不是七个规范拼写之一）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("非法思考档位 {input:?}；合法值：off | minimal | low | medium | high | xhigh | max")]
pub struct ParseEffortLevelError {
    /// 被拒绝的原始输入（保留原样，便于上层诊断）。
    pub input: String,
}

impl FromStr for EffortLevel {
    type Err = ParseEffortLevelError;

    /// **严格**接受七个规范拼写（大小写不敏感，比较前 trim）。
    ///
    /// 刻意不接受 `default`：`default` 是上层「清除设置」的令牌，
    /// 由 `Option<EffortLevel>` 的 `None` 表达，不由枚举值表达。
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "off" => Ok(Self::Off),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            _ => Err(ParseEffortLevelError {
                input: s.to_owned(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [EffortLevel; 7] = [
        EffortLevel::Off,
        EffortLevel::Minimal,
        EffortLevel::Low,
        EffortLevel::Medium,
        EffortLevel::High,
        EffortLevel::XHigh,
        EffortLevel::Max,
    ];

    /// 七个档位逐一 `parse → as_wire → to_string` 往返，且解析结果与原值相等。
    ///
    /// 变异：把 `as_wire` 中任意一个拼写改掉（如 `"xhigh"` → `"x-high"`）
    /// ⇒ 本用例变红（`parse` 不再接受该输出），`serde` 往返用例也会因
    /// 规范拼写与 `rename_all` 不一致而暴露。
    #[test]
    fn test_all_levels_parse_and_display_roundtrip() {
        for level in ALL {
            let wire = level.as_wire();
            assert_eq!(level.to_string(), wire, "Display 必须等于 as_wire");
            let parsed: EffortLevel = wire.parse().unwrap();
            assert_eq!(parsed, level, "{wire} 应解析回原档位");
        }
    }

    /// serde 的 JSON 拼写是落盘 / 协议契约，必须逐档与 `as_wire` 一致。
    ///
    /// 变异：删掉 `#[serde(rename_all = "lowercase")]`（如 `XHigh` 变成 `"XHigh"`），
    /// 或改掉 `as_wire` 中任一拼写 ⇒ 下面的逐档断言变红。
    #[test]
    fn test_serde_json_roundtrip() {
        for level in ALL {
            assert_eq!(
                serde_json::to_value(level).unwrap(),
                serde_json::json!(level.as_wire()),
                "serde 拼写必须与 as_wire 一致：{level:?}"
            );
            let json = serde_json::to_string(&level).unwrap();
            let back: EffortLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(back, level, "{json} 应反序列化回原档位");
        }
    }

    /// `default` 是「清除设置」令牌，不是档位；非法拼写必须报错并列出合法值。
    #[test]
    fn test_invalid_values_rejected() {
        for bad in ["ultra", "default", ""] {
            let err = bad.parse::<EffortLevel>().unwrap_err();
            assert_eq!(err.input, bad, "错误应保留原始输入");
            let msg = err.to_string();
            for legal in ["off", "minimal", "low", "medium", "high", "xhigh", "max"] {
                assert!(msg.contains(legal), "错误消息应列出合法值 {legal}：{msg}");
            }
        }
    }

    /// 大小写不敏感，且 trim 前后空白。
    #[test]
    fn test_case_insensitive_parse() {
        assert_eq!("HIGH".parse::<EffortLevel>().unwrap(), EffortLevel::High);
        assert_eq!(
            "  XHigh  ".parse::<EffortLevel>().unwrap(),
            EffortLevel::XHigh
        );
        assert_eq!("Off".parse::<EffortLevel>().unwrap(), EffortLevel::Off);
    }
}
