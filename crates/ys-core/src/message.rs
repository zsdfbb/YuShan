use serde::{Deserialize, Serialize};

use crate::ToolCallId;
use crate::media::ImageDetail;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
    /// harness 产生、**用户没有说过**的输入：后台命令完成的结果、子 agent 的结果。
    ///
    /// 与 [`Role::User`] 的区别**只在本地**：落盘、回放、统计都据它区分来源，
    /// 模型看到的仍旧是 user 角色（适配器在出站时映射）。
    Harness,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: ToolCallId,
        name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        tool_call_id: ToolCallId,
        content: String,
        is_error: bool,
    },
    /// 图片引用。**只存路径，不存字节** —— 字节在适配器出站时现读。
    ///
    /// 落盘形态是外部标签 `{"Image":{"path":…,"detail":…}}`；`detail` 为 `None` 时
    /// 该键**不出现**（省略等价于用默认档位，写 `null` 会与 [`crate::MediaAttachment`] 分叉）。
    Image {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<ImageDetail>,
    },
}

impl ContentBlock {
    /// 该块携带的正文；非正文块返回 `None`。
    ///
    /// **穷尽 match 是有意的**：本方法在定义 crate 内，新增变体时这里会编译失败，
    /// 从而强制作者决定「新变体算不算正文」。外部 crate 的 `_`/`if let` 通配臂没有
    /// 这个保护（新增变体静默落入 `None`），所以**所有取正文的调用点都应经由此方法**。
    ///
    /// 语义边界：`ToolUse` / `ToolResult` 携带的是**工具数据**而不是可展示的正文 ——
    /// `token` 估算等场景要单独计价（参数 JSON / 结果长度各自口径不同），不归本方法。
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::ToolUse { .. } => None,
            ContentBlock::ToolResult { .. } => None,
            ContentBlock::Image { .. } => None,
        }
    }
}

/// 正文块之间的分隔符 —— **调用方的语义选择**，故显式枚举而不是裸 `&str`。
///
/// 裸字符串会让分隔符在各调用点自由漂移（`"\n"` / `" "` / `""` 随手写），
/// 而那正是「只统一块选择规则」要防的另一半病。三档各自对应真实场景：
///
/// | 变体 | 分隔 | 场景 |
/// |---|---|---|
/// | `Newline` | `"\n"` | 界面回显 / 多块回答分行展示 |
/// | `Space` | `" "` | 单行标题（把多块压成一句话） |
/// | `None` | `""` | 摘要与流式响应 —— 必须无缝 |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextJoin {
    Newline,
    Space,
    None,
}

impl TextJoin {
    /// 本档位的实际分隔符。
    ///
    /// 穷尽 match（无 `_` 臂）：新增档位时必须在这里决定它的字节。
    pub fn separator(self) -> &'static str {
        match self {
            TextJoin::Newline => "\n",
            TextJoin::Space => " ",
            TextJoin::None => "",
        }
    }
}

