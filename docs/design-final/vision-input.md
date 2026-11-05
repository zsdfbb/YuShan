# 图像输入（Vision Input）— as-built

`ContentBlock::Image` 只携带**路径与档位意图**，字节在适配器出站时现读并 base64 内联；
`ToolResult.media` 让工具把图片「附着」到工具结果上；出站前经**两层门控**与**七条降级占位**，
任何一级不满足都退化成一条文本占位、回合不失败。

## 1. 数据流（端到端）

| # | 环节 | 落点 | 关键事实 |
|---|---|---|---|
| 1 | 工具产出引用 | `adapters/tools-basic/src/view_image.rs:217` | 唯一的 `ToolResult` **结构体字面量**（带 `media`）；其余工具一律 `ToolResult::text(..)` |
| 2 | 只读文件头嗅探 | `adapters/tools-basic/src/view_image.rs:48` | `MAX_BYTES = 32 MiB`，判据是 `wire_len()` 后的字节数（`:188`）；工具**不读像素** |
| 3 | 失败即无附件 | `adapters/tools-basic/src/view_image.rs:234` | `failure()` 恒返回 `ToolResult::text(reason, true)` ⇒ `media` 空 |
| 4 | loop 折叠 | `crates/ys-loop/src/basic.rs:1402` | 先解构 `CoreToolResult { content, is_error, media }`，再 push |
| 5 | 同条消息追加 | `crates/ys-loop/src/basic.rs:1407` / `:1413`–`1415` | `ToolResult` 块 + `media.len()` 个 `ContentBlock::Image`，块数 = `1 + media.len()` |
| 6 | 请求构造 | `adapters/model-openai-compatible/src/lib.rs:1394` `translate_request` | 逐消息累积 `image_parts`（`:1432`），与 `text_content` 合并 |
| 7 | 第一层门控 | `adapters/model-openai-compatible/src/lib.rs:1497` | `config.compat.supports_image(&config.model)`；只剩这一处判能力 |
| 8 | 读盘 / 嗅探 / 编码 | `adapters/model-openai-compatible/src/media.rs:296` `resolve_image` | 全仓**唯一**读图字节处；锁外编码、锁内只插入（`:380` 起 reload stat、`:393` 插入） |
| 9 | 单图 / 图数 / 累计 | `adapters/model-openai-compatible/src/lib.rs:1522` / `:1531` | 顺序固定：门控 → 单图 → 图数 → 累计 → push（`:1539`） |
| 10 | 消息出场 | `adapters/model-openai-compatible/src/lib.rs:1558` | 条件含 `|| !image_parts.is_empty()`，否则仅含图的消息整条消失 |
| 11 | wire 形态 | `adapters/model-openai-compatible/src/request.rs:86` | `ContentPart::ImageUrl { image_url }`，`#[serde(tag = "type")]` |
| 12 | 响应流 | 无需改：图片只影响**出站**请求体 | `AgentEvent::ToolResult` 原样携带 `ToolResult`（`crates/ys-event/src/event.rs:107`） |
| 13 | TUI 渲染 | `crates/ys-tui-coding/src/app.rs:933` `image_marker` | `[image: /tmp/a.png (high)]`；路径按**字符**截到 120（`app.rs:923`） |
| 14 | 回放同源 | `crates/ys-tui-coding/src/transcript.rs:162` | 回放与 live 共用 `image_marker`，同一段历史画出同一行 |
| 15 | 子 agent 回传 | `apps/coding-agent/src/subagent.rs:1778` / `:1789` | 不重传字节，只回传同形态的 `[image: /path (high)]` 引用 |

**用户输入侧不解析路径**：文本里的 `/tmp/a.png` 原样留在 `Text` 块里，由模型自己决定调用
`view_image`。全仓没有任何代码从文本中提取图片路径。

## 2. 数据形态

