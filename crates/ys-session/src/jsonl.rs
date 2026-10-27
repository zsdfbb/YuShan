use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use ys_core::{ContentBlock, Role, ToolCallId};

use crate::{Message, Session, SessionError, SessionMeta, SessionRecovery};

const INTERRUPTED_TOOL_RESULT: &str = "Tool execution was interrupted; result unavailable.";

pub struct JsonlSession {
    path: PathBuf,
    /// 会话级元数据，落盘于文件**首行**；缺省即无首行 meta。
    meta: Option<SessionMeta>,
    messages: Vec<Message>,
    /// 打开文件时完成的修复记录；由 [`Session::take_recovery`] 一次性取走。
    recovery: Option<SessionRecovery>,
    /// 仅测试用：`flush_to_file` 的调用次数。用来断言 `replace_all` 只触发一次落盘
    /// （若退回 trait 默认实现，次数会变成 N+1）。
    #[cfg(test)]
    flush_count: std::sync::atomic::AtomicUsize,
}

impl JsonlSession {
    /// 打开或创建 JSONL session 文件
    pub async fn open(path: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let path = path.into();
        let (meta, mut messages, skipped_lines) = if path.exists() {
            Self::load_from_file(&path).await?
        } else {
            (None, Vec::new(), 0)
        };
        let repaired_tool_calls = repair_dangling_tool_calls(&mut messages);
        let normalized_tool_arguments = normalize_tool_arguments(&mut messages);
        let recovery = SessionRecovery {
            repaired_tool_calls,
            normalized_tool_arguments,
            skipped_lines,
        };
        let session = Self {
            path,
            meta,
            messages,
            recovery: (!recovery.is_empty()).then_some(recovery),
            #[cfg(test)]
            flush_count: std::sync::atomic::AtomicUsize::new(0),
        };
        // 修复必须先落盘再交给调用方：只在内存里补结果会让下一次 resume 再次失败。
        // `flush_to_file` 沿用 tmp + rename；失败直接让 open 失败，不返回半修会话。
        if repaired_tool_calls > 0 || normalized_tool_arguments > 0 {
            session.flush_to_file().await?;
        }
        Ok(session)
    }

