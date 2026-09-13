use super::response::{ChatCompletionChunk, ChatUsage, ChunkToolCall};
use futures::StreamExt;
use reqwest::Response;
use ys_model::ModelError;

/// SSE 流事件
pub enum StreamEvent {
    Delta {
        content: Option<String>,
        /// DeepSeek 等 provider 的思考内容增量（`reasoning_content`）。
        reasoning_content: Option<String>,
        tool_calls: Option<Vec<ChunkToolCall>>,
        /// 该 chunk 的 `finish_reason`（最后一个非 `None` 值即终止原因）。
        finish_reason: Option<String>,
        /// 该 chunk 携带的 usage。仅当服务端在流式末包附带统计时出现
        /// （OpenAI 系需请求 `stream_options.include_usage`）。
        usage: Option<ChatUsage>,
    },
    Done,
    Error(String),
}

/// 把一段字节喂入行缓冲，**每解析出一行就立即回调**。
///
/// 这是 [`parse_sse_stream`] 的可测核心：入参不是 `reqwest::Response`，
/// 因此可以用人造字节流做单元测试（含 TCP 分片场景）。
///
/// **缓冲是字节级的**（`Vec<u8>`）：TCP 分片可能切在一个多字节 UTF-8 字符
/// （中文/emoji）的**中间**，若对每个 chunk 单独做 `from_utf8_lossy`，半个字符
/// 会被替换成 U+FFFD 造成不可逆损坏。因此这里只在**字节层面**找 `\n`，
/// **仅对已闭合的完整行**做 UTF-8 解码——此时该行不跨片，多字节字符必然完整。
/// 未闭合的字节（可能只是某个字符的前半截）原样留在 `buffer`，等下一片凑齐。
///
/// 支持跨 chunk 的行边界——不完整的行留在 `buffer` 里，等下一段字节到达后继续。
/// 返回 `Ok(true)` 表示已收到 `data: [DONE]`，调用方应停止读取。
///
/// 非 JSON 的 `data:` 行**跳过**（现状如此）；`StreamEvent::Error` 交由调用方
/// 通过 `on_event` 决定如何处置。
pub fn feed_sse_bytes<F>(
    buffer: &mut Vec<u8>,
    chunk: &[u8],
    on_event: &mut F,
) -> Result<bool, ModelError>
where
    F: FnMut(StreamEvent) -> Result<(), ModelError>,
{
    buffer.extend_from_slice(chunk);

    // 在字节层面定位行边界；未闭合的尾部字节留在 buffer 里等下一片。
    while let Some(line_end) = buffer.iter().position(|&b| b == b'\n') {
        // 只取走完整的一行（含行尾 `\n`），此时行内不跨片。
        let line_bytes: Vec<u8> = buffer.drain(..=line_end).collect();
        // 行内已是完整 UTF-8；用 lossy 仅为兜底（真正非法字节也不 panic）。
        let line = String::from_utf8_lossy(&line_bytes).trim().to_string();

        if line.is_empty() {
            continue;
        }
        // SSE 规范允许 `data:{...}`（无空格）；统一 strip 后再 trim 前导空白。
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim_start();
            if data == "[DONE]" {
                on_event(StreamEvent::Done)?;
                return Ok(true);
            }
            // 非 JSON 行跳过（现状如此），不影响后续行
            let Ok(parsed) = serde_json::from_str::<ChatCompletionChunk>(data) else {
                continue;
            };
            let usage = parsed.usage.clone();
            if let Some(choice) = parsed.choices.first() {
                on_event(StreamEvent::Delta {
                    content: choice.delta.content.clone(),
                    reasoning_content: choice.delta.reasoning_content.clone(),
                    tool_calls: choice.delta.tool_calls.clone(),
                    finish_reason: choice.finish_reason.clone(),
                    usage,
                })?;
            } else if let Some(usage) = usage {
                // usage 尾包通常 `choices: []`——没有 delta 可发，但统计必须上报，
                // 否则流式 token 计数会丢失。
                on_event(StreamEvent::Delta {
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                    finish_reason: None,
                    usage: Some(usage),
                })?;
            }
        }
    }
    Ok(false)
}