| 层 | 类型 | 落盘 / wire 形态 | 指针 |
|---|---|---|---|
| 会话落盘 | `ContentBlock::Image { path, detail: Option<ImageDetail> }` | `{"Image":{"path":"…","detail":"high"}}`；`detail: None` ⇒ 键**不出现** | `crates/ys-core/src/message.rs:44` |
| 工具附件 | `MediaAttachment::Image { path, detail: Option<ImageDetail> }` | 与上者逐字同形（外部标签） | `crates/ys-core/src/media.rs:71` |
| `media` 字段 | `{ content: String, is_error: bool, media: Vec<MediaAttachment> }` | `media` 空时**逐字节等于**加字段前（`skip_serializing_if`） | `crates/ys-core/src/tool.rs:24` |
| wire | `ContentPart::ImageUrl { image_url: { url, detail? } }` | `{"type":"image_url","image_url":{"url":"data:image/png;base64,…"}}` | `adapters/model-openai-compatible/src/request.rs:86`–`101` |

`ImageDetail` 是**意图值**（`Low` / `High` / `Original` / `Auto`），wire 拼写由
`ProviderCompat::image_detail_for`（`adapters/model-openai-compatible/src/compat.rs:281`）
绝对给出。`view_image` 的 schema 只暴露 `["high","original"]`（`view_image.rs:63`），
`low` / `auto` 属**合法数据但不可由本工具产生**。

## 3. 两层门控与七条降级占位

### 3.1 门控

| 层 | 判据 | 实现 | 结果 |
|---|---|---|---|
| 第一层 provider 能力位 | `compat.supports_image_input` | `adapters/model-openai-compatible/src/compat.rs:50` | 默认 `false`；`deepseek()`（`:85`）与 `opencode_go()`（`:133`）置 `true` |
| 第二层 模型 id 可识别性 | `is_vision_model(model)` | `adapters/model-openai-compatible/src/compat.rs:378` | id 含 `vision`/`vl` token ⇒ true；`deepseek-` 前缀且含 `flash` token ⇒ true；其余 fail-closed（音频/TTS 模型例外，永远 false） |
| 合并判据 | `supports_image(model)` | `adapters/model-openai-compatible/src/compat.rs:270` | `supports_image_input && is_vision_model(model)`；**全仓唯一**模型级判断来源 |
| `detail` 下发 | `image_detail_for(model, requested)` | `adapters/model-openai-compatible/src/compat.rs:282` | 先过 `supports_image`，再 `requested?` ⇒ 缺省不发该字段 |

打开 `opencode_go()` 的第一层后，网关目录 29 个模型里真正会发图的**只有 `deepseek-*flash`**
（第二层拦下其余 28 个，含实测能读图的 `glm-5.3-flash`）—— 见 §6.1。

### 3.2 七条占位（按判定顺序）

| # | 触发条件 | 文案 | 指针 | 带路径 | 每批去重 |
|---|---|---|---|---|---|
| 1 | 模型不支持图像 | `image content omitted because you do not support image input` | `lib.rs:1322` | ❌ | ✅ 每消息一次（`lib.rs:1436`） |
| 2 | 读盘失败 / 是目录 | `yushan could not read the local image at \`{path}\`: {reason}` | `lib.rs:1331` | ✅ | ❌ |
| 3 | 不是可用图像 | `Image located at \`{path}\` is invalid: {reason}` | `lib.rs:1334` | ✅ | ❌ |
| 4 | 格式不可送（BMP/TIFF） | `yushan cannot attach image at \`{path}\`: unsupported image \`{mime}\`.` | `lib.rs:1337` | ✅ | ❌ |
| 5 | 单图编码后超限 | `yushan cannot attach image at \`{path}\`: too large ({bytes} > {limit}).` | `lib.rs:1342` | ✅ | ❌ |
| 6 | 请求累计预算超限 | `[image too large: {path} (would exceed the {limit} request budget)]` | `lib.rs:1351` | ✅ | ❌ |
| 7 | 单请求图数超限 | `[image too large: {path} (over the {limit} images per request limit)]` | `lib.rs:1359` | ✅ | ❌ |

限额常量（`adapters/model-openai-compatible/src/media.rs`）：

| 常量 | 值 | 语义 | 行 |
|---|---|---|---|
| `HEAD_BYTES` | 256 KiB | 嗅探读前缀，与 `ocr::DIM_HEAD_BYTES` 同口径 | `:26` |
| `DEFAULT_SINGLE_MAX_BYTES` | 32 MiB | **单图编码后**（含 `data:` 前缀） | `:29` |
| `DEFAULT_TOTAL_MAX_BYTES` | 128 MiB | 缓存总量（LRU） | `:32` |
| `DEFAULT_MAX_ENTRIES` | 64 | 缓存条目数（LRU） | `:35` |
| `DEFAULT_REQUEST_BUDGET_BYTES` | 48 MiB | **单请求**累计预算 | `:41` |
| `DEFAULT_MAX_IMAGES_PER_REQUEST` | 600 | 单请求图数 | `:44` |