    /// 逐行判别：先按 `Message` 解析（绝大多数行），失败再按 `SessionMeta` 解析；
    /// 两者皆失败 ⇒ 跳过（不终止），并把数量交给调用方记录到诊断日志/修复摘要。
    ///
    /// 判别安全：`Message` 无 `type` 字段，`SessionMeta` 的 `type` 为必填 ⇒ 两者互不误判。
    async fn load_from_file(
        path: &Path,
    ) -> Result<(Option<SessionMeta>, Vec<Message>, usize), SessionError> {
        let file = File::open(path)
            .await
            .map_err(|e| SessionError::Storage(format!("failed to open session: {e}")))?;
        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut meta = None;
        let mut msgs = Vec::new();
        let mut skipped_lines = 0usize;
        loop {
            line.clear();
            let read = reader
                .read_until(b'\n', &mut line)
                .await
                .map_err(|e| SessionError::Storage(format!("failed to read session: {e}")))?;
            if read == 0 {
                break;
            }
            if line.last() == Some(&b'\n') {
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
            }
            let Ok(line) = std::str::from_utf8(&line) else {
                skipped_lines += 1;
                continue;
            };
            if let Ok(msg) = serde_json::from_str::<Message>(line) {
                msgs.push(msg);
                continue;
            }
            match serde_json::from_str::<SessionMeta>(line) {
                Ok(m) => meta = Some(m),
                Err(_) => {
                    // 损坏行：跳过、不终止。库层不直接写 stderr —— TUI 起屏后
                    // 任何直写都会冲进输入区；调用方从 `SessionRecovery` 统一告警。
                    skipped_lines += 1;
                }
            }
        }
        Ok((meta, msgs, skipped_lines))
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

    fn take_recovery(&mut self) -> Option<SessionRecovery> {
        self.recovery.take()
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

/// 修复被中断会话中缺少结果的 `ToolUse`。
///
/// OpenAI-compatible 协议要求 assistant `tool_calls` 后紧跟对应的 tool
/// 消息。只在文件末尾补一条结果不够：若调用后已经有用户文本，那条结果仍会
/// 排在用户文本之后。这里对每个不完整的 assistant 调用：
///
/// - 紧随其后的 User 消息已含 `ToolResult` ⇒ 把缺失结果追加到该消息；
/// - 否则立即插入一条只含合成结果的 User 消息。
///
/// 返回实际合成的 block 数。按 id 去重；已有对应结果（即使不在相邻位置）的调用
/// 不会再次合成。
fn repair_dangling_tool_calls(messages: &mut Vec<Message>) -> usize {
    let mut resolved: HashSet<ToolCallId> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect();

    let mut repaired = 0usize;
    let mut index = 0usize;
    while index < messages.len() {
        if messages[index].role != Role::Assistant {
            index += 1;
            continue;
        }

        let mut seen_in_message = HashSet::new();
        let missing: Vec<ToolCallId> = messages[index]
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, .. }
                    if !resolved.contains(id) && seen_in_message.insert(id.clone()) =>
                {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();

        if missing.is_empty() {
            index += 1;
            continue;
        }

        for id in &missing {
            resolved.insert(id.clone());
        }
        repaired += missing.len();
        let synthetic: Vec<ContentBlock> = missing
            .into_iter()
            .map(|tool_call_id| ContentBlock::ToolResult {
                tool_call_id,
                content: INTERRUPTED_TOOL_RESULT.to_string(),
                is_error: true,
            })
            .collect();

        let next_is_result_group = messages.get(index + 1).is_some_and(|message| {
            message.role == Role::User
                && message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        });
        if next_is_result_group {
            messages[index + 1].content.extend(synthetic);
            index += 1;
        } else {
            messages.insert(
                index + 1,
                Message {
                    role: Role::User,
                    content: synthetic,
                },
            );
            index += 2;
        }
    }
    repaired
}

/// 归一化历史里**形状非法**的 `ToolUse.arguments`，返回修正的 block 数。
///
/// 契约与 [`repair_dangling_tool_calls`] 同源：会话文件可能来自旧版本、手工编辑，
/// 或任何在别处生成的坏块，而 `arguments` 只允许是 JSON **对象**——非对象值
/// （尤其 `null`）线上序列化为字面量 `null`，多家端点直接 400（实测 minimax
/// `400 (2013) invalid params`），且**每个回合都会重放同一个错误**。
///
/// 归一化为 `{}` 不改语义（工具层看到的就是「无参数」），却让会话自愈：不必等
/// 用户发现、也不必要求他 `/new`。与悬空 `ToolUse` 的修复一样，**必须先落盘再
/// 交给调用方** —— 只在内存里改会让下一次 resume 再次失败。
fn normalize_tool_arguments(messages: &mut [Message]) -> usize {
    let mut fixed = 0usize;
    for message in messages.iter_mut() {
        for block in message.content.iter_mut() {
            if let ContentBlock::ToolUse { arguments, .. } = block
                && !arguments.is_object()
            {
                *arguments = serde_json::Value::Object(serde_json::Map::new());
                fixed += 1;
            }
        }
    }
    fixed
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

    fn tool_use_msg(calls: &[(&str, &str)]) -> Message {
        Message {
            role: Role::Assistant,
            content: calls
                .iter()
                .map(|(id, name)| ContentBlock::ToolUse {
                    id: ToolCallId((*id).into()),
                    name: (*name).into(),
                    arguments: serde_json::json!({"path": "src/lib.rs"}),
                })
                .collect(),
        }
    }

    fn tool_result_msg(results: &[(&str, bool)]) -> Message {
        Message {
            role: Role::User,
            content: results
                .iter()
                .map(|(id, is_error)| ContentBlock::ToolResult {
                    tool_call_id: ToolCallId((*id).into()),
                    content: format!("result for {id}"),
                    is_error: *is_error,
                })
                .collect(),
        }
    }

    async fn write_messages(path: &Path, messages: &[Message]) {
        let mut body = String::new();
        for message in messages {
            body.push_str(&serde_json::to_string(message).unwrap());
            body.push('\n');
        }
        tokio::fs::write(path, body).await.unwrap();
    }

    fn tool_result_ids(message: &Message) -> Vec<&ToolCallId> {
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn test_jsonl_session_create_and_append() {
        let path = temp_path("new");
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
        let path = temp_path("clear");
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
        let path = temp_path("corrupt");
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
    async fn invalid_utf8_line_is_skipped_without_failing_the_session() {
        let path = temp_path("invalid_utf8");
        let before = text_msg(Role::User, "before");
        let after = text_msg(Role::Assistant, "after");
        let mut content = serde_json::to_vec(&before).unwrap();
        content.extend_from_slice(b"\n\xff\xfe\n");
        content.extend_from_slice(&serde_json::to_vec(&after).unwrap());
        content.push(b'\n');
        tokio::fs::write(&path, content).await.unwrap();

        let mut session = JsonlSession::open(&path).await.unwrap();
        assert_eq!(session.messages(), [before, after]);
        assert_eq!(
            session.take_recovery(),
            Some(SessionRecovery {
                repaired_tool_calls: 0,
                normalized_tool_arguments: 0,
                skipped_lines: 1,
            })
        );

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn test_jsonl_session_new_file() {
        let path = temp_path("nonexistent");
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

        let mut session = JsonlSession::open(&path).await.unwrap();
        assert_eq!(session.messages(), [good], "带 type 的非法行仍应被跳过");
        assert!(session.meta().is_none());
        assert_eq!(
            session.take_recovery(),
            Some(SessionRecovery {
                repaired_tool_calls: 0,
                normalized_tool_arguments: 0,
                skipped_lines: 3,
            }),
            "坏行应计数并交给调用方记录，不得由库层直接打印"
        );

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// 存量会话里的**非法 `arguments` 自愈**：`null` 等非对象值必须被归一化为
    /// `{}` 并**落盘**，否则每次 resume 都会把同一个 400 重放一遍。
    ///
    /// 实测根因：旧版本把「参数 JSON 解析失败」降级为 `Value::Null`，落盘后线上
    /// 序列化为字面量 `null`，minimax 返回 `400 (2013) invalid params`，且之后每个
    /// 回合都在 round=1 失败（只能 `/new` 自救）。
    ///
    /// 变异：删掉 `open()` 里的 `normalize_tool_arguments` 调用
    /// ⇒ `take_recovery` 与 `reopened` 的断言均变红；只改内存不落盘 ⇒ `reopened` 变红。
    #[tokio::test]
    async fn non_object_tool_arguments_are_normalized_and_persisted() {
        let path = temp_path("normalize_args");
        let mut calls = tool_use_msg(&[("c1", "edit")]);
        // 复现坏块：arguments 为 `null`（实际落盘后就是 `"ToolUse":{...,"arguments":null}`）。
        calls.content[0] = ContentBlock::ToolUse {
            id: ToolCallId("c1".into()),
            name: "edit".into(),
            arguments: serde_json::Value::Null,
        };
        write_messages(&path, &[calls, tool_result_msg(&[("c1", false)])]).await;

        let mut session = JsonlSession::open(&path).await.unwrap();
        assert_eq!(
            session.take_recovery(),
            Some(SessionRecovery {
                repaired_tool_calls: 0,
                normalized_tool_arguments: 1,
                skipped_lines: 0,
            }),
            "非对象 arguments 必须计入修复摘要"
        );
        let ContentBlock::ToolUse { arguments, .. } = &session.messages()[0].content[0] else {
            panic!("应仍为 ToolUse");
        };
        assert!(
            arguments.is_object(),
            "arguments 必须归一化为 JSON 对象，实际：{arguments:?}"
        );

        // **落盘**验证：重开不得再有修复（否则 resume 每次都报、且线上仍发 null）。
        let reopened = JsonlSession::open(&path).await.unwrap();
        assert!(
            reopened.recovery.is_none(),
            "修复必须已落盘，重开不应再触发一次"
        );
        let ContentBlock::ToolUse { arguments, .. } = &reopened.messages()[0].content[0] else {
            panic!("应仍为 ToolUse");
        };
        assert!(arguments.is_object(), "落盘后仍必须是对象");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn dangling_tool_use_at_tail_is_repaired_and_persisted() {
        let path = temp_path("repair_tail");
        write_messages(&path, &[tool_use_msg(&[("c1", "read")])]).await;

        let mut session = JsonlSession::open(&path).await.unwrap();
        assert_eq!(
            session.take_recovery(),
            Some(SessionRecovery {
                repaired_tool_calls: 1,
                normalized_tool_arguments: 0,
                skipped_lines: 0,
            })
        );
        assert_eq!(session.messages().len(), 2);
        assert_eq!(session.messages()[1].role, Role::User);
        assert_eq!(
            tool_result_ids(&session.messages()[1]),
            [&ToolCallId("c1".into())]
        );
        assert!(matches!(
            &session.messages()[1].content[0],
            ContentBlock::ToolResult {
                content,
                is_error: true,
                ..
            } if content == INTERRUPTED_TOOL_RESULT
        ));

        let reopened = JsonlSession::open(&path).await.unwrap();
        assert_eq!(reopened.messages(), session.messages());
        assert!(
            reopened.recovery.is_none(),
            "修复落盘后再次打开不应重复补结果"
        );
        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn dangling_tool_result_is_inserted_before_following_user_text() {
        let path = temp_path("repair_before_user");
        write_messages(
            &path,
            &[
                tool_use_msg(&[("c1", "bash")]),
                text_msg(Role::User, "continue please"),
            ],
        )
        .await;

        let session = JsonlSession::open(&path).await.unwrap();
        assert_eq!(session.messages().len(), 3);
        assert_eq!(session.messages()[0].role, Role::Assistant);
        assert_eq!(session.messages()[1].role, Role::User);
        assert_eq!(
            tool_result_ids(&session.messages()[1]),
            [&ToolCallId("c1".into())],
            "合成结果必须紧跟 assistant，不能排到后续用户文本之后"
        );
        assert_eq!(
            session.messages()[2],
            text_msg(Role::User, "continue please")
        );

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn partial_result_group_only_gets_missing_tool_results() {
        let path = temp_path("repair_partial");
        write_messages(
            &path,
            &[
                tool_use_msg(&[("c1", "read"), ("c2", "bash")]),
                tool_result_msg(&[("c1", false)]),
            ],
        )
        .await;

        let session = JsonlSession::open(&path).await.unwrap();
        assert_eq!(session.messages().len(), 2, "缺失结果应追加到紧随的消息");
        let ids = tool_result_ids(&session.messages()[1]);
        assert_eq!(ids, [&ToolCallId("c1".into()), &ToolCallId("c2".into())]);
        assert!(matches!(
            &session.messages()[1].content[1],
            ContentBlock::ToolResult {
                tool_call_id,
                is_error: true,
                ..
            } if tool_call_id == &ToolCallId("c2".into())
        ));

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn clean_session_is_not_rewritten_when_opened() {
        let path = temp_path("clean_no_rewrite");
        write_messages(
            &path,
            &[
                tool_use_msg(&[("c1", "read")]),
                tool_result_msg(&[("c1", false)]),
            ],
        )
        .await;
        let before = tokio::fs::read(&path).await.unwrap();

        let session = JsonlSession::open(&path).await.unwrap();
        assert!(session.recovery.is_none());
        let after = tokio::fs::read(&path).await.unwrap();
        assert_eq!(after, before, "无需修复时不得重写会话文件");

        let _ = tokio::fs::remove_file(&path).await;
    }

    #[tokio::test]
    async fn repair_flush_failure_fails_open_instead_of_returning_memory_only_fix() {
        let path = temp_path("repair_flush_failure");
        write_messages(&path, &[tool_use_msg(&[("c1", "read")])]).await;
        let tmp_path = path.with_extension("jsonl.tmp");
        tokio::fs::create_dir(&tmp_path).await.unwrap();

        let error = JsonlSession::open(&path)
            .await
            .err()
            .expect("合成的 ToolResult 无法落盘时必须让 open 失败，不能只修内存");
        assert!(
            error
                .to_string()
                .contains("failed to create tmp session file"),
            "错误应保留落盘失败分类：{error}"
        );

        tokio::fs::remove_dir(&tmp_path).await.unwrap();
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
