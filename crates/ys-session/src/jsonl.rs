use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{Message, Session, SessionError, SessionMeta};

pub struct JsonlSession {
    path: PathBuf,
    /// 会话级元数据，落盘于文件**首行**；缺省即无首行 meta。
    meta: Option<SessionMeta>,
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
        let (meta, messages) = if path.exists() {
            Self::load_from_file(&path).await?
        } else {
            (None, Vec::new())
        };
        Ok(Self {
            path,
            meta,
            messages,
            #[cfg(test)]
            flush_count: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// 逐行判别：先按 `Message` 解析（绝大多数行），失败再按 `SessionMeta` 解析；
    /// 两者皆失败 ⇒ 告警并跳过（不终止）。
    ///
    /// 判别安全：`Message` 无 `type` 字段，`SessionMeta` 的 `type` 为必填 ⇒ 两者互不误判。
    async fn load_from_file(
        path: &Path,
    ) -> Result<(Option<SessionMeta>, Vec<Message>), SessionError> {
        let file = File::open(path)
            .await
            .map_err(|e| SessionError::Storage(format!("failed to open session: {e}")))?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let mut meta = None;
        let mut msgs = Vec::new();
        let mut line_num = 0u64;
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| SessionError::Storage(format!("failed to read session: {e}")))?
        {
            line_num += 1;
            if let Ok(msg) = serde_json::from_str::<Message>(&line) {
                msgs.push(msg);
                continue;
            }
            match serde_json::from_str::<SessionMeta>(&line) {
                Ok(m) => meta = Some(m),
                Err(e) => {
                    // 损坏行：跳过并告警、不终止（修复 C3）
                    eprintln!("Warning: skipping corrupt session line {line_num}: {e}");
                }
            }
        }
        Ok((meta, msgs))
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
            // 先写 meta 行（若存在）：这是 C1「meta 永不因重写丢失」的唯一落点 ——
            // append / clear / replace_all / set_meta 都经本函数，故不存在漏写路径。
            if let Some(meta) = &self.meta {
                let line = serde_json::to_string(meta)
                    .map_err(|e| SessionError::Storage(format!("serialize meta failed: {e}")))?;
                file.write_all(line.as_bytes())
                    .await
                    .map_err(|e| SessionError::Storage(format!("write meta failed: {e}")))?;
                file.write_all(b"\n").await.map_err(|e| {
                    SessionError::Storage(format!("write meta newline failed: {e}"))
                })?;
            }
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

    fn meta(&self) -> Option<&SessionMeta> {
        self.meta.as_ref()
    }

    async fn set_meta(&mut self, meta: SessionMeta) -> Result<(), SessionError> {
        self.meta = Some(meta);
        self.flush_to_file().await
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
    use ys_core::{ContentBlock, EffortLevel, Role};

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

    fn sample_meta() -> SessionMeta {
        let mut meta = SessionMeta::new(1791234567);
        meta.title = Some("帮我重构 wiring.rs 的会话恢复逻辑".into());
        meta.provider = Some("deepseek".into());
        meta.model = Some("deepseek-chat".into());
        meta.thinking_visible = true;
        meta.effort = Some(EffortLevel::High);
        meta.input_tokens = 12345;
        meta.output_tokens = 678;
        meta.turns = 4;
        meta
    }

    #[tokio::test]
    async fn test_meta_roundtrip() {
        let path = temp_path("meta_roundtrip");
        let meta = sample_meta();
        {
            let mut session = JsonlSession::open(&path).await.unwrap();
            session.set_meta(meta.clone()).await.unwrap();
            assert_eq!(session.meta(), Some(&meta));
        }

        let reopened = JsonlSession::open(&path).await.unwrap();
        assert_eq!(
            reopened.meta(),
            Some(&meta),
            "重新打开后 meta 应精确等于写入值"
        );

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_meta_is_first_line() {
        let path = temp_path("meta_first");
        let meta = SessionMeta::new(1);
        {
            let mut session = JsonlSession::open(&path).await.unwrap();
            session.set_meta(meta.clone()).await.unwrap();
            session.append(text_msg(Role::User, "hi")).await.unwrap();
        }

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        let mut lines = content.lines();
        let first = lines.next().unwrap();
        let parsed: SessionMeta = serde_json::from_str(first).unwrap();
        assert_eq!(parsed, meta, "首行应为 meta");
        assert!(
            first.contains("\"type\":\"meta\""),
            "首行应带判别键：{first}"
        );
        let second = lines.next().unwrap();
        let _: Message = serde_json::from_str(second).expect("第二行应为裸 Message");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_meta_survives_clear_and_replace_all() {
        let path = temp_path("meta_survives");
        let meta = sample_meta();
        {
            let mut session = JsonlSession::open(&path).await.unwrap();
            session.set_meta(meta.clone()).await.unwrap();
            session.append(text_msg(Role::User, "old")).await.unwrap();

            session.clear().await.unwrap();
            assert_eq!(session.meta(), Some(&meta), "clear 后 meta 仍在");

            session.replace_all(Vec::new()).await.unwrap();
            assert_eq!(session.meta(), Some(&meta), "replace_all(空) 后 meta 仍在");

            session
                .replace_all(vec![text_msg(Role::Assistant, "summary")])
                .await
                .unwrap();
            assert_eq!(
                session.meta(),
                Some(&meta),
                "replace_all(非空) 后 meta 仍在"
            );
        }

        // 重新打开后 meta 精确相等：`sample_meta()` 含 `effort`，故这里同时覆盖
        // effort 在磁盘层的保留。
        let reopened = JsonlSession::open(&path).await.unwrap();
        assert_eq!(reopened.meta(), Some(&meta), "落盘后 meta 仍在");
        assert_eq!(reopened.messages(), [text_msg(Role::Assistant, "summary")]);

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_plain_message_file_has_no_meta() {
        let path = temp_path("no_meta");
        let msg = text_msg(Role::User, "hello");
        let content = format!("{}\n", serde_json::to_string(&msg).unwrap());
        tokio::fs::write(&path, content).await.unwrap();

        // 纯消息文件：每行都先被 Message 命中，不落入告警分支。
        let session = JsonlSession::open(&path).await.unwrap();
        assert_eq!(session.messages(), [msg]);
        assert!(session.meta().is_none());

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_meta_like_corrupt_lines_skipped() {
        let path = temp_path("bad_meta");
        let good = text_msg(Role::User, "ok");
        let content = format!(
            "{}\n{}\n{}\n{}\n",
            r#"{"type":"nope"}"#,
            // 形状合法但不含判别键：若 `kind` 允许缺省则会假阳性成 meta，必须落回坏行。
            r#"{"not_a_message":true}"#,
            r#"{"type":"meta","v":"not-a-number"}"#,
            serde_json::to_string(&good).unwrap()
        );
        tokio::fs::write(&path, content).await.unwrap();

        let session = JsonlSession::open(&path).await.unwrap();
        assert_eq!(session.messages(), [good], "带 type 的非法行仍应被跳过");
        assert!(session.meta().is_none());

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// 契约快照：判别的依赖是「`Message` 的序列化形状不含 `type` 键」。
    #[test]
    fn test_message_serde_shape_has_no_type_key() {
        let value = serde_json::to_value(text_msg(Role::User, "hi")).unwrap();
        assert!(
            value.get("type").is_none(),
            "Message 不得带 type 键，否则 meta 判别不再安全：{value}"
        );
    }
}
