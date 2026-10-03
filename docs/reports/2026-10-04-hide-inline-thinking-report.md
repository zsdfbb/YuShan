# hide-inline-thinking 修改报告

## 变更摘要
在 OpenAI 兼容适配器层把 assistant `delta.content` 内联思考标签（`<think>`/`<thinking>`/`<reasoning>`，内置三对、默认开启）剥离为既有 `ModelEvent::ThinkingDelta`；默认隐藏、`/thinking on` 可回看；退出回放遵守 thinking 开关。共 3 个代码文件（1 新建 / 2 修改）+ 2 个文档同步。

## 波次执行摘要
| 单元 | 任务 | 波次 | review 档 | 轮次 | 结果 |
| --- | --- | --- | --- | --- | --- |
| U-A | 适配器 `inline_thinking.rs` + `lib.rs` 接线 | W0 | deep | 2（第 1 轮 P1：finish→flush 接线无测试覆盖，补测后通过） | ✅ 通过 |
| U-B | `draw.rs` D5 退出回放 | W0 | shallow | 1 | ✅ 通过 |

- 基线：git 提交不可用（沙箱 `.git` 只读、且不擅自 commit）→ 使用 /tmp 拷贝基线 `/tmp/sw-baseline-2026-10-04-hide-inline-thinking/`（HEAD `4564142`），review 以 `diff -u` 取字节级证据
- 基线全量 diff：仅 2 个已跟踪文件被改（`lib.rs`、`draw.rs`）+ 1 个新文件（`inline_thinking.rs`），与设计涉及文件清单一致，无计划外改动
- subagent 中断：0 次

## 文件变更清单
| 操作 | 文件 | 说明 |
| --- | --- | --- |
| A | adapters/model-openai-compatible/src/inline_thinking.rs | 过滤器状态机（`InlineThinkingFilter`/`Segment`/`scan`/`push_seg`/`match_delimiter`/`is_proper_prefix`）+ 14 条单测 |
| M | adapters/model-openai-compatible/src/lib.rs | `mod inline_thinking`；`StreamAccumulator.filter` 字段；`handle` content 分支走过滤器；`emit_segment`；`finish(mut self)` flush；+5 条适配器测试 |
| M | crates/ys-tui-coding/src/draw.rs | `transcript_plain_lines` 传 `app.thinking_visible`；注释更新；+1 条 D5 测试 |
| M | docs/design-final/coding-agent-tui.md | 退出回放 thinking 措辞 + 内联标签说明 |
| M | docs/design.md | §12 Coding Agent 行措辞 |

## 测试结果
| 类型 | 命令 | 结果 |
| --- | --- | --- |
| 过滤器单测 | cargo test -p ys-model-openai-compat | ✅ 46 passed（含真值表 11 行、跨片穷举、5000 组随机切分、hold 上界 11/10、UTF-8） |
| TUI 单测 | cargo test -p ys-tui-coding --lib | ✅ 203 passed |
| 全量 | cargo test --workspace | ✅ 全绿；仅 `stream_usage_fallback` 3 条在沙箱内因回环 `TcpListener::bind` 失败，沙箱外重跑 3 passed（非回归） |
| Lint | cargo clippy --all-targets | ✅ 无 warning/error |
| Fmt（本次文件） | cargo fmt --all -- --check | ✅ lib.rs / inline_thinking.rs / draw.rs 均 clean（仓库其余文件存在既有 fmt 漂移，非本次引入，未动） |

## 关键决策（摘自 docs/arch/hide-inline-thinking/design.md 与 ADR-0025）
- D1 只剥被标签包裹的思考；D2 剥离在适配器层（源头上干净：会话历史/-p/--json/退出回放一并）；D3 内置三对标签、默认开启、不做 per-provider 配置、不收 DeepSeek `｜…thinking…｜`；D4 内联思考→`ThinkingDelta`→`TranscriptLine::Thinking`（默认隐藏）；D5 退出回放遵守开关；D6 接受误剥风险（不加转义/白名单）。
- 选用「A 归属 + C 快路径」：`inside: Option<usize>` 配对闭合 + `hold` 跨片扣留（≤11B）+ 无 `<` 时零拷贝 move。
- 未做（design §8）：空白 Text 段不折叠；剥离无可观测计数；开标签文本边界约束暂缓为复访触发条件。

## 审查结果（逐条核对测试合同 N1–N8）
| 条款 | 优先级 | 验收 | 结果 |
| --- | --- | --- | --- |
| N1 三对标签剥离→ThinkingDelta | 必测 | 过滤器 `all_three_tags` + 适配器 `all_tag_pairs_are_stripped_from_content` | ✅ |
| N2 跨片不漏不误 | 必测 | `cross_slice_exhaustive`（每 char 边界）+ `random_text_random_splits`（5000 组） | ✅ |
| N3 未闭合不外泄/不丢字 | 必测 | `unclosed_thinking`、`trailing_partial_open_tag`、`finish_flush_releases_*` | ✅ |
| N4 finish flush、hold 归零 | 必测 | 上述两条半截标签用例 + 变异验证（删 flush 即变红） | ✅ |
| N5 只作用 content；既有分支不回归 | 必测 | `thinking_delta_only_emitted_when_compat_enabled`、`plain_content_stream_emits_only_text_deltas`、`streamed_text_deltas_...byte_for_byte` | ✅ |
| N6 适配器出口 TextDelta 无思考、Text==拼接、Thinking 拼接==原文 | 必测 | `inline_thinking_tags_split_across_content_chunks`、`inline_thinking_coexists_with_reasoning_content` | ✅ |
| N7 D5 双向可见性 | 必测 | `draw::tests::test_plain_lines_thinking_follows_visibility_toggle` | ✅ |
| N8 文档同步 | 必测 | 本文件 + design-final + design.md + draw.rs 注释 | ✅ |

验收循环次数：U-A 2 轮（deep review 第 1 轮 P1 → 补测 → 第 2 轮通过），U-B 1 轮。无 MANUAL_ACK_REQUIRED。

## 已知残余风险（接受项）
误剥普通正文会同时从 TUI 与会话历史消失（D6 取舍，复访触发条件见 design §8）；多块思考间的 `\n\n` 会渲染为空行。