/// 从 OpenAI 兼容 API 解析 SSE 流，**边解析边回调** `on_event`。
///
/// 每解析出一行就立刻回调，不先攒成 `Vec`——这是「增量实时推送」的前提。
/// 传输层错误回调 [`StreamEvent::Error`]（不在此返回 `Err`），让调用方统一
/// 在流结束时收敛；`on_event` 自身的错误则原样上抛。
pub async fn parse_sse_stream<F>(response: Response, mut on_event: F) -> Result<(), ModelError>
where
    F: FnMut(StreamEvent) -> Result<(), ModelError>,
{
    // 字节级缓冲：`Bytes` 直接按字节喂入，绝不在喂入前转 String。
    let mut buffer: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();

    while let Some(chunk_result) = stream.next().await {
        match chunk_result {
            Ok(chunk) => {
                if feed_sse_bytes(&mut buffer, &chunk, &mut on_event)? {
                    return Ok(());
                }
            }
            Err(e) => {
                on_event(StreamEvent::Error(e.to_string()))?;
                return Ok(());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 收集回调序列，便于断言。
    fn feed_all(chunks: &[&[u8]]) -> (Vec<&'static str>, Vec<Option<String>>, bool) {
        let mut buffer: Vec<u8> = Vec::new();
        let mut kinds: Vec<&'static str> = Vec::new();
        let mut contents: Vec<Option<String>> = Vec::new();
        let mut done = false;
        for chunk in chunks {
            let stream_done = feed_sse_bytes(&mut buffer, chunk, &mut |ev| {
                match ev {
                    StreamEvent::Delta { content, .. } => {
                        kinds.push("delta");
                        contents.push(content);
                    }
                    StreamEvent::Done => kinds.push("done"),
                    StreamEvent::Error(_) => kinds.push("error"),
                }
                Ok(())
            })
            .unwrap();
            if stream_done {
                done = true;
                break;
            }
        }
        (kinds, contents, done)
    }

    /// 1. 行边界跨 chunk：一条 `data:` 行被切成两半喂入，仍解析出完整 delta。
    #[test]
    fn feed_sse_bytes_handles_split_line_boundary() {
        let first = br#"data: {"choices":[{"index":0,"delta":{"content":"Hel"#;
        let second = br#"lo"}}]}"#;
        let third = b"\n";
        let (kinds, contents, done) = feed_all(&[first, second, third]);
        assert!(!done);
        assert_eq!(kinds, vec!["delta"]);
        assert_eq!(contents, vec![Some("Hello".to_string())]);
    }

    /// 2. 单个 chunk 内含多行 `data:`：逐行回调，且 `[DONE]` 终止。
    #[test]
    fn feed_sse_bytes_multiple_data_lines_then_done() {
        let chunk = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\"}}]}\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"b\"}}]}\n\
data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\
data: [DONE]\n";
        let (kinds, contents, done) = feed_all(&[chunk]);
        assert!(done);
        assert_eq!(kinds, vec!["delta", "delta", "delta", "done"]);
        assert_eq!(
            contents,
            vec![Some("a".to_string()), Some("b".to_string()), None]
        );
    }

    /// 3. 非 JSON 行被跳过，其后合法行仍能解析。
    #[test]
    fn feed_sse_bytes_skips_non_json_lines() {
        let chunk = b": keep-alive comment\n\
data: not-json\n\
data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n";
        let (kinds, contents, done) = feed_all(&[chunk]);
        assert!(!done);
        assert_eq!(kinds, vec!["delta"]);
        assert_eq!(contents, vec![Some("ok".to_string())]);
    }

    /// 4. `reasoning_content` 字段被解析进事件（是否采用由适配器的 compat 决定）。
    #[test]
    fn feed_sse_bytes_parses_reasoning_content() {
        let chunk =
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"think\"}}]}\n";
        let mut buffer: Vec<u8> = Vec::new();
        let mut seen: Option<Option<String>> = None;
        feed_sse_bytes(&mut buffer, chunk, &mut |ev| {
            if let StreamEvent::Delta {
                reasoning_content, ..
            } = ev
            {
                seen = Some(reasoning_content);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, Some(Some("think".to_string())));
    }

    /// 5. 未闭合的行留在缓冲里，不产生事件（等下一片）。
    #[test]
    fn feed_sse_bytes_buffers_incomplete_line() {
        let mut buffer: Vec<u8> = Vec::new();
        let mut count = 0;
        feed_sse_bytes(&mut buffer, b"data: {\"choices\":[{\"inde", &mut |_| {
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 0);
        assert!(!buffer.is_empty());
    }

    /// 6. **跨分片多字节 UTF-8 回归**：一个汉字的 3 字节被 TCP 切在中间
    ///    （`你` = E4 B8 AD，先喂 E4，再喂 B8 AD），不得出现 U+FFFD。
    ///
    ///    修复前 buffer 为 `String` 且逐 chunk `from_utf8_lossy`，
    ///    两个半截都变 U+FFFD → 解析出 `���`；修复后按字节缓冲，
    ///    只为完整行解码，拼出的必是 `你好世界`。
    #[test]
    fn feed_sse_bytes_preserves_multibyte_utf8_split_across_chunks() {
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好世界\"}}]}\n"
            .as_bytes()
            .to_vec();
        // 切在「你」与「世」两个字符的中间（各取首字节之后）
        let ni = body
            .windows(3)
            .position(|w| w == "你".as_bytes())
            .expect("应含「你」");
        let shi = body
            .windows(3)
            .position(|w| w == "世".as_bytes())
            .expect("应含「世」");
        let (a, b, c) = (&body[..ni + 1], &body[ni + 1..shi + 1], &body[shi + 1..]);

        let (kinds, contents, done) = feed_all(&[a, b, c]);
        assert!(!done);
        assert_eq!(kinds, vec!["delta"]);
        assert_eq!(
            contents,
            vec![Some("你好世界".to_string())],
            "跨分片多字节字符不得被替换为 U+FFFD"
        );
        assert!(
            !contents[0].as_deref().unwrap().contains('\u{FFFD}'),
            "输出不应含替换字符"
        );
    }

    /// 7. SSE 前缀容错：规范允许 `data:{...}`（无空格），也应被解析。
    #[test]
    fn feed_sse_bytes_accepts_data_prefix_without_space() {
        let chunk = b"data:{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"nospace\"}}]}\n";
        let (kinds, contents, done) = feed_all(&[chunk]);
        assert!(!done);
        assert_eq!(kinds, vec!["delta"]);
        assert_eq!(contents, vec![Some("nospace".to_string())]);
    }

    /// 8. 末包只给 `finish_reason`、不含 `delta` 字段时仍能解析，
    ///    且 `finish_reason` 被捕获（不得因缺 `delta` 整行反序列化失败被跳过）。
    #[test]
    fn feed_sse_bytes_parses_chunk_without_delta_field() {
        // 注意：JSON 里**没有** `"delta"` 键
        let chunk = b"data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\"}]}\n";
        let mut buffer: Vec<u8> = Vec::new();
        let mut finish: Option<Option<String>> = None;
        feed_sse_bytes(&mut buffer, chunk, &mut |ev| {
            if let StreamEvent::Delta { finish_reason, .. } = ev {
                finish = Some(finish_reason);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(finish, Some(Some("stop".to_string())));
    }

    /// 9. 兼容端点不返回 `total_tokens` 时，`ChatUsage` 仍能反序列化（按 0 计）。
    #[test]
    fn chat_usage_tolerates_missing_total_tokens() {
        let parsed: ChatCompletionChunk = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4}}"#,
        )
        .expect("缺 total_tokens 不应导致反序列化失败");
        let usage = parsed.usage.expect("usage 应存在");
        assert_eq!(usage.prompt_tokens, 3);
        assert_eq!(usage.completion_tokens, 4);
        assert_eq!(usage.total_tokens, 0);
    }
}