## 4. 关键实现点与设计计划的对照

> 左列来自 `docs/arch/vision-input/design.md` 与 `docs/design-plans/2026-11-05-vision-input.md`
> 的**当前文本**，右列是代码现状。

| # | 计划怎么说 | 实现实际怎么做 | 指针 |
|---|---|---|---|
| C1 | `SessionMeta.v → 2`（design.md §11 步 11）；plan C1 裁决「不做」 | **保持 1**，`default_version()` 与 `new()` 都写 1；全仓唯一读点是 `wiring.rs:593` 的测试断言 ⇒ `v` 只写不读 | `crates/ys-session/src/meta.rs:22`、`:35`、`:71` |
| C2 | `ocr` 文案改为「陈述作用域」，不再声明独家；`view_image` 的 description **写明何时改用 `ocr`**（design.md §3.3） | 已落地：`ocr` 的 description 只说「取图内文字 + 像素位置」；`view_image` 为三层 —— 场景（visual inspection）→ 分流（“use `ocr` instead when you need the text … together with its pixel coordinates”）→ 消歧（`URLs are not accepted`）。分流句在 `spec()` 与 `prompt.rs` 的 `tool_snippet` **逐字同一份**，两处各有钉死用例 | `adapters/tools-basic/src/ocr/mod.rs:138`、`adapters/tools-basic/src/view_image.rs:92`–`100`、`apps/coding-agent/src/prompt.rs:283`–`285` |
| C6 | 5 处取文本统一到 `as_text()`，分隔符由 `TextJoin` 枚举三档 | `message_text` / `user_text` / `summarize` / `compact` 已收口；`User` 检查与空白过滤**留在原处** | `message.rs:60`（穷尽 match）、`:82`（`TextJoin`）、`:108`（`joined_text`）；`sessions.rs:185`–`200` |
| C7 | `turn_budget` 计入图片 | **有意不计入**：`_ => 0` + 四处注释 + 三条反向用例 | `crates/ys-loop/src/turn_budget.rs:38`–`42` |
| C8 | 体积按 **base64 后**判 | 已实现，两道闸门（编码前按 `metadata().len()` 预估、编码后按 `url.len()` 复核） | `adapters/model-openai-compatible/src/media.rs:345`、`:358`、`:375` |
| C9 | 循环外序列化一次 | **改为惰性缓存** `Option<Arc<Vec<u8>>>`，只在剥掉 `stream_options` 时失效 | `adapters/model-openai-compatible/src/lib.rs:386` `cached_body`、`:521` 分配、`:739` 失效 |
| E1 | `ToolResult` 构造点 77/91（design.md）→ 43/28（plan） | 当前 **47 生产 + 35 测试/集成 = 82 处**（44 处 `::text(..)` 生产 + 1 处字面量 + 3 处集成 `::text`；测试 30 处 `::text` + 2 处字面量） | 唯一的带 `media` 字面量：`view_image.rs:217`；`::text` 主体在 `crates/ys-loop/src/basic.rs`（11 处生产） |
| E3 | `sessions.rs` 的 `user_text` 改用 `TextJoin::Space` | 已改，但 `Role::User` 检查与「非空 `Text`」过滤仍在原处（两类约束不外包） | `apps/coding-agent/src/sessions.rs:185`–`200` |
| E4 | `draw.rs` 有 6 处 `ContentBlock` 通配臂 | 实测 **0 处**：该文件不出现 `ContentBlock` 符号。6 处 `_ =>` 分属 `TranscriptLine` 缩进/可见性（`:1170`、`:1187`）、`StopReason` 渲染/序号/文案（`:1441`、`:1457`、`:1473`）与测试里的 `TranscriptLine` 过滤（`:4677`）—— 均与图片无关 | `crates/ys-tui-coding/src/draw.rs:1170`、`:1187`、`:1441`、`:1457`、`:1473`、`:4677` |

### 4.1 未被计划列出的三条实现选择

