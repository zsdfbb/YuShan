//! 一次性摘要能力 —— 把一段**不可信文本**按调用方意图压成摘要。
//!
//! 形状刻意窄（`&str` 进 / `String` 出）：`ys-model` 依赖 `ys-tool`（为 `ToolSpec`）
//! ⇒ 反向依赖成环 ⇒ 此处**不得**出现 `ModelRequest` / `ModelError` / `Usage`。
//! 与 [`crate::ApprovalHandler`] 同形：async、由上层实现、只在工具里被读。

use async_trait::async_trait;

#[async_trait]
pub trait Summarizer: Send + Sync {
    /// `content` 是**不可信数据**（已由调用方按自己的闸截断）；`prompt` 是调用方意图（已 trim、非空）。
    ///
    /// `Err` 内的字符串是**给人/给模型看的失败原因**，会被原样渲染进工具结果的 notice ——
    /// 故是 `String` 而非枚举（三种失败：超时 / 供应商报错 / 空输出，处置完全相同，只有文案不同）。
    async fn summarize(&self, content: &str, prompt: &str) -> Result<String, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 极简 fake —— 同时证明本 trait **object-safe**（能藏在 `&dyn Summarizer` 后面）。
    struct EchoSummarizer;

    #[async_trait]
    impl Summarizer for EchoSummarizer {
        async fn summarize(&self, content: &str, prompt: &str) -> Result<String, String> {
            if content.is_empty() {
                return Err("空输入".to_string());
            }
            Ok(format!("{prompt} :: {content}"))
        }
    }

    #[tokio::test]
    async fn test_summarizer_is_object_safe_and_usable() {
        let fake = EchoSummarizer;
        // 关键：trait object 可构造（object-safe），这正是 `ToolContext` 字段的形态。
        let as_dyn: &dyn Summarizer = &fake;

        assert_eq!(
            as_dyn.summarize("正文", "鉴权怎么填").await.unwrap(),
            "鉴权怎么填 :: 正文"
        );
        assert_eq!(
            as_dyn.summarize("", "任意意图").await.unwrap_err(),
            "空输入"
        );
    }
}
