//! agent-session: 会话状态管理

mod jsonl;
mod memory;

pub use jsonl::JsonlSession;
pub use memory::*;

// 为方便起见重导出 Session trait
pub use agent_core::Message;

use async_trait::async_trait;

/// Session trait —— 消息历史管理
#[async_trait]
pub trait Session: Send {
    /// 获取 session 中的所有消息
    fn messages(&self) -> &[Message];

    /// 向 session 追加一条消息
    async fn append(&mut self, message: Message) -> Result<(), SessionError>;

    /// 清空 session 中的所有消息
    async fn clear(&mut self) -> Result<(), SessionError> {
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
    use agent_core::{ContentBlock, Role};

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
}