| 事实 | 为什么是这样 | 指针 |
|---|---|---|
| `unsupported_notice_sent` 按**消息**去重，不是按请求 | 请求级去重会让后续「只含图」的消息整条不出场，破坏消息数守恒 | `lib.rs:1436`（注释在 `:1434`–`1435`） |
| 图片**先于**累计预算记账才 `image_count += 1` | 降级的图不占预算、不占计数；否则一串降级图会把后面合法的图拖下水 | `lib.rs:1537`–`1538` |
| 编码前用 `metadata().len()` 预估一次 | 省掉「注定被拒的图仍先分配 32+ MiB `String`」；预估不准时由编码后那道闸门兜底 | `media.rs:345`（预估）、`:375`（复核） |

## 5. 测试矩阵

| 不变量 | 守护用例 | crate / 文件 |
|---|---|---|
| **K2 上半**：`Image` 臂不被 `_ => {}` 吞掉 | `image_only_message_is_not_dropped` | `adapters/model-openai-compatible/tests/translate_image.rs:162` |
| **K2 下半**：推送条件含 `image_parts` | 同一用例（`image_only_message_is_not_dropped`）。`:154`–`:160` 的注释就是判据：「只改 `_ => {}` → 显式 `Image` 臂而**不改**推送条件 ⇒ 整条消息不出场」 | 同上 |
| 消息数守恒（混合文本 / 图 / 工具，出站 ≥ 入站） | `message_count_is_conserved` | 同上 `:186` |
| 纯文本请求线上**字节不变** | `text_only_request_keeps_string_content` | 同上 `:214` |
| 模型不支持 ⇒ 占位且零图片 | `unsupported_model_gets_placeholder_and_no_image` | 同上 `:226` |
| 「不支持」占位**每批只一次** | `unsupported_placeholder_appears_once_per_batch` | 同上 `:252` |
| 第一层能力位关闭**压住**第二层 | `provider_bit_off_disables_images_even_for_vision_model` | 同上 `:275` |
| 四条 `MediaError` 占位逐字 | `placeholder_read_failed_is_verbatim` `:298`、`placeholder_invalid_image_is_verbatim` `:324`、`placeholder_unsupported_format_is_verbatim` `:350`、`placeholder_single_too_large_is_verbatim` `:381` | 同上 |
| 累计预算占位 + 第二张图降级 | `placeholder_request_budget_is_verbatim_and_second_image_degrades` | 同上 `:421` |
| 图数上限占位 | `placeholder_image_count_is_verbatim` | 同上 `:458` |
| **占位只进出站副本，不写回入参** | `placeholder_is_not_written_back_to_the_request` | 同上 `:496` |
| `detail` 仅在请求且被支持时下发 | `detail_is_sent_only_when_requested_and_supported` | 同上 `:538` |
| **`is_error: true ⇒ media 恒空** | `failures_report_facts_with_empty_media` | `adapters/tools-basic/src/view_image.rs:519`（`:541` 断言 `media.is_empty()`） |
| 同上（第二条路径：编码后超限） | `rejects_when_encoded_size_exceeds_limit_though_raw_does_not` | `view_image.rs:567`（`:591` 断言） |
| 工具 `content` 有界、`media` 持引用 | `content_is_bounded_metadata_and_media_holds_the_reference` | `view_image.rs:333` |
| `detail` schema 恰为 `["high","original"]` | `spec_detail_enum_is_exactly_high_and_original` | `view_image.rs:399` |
| 折叠：图片与 `ToolResult` **同一条消息** | `collapse_appends_images_to_the_same_tool_result_message` | `crates/ys-loop/src/basic.rs:6833` |
| `token.rs` 计入图片（K3） | `test_estimate_tokens_counts_image_block` | `crates/ys-loop/src/token.rs:108` |
| `turn_budget` **不**计入图片（C7 反向） | `tool_result_tokens_ignores_image_blocks` `:360`、`tool_result_tokens_zero_for_empty_text_with_images` `:378`、`plan_elisions_empty_when_overage_is_caused_only_by_images` `:395` | `crates/ys-loop/src/turn_budget.rs` |
| **C8：编码后判体积**（原图 24 MiB 过不了 32 MiB 编码后） | `limit_is_judged_on_encoded_bytes_not_raw_bytes` `:651`、`d18_real_24_mib_image_exceeds_the_default_limit` `:747` | `adapters/model-openai-compatible/src/media.rs` |
| 工具侧同口径（编码后超限） | `rejects_when_encoded_size_exceeds_limit_though_raw_does_not` | `adapters/tools-basic/src/view_image.rs:567` |
| 编码后最终复核（`url.len() > limit`）有运行期覆盖 | `truncated_read_is_caught_by_the_encoded_size_gate`（FIFO 构造：`metadata().len()==0` ⇒ 编码前预估必放行，只剩编码后闸门能拒） | `media.rs:691` |
| 缓存键含 `(path, mtime, len)` | `mtime_is_part_of_the_cache_key` `:561`、`len_is_part_of_the_cache_key` `:606` | `media.rs` |
| LRU 上界（条目数 + 总字节） | `cache_evicts_by_entry_count_and_total_bytes` `:863`、`an_entry_larger_than_the_total_budget_is_not_cached` `:896` | `media.rs` |
| 锁内无文件系统调用 | `no_filesystem_call_inside_the_lock_holders` | `media.rs:992` |
| 六格式嗅探 + 拒绝路径 | `sniffs_all_six_formats` `:409`、`rejects_empty_and_non_image_heads` `:466`、`rejects_truncated_heads` `:474`、`rejects_zero_dimensions` `:494`、`every_prefix_of_a_valid_head_is_rejected_without_panicking` `:586` | `crates/ys-core/src/media.rs` |
| `as_text` 语义：`Image` 无正文 | `as_text_returns_none_for_image_block` `:197`、`as_text_only_matches_text_variant` `:210` | `crates/ys-core/src/message.rs` |
| `TextJoin` 三档分隔符 | `joined_text_uses_caller_separator_and_skips_non_text`（`:234`） | `crates/ys-core/src/message.rs` |
| `media` 空时序列化**逐字节不变** | `tool_result_without_media_serializes_byte_identically` | `crates/ys-core/src/tool.rs:45` |
| `ToolResult` 事件无 `media` 键 | `tool_result_event_without_media_has_no_media_key` | `crates/ys-event/src/event.rs:305` |
| 块集合白名单（TUI 两条路径） | `u4_message_text_includes_image_marker`（`app.rs:2825`）、`u4_replay_lines_must_render_image_block`（`transcript.rs:588`）、`u9_replay_renders_image_on_both_paths_with_and_without_detail`（`transcript.rs:636`）、`p2_3_only_text_and_tool_use_produce_lines`（`transcript.rs:545`） | `crates/ys-tui-coding` |
| 回放与 live 逐行相等 | `u9_replay_user_image_matches_local_echo_text`（`transcript.rs:665`）；另有一条不含图的一般性回放相等断言（`transcript.rs:533`–`537`） | `crates/ys-tui-coding/src/transcript.rs` |
| 子 agent 回传只带引用 | `text_of_renders_image_as_reference` `:3048`、`transcript_body_renders_image_refs` `:3006` | `apps/coding-agent/src/subagent.rs` |
| 两层门控各自成立 + 合并判据 | `image_input_bit_is_conservative_by_default` `:863`（含 `opencode_go()` 为 `true`）、`image_gate_needs_both_layers` `:901`、`unknown_model_ids_fail_closed` `:917`、`image_detail_folds_through_the_gate` `:946`、`flash_alone_is_not_a_vision_marker` `:935` | `adapters/model-openai-compatible/src/compat.rs` |
| `opencode_go` 图像位的**实际射程**（只放行 `deepseek-*flash`；`glm-5.3-flash` 不得放行） | `opencode_go_image_bit_only_reaches_deepseek_flash` `:887` | 同上 |
| 两个工具表指纹锚（工具表未被静默改动） | `prefix_fingerprint_is_anchored_and_stable` | `apps/coding-agent/src/toolset.rs:635` |
| `v` 恒为 1 | `wiring.rs:593` 的 `assert_eq!(meta.v, 1)` | `apps/coding-agent/src/wiring.rs:593` |

**当前状态**：`./scripts/par-test.sh` 全绿（40 个测试二进制，并行度 8）；
`python3 scripts/vision-mutation.py` **18/18 全部被抓住**（§7.1）。

### 5.1 `as_text` 的编译期守护（不是运行期用例）

删掉 `crates/ys-core/src/message.rs:65` 的 `ContentBlock::Image { .. } => None` 臂会得到
`error[E0004]: non-exhaustive patterns`。原因是 `#[non_exhaustive]` 只在**跨 crate** 禁止穷尽
匹配，在定义 crate 内部不生效 ⇒ `ys-core` 内的穷尽 match 有真实守护力。

