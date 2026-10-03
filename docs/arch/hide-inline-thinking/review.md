# hide-inline-thinking — 架构质量分析报告

## 分析范围

- **对象**：[`design.md`](./design.md) 的最终方案（融合 A+C）+ [`../../adr/0025-hide-inline-thinking-tags.md`](../../adr/0025-hide-inline-thinking-tags.md)
- **维度**：全量（可行性 / 可维护性 / 可理解性 / 性能与可靠性）
- **来源**：`context.md` + `design.md` + 对 `adapters/model-openai-compatible`、`crates/ys-tui-coding` 的实读校验

## 各维度判断

| 维度 | 判断 | 关键发现 |
|------|------|----------|
| 1.1 技术可实现性 | 🟢 | 算法有同仓库可用参考（CodeWhale `chat.rs:3774`）与自有实证；无「黑科技」 |
| 1.2 依赖成熟度 | 🟢 | **零新增依赖**（明确不用传递引入的 `memchr`）；`std` 的 `find`/`starts_with` 足够 |
| 1.3 实现周期 | 🟢 | 约 120 行实现 + 测试；步骤清晰、可独立验证 |
| 2.1 模块边界 | 🟢 | 方言留在适配器，符合「方言归适配器」；`ys-loop`/`ys-event`/TUI 思考链零改动 |
| 2.2 接口稳定性 | 🟢 | `pub(crate)`，非公开契约；未来 `ReasoningStreamStyle` 迁移只需替换 `const` 表实参 |
| 2.3 错误处理 | 🟢 | 过滤器是纯函数、无失败路径；`sink.emit` 沿用既有 `let _ =`（消费者消失不阻断累积） |
| 2.4 并发安全 | 🟢 | 过滤器为 `StreamAccumulator` 私有、单任务持有，无共享状态/锁 |
| 2.5 资源管理 | 🟢 | `hold` 上界 11B（已推导）；无「为找闭合而缓冲整条消息」路径 |
| 3.1 概念一致性 | 🟡 | 标签名 `reasoning` 与领域词 `Thinking` 不同名 —— 需注释点明「`reasoning` 标签 → `ThinkingDelta`」 |
| 3.2 上手成本 | 🟡 | `scan` 的下标算术（`lt`/`pos`/`lt+1`）是最晦涩处；配对闭合语义需一段契约注释 |
| 3.3 文档一致度 | 🟡 | **D5 与既有文档冲突未列全**（见 R2） |
| 4.1 性能模型 | 🟢 | 见下方 back-of-envelope；快路径保住现状零额外分配 |
| 4.2 故障模式 | 🟡 | 误剥/未闭合会**静默吞正文**（见 R1）；无降级或可观测信号 |
| 4.3 可观测性 | 🟡 | 剥离发生与否无任何信号，排障靠猜（且**不能**用 `eprintln`，见下） |

## 算法正确性校验（实读推导）

按 `design.md` §5.2 的 `scan` 逐条走查，**未发现正确性缺陷**，几处易错点已自洽：

- `find('<')` 恒落 char 边界：`0x3C` 不可能是 UTF-8 续字节（`0x80–0xBF`）或首字节（`≥0xC2`）。
- 切点 `pos = lt + 1` / `lt + d.len()` 均为 char 边界；`hold` 由 `s.len() < d.len()` 保证 ≤11B。
- 假前缀不复用错误状态：`hold="<thi"` + 下片 `"x"` 合成 `<thix`，`is_proper_prefix` 判否 → 正确地把 `<` 当字面量吐出。
- **空段不会产生**：`if lt > pos` 守卫 + 循环条件结构性地排除了空 `Text`/`Thinking`，无需额外过滤（建议写进注释）。
- 正文态遇到孤立的 `</think>` 原样保留（符合 D1）。
- 配对闭合：`inside: Option<usize>` 下，`<think>a<reasoning>b</reasoning>c</think>` 的内层标签留在 Thinking 中 —— 行为已定为意图，需测试钉住。

## 风险排序

| # | 风险 | 影响 | 可能性 | 优先级 | 建议 |
|---|------|------|--------|--------|------|
| R1 | **误剥 / 未闭合 `<think>` 吞掉其后全部正文**，且因剥离发生在写入 `self.text` 之前，内容**从会话历史中永久消失**（比 TUI 隐藏更重；模型后续也看不到） | 高 | 中 | **P1** | 见「需讨论」1（边界约束）；至少在 design 里把「历史也被删」写明，供 reviewer 决策 |
| R2 | **D5 与既有文档/测试的冲突清单不全**：`design-final/coding-agent-tui.md` §5「thinking 总是打印」、`docs/design.md` §12 Coding Agent 行「退出打印完整对话」、`draw.rs:539` 注释；且 `transcript_plain_lines` **当前无 thinking 相关测试** | 中 | 高 | **P1** | 实施步骤里加一步「同步文档 + 新增 D5 测试」，否则设计与 as-built 立即分叉 |
| R3 | 多块思考之间的 `\n\n` 会成为**只含空白的 Assistant 行**（MiniMax 实测串就是 `<think>a</think>\n\n<think>b</think>\n\n正文`）；`/copy` 取「最后一条非空 Assistant」，极端情况（无最终答复）会取到空白 | 低 | 高 | P2 | 见「需讨论」2：是否折叠与 Thinking 相邻的纯空白 Text 段 |
| R4 | `--json` 事件流对标签型 provider **新增 `ModelThinkingDelta`** | 低 | 中 | P2 | 契约内（`#[non_exhaustive]`），但需在 ADR/CHANGELOG 知会；检查无断言精确序列的测试 |
| R5 | 剥离无任何可观测信号 | 低 | 中 | P2 | 可加一个进程内计数（经既有日志文件路径）；**不得 `eprintln`** |
| R6 | 大小写 / 带属性 / 空白变体（`<THINK>`、`<think >`、`<think\n>`）不支持 | 低 | 低 | P3 | 明确写为范围外即可 |
| R7 | provider 同时给 `reasoning_content` 与内联标签时，思考可能**重复显示** | 低 | 低 | P3 | 记录为已知残留；无实测，不做去重 |

