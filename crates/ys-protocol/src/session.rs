//! 会话目录条目的纯数据类型。
//!
//! 这是会话目录中一个条目的**纯数据**（零 tokio）：由 app 侧**只读会话文件
//! 首行**（4 KiB 封顶）得到，**不含消息体** —— 列表因此不必加载整段会话历史。

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// 会话目录中的一个条目。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    /// 最近活动时间。用 `SystemTime` 而非裸 `u64`：类型自证、无需约定单位。
    pub mtime: SystemTime,
    pub title: String,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use serde::{Deserialize, Serialize};

    use super::SessionSummary;

    /// 通用断言：值 → JSON → 值，必须与原值相等；顺带记录 JSON 形态。
    fn assert_roundtrip<T>(value: &T)
    where
        T: Serialize + for<'de> Deserialize<'de> + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(*value, back, "roundtrip 不一致，json = {json}");
    }

    fn sample_summary() -> SessionSummary {
        SessionSummary {
            id: "2026-10-10-abc123".into(),
            path: PathBuf::from("/tmp/sessions/2026-10-10-abc123.jsonl"),
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_760_000_000),
            title: "修复 UTF-8 边界".into(),
        }
    }

    /// 变异：给 `mtime` 加 `#[serde(skip, default = "std::time::SystemTime::now")]`
    /// （序列化跳过该字段、反序列化用 `now()` 兜底）⇒ 本用例在**运行期**变红：
    /// 往返后 `mtime` 被默认值覆盖，或把 `mtime` 换成裸 `u64` 且不更新断言也变红。
    /// 断言 `PathBuf` / `SystemTime` 字段真的经 serde 往返，而非被默认值吞掉。
    ///
    /// 裸 `#[serde(skip)]` 不适用：`SystemTime` 未实现 `Default`，那样是**编译
    /// 错误**（`E0277`），跑不到断言；故本变异须显式给 `default`。
    #[test]
    fn test_session_summary_roundtrip_pathbuf_and_mtime() {
        assert_roundtrip(&sample_summary());
    }
}