这条**无法用 `cargo test` 表达**（红的是编译，不是断言）。`as_text_returns_none_for_image_block`
（`:197`）只锁语义，其 doc 注释明确写了「删臂 ⇒ 本用例不变红，但编译失败」。
`scripts/vision-mutation.py` **有意不收录**这条变异（理由见 §7.1）：注入后运行期行为不变，只会有
一条永远绿的假 mutant。⇒ 目前**没有任何自动化在跑这条编译检查**，靠代码审查与 D17 纪律保证。

## 6. 已知边界

### 6.1 `opencode-go` 网关：已实测支持，但只对 `deepseek-*flash` 生效

`ProviderCompat::opencode_go()`（`adapters/model-openai-compatible/src/compat.rs:133`）
显式置 `supports_image_input: true`。依据是一次真实凭证探测（2026-11-05，
`https://opencode.ai/zen/go/v1/chat/completions`，块数组 `image_url` + base64 data URI）：

| 探测 | 结果 |
|---|---|
| 128×128 已知答案图（黑色字母 K），`deepseek-v4.1-flash` 连发 3 次 | **3/3 答对 `K`**，`finish_reason: stop` |
| 同上 + `image_url.detail: "high"` | **200 + 答对** ⇒ 该字段也被接受 |
| 同请求体发给 `glm-5.2` / `glm-5.3` | **`400 Invalid image or video input`** |
| `gpt-5.6-luna` / `gpt-6-luna` / `beam-free` / `muse-spark-*` | `403` 地区限制（与该功能无关） |

