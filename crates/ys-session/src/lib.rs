//! ys-session: 会话状态管理

mod jsonl;
mod memory;
mod meta;

pub use jsonl::JsonlSession;
pub use memory::*;
pub use meta::SessionMeta;

// 为方便起见重导出 Session trait
pub use ys_core::Message;

use async_trait::async_trait;

/// 打开既有会话时执行的一次性修复记录。
///
/// 会话实现可把修复结果暂存到自身，由接线器通过 [`Session::take_recovery`]
/// 取走；`take` 语义保证同一结果只被消费一次。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionRecovery {
    /// 为中断的 `ToolUse` 合成的 `ToolResult` 数量。
    pub repaired_tool_calls: usize,
    /// 形状非法（非 JSON 对象）而被归一化为 `{}` 的 `ToolUse.arguments` 数量。
    ///
    /// 单独计数而非合并进 `repaired_tool_calls`：两者的用户可见后果不同 ——
    /// 悬空调用是「少了一条工具结果」，参数非法是「会话会被端点持续 400 拒绕」
    /// （实测 minimax），后者更需要用户知道。
    pub normalized_tool_arguments: usize,
    /// 读取时跳过、无法判别为 `Message` 或 `SessionMeta` 的行数。
    pub skipped_lines: usize,
}

impl SessionRecovery {
    pub fn is_empty(&self) -> bool {
        self.repaired_tool_calls == 0
            && self.normalized_tool_arguments == 0
            && self.skipped_lines == 0
    }
}

/// Session trait —— 消息历史管理
#[async_trait]
pub trait Session: Send {
    /// 获取 session 中的所有消息
    fn messages(&self) -> &[Message];

    /// 取走打开会话时产生的修复记录。
    ///
    /// 默认实现返回 `None`：不持久化、也不需要修复的会话无需关心本能力。
    fn take_recovery(&mut self) -> Option<SessionRecovery> {
        None
    }

    /// 会话级元数据；不支持的实现返回 `None`（默认实现让 `MemorySession` 零改动）。
    fn meta(&self) -> Option<&SessionMeta> {
        None
    }

    /// 写入会话级元数据。默认实现为无操作，落盘由支持持久化的实现覆写。
    async fn set_meta(&mut self, _meta: SessionMeta) -> Result<(), SessionError> {
        Ok(())
    }

    /// 向 session 追加一条消息
    async fn append(&mut self, message: Message) -> Result<(), SessionError>;

    /// 清空 session 中的所有消息
    async fn clear(&mut self) -> Result<(), SessionError> {
        Ok(())
    }

    /// 整体替换 session 内容。
    ///
    /// ⚠️ **默认实现非原子** —— 它是 `clear()` + 逐条 `append()`，中途失败会留下被截断的 session。
    /// 要求原子性的实现**必须覆写**本方法（见 `JsonlSession`）。
    ///
    /// 另外，对「`append` 每次全量重写整个文件」的实现（如 `JsonlSession`），默认实现会把整体替换
    /// 退化成 **O(n²)**：`clear()` 一次全量写，其后 N 次 `append()` 又各写一遍整个文件 ——
    /// 而整体替换恰恰出现在会话最大时（压缩场景），代价最高。覆写为「赋值一次 + 单次落盘」
    /// 可把它降到一次写。
    async fn replace_all(&mut self, messages: Vec<Message>) -> Result<(), SessionError> {
        self.clear().await?;
        for m in messages {
            self.append(m).await?;
        }
        Ok(())
    }
}

