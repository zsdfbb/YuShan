//! 会话级元数据（`SessionMeta`）——JSONL 落盘时的**首行**。
//!
//! 其余行仍是裸 `Message`（`crates/ys-core/src/message.rs`），本类型只描述首行形状。
//! 判别安全依赖两点：`Message` 无 `type` 字段；本类型的 `type` 为**必填**判别键。

use serde::{Deserialize, Serialize};

/// 判别键 `"type"` 的取值域——恒为 `"meta"`。
///
/// 单变体枚举 + **必填**（字段上不加 `serde(default)`）保证：一条合法的
/// `Message` 行（无 `type`）反序列化本类型时必然失败，从而不会产生假阳性。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum MetaKind {
    #[default]
    #[serde(rename = "meta")]
    Meta,
}

fn default_version() -> u32 {
    1
}

/// JSONL 首行的会话级元数据。
///
/// `title` 由上层截断到 500 字符后再写入，本类型不负责截断，也不额外放大字段。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SessionMeta {
    /// 判别键，必填；`Message` 无此键 ⇒ 两类型互不误判。
    #[serde(rename = "type")]
    pub kind: MetaKind,
    /// 格式版本（廉价前瞻；当前恒为 1）。
    #[serde(default = "default_version")]
    pub v: u32,
    /// 主题（首条真实用户消息的有界片段）。
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub thinking_visible: bool,
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub turns: u32,
    #[serde(default)]
    pub created_at: u64,
}

impl SessionMeta {
    /// 便捷构造：除 `created_at` 外全取默认，`v = 1`。
    pub fn new(created_at: u64) -> Self {
        Self {
            v: 1,
            created_at,
            ..Default::default()
        }
    }
}