**打开后的实际射程只有 1 个模型**：网关目录 29 个模型中，第二层 `is_vision_model` 仅放行
`deepseek-v4.1-flash`（`deepseek-` 前缀 + `flash` 分段）。⇒ 能力位虽为 `true`，被拦下的 28 个
**仍走 §3.2 第 1 条占位**，包括实测能读图但 id 无 `vision` / `vl` 分段的 `glm-5.3-flash`。
这是**有意**的 fail-closed，不是漏配：为它放宽 `is_vision_model` 会把同样含 `flash` 分段、
但真实返 400 的 `glm-5.2` / `glm-5.3` 一起放进去。

> 本次只实测 `deepseek-v4.1-flash`。同网关的 `minimax-*` / `kimi-*` / `qwen3.8-*` 等模型
> 同样 200 答对（旁证：多模态在该网关是普遍能力而非 flash 独有），但本仓**不**据此放开它们 ——
> 能力位与 `is_vision_model` 均保持现状，需时另行实测再改。

### 6.2 `ocr` 的 BMP / TIFF 尺寸偏移 bug（**不在本次范围**）

`adapters/tools-basic/src/ocr/mod.rs` 的 `dims_from_header` 在 `strip_prefix` 之后仍用**相对
文件起点**的偏移量：

| 格式 | 现状 | 正确偏移 | 指针 |
|---|---|---|---|
| BMP | `rest.get(18..22)` / `rest.get(22..26)` ⇒ 整体**偏 +2 字节** | `head.get(18..22)` / `head.get(22..26)` | `ocr/mod.rs:552`–`555` |
| TIFF | `strip_prefix("II\x2a\x00")` 后把 `read32(rest)` 当成 IFD 偏移 ⇒ 整体**偏 −4 字节**（方向相反） | 用 `head` 而非 `rest` 计算 IFD 偏移 | `ocr/mod.rs:561`–`563`、`:603`、`:621`–`622` |

实测（按同一算法独立复算）：宽 7 高 9 的 BMP 得 `(589824, 0)`（应为 `(7, 9)`）；宽 100 高 50
的小端 TIFF 得 `None`（应为 `(100, 50)`）。两者**无任何测试覆盖**（`ocr/mod.rs` 里没有
`dims_from_header` 的用例）。

