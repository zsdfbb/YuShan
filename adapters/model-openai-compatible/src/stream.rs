use super::response::{ChatCompletionChunk, ChunkToolCall};
use futures::StreamExt;
use reqwest::Response;

/// SSE 流事件
pub enum StreamEvent {
    Delta {
        content: Option<String>,
        tool_calls: Option<Vec<ChunkToolCall>>,
    },
    Done,
    Error(String),
}

/// 从 OpenAI 兼容 API 解析 SSE 流
pub async fn parse_sse_stream(response: Response) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    let mut buffer = String::new();

    let mut stream = response.bytes_stream();

    while let Some(chunk_result) = stream.next().await {
        let chunk = match chunk_result {
            Ok(c) => c,
            Err(e) => {
                events.push(StreamEvent::Error(e.to_string()));
                return events;
            }
        };

        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(line_end) = buffer.find('\n') {
            let line = buffer[..line_end].trim().to_string();
            buffer = buffer[line_end + 1..].to_string();

            if line.is_empty() {
                continue;
            }
            if line == "data: [DONE]" {
                events.push(StreamEvent::Done);
                return events;
            }
            if let Some(data) = line.strip_prefix("data: ") {
                match serde_json::from_str::<ChatCompletionChunk>(data) {
                    Ok(chunk) => {
                        if let Some(choice) = chunk.choices.first() {
                            events.push(StreamEvent::Delta {
                                content: choice.delta.content.clone(),
                                tool_calls: choice.delta.tool_calls.clone(),
                            });
                        }
                    }
                    Err(_) => {} // 跳过非 JSON 的 chunk
                }
            }
        }
    }

    events
}
