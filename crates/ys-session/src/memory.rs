use ys_core::Message;

use super::{Session, SessionError};

/// 内存版 session 实现
pub struct MemorySession {
    messages: Vec<Message>,
}

impl MemorySession {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
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

    async fn append(&mut self, message: Message) -> Result<(), SessionError> {
        self.messages.push(message);
        Ok(())
    }

    async fn clear(&mut self) -> Result<(), SessionError> {
        self.messages.clear();
        Ok(())
    }
}