## 性能 back-of-envelope

场景：2KB 回复、每 delta ≈5 字符（≈410 片）、其中 2 个 think 块。

| 路径 | 触发次数 | 额外分配/拷贝 |
|---|---|---|
| 快路径（无 `<`） | ≈408 片 | **0 分配**；仅一次 `contains('<')` O(len) 扫描 |
| 慢路径（含 `<`） | ≈4 片（标签边界） | 每片 1 个 `Vec` + 1–2 个 `String`；`hold` 非空时一次 `format!`（拷贝 ≤11B + 该片） |
| **合计** | | **约十数个小 String**；无 O(message) 缓冲 |

对比被否的 A/B 形态：每片多 1 次 `pending.push_str` + 1 次 `to_string` + 1 次 `drain` memmove（≈410 次额外分配）。
结论：快路径把本设计与现状的差异压到「一次 O(n) 扫描」，值得。

## 改进建议

### 易修复（低风险快速改进）

1. **补实测推导出的两个实现约束**（写进 `design.md` §5.2/§5.3）：
   - 必须**保留** `handle` 里既有的 `if !content.is_empty()` 守卫 —— 否则空 delta 会走快路径发出空 `TextDelta`，在 TUI 落一条空 Assistant 行；
   - `finish` 改为 `fn finish(mut self)`，且 `flush` 在**错误检查之后、构造 `content_blocks` 之前**执行，`flush` 幂等。
2. **补文档同步清单**（R2）：`design-final/coding-agent-tui.md` §5 与 §6 命令表、`docs/design.md` §12 的「退出打印完整对话」措辞、`draw.rs:539-540` 注释；并新增 `transcript_plain_lines` 的 D5 测试（`thinking_visible=false` 不含 Thinking、`=true` 含）。
3. **契约注释**：`TAGS` 旁写明「`reasoning` 标签映射到 `ThinkingDelta`（与领域词 `Thinking` 同义）」；`scan` 旁写明「`if lt > pos` 即空段不产出，无需额外过滤」。
4. 在 `design.md` §8 补一句：`tool_calls_as_text`（MiniMax）虽为 `true`，但**生产代码从未使用该字段**（仅 `compat.rs` 定义 + 测试断言），故「工具 JSON 里含 `<think>` 被误剥」当前是理论风险、非现实风险。

### 需讨论（需要团队决策）

1. **是否为开标签加「文本边界」约束**（R1 的主要缓解）：只把「位于文本起始或 `\n` 之后」的 `<think>` 当作思考开关。
   - 收益：几乎消除「正文/代码块里讨论 `<think>`」的误剥，以及最痛的「未闭合吞掉长答案」。
   - 代价：过滤器需多记「上一个字符是否换行」，跨 delta 也要维护；且可能漏掉真实用例（若某 provider 把标签直接贴在词后）。
   - 我的倾向：**先按 D6 不加**，等真出现误剥投诉再加；但应把这条作为「复访触发条件」写进 ADR。
2. **多块思考之间的纯空白 Text 段是否折叠**（R3）：可只在 `flush`/`emit` 时丢弃「紧邻 Thinking 的纯空白 Text」。收益是 TUI 无空行、`/copy` 更稳；代价是引入「空白策略」（与 D1「只剥标签」略有张力）。
3. **可观测性**（R5）：是否加一个轻量计数（例如剥离了多少字节），经既有日志文件暴露。

### 架构级（影响面大，需要跨 phase 规划）

1. **`ProviderCompat` 结构化（gap-closure F11）**：ADR 已留口（`ReasoningStreamStyle{None, SeparateField, InlineTags(&[ThinkingTag])}`）。本轮不做，但实现时**不要再往 `ProviderCompat` 加布尔**，以免加重「布尔堆叠」。
2. 若未来出现第二个模型适配器，再评估把「状态机」上提为 `ys-model` 的纯组件（把 `const TAGS` 留在各自适配器）—— 这正是被否的候选 B 的**局部**形态，接口已预留。

## 旁注（非本设计引入，仅记录）

`adapters/model-openai-compatible/src/lib.rs` 现有两处 `eprintln!` 警告：其中「tool_call arguments 非法 JSON」发生在**回合进行中**，而 TUI 处于 alt-screen —— 该警告会直接冲花整屏。这是既有问题，与本设计无关，但正因如此，**本设计不应再新增 `eprintln`**（可观测性若要做，走日志文件）。建议另立 issue 处理。

## 总体评价

**整体健康度：🟢**。方案小而聚焦、零新依赖、依赖方向正确、性能决策有据、测试（含变异测试）计划完整；算法经逐条走查未发现正确性缺陷。主要风险集中在**误剥的静默性**（R1）与**文档同步**（R2）两点。

**推荐下一步**：采纳「易修复」4 项（都是十几分钟的事）后即可按 `design.md` §7 实施；R1 的边界约束作为**复访触发条件**留待实测，不必现在加。实施时先落「跨片 + 未闭合 + 多块 + 空 delta」四条单测（红），再接适配器。
