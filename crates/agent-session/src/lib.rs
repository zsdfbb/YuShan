//! agent-session: 会话状态管理

mod jsonl;
mod memory;

pub use jsonl::JsonlSession;
pub use memory::*;

// Re-export Session trait for convenience
pub use agent_core::Message;

use async_trait::async_trait;

/// Session trait - message history management
#[async_trait]
pub trait Session: Send {
    /// Get all messages in the session
    fn messages(&self) -> &[Message];

    /// Append a message to the session
    async fn append(&mut self, message: Message) -> Result<(), SessionError>;

    /// Clear all messages from the session
    async fn clear(&mut self) -> Result<(), SessionError> {
        Ok(())
    }
}

/// Session error types
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
        // MemorySession append should never fail
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
