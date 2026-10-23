//! 会话级元数据（`SessionMeta`）——JSONL 落盘时的**首行**。
//!
//! 其余行仍是裸 `Message`（`crates/ys-core/src/message.rs`），本类型只描述首行形状。
//! 判别安全依赖两点：`Message` 无 `type` 字段；本类型的 `type` 为**必填**判别键。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ys_core::EffortLevel;

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
    /// 会话的思考档位（`None` = 未设置 / provider 默认）。
    ///
    /// `#[serde(default)]` 是回写兼容契约：早期 JSONL 首行无此键 ⇒ `None`。
    #[serde(default)]
    pub effort: Option<EffortLevel>,
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
    #[serde(default)]
    pub turns: u32,
    #[serde(default)]
    pub created_at: u64,
    /// 本会话绑定的工作树绝对路径（`worktree attach` 或从记录恢复时写）。
    ///
    /// `None` = 未绑定（若曾绑定而树已删，恢复期以「已不存在」告知后按共享工作区启动）。
    /// `#[serde(default)]` 是**回写兼容契约**：早期 JSONL 首行无此键 ⇒ `None`，仍可反序列化。
    #[serde(default)]
    pub workspace: Option<PathBuf>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// `workspace` 是**可缺省**字段：无该键的旧 JSONL 首行照常反序列化 ⇒ `None`；
    /// 带该键 ⇒ `Some`。这是本结构加字段时唯一的兼容保证点。
    #[test]
    fn workspace_is_optional_and_backward_compatible() {
        // 旧首行：完全不含 `workspace` 键（本结构加字段之前落盘的形状）。
        let old = r#"{"type":"meta","v":1,"title":"t","provider":"p","model":"m",
            "thinking_visible":false,"input_tokens":3,"output_tokens":4,"turns":1,"created_at":42}"#;
        let meta: SessionMeta = serde_json::from_str(old).expect("旧首行应仍可反序列化");
        assert_eq!(meta.workspace, None, "无 workspace 键 ⇒ None");
        assert_eq!(meta.title.as_deref(), Some("t"), "其余字段不受影响");

        // 新首行：带 `workspace` 键 ⇒ Some。
        let with = r#"{"type":"meta","workspace":"/tmp/yushan/tree","created_at":7}"#;
        let meta: SessionMeta = serde_json::from_str(with).unwrap();
        assert_eq!(
            meta.workspace.as_deref(),
            Some(Path::new("/tmp/yushan/tree"))
        );

        // 显式 `null` 亦按 None 处理（serde 的 Option 语义）。
        let null = r#"{"type":"meta","workspace":null,"created_at":7}"#;
        assert_eq!(
            serde_json::from_str::<SessionMeta>(null).unwrap().workspace,
            None
        );
    }

    /// `effort` 的 serde 往返：`Some(High)` 必须精确回到原值。
    #[test]
    fn effort_serde_roundtrip() {
        let mut meta = SessionMeta::new(7);
        meta.effort = Some(EffortLevel::High);
        let json = serde_json::to_string(&meta).unwrap();
        assert!(
            json.contains("\"effort\":\"high\""),
            "档位线拼写必须为小写：{json}"
        );
        let back: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.effort, Some(EffortLevel::High));
    }

    /// `effort` 是**可缺省**字段：无该键的旧 JSONL 首行照常反序列化 ⇒ `None`。
    ///
    /// `Option<EffortLevel>` 的缺键语义本就是 `None`；`#[serde(default)]` 是显式的
    /// 可缺省意图表达，与结构体中其余可缺省字段（如 `workspace`）风格一致。
    #[test]
    fn effort_is_optional_and_backward_compatible() {
        let old = r#"{"type":"meta","v":1,"title":"t","provider":"p","model":"m",
            "thinking_visible":false,"input_tokens":3,"output_tokens":4,"turns":1,"created_at":42}"#;
        let meta: SessionMeta = serde_json::from_str(old).expect("旧首行应仍可反序列化");
        assert_eq!(meta.effort, None, "无 effort 键 ⇒ None");
        assert_eq!(meta.title.as_deref(), Some("t"), "其余字段不受影响");

        // 显式 `null` 亦按 None 处理（serde 的 Option 语义）。
        let null = r#"{"type":"meta","effort":null,"created_at":7}"#;
        assert_eq!(
            serde_json::from_str::<SessionMeta>(null).unwrap().effort,
            None
        );
    }
}
