//! `TodoTool` —— 模型唯一能写清单的入口：解析 → `store.apply` → 回喂渲染结果。
//!
//! # 为什么是 Tool 而不是别的东西
//!
//! 判断链（`docs/extension-points.md`）：**模型决定何时做 ⇒ Tool**。写计划是模型的决定，
//! 因此它是工具；而「回合结束后要不要再来一轮」无论模型怎么想都必然发生 ⇒ 那是 follow-up，
//! 不是工具。两个机制各管一半，没有第三个。
//!
//! # 永不声明 `is_concurrency_safe`
//!
//! 默认 `false`。清单是**共享状态**，「同一批里不并行两个 `todo`」是唯一的防线 ——
//! 打开它等于允许两个 `apply` 交错，后一个的「既有项」会是前一个刚写完的中间态。

use std::sync::Arc;

use serde_json::{Value, json};
use ys_core::ToolResult;
use ys_tool::{Tool, ToolContext, ToolError, ToolSpec};

use crate::item::TodoItem;
use crate::render::render_block;
use crate::store::{ApplyOutcome, TodoStore};

/// `todo` 工具。
pub struct TodoTool {
    store: Arc<TodoStore>,
}

impl TodoTool {
    pub fn new(store: Arc<TodoStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl Tool for TodoTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "todo",
            "维护本次任务的计划清单（长程任务的唯一真值，落盘、跨重启保留）。\
             \n\n**每次调用都提交完整清单**（全量重写，不是增量）—— 既有项要原样带上，\
             只改它们的 status。\n\n规则（由代码强制，违反会被拒绝）：\
             \n- 不能删除仍在 pending / in_progress 的项；不做的事请标成 cancelled。\
             \n- 所有项都到达终态（completed 或 cancelled）时清单会被清空并删除 —— \
             这时结果会明确告诉你「计划已完成并清空」。\
             \n\n长任务开工前先写清单，之后每完成一项就更新它；这是防止目标漂移的唯一手段。",
            json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "完整清单（全量替换当前清单）。空数组只在没有未完成项时可用。",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "description": "任务内容，一句话、可执行、可验收"
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed", "cancelled"],
                                    "description": "pending=待办；in_progress=正在做（同一时间建议只有一项）；completed=已完成；cancelled=决定不做了（诚实停下用这个，不要用 completed 伪装完成）"
                                },
                                "id": {
                                    "type": "string",
                                    "description": "可选。仅用于你区分条目，不落盘、不参与渲染"
                                }
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        )
    }

    // ⚠️ 刻意**不实现** `is_concurrency_safe`（保持默认 `false`）：见模块头。

    async fn call(&self, input: Value, _ctx: ToolContext<'_>) -> Result<ToolResult, ToolError> {
        let incoming = match parse(&input) {
            Ok(items) => items,
            // 入参错误是**模型可见的**结果，不是基础设施失败：让它自己改正。
            Err(message) => return Ok(ToolResult::text(message, true)),
        };

        match self.store.apply(incoming) {
            Ok(outcome) => Ok(ToolResult::text(self.render_result(outcome), false)),
            // 拒绝原因必须回到模型眼前 —— 否则它只会重试同样的写入。
            Err(err) => Ok(ToolResult::text(err.to_string(), true)),
        }
    }
}

impl TodoTool {
    /// 回喂文本 = 最新渲染（与注入块同源，模型看到的就是被推进上下文的那份）。
    fn render_result(&self, outcome: ApplyOutcome) -> String {
        let items = self.store.snapshot();
        let block = render_block(&items, self.store.revision());
        match outcome {
            ApplyOutcome::ClearedAllDone => {
                format!("计划已完成并清空（全部项都到达终态），文件已删除。当前没有计划。\n{block}")
            }
            ApplyOutcome::Updated if items.is_empty() => {
                format!("清单已更新（当前为空）。\n{block}")
            }
            ApplyOutcome::Updated => {
                let open = items.iter().filter(|i| !i.status.is_terminal()).count();
                let total = items.len();
                format!("清单已更新：{total} 项，其中 {open} 项未完成。\n{block}")
            }
        }
    }
}