impl Message {
    /// 按 `join` 拼接**全部**正文块；非正文块跳过。
    ///
    /// ⚠️ 本方法只统一**「哪些块算正文」**，分隔符仍归调用方选择（D16）：界面回显要换行、
    /// 标题要空格、摘要要无缝，统一任何一个都会改动现有输出字节。
    ///
    /// 内部的 `filter_map` **必须**走 [`ContentBlock::as_text`]：那是在定义 crate 里
    /// 穷尽 match 的唯一入口，在这里重新 match 就把新增变体的保护丢掉了。
    pub fn joined_text(&self, join: TextJoin) -> String {
        self.content
            .iter()
            .filter_map(ContentBlock::as_text)
            .collect::<Vec<_>>()
            .join(join.separator())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 图片引用的落盘形态是**对外契约**：会话 JSONL 由它组成，`/resume` 由它回放。
    /// `detail` 的小写拼写来自 [`ImageDetail`] 自身的 serde 属性，本枚举不重复声明。
    #[test]
    fn test_content_block_image_roundtrip() {
        let block = ContentBlock::Image {
            path: "/tmp/a.png".into(),
            detail: Some(ImageDetail::High),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert_eq!(
            json, r#"{"Image":{"path":"/tmp/a.png","detail":"high"}}"#,
            "图片块是外部标签 + 小写 detail"
        );
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
    }

    /// `detail: None` 时该键**必须不出现**：老会话读回时不得因为多一个 `null` 而分叉。
    ///
    /// 变异：删掉 `skip_serializing_if` ⇒ `"detail":null` 出现，前一条断言变红；
    /// 再把 `default` 一并删掉 ⇒ 反序列化失败，后一条断言变红。
    #[test]
    fn test_content_block_image_without_detail_omits_the_key() {
        let block = ContentBlock::Image {
            path: "/tmp/a.png".into(),
            detail: None,
        };
        let json = serde_json::to_string(&block).unwrap();
        assert_eq!(
            json, r#"{"Image":{"path":"/tmp/a.png"}}"#,
            "detail 为 None 时不得出现该键"
        );
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(back, block);
    }

    /// 既有三个变体的字节形态是**硬契约**：golden（`json_no_debug_baseline.jsonl` 首行）
    /// 与磁盘上的既有会话文件都逐字节依赖它们。加拉式新增变体不得改动任何一个字。
    ///
    /// 下面的字符串取自加 `Image` 变体**之前**实测的输出（serde 按字段声明顺序输出）。
    /// 变异：改任一字段名 / 字段顺序 / 给某变体加 `#[serde(...)]` 属性 ⇒ 对应断言变红。
    #[test]
    fn test_existing_content_block_variants_bytes_unchanged() {
        let text = ContentBlock::Text { text: "hi".into() };
        assert_eq!(
            serde_json::to_string(&text).unwrap(),
            r#"{"Text":{"text":"hi"}}"#
        );

        let tool_use = ContentBlock::ToolUse {
            id: ToolCallId("c1".into()),
            name: "read".into(),
            arguments: serde_json::json!({"path": "/tmp/a.txt"}),
        };
        assert_eq!(
            serde_json::to_string(&tool_use).unwrap(),
            r#"{"ToolUse":{"id":"c1","name":"read","arguments":{"path":"/tmp/a.txt"}}}"#
        );

        let tool_result = ContentBlock::ToolResult {
            tool_call_id: ToolCallId("c1".into()),
            content: "x".into(),
            is_error: false,
        };
        assert_eq!(
            serde_json::to_string(&tool_result).unwrap(),
            r#"{"ToolResult":{"tool_call_id":"c1","content":"x","is_error":false}}"#
        );
    }

    // ============ C6：`as_text` / `joined_text` ============

    /// 变异断言：把 `as_text` 里的 `Image` 臂删掉（或改成 `_ => None`）⇒ **本用例不变红**，
    /// 但**删掉整个 `Image` 臂**会让那个 match 编译失败（`error[E0004]`，非穷尽）——
    /// 这正是 C6 要的保护。本用例只锁定语义：`Image` 的正文是 `None`。
    #[test]
    fn as_text_returns_none_for_image_block() {
        let block = ContentBlock::Image {
            path: "/tmp/a.png".into(),
            detail: None,
        };
        assert_eq!(block.as_text(), None, "图片块不携带可展示正文");
    }

    /// `Text` 是唯一携带正文的变体；工具两个变体各带自己的数据，不算正文。
    ///
    /// 变异：把 `ToolResult` 臂改成 `Some(content)` ⇒ 本用例红（那会改变
    /// `joined_text` 的输出，进而改变渲染与摘要）。
    #[test]
    fn as_text_only_matches_text_variant() {
        let text = ContentBlock::Text { text: "hi".into() };
        assert_eq!(text.as_text(), Some("hi"));

        let tool_use = ContentBlock::ToolUse {
            id: ToolCallId("c1".into()),
            name: "read".into(),
            arguments: serde_json::json!({"path": "/tmp/a.txt"}),
        };
        assert_eq!(tool_use.as_text(), None);

        let tool_result = ContentBlock::ToolResult {
            tool_call_id: ToolCallId("c1".into()),
            content: "x".into(),
            is_error: false,
        };
        assert_eq!(tool_result.as_text(), None);
    }

    /// 分隔符由调用方选择、原样使用：`None` / `Newline` / `Space` 三档都是真实存在的调用点契约。
    ///
    /// 变异：把 `separator()` 写死成 `"\n"` ⇒ 前两条断言红；改成保留空串块参与拼接
    /// （不过滤）⇒ `None` 那档多出分隔符，第一条断言红。
    #[test]
    fn joined_text_uses_caller_separator_and_skips_non_text() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text { text: "a".into() },
                ContentBlock::Image {
                    path: "/tmp/a.png".into(),
                    detail: None,
                },
                ContentBlock::Text { text: "b".into() },
            ],
        };
        assert_eq!(msg.joined_text(TextJoin::None), "ab");
        assert_eq!(msg.joined_text(TextJoin::Newline), "a\nb");
        assert_eq!(msg.joined_text(TextJoin::Space), "a b");

        // 无正文块 ⇒ 空串，而不是分隔符堆。
        let tool_only = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: ToolCallId("c1".into()),
                content: "x".into(),
                is_error: false,
            }],
        };
        assert_eq!(tool_only.joined_text(TextJoin::Newline), "");
        assert_eq!(
            Message {
                role: Role::User,
                content: vec![]
            }
            .joined_text(TextJoin::Newline),
            ""
        );
    }
}
