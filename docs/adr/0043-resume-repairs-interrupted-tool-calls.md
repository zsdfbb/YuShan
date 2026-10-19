# 0043 — resume 自动修复被中断的工具调用

## Status

accepted（2026-10-19）

## 背景

一次工具回合落盘时，assistant 消息先写入，工具执行结果随后才写入。进程若在两者之间被终止，
会话文件会留下没有对应 `ToolResult` 的 `ToolUse`。该历史仍能逐行反序列化，所以仅靠「坏行跳过」
无法发现；下一次模型请求却会因 OpenAI-compatible 协议的 `tool_calls` / `tool` 配对不完整而失败。

同类问题还有无法反序列化的损坏行。`ys-session` 过去直接向 stderr 打印告警；TUI 起屏后这条输出会
冲进输入区，且每次 resume 都会重复出现。

## 决策

1. **打开会话时规范化工具配对**：`JsonlSession::open` 在内存中为每个缺失结果的 `ToolUse` 合成
   一条 `is_error = true` 的 `ToolResult`，内容为
   `Tool execution was interrupted; result unavailable.`。
2. **合成结果必须紧跟 assistant**：
   - 紧随其后的 User 消息已经是工具结果组时，把缺失结果追加到该消息；
   - 否则在该 assistant 后插入一条只含合成结果的 User 消息。
   这样后续用户文本仍排在这批工具结果之后，重写为 OpenAI 消息时顺序合法。
3. **不重新执行工具**。工具可能有写入、联网或其他副作用，resume 不能替用户重放；错误结果只负责
   告诉模型「该调用结果不可用」，由模型决定下一步。
4. **修复必须在返回前原子落盘**。只修内存会让下一次 resume 再次遇到同一缺口；落盘失败时
   `open` 直接失败，不返回一个看似可用、实际仍损坏的会话。无需修复时不重写文件。
5. **坏行只跳过并计数，不在库层打印**。读取按字节收集完整行，再对整行做 UTF-8 与 JSON
   解码；非 UTF-8、非法 JSON、无法判别为消息或 meta 的行都只计为一次跳过。`JsonlSession`
   通过 `SessionRecovery` 报告本次打开时合成的工具结果数与跳过的坏行数；
   `Session::take_recovery` 保证摘要只消费一次。
6. **应用层统一展示摘要**：
   - 启动 TUI：摘要进入启动期 `Output`，排在 `SessionResumed` 回放之后；
   - 运行期 `/resume`：摘要进入 `Output`，同样排在 `SessionResumed` 之后；
   - `-p` / `--json`：摘要写 stderr，绝不进入 JSON stdout。

## Considered Options

- **重放未完成的工具调用**：可能重复写文件、重复联网或执行 shell 命令。否决。
- **静默删除悬空 `ToolUse`**：历史失去「模型曾发起该调用」的事实，也让模型无法解释后续行为。否决。
- **在模型适配器里修补消息**：每个适配器都会重复实现，且会话文件仍是坏的；下一次请求前才暴露。
  否决。
- **每次打开都整形并重写**：干净会话也会发生写入，扩大丢失更新与权限问题的窗口。否决。
- **库层继续直接打印 warning**：破坏输出时机边界，TUI 起屏后会污染输入区。否决。

## Consequences

- 被中断的会话可以直接继续；模型看到的是明确的错误结果，不会把缺失结果误当成成功。
- 修复会产生一次原子全文件重写，并把无法解析的行从文件中移除。坏行数在本次 resume 中可见，
  下一次打开不再重复。
- 只读会话文件若需要修复，会因无法落盘而 resume 失败；这是「不返回半修会话」的代价。
- 已有结果按 `ToolCallId` 全局计为已配对。当前写路径只产生「assistant 后紧跟结果组」的形状；
  非该形状但结果位于别处的历史会被保留而不重排。该边界不影响本决策要处理的进程中断形状。

## 参考

- [`0033-runtime-resume-command.md`](./0033-runtime-resume-command.md) — resume 的状态切换与出站顺序
- [`../design-final/runtime-resume.md`](../design-final/runtime-resume.md) — as-built 与测试钉子