/// Session 错误类型
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("session storage error: {0}")]
    Storage(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use ys_core::{ContentBlock, Role};

    #[tokio::test]
    async fn test_memory_session_append_and_read() {
        let mut session = MemorySession::new();
        assert!(session.messages().is_empty());

        session
            .append(Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
            })
            .await
            .unwrap();

        assert_eq!(session.messages().len(), 1);
        assert!(matches!(&session.messages()[0].role, Role::User));
    }

    #[tokio::test]
    async fn test_memory_session_multiple_messages() {
        let mut session = MemorySession::new();

        session
            .append(Message {
                role: Role::User,
                content: vec![],
            })
            .await
            .unwrap();
        session
            .append(Message {
                role: Role::Assistant,
                content: vec![],
            })
            .await
            .unwrap();
        session
            .append(Message {
                role: Role::User,
                content: vec![],
            })
            .await
            .unwrap();

        assert_eq!(session.messages().len(), 3);
    }

    #[tokio::test]
    async fn test_memory_session_never_fails() {
        let mut session = MemorySession::new();
        // MemorySession 的 append 不应失败
        let result = session
            .append(Message {
                role: Role::User,
                content: vec![],
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_memory_session_clear() {
        let mut session = MemorySession::new();
        session
            .append(Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hello".into(),
                }],
            })
            .await
            .unwrap();
        session
            .append(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "world".into(),
                }],
            })
            .await
            .unwrap();
        assert_eq!(session.messages().len(), 2);

        session.clear().await.unwrap();
        assert!(session.messages().is_empty());
    }

    /// 计数替身：记录 `clear` / `append` 的调用次数，**有意不覆写 `replace_all`**，
    /// 用来验证 trait 默认实现确实是 `clear()×1 + append()×N`。
    #[derive(Default)]
    struct CallCounts {
        clear: usize,
        append: usize,
    }

    struct CountingSession {
        messages: Vec<Message>,
        counts: Arc<Mutex<CallCounts>>,
    }

    #[async_trait::async_trait]
    impl Session for CountingSession {
        fn messages(&self) -> &[Message] {
            &self.messages
        }

        async fn append(&mut self, message: Message) -> Result<(), SessionError> {
            self.counts.lock().unwrap().append += 1;
            self.messages.push(message);
            Ok(())
        }

        async fn clear(&mut self) -> Result<(), SessionError> {
            self.counts.lock().unwrap().clear += 1;
            self.messages.clear();
            Ok(())
        }
    }

    fn text_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    #[tokio::test]
    async fn test_default_replace_all_is_clear_plus_n_appends() {
        let counts = Arc::new(Mutex::new(CallCounts::default()));
        let mut session = CountingSession {
            messages: Vec::new(),
            counts: Arc::clone(&counts),
        };
        // 先垫一条陈旧消息，确认 replace_all 会先清空
        session.append(text_msg("stale")).await.unwrap();
        {
            let mut c = counts.lock().unwrap();
            c.clear = 0;
            c.append = 0;
        }

        session
            .replace_all(vec![text_msg("a"), text_msg("b"), text_msg("c")])
            .await
            .unwrap();

        let c = counts.lock().unwrap();
        assert_eq!(c.clear, 1, "默认实现应当恰好 clear 一次");
        assert_eq!(c.append, 3, "默认实现应当逐条 append，共 3 次");
        assert_eq!(
            session.messages(),
            [text_msg("a"), text_msg("b"), text_msg("c")]
        );
    }

    #[tokio::test]
    async fn test_default_replace_all_empty_vec() {
        let counts = Arc::new(Mutex::new(CallCounts::default()));
        let mut session = CountingSession {
            messages: Vec::new(),
            counts: Arc::clone(&counts),
        };
        session.append(text_msg("stale")).await.unwrap();

        session.replace_all(Vec::new()).await.unwrap();

        let c = counts.lock().unwrap();
        assert_eq!(c.clear, 1, "replace_all 仍会 clear 一次");
        assert_eq!(c.append, 1, "仅垫数据那次 append，空 Vec 不再 append");
        assert!(session.messages().is_empty());
    }

    #[tokio::test]
    async fn test_memory_session_replace_all_uses_default_impl() {
        let mut session = MemorySession::new();
        session.append(text_msg("old")).await.unwrap();

        session
            .replace_all(vec![text_msg("new1"), text_msg("new2")])
            .await
            .unwrap();

        assert_eq!(session.messages(), [text_msg("new1"), text_msg("new2")]);
    }
}
