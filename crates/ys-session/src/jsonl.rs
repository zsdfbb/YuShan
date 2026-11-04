use std::collections::HashSet;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};

use ys_core::{ContentBlock, Role, ToolCallId};

use crate::{Message, Session, SessionError, SessionMeta, SessionRecovery};

const INTERRUPTED_TOOL_RESULT: &str = "Tool execution was interrupted; result unavailable.";

pub struct JsonlSession {
    path: PathBuf,
    /// 会话级元数据，落盘于文件**首行**；缺省即无首行 meta。
    meta: Option<SessionMeta>,
    messages: Vec<Message>,
    /// 已落盘的消息条数 —— `append` 的增量起点。
    ///
    /// 与 `messages.len()` 的差值就是尚未写进文件的尾巴。全量路径
    /// （`clear` / `replace_all` / `open` 的修复落盘）成功后必须把它对齐到
    /// `messages.len()`；`append` 成功后 +1。`set_meta` 不改 `messages`，全量写之后本就对齐。
    persisted: usize,
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
        let existed = path.exists();
        let (meta, mut messages, skipped_lines) = if existed {
            Self::load_from_file(&path).await?
        } else {
            (None, Vec::new(), 0)
        };
        // 末尾残行必须在构造 `Self` 之前截掉：`append` 只往文件尾部追加，残行留着会让
        // 下一条消息接在它后面，两条消息缝成一行坏数据。
        // 残行已由 `load_from_file` 计入 `skipped_lines`（它是被读出的坏行）；本函数只负责
        // 把它从磁盘切掉，**不重复计数**。返回的字节数仅用于日志/诊断。
        if existed {
            let _ = Self::truncate_partial_tail(&path).await?;
        }
        let repaired_tool_calls = repair_dangling_tool_calls(&mut messages);
        let normalized_tool_arguments = normalize_tool_arguments(&mut messages);
        let recovery = SessionRecovery {
            repaired_tool_calls,
            normalized_tool_arguments,
            skipped_lines,
        };
        let persisted = messages.len();
        let session = Self {
            path,
            meta,
            messages,
            persisted,
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

    /// 截掉文件末尾**没有换行结尾**的残行，返回被截掉的字节数（`原长度 - keep`，
    /// 未截断时为 `0`）。
    ///
    /// 一次写入可能只落下一行 JSON 的前缀（崩溃或短写），尾部于是留下既无 `\n` 也无法
    /// 解析的残行。截断保留到最后一个 `\n` 为止；整份文件没有 `\n` ⇒ 截为空文件。
    ///
    /// 只在 `open` 里调用，且必须早于构造 `Self`：`append` 是纯追加，残行不清会让
    /// 后续消息接在它后面。已读入内存的行不受影响 —— 本条只处理磁盘上的尾巴。
    ///
    /// 不整份读：先看末字节（常见路径是「文件以 `\n` 结尾」，读完 1 字节即返回），
    /// 只有确实要找最后一个 `\n` 时才按块从尾部向前扫，命中即停。
    async fn truncate_partial_tail(path: &Path) -> Result<u64, SessionError> {
        const CHUNK: u64 = 8 * 1024;
        let len = tokio::fs::metadata(path)
            .await
            .map_err(|e| SessionError::Storage(format!("failed to stat session: {e}")))?
            .len();
        if len == 0 {
            return Ok(0);
        }
        let mut file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .await
            .map_err(|e| SessionError::Storage(format!("failed to open session: {e}")))?;
        file.seek(SeekFrom::End(-1))
            .await
            .map_err(|e| SessionError::Storage(format!("failed to seek session: {e}")))?;
        let mut last = [0u8; 1];
        file.read_exact(&mut last)
            .await
            .map_err(|e| SessionError::Storage(format!("failed to read session: {e}")))?;
        if last[0] == b'\n' {
            return Ok(0);
        }
        // `end` 是尚未扫描过的区域末端（开区间）。命中 `\n` ⇒ keep = 它之后的位置。
        let mut end = len;
        let keep = loop {
            if end == 0 {
                break 0;
            }
            let start = end.saturating_sub(CHUNK);
            let mut buf = vec![0u8; (end - start) as usize];
            file.seek(SeekFrom::Start(start))
                .await
                .map_err(|e| SessionError::Storage(format!("failed to seek session: {e}")))?;
            file.read_exact(&mut buf)
                .await
                .map_err(|e| SessionError::Storage(format!("failed to read session: {e}")))?;
            if let Some(index) = buf.iter().rposition(|byte| *byte == b'\n') {
                break start + index as u64 + 1;
            }
            end = start;
        };
        file.set_len(keep)
            .await
            .map_err(|e| SessionError::Storage(format!("failed to truncate session: {e}")))?;
        Ok(len - keep)
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
            // 先写 meta 行（若存在）。本函数是 meta 行的**唯一**写入实现 ——
            // `set_meta` / `clear` / `replace_all` / `open` 的修复落盘都经它，故 meta 不会被某条路径漏写。
            // `append` 是唯一不经过本函数的写入路径，它只追加消息行、完全不碰 meta。
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
        // `flush_to_file` 全量写 `messages`，而本方法不改变 `messages` ⇒ 它之前
        // `persisted` 已等于 `messages.len()`，不需要再对齐。
        self.flush_to_file().await
    }

    /// 只往文件尾部追加一行，不重写既有内容：代价与已落盘条数无关。
    ///
    /// 单次 `write_all` 写完整行（含 `\n`）：分开写会在 `O_APPEND` 下留下半行，
    /// 崩溃后那条半行把下一次 append 串坏。
    ///
    /// 必须带 `.create(true)` —— `open` 从不创建文件，全新会话的第一次 append 时
    /// 文件还不存在。
    async fn append(&mut self, message: Message) -> Result<(), SessionError> {
        self.messages.push(message);
        // `persisted` 只由本类型维护，正常路径下恒 `<= messages.len()`。若曾出现越界
        // （全量路径漏对齐、或写成功但计数前中断），退化为「从当前尾部重新对齐」——
        // 宁可多写一行也不要把消息静默留在内存里，更不要索引越界崩掉整个会话。
        if self.persisted >= self.messages.len() {
            debug_assert!(
                false,
                "persisted={} 越界 (len={})",
                self.persisted,
                self.messages.len()
            );
            self.persisted = self.messages.len() - 1;
        }
        let line = serde_json::to_string(&self.messages[self.persisted])
            .map_err(|e| SessionError::Storage(format!("serialize failed: {e}")))?;
        // 单次 write_all 写完整行：O_APPEND 下分两次写会留下半行，
        // 崩溃后那条半行会污染下一次 append。
        let mut buf = line.into_bytes();
        buf.push(b'\n');
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)
            .await
            .map_err(|e| SessionError::Storage(format!("failed to open session: {e}")))?;
        file.write_all(&buf)
            .await
            .map_err(|e| SessionError::Storage(format!("write failed: {e}")))?;
        file.flush()
            .await
            .map_err(|e| SessionError::Storage(format!("flush failed: {e}")))?;
        self.persisted += 1;
        Ok(())
    }

