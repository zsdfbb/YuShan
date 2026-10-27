# minimax 400 (2013) 诊断报告 — `arguments: null` 污染会话

> 性质：**诊断记录**。建成形态见 `docs/design-final/tool-arguments-shape.md`。
> 日期：2026-10-21。触发场景：`minimax` / `MiniMax-M3.1-Flash`。

## 1. 现象

TUI 会话在第 2 回合末尾开始持续失败：

```
turn 2 failed: model error: model provider error: API error 400 Bad Request:
{"type":"error","error":{"type":"bad_request_error","message":"invalid params, 400 (2013)","http_code":"400"}}
```

关键特征：**之后 turn 3 / turn 4 都在 `round=1` 立刻失败**（`elapsed=0s~1s`），
即请求根本没能推进一个回合。换 `/new` 才能恢复。错误体只有 `invalid params`，
不指出是哪个字段 —— 这是诊断难的原因。

## 2. 定位路径

1. **缩小范围**：`yushan.log` 里同一会话有 69 个成功的 round，第 70 次失败 ⇒
   不是凭证/配额/网络问题（否则第一次就挂），而是**请求内容**变了。
2. **找差异**：失败点前一条日志是
   `warning: model tool_call arguments for `edit` are not valid JSON ... falling back to null`。
   即模型吐了被截断的参数 JSON，适配器按旧设计降级为 `Value::Null`。
3. **扫会话文件**：脚本遍历 JSONL，找出 `ToolUse.arguments` 为 `null` 的块 ——
   恰好 1 处（该 `edit` 调用）。再扫其余会话，发现另一个历史会话也有 1 处。
4. **线上复现**（真实凭证对照实验，见下表）：只把这一处从 `"null"` 换成 `"{}"`，
   同一段历史立刻 200。

## 3. 对照实验（真实凭证，逐项注入）

| 变体 | 结果 |
|---|---|
| 最小请求（无工具、无历史） | 200 |
| 历史含 `arguments: "null"` | **400 (2013) invalid params** |
| 历史含 `arguments: "{}"` | 200 |
| 历史含 `arguments: ""`（空串） | 200 |
| 历史含 `arguments: "\"null\""`（字面量字符串） | 200 |
| 带 `stream_options.include_usage`（流式） | 200 |
| 带 `reasoning_effort: "high"` | 200 |
| 带 `max_tokens: 100` | 200 |
| 连续三条 `user`（插话形态） | 200 |
| `assistant.content: ""` + `tool_calls` | 200 |
| `assistant.content: null` + `tool_calls` | 200 |

结论：**`arguments` 的值是字面量 `"null"` 是唯一触发点**，其余可疑项全部排除。

## 4. 根因

`ContentBlock::ToolUse.arguments` 会随会话历史回传为 `function.arguments`
**字符串**。`serde_json::Value::Null` 序列化后是**字面量 `null`** ——
不是 `"{}"`、不是 `""`。minimax 要求该字段是 JSON **对象**的字符串表示，
收到 `null` 即拒。

旧实现在两处把降级目标设成 `Value::Null`：

- `ToolCallAccumulator::into_content_block`：参数 JSON 解析失败 / 空串
- 其结果经 `jsonl.rs` 落盘 ⇒ **坏块进入会话历史**

所以这不是「一次请求失败」，而是**会话被永久污染**：每个后续回合都把同一个
坏块重新发一遍，于是每回合都在 `round=1` 失败。这一点是全诊断里最关键的认知
—— 它解释了「为什么重试没用、为什么 `/new` 才是唯一出路」。

## 5. 为什么测试没抓到

既有测试 `invalid_tool_arguments_warning_uses_logger` **断言了旧行为**
（`assert_eq!(arguments, &serde_json::Value::Null)`），文档注释还写着
「仍按设计降级为 null」。也就是说：**测试忠实地保护了这个 bug**。

教训：断言「降级产物是什么」时，要问的不是「实现现在返回什么」，而是
「这个产物到了**下一跳**（这里是线上 JSON）是否合法」。降级值的安全性属于
**跨组件契约**，不是局部实现细节。

## 6. 处置

三道彼此独立的护栏（详见 `docs/design-final/tool-arguments-shape.md`）：

1. 产生端：降级目标改 `{}`
2. 发送端：`translate_request` 对非对象 `arguments` 一律归一化为 `{}`
3. 加载端：`normalize_tool_arguments` 在 `open()` 时自愈存量坏块并落盘

已就地修复的两个存量会话文件：
`1792904246_962190000.jsonl`、`1793013602_262559000.jsonl`。

## 7. 未纳入（同类风险）

- **孤儿 `ToolResult`**：`repair_dangling_tool_calls` 只处理「`ToolUse` 缺
  `ToolResult`」，不处理反向（`tool_call_id` 指向不存在的 `ToolUse`）。后者同样
  会被 OpenAI 兼容端点拒绝。
- **`tool_call_id` 为空**、`type` 非 `function`、工具名超长：均由端点自己拒，
  未在适配器层拦。
