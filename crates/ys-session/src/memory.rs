use std::sync::Arc;

use ys_core::Message;

use super::{Session, SessionError};

/// 内存版 session 实现
///
/// 消息存为 `Arc<Vec<Message>>`：`shared_messages()` 能直接交出快照（O(1)），
/// 写路径走 `Arc::make_mut` —— 快照已被释放时原地修改，仍有外部持有时才复制。
pub struct MemorySession {
    messages: Arc<Vec<Message>>,
}

impl MemorySession {
    pub fn new() -> Self {
        Self {
            messages: Arc::new(Vec::new()),
        }
    }
}

impl Default for MemorySession {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Session for MemorySession {
    fn messages(&self) -> &[Message] {
        &self.messages
    }

    fn shared_messages(&self) -> Arc<Vec<Message>> {
        Arc::clone(&self.messages)
    }

    async fn append(&mut self, message: Message) -> Result<(), SessionError> {
        Arc::make_mut(&mut self.messages).push(message);
        Ok(())
    }

    async fn clear(&mut self) -> Result<(), SessionError> {
        Arc::make_mut(&mut self.messages).clear();
        Ok(())
    }
}