/// 解析 `{"todos": [...]}`。错误文案面向模型（说明**怎么改**，不只说错在哪）。
fn parse(input: &Value) -> Result<Vec<TodoItem>, String> {
    let raw = input.get("todos").ok_or_else(|| {
        "缺少 `todos` 字段。请提交完整清单：{\"todos\": [{\"content\": \"…\", \"status\": \"pending\"}]}".to_string()
    })?;
    let raw = raw
        .as_array()
        .ok_or_else(|| "`todos` 必须是数组（提交完整清单，不是增量）。".to_string())?;

    let mut items = Vec::with_capacity(raw.len());
    for (index, entry) in raw.iter().enumerate() {
        let item: TodoItem = serde_json::from_value(entry.clone()).map_err(|err| {
            format!(
                "第 {} 项解析失败：{err}。每项需要 `content`（字符串）与 `status`\
                 （pending / in_progress / completed / cancelled 之一）。",
                index + 1
            )
        })?;
        items.push(item);
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ctx, test_dir};

    async fn call(tool: &TodoTool, input: Value) -> ToolResult {
        tool.call(input, ctx()).await.expect("工具不应返回 Err")
    }

    fn store_in(dir: &std::path::Path, stem: &str) -> Arc<TodoStore> {
        TodoStore::open(Some(&dir.join(format!("{stem}.jsonl"))))
    }

    /// spec 的硬形状：名字是 `todo`，`todos` 必填，状态枚举含 `cancelled`。
    ///
    /// 变异：把 `cancelled` 从 enum 里去掉 ⇒ 模型失去「诚实停下」的出口 ⇒ 本用例变红。
    #[test]
    fn spec_shape() {
        let store = TodoStore::open(None);
        let spec = TodoTool::new(store).spec();

        assert_eq!(spec.name, "todo");
        assert_eq!(
            spec.parameters["required"],
            json!(["todos"]),
            "`todos` 必填"
        );
        let statuses =
            &spec.parameters["properties"]["todos"]["items"]["properties"]["status"]["enum"];
        assert!(
            statuses
                .as_array()
                .expect("enum 是数组")
                .iter()
                .any(|v| v == "cancelled"),
            "必须提供 cancelled 终止态；实际：{statuses}"
        );
        assert!(
            spec.description.contains("全量"),
            "描述必须说清「每次提交完整清单」"
        );
    }

    /// **永不**声明并发安全 —— 共享状态 + 并行是唯一防线。
    ///
    /// 变异：加上 `fn is_concurrency_safe(&self) -> bool { true }` ⇒ 本用例变红。
    #[test]
    fn never_concurrency_safe() {
        let store = TodoStore::open(None);
        assert!(
            !TodoTool::new(store).is_concurrency_safe(),
            "todo 是共享状态，同一批里不得并行两个调用"
        );
    }

    /// 正常写入 ⇒ `is_error = false`，结果里带渲染块。
    #[tokio::test]
    async fn call_writes_and_returns_rendered_block() {
        let dir = test_dir("tool_ok");
        let tool = TodoTool::new(store_in(&dir, "s1"));

        let result = call(
            &tool,
            json!({"todos": [
                {"content": "读设计", "status": "completed"},
                {"content": "写代码", "status": "in_progress"}
            ]}),
        )
        .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("2 项，其中 1 项未完成"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("<todo_list revision="),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("2. [in_progress] 写代码"),
            "{}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 被拒绝时 ⇒ `is_error = true`，且**文案说明怎么改**（不只说错在哪）。
    ///
    /// 变异：把 `apply` 的错误吞成 `is_error = false` ⇒ 模型看不见拒绝原因，会原样重试。
    #[tokio::test]
    async fn rejection_is_visible_to_model() {
        let dir = test_dir("tool_reject");
        let tool = TodoTool::new(store_in(&dir, "s1"));

        call(
            &tool,
            json!({"todos": [{"content": "写代码", "status": "in_progress"}]}),
        )
        .await;

        let rejected = call(&tool, json!({"todos": []})).await;
        assert!(rejected.is_error, "丢弃 open 项必须被拒绝");
        assert!(
            rejected.content.contains("cancelled"),
            "{}",
            rejected.content
        );
        assert!(rejected.content.contains("写代码"), "{}", rejected.content);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 全部终态 ⇒ 结果**显式**告诉模型「计划已完成并清空」（否则它困惑于列表去哪了）。
    #[tokio::test]
    async fn cleared_all_done_is_explicit() {
        let dir = test_dir("tool_cleared");
        let tool = TodoTool::new(store_in(&dir, "s1"));

        let result = call(
            &tool,
            json!({"todos": [
                {"content": "a", "status": "completed"},
                {"content": "b", "status": "cancelled"}
            ]}),
        )
        .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("计划已完成并清空"),
            "必须显式回喂删除事实；实际：{}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 入参错误 ⇒ 模型可见的错误结果，**不是** `Err`（不该中断循环）。
    #[tokio::test]
    async fn malformed_input_is_error_result_not_err() {
        let store = TodoStore::open(None);
        let tool = TodoTool::new(store);

        let missing = call(&tool, json!({})).await;
        assert!(missing.is_error);
        assert!(missing.content.contains("`todos`"), "{}", missing.content);

        let not_array = call(&tool, json!({"todos": "写代码"})).await;
        assert!(not_array.is_error);
        assert!(not_array.content.contains("数组"), "{}", not_array.content);

        let bad_status = call(
            &tool,
            json!({"todos": [{"content": "a", "status": "done"}]}),
        )
        .await;
        assert!(bad_status.is_error);
        assert!(
            bad_status.content.contains("第 1 项"),
            "{}",
            bad_status.content
        );

        let missing_content = call(&tool, json!({"todos": [{"status": "pending"}]})).await;
        assert!(missing_content.is_error);
    }

    /// 入参里的 `id` 被忽略，不影响写入。
    #[tokio::test]
    async fn id_is_ignored() {
        let store = TodoStore::open(None);
        let tool = TodoTool::new(store);

        let result = call(
            &tool,
            json!({"todos": [{"id": "t1", "content": "a", "status": "pending"}]}),
        )
        .await;

        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("1. [pending] a"),
            "{}",
            result.content
        );
    }

    /// 回喂块与 `render_block` **同源** —— 模型在工具结果里看到的，就是会被推进上下文的那份。
    #[tokio::test]
    async fn result_block_matches_render_block() {
        let store = TodoStore::open(None);
        let tool = TodoTool::new(store.clone());

        call(
            &tool,
            json!({"todos": [{"content": "a", "status": "pending"}]}),
        )
        .await;

        let expected = render_block(&store.snapshot(), store.revision());
        let result = call(
            &tool,
            json!({"todos": [{"content": "a", "status": "pending"}]}),
        )
        .await;
        assert!(result.content.contains(&expected), "{}", result.content);
    }
}