    async fn clear(&mut self) -> Result<(), SessionError> {
        self.messages.clear();
        self.flush_to_file().await?;
        self.persisted = 0;
        Ok(())
    }

    /// 覆写默认实现：**赋值一次 + 单次落盘**，而不是 `clear()` + N 次 `append()`。
    ///
    /// 必要性有二：
    /// 1. 默认实现**非原子** —— `clear()` 成功后某个 `append()` 失败会留下被截断的 session。
    ///    这里复用 `flush_to_file()`（tmp + rename）的原子路径，中途失败不落盘；
    /// 2. `flush_to_file` 是 meta 行的**唯一**写入实现，整体替换必须经它，meta 才不会丢。
    async fn replace_all(&mut self, messages: Vec<Message>) -> Result<(), SessionError> {
        self.messages = messages;
        self.flush_to_file().await?;
        self.persisted = self.messages.len();
        Ok(())
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

    /// 文件当前字节数（从内容读出）。
    ///
    /// 不用 `metadata().len()`：本机上刚 `append` 完就去 stat，size 会滞后一步（读到的
    /// 内容已是新长度，metadata 还报旧值，连新开 fd 的 `fstat` 也会滞后），断言会随机变红。
    /// 读出的字节数则是实时的。
    async fn file_len(path: &Path) -> u64 {
        tokio::fs::read(path).await.unwrap().len() as u64
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
        // append 是纯追加，不经 flush_to_file —— 增量写入的断言见
        // `append_writes_only_new_line_bytes`，那里才校验「每次都写了新行」。
        assert_eq!(before, 0, "append 不走 flush_to_file，故一次都不该计数");

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

    /// append 只往文件尾追加新行，不重写既有内容。
    ///
    /// 断言：文件长度增量 == 该行序列化长度 + 1（换行），且文件尾部就是这行字节。
    ///
    /// 变异：若 append 退回全量重写，增量会等于「两行的总长度」⇒ 断言红。
    #[tokio::test]
    async fn append_writes_only_new_line_bytes() {
        let path = temp_path("append_incremental");
        let _ = tokio::fs::remove_file(&path).await;

        let mut session = JsonlSession::open(&path).await.unwrap();
        let first = text_msg(Role::User, "first");
        session.append(first.clone()).await.unwrap();
        let baseline = file_len(&path).await;
        let before_body = tokio::fs::read_to_string(&path).await.unwrap();

        let second = text_msg(Role::Assistant, "second");
        let second_line = serde_json::to_string(&second).unwrap();
        let expected_delta = second_line.len() as u64 + 1;
        session.append(second).await.unwrap();

        let grown = file_len(&path).await;
        assert_eq!(
            grown - baseline,
            expected_delta,
            "增量必须只等于新行的字节数，否则说明既有内容被重写"
        );

        let body = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(
            body,
            format!("{before_body}{second_line}\n"),
            "既有内容必须原样在前，新行紧随其后"
        );
        assert!(
            body.ends_with(&format!("{second_line}\n")),
            "文件尾部必须就是新行：{body}"
        );
        assert!(
            body.starts_with(&serde_json::to_string(&first).unwrap()),
            "首行内容必须原样保留：{body}"
        );

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// append 不经 flush_to_file：flush_count 在多次 append 前后不变。
    ///
    /// 变异：若 append 走 flush_to_file（全量重写），计数被 +3 ⇒ 断言红。
    #[tokio::test]
    async fn append_does_not_flush() {
        let path = temp_path("append_no_flush");
        let _ = tokio::fs::remove_file(&path).await;

        let mut session = JsonlSession::open(&path).await.unwrap();
        let before = session.flush_count.load(Ordering::SeqCst);
        assert_eq!(before, 0, "新建会话尚未有任何一次落盘");

        for i in 0..3 {
            session
                .append(text_msg(Role::User, &format!("m{i}")))
                .await
                .unwrap();
        }

        assert_eq!(
            session.flush_count.load(Ordering::SeqCst),
            before,
            "append 是纯追加，不得触发 flush_to_file"
        );

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// set_meta 落盘后不得丢掉既有消息；重开后 meta 与消息双双还原。
    ///
    /// 变异：若 set_meta 只写 meta 行、不清写/保留消息 ⇒ 重开消息数变少，断言红。
    #[tokio::test]
    async fn set_meta_keeps_meta_and_messages() {
        let path = temp_path("set_meta_keeps");
        let _ = tokio::fs::remove_file(&path).await;

        let meta = SessionMeta {
            title: Some("mid-session meta".into()),
            turns: 2,
            ..Default::default()
        };

        let expected = vec![
            text_msg(Role::User, "m0"),
            text_msg(Role::Assistant, "m1"),
            text_msg(Role::User, "m2"),
        ];
        {
            let mut session = JsonlSession::open(&path).await.unwrap();
            for msg in &expected {
                session.append(msg.clone()).await.unwrap();
            }
            session.set_meta(meta.clone()).await.unwrap();
        }

        let reopened = JsonlSession::open(&path).await.unwrap();
        assert_eq!(reopened.meta(), Some(&meta), "meta 字段必须逐项还原");
        assert_eq!(
            reopened.messages(),
            expected.as_slice(),
            "消息既不能丢也不能乱序"
        );
        assert!(reopened.recovery.is_none());

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// 末尾残行只计一次 `skipped_lines`，且 `open` 必须把它从磁盘切掉。
    ///
    /// 变异：截断也计数 ⇒ `skipped_lines` 变 2；不截断 ⇒ 文件不以 `\n` 结尾，
    /// 且后续 append 会与残行粘连。
    #[tokio::test]
    async fn open_skips_truncated_tail_line() {
        let path = temp_path("truncated_tail");
        let _ = tokio::fs::remove_file(&path).await;

        let msg = text_msg(Role::User, "complete");
        let mut body = serde_json::to_string(&msg).unwrap();
        body.push('\n');
        body.push_str("{\"role\":\"User\",\"content\":[");
        tokio::fs::write(&path, body.as_bytes()).await.unwrap();

        let mut session = JsonlSession::open(&path).await.unwrap();
        assert_eq!(session.messages(), std::slice::from_ref(&msg));
        let recovery = session.take_recovery().expect("残行必须计入修复摘要");
        assert_eq!(recovery.skipped_lines, 1, "残行只计一次，不得重复计数");
        assert_eq!(recovery.repaired_tool_calls, 0);
        assert_eq!(recovery.normalized_tool_arguments, 0);

        let after_truncate = tokio::fs::read(&path).await.unwrap();
        assert_eq!(
            after_truncate.last(),
            Some(&b'\n'),
            "open 必须把残行截到最后一个换行为止"
        );

        session
            .append(text_msg(Role::Assistant, "next"))
            .await
            .unwrap();
        drop(session);

        let reopened = JsonlSession::open(&path).await.unwrap();
        assert_eq!(
            reopened.messages().len(),
            2,
            "残行截断后新消息应成为独立一行"
        );
        assert!(reopened.recovery.is_none(), "截断后不应再有残行可计");

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// 多条 append 中间夹一次 set_meta：重开后消息条数与顺序都不变。
    ///
    /// 变异：若 set_meta 或 append 的 `persisted` 对齐错位 ⇒ 重开条数不对。
    #[tokio::test]
    async fn reopen_after_multi_append_counts_messages() {
        let path = temp_path("multi_append");
        let _ = tokio::fs::remove_file(&path).await;

        let expected = vec![
            text_msg(Role::User, "m0"),
            text_msg(Role::Assistant, "m1"),
            text_msg(Role::User, "m2"),
            text_msg(Role::Assistant, "m3"),
            text_msg(Role::User, "m4"),
        ];
        {
            let mut session = JsonlSession::open(&path).await.unwrap();
            session.append(expected[0].clone()).await.unwrap();
            session.append(expected[1].clone()).await.unwrap();
            session.set_meta(SessionMeta::default()).await.unwrap();
            for msg in &expected[2..] {
                session.append(msg.clone()).await.unwrap();
            }
        }

        let reopened = JsonlSession::open(&path).await.unwrap();
        assert_eq!(reopened.messages().len(), 5);
        assert_eq!(
            reopened.messages(),
            expected.as_slice(),
            "顺序必须与追加顺序一致"
        );
        assert!(reopened.meta().is_some());
        assert!(reopened.recovery.is_none());

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// meta 不因 `replace_all(vec![])` 丢失：清空消息后文件仍有 meta 行。
    ///
    /// 变异：若 `replace_all` 绕开 `flush_to_file` 自己写文件、不写 meta ⇒ 文件为空，断言红。
    #[tokio::test]
    async fn replace_all_without_prior_meta_keeps_meta() {
        let path = temp_path("replace_keeps_meta");
        let _ = tokio::fs::remove_file(&path).await;

        let meta = SessionMeta::new(11);
        let mut session = JsonlSession::open(&path).await.unwrap();
        // 先写 meta 再测：从未有过 meta 的会话清空后本就该是空文件。
        session.set_meta(meta.clone()).await.unwrap();
        session.append(text_msg(Role::User, "a")).await.unwrap();
        session
            .append(text_msg(Role::Assistant, "b"))
            .await
            .unwrap();

        session.replace_all(Vec::new()).await.unwrap();
        assert!(session.messages().is_empty());

        let len = file_len(&path).await;
        assert!(len > 0, "meta 行必须留在文件里，清空消息不等于清空文件");
        drop(session);

        let reopened = JsonlSession::open(&path).await.unwrap();
        assert_eq!(reopened.meta(), Some(&meta));
        assert!(reopened.messages().is_empty());

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// `replace_all` 之后 append 必须落在文件尾部，且不重复写已有行。
    ///
    /// 变异：若 `replace_all` 忘记把 `persisted` 对齐到 `messages.len()`，append 会
    /// 重复写 `"new"` 或跳过本次消息 ⇒ 重开条数/内容断言红。
    #[tokio::test]
    async fn append_after_replace_all_lands_at_end() {
        let path = temp_path("append_after_replace");
        let _ = tokio::fs::remove_file(&path).await;

        let mut session = JsonlSession::open(&path).await.unwrap();
        session.append(text_msg(Role::User, "old0")).await.unwrap();
        session
            .append(text_msg(Role::Assistant, "old1"))
            .await
            .unwrap();
        session
            .replace_all(vec![text_msg(Role::User, "new")])
            .await
            .unwrap();
        session
            .append(text_msg(Role::Assistant, "tail"))
            .await
            .unwrap();
        drop(session);

        let reopened = JsonlSession::open(&path).await.unwrap();
        assert_eq!(reopened.messages().len(), 2);
        assert_eq!(
            reopened.messages(),
            [
                text_msg(Role::User, "new"),
                text_msg(Role::Assistant, "tail")
            ]
        );
        assert!(reopened.recovery.is_none());

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// 首次 append 自己创建文件：`open` 从不创建，`.create(true)` 缺一不可。
    ///
    /// 变异：`OpenOptions` 去掉 `.create(true)` ⇒ append 返回 `Err` ⇒ 断言红。
    #[tokio::test]
    async fn first_append_creates_file() {
        let path = temp_path("first_append_creates");
        let _ = std::fs::remove_file(&path);
        assert!(!path.exists(), "前置条件：文件尚不存在");

        let mut session = JsonlSession::open(&path).await.unwrap();
        assert!(!path.exists(), "open 不得创建文件");
        let msg = text_msg(Role::User, "first ever");
        session.append(msg.clone()).await.unwrap();
        drop(session);

        assert!(path.exists(), "第一次 append 必须创建文件");
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(
            body,
            format!("{}\n", serde_json::to_string(&msg).unwrap()),
            "文件内容应恰为该行加换行"
        );

        let _ = tokio::fs::remove_file(&path).await;
    }

    /// 残行被截掉后 append 不与它粘连：重开仍是两条独立消息。
    ///
    /// 变异：去掉 `open` 的尾行截断 ⇒ 半行与 `"after"` 缝成一行坏数据，重开只剩 1 条。
    #[tokio::test]
    async fn append_after_truncated_tail_does_not_merge() {
        let path = temp_path("truncated_tail_append");
        let _ = tokio::fs::remove_file(&path).await;

        let kept = text_msg(Role::User, "kept");
        let mut body = serde_json::to_string(&kept).unwrap();
        body.push('\n');
        body.push_str("{\"broken\":");
        tokio::fs::write(&path, body.as_bytes()).await.unwrap();

        let mut session = JsonlSession::open(&path).await.unwrap();
        session
            .append(text_msg(Role::Assistant, "after"))
            .await
            .unwrap();
        drop(session);

        let reopened = JsonlSession::open(&path).await.unwrap();
        assert_eq!(
            reopened.messages().len(),
            2,
            "残行若未截断，半行会与新消息粘连成一行坏数据"
        );
        assert_eq!(
            reopened.messages(),
            [kept, text_msg(Role::Assistant, "after")]
        );
        assert!(reopened.recovery.is_none(), "截断后重开不应再报残行");

        let _ = tokio::fs::remove_file(&path).await;
    }
}
