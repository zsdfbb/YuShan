use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ys_component::{RunLimits, RuntimeContext};
use ys_core::{ContentBlock, Message, Role, StopReason, ToolCallId};
use ys_event::{AgentEvent, CollectingSink};
use ys_loop::{AgentInput, AgentLoop, BasicLoop};
use ys_model::{FinishReason, Model, ModelError, ModelEventSink, ModelRequest, ModelResponse};
use ys_session::MemorySession;
use ys_tool::{Tool, ToolContext};
use ys_tools_basic::exec::SearchBackends;
use ys_tools_basic::{EditTool, GrepTool, ReadTool, WriteTool};

/// 进程内唯一临时目录：**原子序号 + `process::id()`**。
///
/// 固定名（如 `yushan_test`）在并行测试下会互相 `remove_dir_all`（血的教训）；
/// `SystemTime::as_nanos()` 在 macOS 只到微秒级，单靠它也会撞名。
fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "yushan_integration_{tag}_{}_{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// 回显 tool call 的 mock model
struct MockCodingModel;

#[async_trait::async_trait]
impl Model for MockCodingModel {
    fn model_id(&self) -> &str {
        "mock-coding"
    }
    async fn complete(
        &self,
        request: ModelRequest,
        _sink: &mut dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError> {
        // 检查是否有 tool result——若有则结束
        let has_tool_results = request
            .messages
            .last()
            .map(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            })
            .unwrap_or(false);

        if has_tool_results {
            Ok(ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "Task completed.".into(),
                    }],
                },
                usage: Default::default(),
                finish_reason: FinishReason::Stop,
            })
        } else {
            // 返回一个 read tool call
            Ok(ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: ToolCallId("call_1".into()),
                        name: "read".into(),
                        arguments: serde_json::json!({ "path": "test.txt" }),
                    }],
                },
                usage: Default::default(),
                finish_reason: FinishReason::ToolCalls,
            })
        }
    }
}

/// 仿 [`MockCodingModel`]，但首次回一个 **`grep`** tool call（N12：搜索工具经循环的端到端）。
///
/// 第二次（看到 tool result 后）回文本 → 循环以 `Completed` 收束。
struct MockGrepModel;

#[async_trait::async_trait]
impl Model for MockGrepModel {
    fn model_id(&self) -> &str {
        "mock-grep"
    }
    async fn complete(
        &self,
        request: ModelRequest,
        _sink: &mut dyn ModelEventSink,
    ) -> Result<ModelResponse, ModelError> {
        let has_tool_results = request
            .messages
            .last()
            .map(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            })
            .unwrap_or(false);

        if has_tool_results {
            Ok(ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "Task completed.".into(),
                    }],
                },
                usage: Default::default(),
                finish_reason: FinishReason::Stop,
            })
        } else {
            Ok(ModelResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: ToolCallId("call_grep".into()),
                        name: "grep".into(),
                        arguments: serde_json::json!({ "pattern": "needle" }),
                    }],
                },
                usage: Default::default(),
                finish_reason: FinishReason::ToolCalls,
            })
        }
    }
}

#[tokio::test]
async fn test_read_write_edit_roundtrip() {
    let tmp = unique_dir("read_write_edit");

    // 写入文件（工具不带轮边界源：`None` = 无插话/取消）
    let write_tool = WriteTool::new(tmp.clone());
    let ctx = ToolContext::new(None, tmp.clone(), tmp.clone());
    let result = write_tool
        .call(
            serde_json::json!({ "path": "test.txt", "content": "hello world" }),
            ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error);

    // 读回
    let read_tool = ReadTool::new(tmp.clone());
    let ctx = ToolContext::new(None, tmp.clone(), tmp.clone());
    let result = read_tool
        .call(serde_json::json!({ "path": "test.txt" }), ctx)
        .await
        .unwrap();
    assert!(result.content.contains("hello world"));

    // 编辑它
    let edit_tool = EditTool::new(tmp.clone());
    let ctx = ToolContext::new(None, tmp.clone(), tmp.clone());
    let result = edit_tool
        .call(
            serde_json::json!({
                "path": "test.txt",
                "edits": [{ "oldText": "hello", "newText": "goodbye" }]
            }),
            ctx,
        )
        .await
        .unwrap();
    assert!(!result.is_error);

    // 再次读取以验证
    let ctx = ToolContext::new(None, tmp.clone(), tmp.clone());
    let result = read_tool
        .call(serde_json::json!({ "path": "test.txt" }), ctx)
        .await
        .unwrap();
    assert!(result.content.contains("goodbye"));

    // 清理
    std::fs::remove_dir_all(&tmp).ok();
}