影响面受限：`image_dims` 的结果只用于 `ocr` 输出头行的 `WxH` **展示**（`ocr/mod.rs:259`），
**不参与任何决策**。`ys-core::media` 的同一组嗅探函数偏移正确且有测试（`crates/ys-core/src/media.rs:138`、`:179`）。

### 6.3 指纹漂移 ⇒ 所有用户提示缓存冷启动一次

当前锚点：`0xf017256b30c9d6f7`（`fp_tools`）、`0x318be4770f32c8e6`（`fp_parent`）、
`0x2fd268573e284877`（`main_system_prompt_body_is_byte_stable` 的提示主体）。
历次漂移：① 工具表新增 `view_image`（同时 `ocr` 去掉「唯一能看图」那句）；
② `view_image` 的 description 补上到 `ocr` 的分流指引与 `URLs are not accepted`。

| 锚点 | 覆盖范围 | 断言行 |
|---|---|---|
| `fp_tools` | **只**基础 11 元（子 agent 的工具表） | `apps/coding-agent/src/toolset.rs:726` |
| `fp_parent` | 基础 11 元 + 追加段（`bash_bg` / `subagent_async` / `subagent_send`） | `apps/coding-agent/src/toolset.rs:736` |
| 提示主体 | `<project_context>` 之前的整段（含工具清单与全部 guideline） | `apps/coding-agent/src/prompt.rs:2112` |

面向上游的后果：工具清单进了系统提示的常驻段，指纹一变即代表**提示前缀改变** ⇒ 既有用户的
provider 侧提示缓存全部失效，冷启动一次（只影响首回合的 `cache_read`，不影响正确性）。

### 6.4 编码后最终复核的覆盖情况

`media.rs:375` 的 `if url.len() > limit` 是「`metadata` 与读盘之间文件长大」的兜底。
**有运行期覆盖**：`truncated_read_is_caught_by_the_encoded_size_gate`（`media.rs:689`）用命名
管道构造该场景，并显式论证了 `raw.len() == limit`（第一道闸门不触发）而编码后超限。

局限：该用例 `#[cfg(unix)]` + 依赖 `mkfifo`，取不到时**打印 skip 并直接 return**（`media.rs:697`–`703`）
—— 非 Unix 或裁剪环境里这道复核实际无覆盖。

## 7. 变异测试

### 7.1 `scripts/vision-mutation.py`：18 个 mutant，**18/18 全部被抓住**

脚本已落盘（`scripts/vision-mutation.py`，603 行），`--list` 给 **18 条**。实现要点：
逐 mutant 前**快照**、跑完**还原**（`finally` 兜底，不用 `git` 命令）；`old` 文本必须**恰好命中
一次**（`apply_edits`，`:426`–`:442`）—— 命中 0 次即 `InjectionError`，**不算**“被抓住”。

