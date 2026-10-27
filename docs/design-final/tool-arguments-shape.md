# 工具参数非对象值的自愈 — As-built

> 性质：**实际建成记录**。成因诊断见 `docs/reports/2026-10-21-minimax-400-null-arguments-report.md`。
> 日期：2026-10-21

## 一句话

`ContentBlock::ToolUse.arguments` 只允许是 **JSON 对象**。非对象值（尤其 `null`）随会话
历史回传成 `function.arguments` 字符串时是字面量 `null`，多家端点直接 400（实测
minimax：`400 (2013) invalid params`）—— 且坏块已落盘，**之后每个回合都在 round=1
失败**，用户只能 `/new` 自救。本改动把三类降级目标改为 `{}`，并补上「发送前校验 +
存量会话自愈」两道护栏。

## 线上契约（实测，非推测）

| 维度 | 行为 |
|---|---|
| 最小请求（无工具、无历史） | 200 |
| 历史含 `arguments: "null"` | **400 (2013) invalid params** |
| 历史含 `arguments: "{}"` | 200 |
| 历史含 `arguments: ""`（空串） | 200 |
| 历史含 `arguments: "\"null\""`（字面量字符串） | 200 |
| 带 `stream_options` / `reasoning_effort` / `max_tokens` | 均 200（minimax 放行） |
| 连续多条 `user`（插话形态） | 200 |
| `assistant.content` 为空串 / `null` + `tool_calls` | 均 200 |

结论：**`arguments` 是 `null` 是唯一触发点**，其余可疑项均排除。

## 落点

```
adapters/model-openai-compatible/src/lib.rs   ToolCallAccumulator::into_content_block（降级目标）
                                              translate_request（发送前兜底）
crates/ys-session/src/jsonl.rs                normalize_tool_arguments + open() 接线
crates/ys-session/src/lib.rs                  SessionRecovery.normalized_tool_arguments
apps/coding-agent/src/capabilities.rs         session_recovery_notice 新增一个分句
```

## 三道护栏（彼此独立，不可互相替代）

1. **产生端**（`into_content_block`）：空串 / 非法 JSON 的降级目标从 `Value::Null`
   改为 `Value::Object(Map::new())`。不再**生成**坏块。
2. **发送端**（`translate_request`）：`arguments` 非对象 ⇒ 序列化为 `{}`。
   这是唯一能防住**未知来源**脏数据的关——历史可能来自旧版本落盘的 JSONL、手工编辑，
   或任何在别处生成的坏块。
3. **加载端**（`normalize_tool_arguments`）：打开会话时归一化存量坏块并**落盘**。
   用户不必发现、也不必 `/new`。**必须先落盘再交给调用方**——只改内存会让下一次
   resume 再次失败（与 `repair_dangling_tool_calls` 同一条契约）。

## 关键行为（实现期定死，勿凭直觉改）

**降级目标不能是 `Value::Null`**。`ToolUse.arguments` 会随历史回传为
`function.arguments` **字符串**；`Value::Null` 序列化后是字面量 `null`。空对象在工具层
语义相同（「无参数」），线上却是合法形态。

**校验用 `is_object()`，不是「非 null」**。数组、数字、字符串同样非法——它们是
「参数的 JSON 表示」这一契约的破坏，与 `null` 同类。

**计数单独一栏**（`SessionRecovery.normalized_tool_arguments`），不并入
`repaired_tool_calls`：悬空调用是「少了一条工具结果」，参数非法是「会话会被端点持续
400 拒绝」，后者更需要用户知道。

**修复必须落盘再返回**。`open()` 里 `repaired_tool_calls > 0 || normalized_tool_arguments > 0`
共同触发 `flush_to_file()`；失败直接让 `open` 失败，不返回半修会话。

## 变异测试（自检记录）

| 注入的 bug | 期望变红的用例 |
|---|---|
| 降级目标改回 `Value::Null` | `empty_or_invalid_tool_arguments_degrade_to_empty_object_not_null`、`invalid_tool_arguments_warning_uses_logger` |
| 删掉 `translate_request` 的 `is_object()` 分支 | `translate_request_never_emits_non_object_tool_arguments` |
| 删掉 `open()` 里的 `normalize_tool_arguments` 调用 | `non_object_tool_arguments_are_normalized_and_persisted` |
| 只改内存不落盘（`flush_to_file` 条件去掉 `normalized`） | `non_object_tool_arguments_are_normalized_and_persisted` |

四者均已实测变红。

## 已修复的存量会话

`~/.yushan/sessions/` 下两个文件曾有 `arguments: null` 的 `ToolUse`
（`1792904246_962190000.jsonl`、`1793013602_262559000.jsonl`），已就地归一化为 `{}`。
新代码会让同类情况自动发生，无需人工介入。

## 未做

- **孤儿 `ToolResult` 不检查**：`repair_dangling_tool_calls` 只处理「`ToolUse` 缺
  `ToolResult`」，不处理反向（`ToolResult.tool_call_id` 指向不存在的 `ToolUse`）。
  后者同样会被 OpenAI 兼容端点拒绝，属**同类风险**，本次未纳入。
- **不校验 `type`/`name`**：工具名非法字符、超长等由端点自己拒，不在此层拦。