#[tokio::test]
async fn test_full_agent_turn_with_mock_model() {
    let tmp = unique_dir("agent_turn");
    std::fs::write(tmp.join("test.txt"), "initial content").unwrap();

    let model = MockCodingModel;
    let mut session = MemorySession::new();
    let mut events = CollectingSink::new();
    let registry = ys_tool::ToolRegistry::build(vec![
        Box::new(ReadTool::new(tmp.clone())),
        Box::new(WriteTool::new(tmp.clone())),
        Box::new(EditTool::new(tmp.clone())),
    ])
    .unwrap();
    let limits = RunLimits::new(5);

    let result = {
        let mut ctx = RuntimeContext::new(
            &model,
            &registry,
            &mut session,
            &mut events,
            limits,
            tmp.clone(),
            tmp.clone(),
            None,
            None,
        );
        let input = AgentInput::text("read test.txt");
        BasicLoop.run_turn(input, &mut ctx).await.unwrap()
    };

    assert_eq!(result.stop_reason, ys_core::StopReason::Completed);
    assert!(result.final_message.is_some());

    // 清理
    std::fs::remove_dir_all(&tmp).ok();
}

/// **N12 —— 搜索工具经循环的端到端**：mock model 回一个 `grep` tool call，
/// `BasicLoop::run_turn` 必须真正执行它、把**非错误**的工具结果喂回模型，并以
/// `Completed` 收束。
///
/// 后端用 [`SearchBackends::posix_only`]**注入降级路径**：不读进程 `PATH`、不依赖
/// 开发机是否装了 `rg`，结果确定（POSIX `grep -r` 到处都有）。
///
/// 变异：把 `GrepTool` 从 `ToolRegistry::build(...)` 里去掉（或让 mock 的 grep 调用
/// 失败）→ 循环拿到的 tool result `is_error == true` → 本用例红。
#[tokio::test]
async fn test_grep_tool_via_loop_end_to_end() {
    let tmp = unique_dir("grep_e2e");
    std::fs::create_dir_all(tmp.join("src")).unwrap();
    std::fs::write(tmp.join("src/xx.txt"), "a needle in the haystack\n").unwrap();

    let model = MockGrepModel;
    let mut session = MemorySession::new();
    let mut events = CollectingSink::new();
    let registry = ys_tool::ToolRegistry::build(vec![Box::new(GrepTool::new(
        tmp.clone(),
        SearchBackends::posix_only(),
    ))])
    .unwrap();
    let limits = RunLimits::new(5);

    let result = {
        let mut ctx = RuntimeContext::new(
            &model,
            &registry,
            &mut session,
            &mut events,
            limits,
            tmp.clone(),
            tmp.clone(),
            None,
            None,
        );
        BasicLoop
            .run_turn(AgentInput::text("grep for needle"), &mut ctx)
            .await
            .unwrap()
    };

    assert_eq!(
        result.stop_reason,
        StopReason::Completed,
        "工具跑通后模型回文本，循环应以 Completed 收束"
    );

    let tool_result = events
        .events()
        .iter()
        .find_map(|e| match e {
            AgentEvent::ToolResult { result, .. } => Some(result),
            _ => None,
        })
        .expect("循环必须发出 ToolResult 事件（grep 真的被执行了）");
    assert!(
        !tool_result.is_error,
        "grep 经循环的结果不得是 error；实际：{}",
        tool_result.content
    );
    assert!(
        tool_result.content.contains("src/xx.txt"),
        "降级 grep 应命中 src/xx.txt；实际：{}",
        tool_result.content
    );

    std::fs::remove_dir_all(&tmp).ok();
}
