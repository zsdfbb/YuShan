use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{Message, Session, SessionError};

pub struct JsonlSession {
    path: PathBuf,
    messages: Vec<Message>,
}

impl JsonlSession {
    /// Open or create a JSONL session file
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let messages = if path.exists() {
            Self::load_from_file(&path).await?
        } else {
            Vec::new()
        };
        Ok(Self { path, messages })
    }

    async fn load_from_file(path: &Path) -> Result<Vec<Message>, SessionError> {
        let file = File::open(path)
            .await
            .map_err(|e| SessionError::Storage(format!("failed to open session: {e}")))?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut msgs = Vec::new();
        let mut line_num = 0u64;
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| SessionError::Storage(format!("failed to read session: {e}")))?
        {
            line_num += 1;
            match serde_json::from_str::<Message>(&line) {
                Ok(msg) => msgs.push(msg),
                Err(e) => {
                    // Corrupt line: skip + warn, don't terminate (fix C3)
                    eprintln!("Warning: skipping corrupt session line {line_num}: {e}");
                }
            }
        }
        Ok(msgs)
    }

    /// Atomic write: write to tmp file, then rename (prevent data loss on crash)
    async fn flush_to_file(&self) -> Result<(), SessionError> {
        let tmp_path = self.path.with_extension("jsonl.tmp");
        {
            let mut file = File::create(&tmp_path).await.map_err(|e| {
                SessionError::Storage(format!("failed to create tmp session file: {e}"))
            })?;
            for msg in &self.messages {
                let line = serde_json::to_string(msg)
                    .map_err(|e| SessionError::Storage(format!("serialize failed: {e}")))?;
                file.write_all(line.as_bytes())
                    .await
                    .map_err(|e| SessionError::Storage(format!("write failed: {e}")))?;
                file.write_all(b"\n")
                    .await
                    .map_err(|e| SessionError::Storage(format!("write newline failed: {e}")))?;
            }
        }
        // Atomic rename
        tokio::fs::rename(&tmp_path, &self.path)
            .await
            .map_err(|e| SessionError::Storage(format!("rename failed: {e}")))?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Session for JsonlSession {
    fn messages(&self) -> &[Message] {
        &self.messages
    }

    async fn append(&mut self, message: Message) -> Result<(), SessionError> {
        self.messages.push(message);
        self.flush_to_file().await
    }

    async fn clear(&mut self) -> Result<(), SessionError> {
        self.messages.clear();
        self.flush_to_file().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{ContentBlock, Role};

    #[tokio::test]
    async fn test_jsonl_session_create_and_append() {
        let path = std::env::temp_dir().join("test_jsonl_new.jsonl");
        let _ = tokio::fs::remove_file(&path).await;

        {
            let mut session = JsonlSession::open(&path).await.unwrap();
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

            session
                .append(Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "hi there".into(),
                    }],
                })
                .await
                .unwrap();

            assert_eq!(session.messages().len(), 2);
        }

        // Reopen and verify recovery
        {
            let session = JsonlSession::open(&path).await.unwrap();
            assert_eq!(session.messages().len(), 2);
            assert_eq!(session.messages()[0].role, Role::User);
            assert_eq!(session.messages()[1].role, Role::Assistant);
        }

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_jsonl_session_clear() {
        let path = std::env::temp_dir().join("test_jsonl_clear.jsonl");
        let _ = tokio::fs::remove_file(&path).await;

        {
            let mut session = JsonlSession::open(&path).await.unwrap();
            session
                .append(Message {
                    role: Role::User,
                    content: vec![],
                })
                .await
                .unwrap();
            assert_eq!(session.messages().len(), 1);

            session.clear().await.unwrap();
            assert!(session.messages().is_empty());
        }

        // Reopen and verify empty
        {
            let session = JsonlSession::open(&path).await.unwrap();
            assert!(session.messages().is_empty());
        }

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_jsonl_session_corrupt_line_skipped() {
        let path = std::env::temp_dir().join("test_jsonl_corrupt.jsonl");
        let _ = tokio::fs::remove_file(&path).await;

        // Write a corrupt line followed by a valid line
        let valid_msg = Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "valid".into(),
            }],
        };
        let valid_json = serde_json::to_string(&valid_msg).unwrap();
        let content = format!("this is not json\n{valid_json}\n");
        tokio::fs::write(&path, content).await.unwrap();

        let session = JsonlSession::open(&path).await.unwrap();
        // Corrupt line should be skipped, only valid message recovered
        assert_eq!(session.messages().len(), 1);
        assert_eq!(session.messages()[0].role, Role::User);

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_jsonl_session_new_file() {
        let path = std::env::temp_dir().join("test_jsonl_nonexistent.jsonl");
        let _ = tokio::fs::remove_file(&path).await;

        let session = JsonlSession::open(&path).await.unwrap();
        assert!(session.messages().is_empty());

        let _ = tokio::fs::remove_file(&path).await;
    }
}
