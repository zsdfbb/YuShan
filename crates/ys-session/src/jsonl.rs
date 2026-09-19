use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{Message, Session, SessionError};

pub struct JsonlSession {
    path: PathBuf,
    messages: Vec<Message>,
    /// 仅测试用：`flush_to_file` 的调用次数。用来断言 `replace_all` 只触发一次落盘
    /// （若退回 trait 默认实现，次数会变成 N+1）。
    #[cfg(test)]
    flush_count: std::sync::atomic::AtomicUsize,
}

impl JsonlSession {
    /// 打开或创建 JSONL session 文件
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let messages = if path.exists() {
            Self::load_from_file(&path).await?
        } else {
            Vec::new()
        };
        Ok(Self {
            path,
            messages,
            #[cfg(test)]
            flush_count: std::sync::atomic::AtomicUsize::new(0),
        })
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
                    // 损坏行：跳过并告警、不终止（修复 C3）
                    eprintln!("Warning: skipping corrupt session line {line_num}: {e}");
                }
            }
        }
        Ok(msgs)
    }

    /// 原子写入：先写临时文件，再 rename（防止崩溃时数据丢失）
    async fn flush_to_file(&self) -> Result<(), SessionError> {
        #[cfg(test)]
        self.flush_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
        // 原子 rename
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

    /// 覆写默认实现：**赋值一次 + 单次落盘**，而不是 `clear()` + N 次 `append()`。
    ///
    /// 必要性有二：
    /// 1. 默认实现对本类型是 **O(n²)** —— `append` 每次都全量重写整个文件，
    ///    而压缩场景恰逢会话最大时，整体替换的代价最高；
    /// 2. 默认实现**非原子** —— `clear()` 成功后某个 `append()` 失败会留下被截断的 session。
    ///    这里复用 `flush_to_file()`（tmp + rename）的原子路径，中途失败不落盘。
    async fn replace_all(&mut self, messages: Vec<Message>) -> Result<(), SessionError> {
        self.messages = messages;
        self.flush_to_file().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use ys_core::{ContentBlock, Role};

    /// 唯一临时路径：进程内原子序号 + `process::id()`，避免并行测试撞名互删文件。
    fn temp_path(tag: &str) -> PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("ys_session_{tag}_{}_{n}.jsonl", std::process::id()))
    }

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

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

        // 重新打开并验证恢复
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

        // 重新打开并验证为空
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

        // 先写一行损坏数据，后跟一条有效行
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
        // 损坏行应被跳过，只恢复有效消息
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

    #[tokio::test]
    async fn test_jsonl_replace_all_flushes_exactly_once() {
        let path = temp_path("replace_once");
        let mut session = JsonlSession::open(&path).await.unwrap();
        for i in 0..3 {
            session
                .append(text_msg(Role::User, &format!("old{i}")))
                .await
                .unwrap();
        }
        let before = session.flush_count.load(Ordering::SeqCst);
        assert_eq!(before, 3, "3 次 append 各落盘一次");

        session
            .replace_all(vec![
                text_msg(Role::User, "n1"),
                text_msg(Role::Assistant, "n2"),
            ])
            .await
            .unwrap();

        let after = session.flush_count.load(Ordering::SeqCst);
        assert_eq!(
            after - before,
            1,
            "覆写后只应落盘一次；若退回默认实现（clear + 2 次 append）会是 3 次"
        );

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_jsonl_replace_all_matches_input_vec() {
        let path = temp_path("replace_matches");
        let mut session = JsonlSession::open(&path).await.unwrap();
        session.append(text_msg(Role::User, "stale")).await.unwrap();

        let expected = vec![
            text_msg(Role::User, "hello"),
            text_msg(Role::Assistant, "hi"),
            text_msg(Role::User, "bye"),
        ];
        session.replace_all(expected.clone()).await.unwrap();

        // 内存中精确等于传入的 Vec（不是「含」或「超集」）
        assert_eq!(session.messages(), expected.as_slice());

        // 落盘内容也精确一致（重新打开恢复）
        let reopened = JsonlSession::open(&path).await.unwrap();
        assert_eq!(reopened.messages(), expected.as_slice());

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_jsonl_replace_all_empty_vec_truncates_file() {
        let path = temp_path("replace_empty");
        let mut session = JsonlSession::open(&path).await.unwrap();
        session.append(text_msg(Role::User, "stale")).await.unwrap();

        session.replace_all(Vec::new()).await.unwrap();
        assert!(session.messages().is_empty());

        let reopened = JsonlSession::open(&path).await.unwrap();
        assert!(reopened.messages().is_empty(), "落盘后文件应为空");

        let _ = tokio::fs::remove_file(&path).await;
    }
}