| # | 目标 | 包 | 期望红 | 实测 |
|---|---|---|---|---|
| 1 | K2 上半：`Image` 臂改回 `_ => {}` | `ys-model-openai-compat` | `message_count_is_conserved` | ✅ |
| 2 | K2 下半：推送条件去掉 `\|\| !image_parts.is_empty()` | 同上 | `image_only_message_is_not_dropped` | ✅ |
| 3 | K3 上半：`token.rs` 的 `Image` 分支删掉 | `ys-loop` | `test_estimate_tokens_counts_image_block` | ✅ |
| 4 | K3 下半（反向）：`turn_budget` 的 `_ => 0` 改成计入 | `ys-loop` | `plan_elisions_empty_when_overage_is_caused_only_by_images` | ✅ |
| 5 | A1：折叠时不追加 `Image` 块 | `ys-loop` | `collapse_appends_images_to_the_same_tool_result_message` | ✅ |
| 6 | A2：图片另起一条消息 | `ys-loop` | 同上 | ✅ |
| 7 | A3：`app.rs` `message_text` 的 `Image` 臂改回丢弃 | `ys-tui-coding` | `u9_message_text_renders_image_marker_with_and_without_detail` | ✅ |
| 8 | A4：`transcript.rs` 回放 assistant 侧 `Image` 臂改回 `None` | `ys-tui-coding` | `u4_replay_lines_must_render_image_block` | ✅ |
| 9 | A5：`expand_skill_invocation` 只建一个 `Text` 块 | `ys-coding-agent` | `skill_expansion_keeps_image_block` | ✅ |
| 10 | M1：单图判据改回按原图判（两处闸门） | `ys-model-openai-compat` | `limit_is_judged_on_encoded_bytes_not_raw_bytes` | ✅ |
| 11 | M2：缓存键去掉 `mtime` | 同上 | `mtime_is_part_of_the_cache_key` | ✅ |
| 12 | M3：缓存键去掉 `len` | 同上 | `len_is_part_of_the_cache_key` | ✅ |
| 13 | M4：`Content` 去掉 `#[serde(untagged)]` | 同上 | `pure_text_content_serializes_as_a_json_string` | ✅ |
| 14 | M5：`ImageUrl.detail` 去掉 `skip_serializing_if` | 同上 | `image_part_omits_absent_detail` | ✅ |
| 15 | G1：门控 `supports_image` 恒 `true`（fail-open） | 同上 | `unsupported_model_gets_placeholder_and_no_image` | ✅ |
| 16 | G2：「不支持」占位改成带路径（每图一条） | 同上 | `unsupported_placeholder_appears_once_per_batch` | ✅ |
| 17 | C9：删掉剥 `stream_options` 后的 `*body_cache = None;` | 同上 | `stream_options_fallback_resends_a_body_without_the_field` | ✅ |
| 18 | E1：删掉编码后的体积闸门（`media.rs:375`） | 同上 | `truncated_read_is_caught_by_the_encoded_size_gate` | ✅ |

**实测结果：18/18 被抓住**。注入失败的判定是 `InjectionError`（`old` 文本找不到或命中多次），
**不被当成"通过"也不算"覆盖缺口"**：脚本会以非 0 退出并点名哪条锚点失效（`scripts/vision-mutation.py:426`–`442`）。
这层判定是承重的 —— 锚点随源码演进而失效是常态，静默算绿就是测试套件在做假承诺。

**为什么没有 `as_text()` 的 mutant**：把 `ContentBlock::Image` 臂并进 `_ => None` 后**运行期
行为不变**（两者都返回 `None`），`cargo test -p ys-core` 全绿 —— 实测确认无任何用例能抓。
它是**类型层契约**，不是运行期不变量；写进来只会是永远绿的假 mutant。同理
`Mutant.compile_is_catch` 字段保留但**当前无条目使用**（`scripts/vision-mutation.py:23`–`26`）。

### 7.2 已接入 `scripts/verify.sh`

`scripts/verify.sh:60` 的变异循环已列入 `vision-mutation`（与
`cancellation-mutation wheel-mutation selection-mutation theme-mutation` 同批）⇒
`./scripts/verify.sh` 会跑到这 18 条。`scripts/verify.sh:32` 的注释说明了它**不需要**别的
变异脚本共用的前置步骤。

### 7.3 手工运行方式

```
python3 scripts/vision-mutation.py        # 全部 18 条
python3 scripts/vision-mutation.py --list # 只列清单
python3 scripts/vision-mutation.py 8      # 只跑第 8 条
```

## 8. 未做 / 不做

| 项 | 状态 | 依据 |
|---|---|---|
| 字节存进会话（「完全可复现」） | **不做** | 会话永远只有路径；原图被改写 ⇒ 下次出站送新内容 |
| 用户输入侧路径自动解析 | **不做** | 歧义（文本里哪段是图片）与 `expand_skill_invocation` 重建 Message 的行为冲突 |
| `SessionMeta.v → 2` | **不做** | `v` 只写不读，升版本只换来一次 `O(会话总字节)` 的全量重写（`crates/ys-session/src/meta.rs:22`） |
| 图片压缩 / 缩放 | **不做** | 无缩放能力 ⇒ 超限只能拒绝并说明（占位 5/6/7，`lib.rs:1340`） |
| 为 `glm-*flash` 放宽 `is_vision_model` | **不做** | `glm-5.3-flash` 实测能读图，但同名字段的 `glm-5.2` / `glm-5.3` 返 400；id 无法区分 ⇒ 整体 fail-closed（§6.1） |
| 把 `opencode_go()` 的图像位扩散到别的方言 | **不做** | 只在 `deepseek-v4.1-flash` 上实测过（§6.1） |
