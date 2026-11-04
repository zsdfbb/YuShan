# 执行计划：图像输入（Vision Input）

> 日期：2026-11-05
> 设计计划（权威）：`docs/design-plans/2026-11-05-vision-input.md`（下文引作「plan §N」）
> 上游设计：`docs/arch/vision-input/design.md`（「design.md §N」）
> 范围：8 个 crate / 目录，**零新增 crate**，**一条显式依赖**（`base64`，已在 `Cargo.lock:44`）
> 总任务数：**42**（impl 21 / test 21），每个 `impl` 恰有一条配对 `test`（同编号 + `t`）

## 使用方式

- 本文件是**给执行器用的**：每个任务是表格中的一行，字段固定，可直接按「涉及文件」算波次。
- **波次判据**：同一波次内所有任务的「涉及文件」集合**两两不相交**。同一文件出现在两个任务里
  ⇒ 必须不同波次（或合并）。
- **每个任务 ≤ 5 个文件、≤ 10 分钟**。`T02`/`T03`/`T04` 是同一编译单元的三段，各自 ≤5 文件。
- 验收命令分两级，不要混：
  - **单元验收**（本文件表格里写的）用 `cargo test -p <crate>` —— 它接受 `-p`，输出干净、
    能直接对照单条用例名，适合实现者自检。
  - **波次门禁 / 提交前**用 `./scripts/par-test.sh`（全量、哑运行）。
  - ⚠️ **`./scripts/par-test.sh -p <crate>` 是一条会静默假绿的命令**：脚本把第一个位置参数
    当并行度，`-p` 会被当成 `xargs -P ys-core`，报「选项 -P 的数值无效」后仍然打印
    「par-test: 全部通过」并以 **0 退出**（实测验证）。所以 `par-test.sh` 只能不带 `-p` 跑。
- **有两条 test 必须先写红**（design.md §11 步 4）：`T20t`（块集合白名单，依赖 `T05` 后有变体即可写红，
  `T20` 后转绿）与 `T21t`（变异脚本自身）。其余 test 与 impl 同波或紧随其后。

## 总览

| | 数量 | 说明 |
|---|---|---|
| `impl` 任务 | 21 | 生产代码 |
| `test` 任务 | 21 | 与 impl 一一配对 |
| 需串行的编译单元 | 1 | `T02 → T03 → T04`：`ToolResult` 加字段期间工作区不编译 |
| 波次数 | 9（W0–W8） | 见 §波次划分 |

---

## 任务表（按编号）

| 编号 | 类型 | 标题 | 依赖 | 涉及文件 | 验收 |
|---|---|---|---|---|---|
| T01 | impl | `ys-core::media`：`ImageFormat` / `ImageDetail` / `MediaAttachment` / `sniff_image` / `mime()` | — | `crates/ys-core/src/media.rs`（新）<br>`crates/ys-core/src/lib.rs` | `cargo test -p ys-core` 全绿；`cargo tree -p ys-core \| grep tokio` 只命中 dev-deps |
| T01t | test | 六格式正例 + 四反例 + `x.png` 实为 JPEG + serde 小写 + `Default = High` | T01 | `crates/ys-core/src/media.rs`（`mod tests`） | 十条全绿；`Detail::default() == High`；`to_string(&Png) == "\"png\""` |
| T02 | impl | `ToolResult.media` 字段 + `ToolResult::text()`；改 `tools-basic` 侧 4 个构造点文件 | T01 | `crates/ys-core/src/tool.rs`<br>`adapters/tools-basic/src/read.rs`<br>`adapters/tools-basic/src/grep.rs`<br>`adapters/tools-basic/src/ls.rs`<br>`adapters/tools-basic/src/bash.rs` | `./scripts/cargo-build.sh check -p ys-tools-basic` 零 error |
| T02t | test | 无 media 时 `ToolResult` 的 JSON 逐字节不变 | T02 | `crates/ys-core/src/tool.rs`（`mod tests`） | `to_string(&ToolResult::text("x", false)) == r#"{"content":"x","is_error":false}"#`（无 `media` 键） |
| T03 | impl | `ToolResult` 构造点第二段：`ys-loop/basic.rs` **12 处**（别名 `CoreToolResult`）+ `tools-web` 13 处 + **`subagent.rs` 2 处生产构造点** | T02 | `crates/ys-loop/src/basic.rs`<br>`adapters/tools-web/src/search.rs`<br>`adapters/tools-web/src/fetch.rs`<br>`apps/coding-agent/src/subagent.rs`（`:2006` `spawn` / `:2072` `send`） | `./scripts/cargo-build.sh check -p ys-loop -p ys-tools-web -p ys-coding-agent --all-targets` 零 error |
| T03t | test | `tools-web` 既有 contracts 用例全绿（迁移未改语义） | T03 | `adapters/tools-web/tests/contracts.rs`（**不改**，只跑） | `cargo test -p ys-tools-web` 全绿 |
| T04 | impl | `ToolResult` 构造点第三段：`find` **4 处** / `ocr` **4 处** / `write` 1 / `edit` 1 / `bg_tools` 1，**编译单元关闭** | T03 | `adapters/tools-basic/src/find.rs`<br>`adapters/tools-basic/src/ocr/mod.rs`<br>`adapters/tools-basic/src/write.rs`<br>`adapters/tools-basic/src/edit.rs`<br>`adapters/tools-basic/src/bg_tools.rs` | `./scripts/cargo-build.sh check --workspace --all-targets` **零 error**（编译单元关闭） |
| T04t | test | `tools-basic` 全部既有用例绿（含 `ocr` 的 5 条） | T04 | `adapters/tools-basic/src/ocr/mod.rs`（`mod tests`，只跑） | `cargo test -p ys-tools-basic` 全绿 |
| T05 | impl | `ContentBlock::Image { path, detail }` 变体 | T01 | `crates/ys-core/src/message.rs` | `check --workspace --all-targets` **仍零 error**（plan §4.1 的地基：加变体不改编译结果） |
| T05t | test | `Image` 块 serde round-trip + `detail: None` 无该键 + 既有变体字节不变 | T05 | `crates/ys-core/src/message.rs`（`mod tests`） | 三断言绿；`cargo test -p ys-coding-agent debug_log` 绿（golden 守既有字节） |
| T06 | impl | 适配器 `media.rs`：读盘/嗅探/base64/`MediaCache`（128 MiB / 64 条 LRU，锁外 IO）+ `Cargo.toml` 加 `base64` | T01 | `adapters/model-openai-compatible/src/media.rs`（新）<br>`adapters/model-openai-compatible/Cargo.toml`<br>`adapters/model-openai-compatible/src/lib.rs`（仅加 `mod media;` 一行） | `cargo test -p ys-model-openai-compat` 全绿 |
| T06t | test | 缓存上界 / 缓存键含 mtime+len / 锁外并发 / **单图按编码后判** | T06 | `adapters/model-openai-compatible/src/media.rs`（`mod tests`） | 四条绿；含「24 MiB 原图（编码后 ≈32.9 MiB）⇒ 拒」 |
| T07 | impl | `request.rs`：`Content` 枚举（字符串 \| 块数组）+ `ChatMessage.content: Option<Content>` | T06 | `adapters/model-openai-compatible/src/request.rs` | `check -p ys-model-openai-compat` 零 error（`lib.rs` 三处构造点由 T09 收口，此处允许短暂报错） |
| T07t | test | **纯文本请求线上字节不变**（golden） | T07 | `adapters/model-openai-compatible/tests/translate_golden.rs`（新） | 同一 `ModelRequest` 序列化前后逐字节相同 |
| T08 | impl | `compat.rs`：`supports_image_input` + `supports_image(model)` + `image_detail_for` | T01 | `adapters/model-openai-compatible/src/compat.rs` | `cargo test -p ys-model-openai-compat` 全绿 |
| T08t | test | 门控两层（provider 位 × 模型 id）；无法识别 ⇒ fail-closed | T08 | `adapters/model-openai-compatible/src/compat.rs`（`mod tests`） | 各 provider 取值钉死；未知 id 为 `false` |
| T09 | impl | **K2 两半**：`:1311` 显式 Image 臂（含 48 MiB 累计 + 600 图计数）+ `:1316` 推送条件 | T07, T08 | `adapters/model-openai-compatible/src/lib.rs` | `cargo test -p ys-model-openai-compat` 全绿；仅含 Image 的消息不再产出 `"messages":[]` |
| T09t | test | **消息数守恒** + 六条占位逐字 + 累计预算（小上限 1 KiB） | T09 | `adapters/model-openai-compatible/tests/translate_image.rs`（新） | 守恒 + 六条逐字 + 两张 1.5 KiB 图其第二张降级 |
| T10 | impl | `view_image` 工具 + 导出 + 注册 | T06 | `adapters/tools-basic/src/view_image.rs`（新）<br>`adapters/tools-basic/src/lib.rs`<br>`apps/coding-agent/src/toolset.rs` | `cargo test -p ys-tools-basic -p ys-coding-agent` 全绿 |
| T10t | test | 不读全文件 + `content` 不含 `base64` 且体积有界 + `detail` schema 恰为 `["high","original"]` | T10 | `adapters/tools-basic/src/view_image.rs`（`mod tests`） | 40 MiB 夹具（头合法+垃圾尾）三断言绿；传 `low` ⇒ `InvalidInput` |
| T11 | impl | `basic.rs:1410` 折叠：`ToolResult` 后追加 `Image` 块（**同一条**消息） | T03, T05 | `crates/ys-loop/src/basic.rs` | `cargo test -p ys-loop` 全绿 |
| T11t | test | 折叠多一块、最后是 `Image`、消息数不变 | T11 | `crates/ys-loop/src/basic.rs`（`mod tests`） | 三断言绿 |
| T12 | impl | `token.rs:12` 计入 `IMAGE_TOKEN_ESTIMATE = 1024` | T05 | `crates/ys-loop/src/token.rs` | `cargo test -p ys-loop` 全绿 |
| T12t | test | 含 1 张图的 100 字符消息恰多 1024 | T12 | `crates/ys-loop/src/token.rs`（`mod tests`） | 差值断言恰为 1024 |
| T13 | impl | **C6 收口**：`ContentBlock::as_text()`（穷尽 match）+ `TextJoin` + `Message::joined_text()` | T05 | `crates/ys-core/src/message.rs` | `cargo test -p ys-core` 全绿；`as_text` **无 `_` 臂** |
| T13t | test | `as_text` 逐变体断言（`Text` ⇒ `Some`，其余 ⇒ `None`） | T13 | `crates/ys-core/src/message.rs`（`mod tests`） | 逐变体绿；另加 `joined_text` 三种分隔符各一条 |
| T14 | impl | 五处取文本改调 `joined_text`（`app`/`transcript`/`sessions`/`subagent`） | T13 | `crates/ys-tui-coding/src/app.rs`<br>`crates/ys-tui-coding/src/transcript.rs`<br>`apps/coding-agent/src/sessions.rs`<br>`apps/coding-agent/src/subagent.rs` | `cargo test -p ys-tui-coding -p ys-coding-agent` 全绿 |
| T14t | test | 三处既有用例**保留且绿**（`app.rs:2695` / `transcript.rs:538` / `sessions.rs:453`） | T14 | 无新增文件（只跑既有用例） | 三用例绿；`git diff` 显示它们**未被删除** |
| T15 | impl | `summarize.rs:106` + `compact.rs:374` 改调 `joined_text(None)` | T13 | `crates/ys-loop/src/summarize.rs`<br>`crates/ys-loop/src/compact.rs` | `cargo test -p ys-loop` 全绿 |
| T15t | test | 摘要/压缩的「空或全空白 = 失败」判定不变 | T15 | `crates/ys-loop/src/summarize.rs`（`mod tests`） | 既有用例绿；空白响应仍判失败 |
| T16 | impl | **C7**：`turn_budget.rs:38` **保持** `_ => 0`，补注释 + 指向 `token.rs` | T05 | `crates/ys-loop/src/turn_budget.rs` | `cargo test -p ys-loop` 全绿；`rg "Image.*1024" crates/ys-loop/src/turn_budget.rs` ⇒ 0 |
| T16t | test | **反向用例**：含 `Image` 的消息 `tool_result_tokens == 0` 且 `plan_elisions` 为空 | T16 | `crates/ys-loop/src/turn_budget.rs`（`mod tests`） | 两断言绿（把「不做」钉成要求） |
| T17 | impl | **C9** 序列化塔存：`Option<Arc<Vec<u8>>>` + `:654` 后置 `None` | T09 | `adapters/model-openai-compatible/src/lib.rs` | `cargo test -p ys-model-openai-compat` 全绿 |
| T17t | test | **回退路径两次请求体不同**（必须保持绿）+ 429 重试后请求体逐字节相同 | T17 | `adapters/model-openai-compatible/tests/retry.rs`<br>`apps/coding-agent/tests/stream_usage_fallback.rs`（**不改**，只确认） | `:361` 保持绿；新增 429 复用用例绿 |
| T18 | impl | **A 档**：`expand_skill_invocation` 保留非 `Text` 块 | T05 | `apps/coding-agent/src/app_loop.rs` | `cargo test -p ys-coding-agent` 全绿 |
| T18t | test | 展开后 `content.len()==2` 且第二块仍是同路径 `Image` | T18 | `apps/coding-agent/src/app_loop.rs`（`mod tests`） | 两断言绿；变异回 `vec![Text]` ⇒ 红 |
| T19 | impl | **C2 文案**：`ocr` spec + `prompt.rs` fragment + `view_image` snippet + 重写 `:273-278` 注释 | T10 | `adapters/tools-basic/src/ocr/mod.rs`<br>`apps/coding-agent/src/prompt.rs` | `cargo test -p ys-tools-basic -p ys-coding-agent` 全绿 |
| T19t | test | 跨 crate 一致：`tool_snippet("ocr") == OcrTool::spec().description`；`the only way` 零命中 | T19 | `apps/coding-agent/src/prompt.rs`（`:913` 逐字用例改写） | `rg -c "the only way" adapters/ apps/` ⇒ 0；`view_image` 不再是 `Custom tool` |
| T20 | impl | 块集合白名单 + 渲染：`transcript.rs`（`:157`/`:163`）+ `app.rs`（`:908`）+ `subagent.rs:1824` + `main.rs:673` | T05, T13, T14 | `crates/ys-tui-coding/src/transcript.rs`<br>`crates/ys-tui-coding/src/app.rs`<br>`apps/coding-agent/src/subagent.rs`<br>`apps/coding-agent/src/main.rs` | `cargo test -p ys-tui-coding -p ys-coding-agent` 全绿；`T20t` 的白名单用例**转绿** |
| T20t | test | **白名单用例先写红**（块种类集合）+ replay == live + `message_bytes` 计入路径 | T05（写红）／T20（转绿） | `crates/ys-tui-coding/src/transcript.rs`（`mod tests`，照 `:538` 形态）<br>`crates/ys-tui-coding/src/app.rs`（`mod tests`） | **写红阶段**：`cargo test -p ys-tui-coding` 指定用例红；**T20 后**：转绿 |
| T21 | impl | `scripts/vision-mutation.py`（≥15 mutant）+ `verify.sh:55` 追加 | T09, T12, T16, T17, T18, T19, T20 | `scripts/vision-mutation.py`（新）<br>`scripts/verify.sh` | `python3 scripts/vision-mutation.py --list` ≥15 条 |
| T21t | test | 全部 mutant「被抓住」+ 锚点失效即非 0 退出 | T21 | 无新增文件（跑脚本自身） | `python3 scripts/vision-mutation.py` 全绿；人为改一处锚点 ⇒ 非 0 并指出失效锚点 |

---

## 波次划分（同波内「涉及文件」两两不相交）

### W0 —— `ys-core` 纯数据

| 任务 | 涉及文件 |
|---|---|
| T01 | `ys-core/src/media.rs`（新）、`ys-core/src/lib.rs` |
| T02 | `ys-core/src/tool.rs`、`tools-basic/{read,grep,ls,bash}.rs` |
| T05 | `ys-core/src/message.rs`（+ 依赖 T01 的 `ImageDetail`） |

> 三个任务同 crate 不同文件 ⇒ 可并行。`T05` 与 `T02` 都用 `T01` 的 `ImageDetail`/`MediaAttachment`。

### W1 —— 编译单元关闭

| 任务 | 依赖 | 涉及文件 |
|---|---|---|
| T03 | T02 | `ys-loop/basic.rs`、`tools-web/{search,fetch}.rs` |
| T04 | T03 | `tools-basic/{find,ocr,write,edit,bg_tools}.rs` |
| T20t（写红） | T05 | `ys-tui-coding/{transcript,app}.rs` |

> **S1**：`T02 → T03 → T04` 必须连续，中间不插任务 —— 期间全 workspace 不编译。
> `T20t` 的白名单用例在此写好并**变红**，是正确的中间状态。

### W2 —— 适配器 IO 单点 + 取文本收口

| 任务 | 依赖 | 涉及文件 |
|---|---|---|
| T06 | T01 | `model-openai-compatible/{media.rs(新), Cargo.toml, lib.rs}` |
| T13 | T05 | `ys-core/src/message.rs` |
| T11 | T03, T05 | `ys-loop/basic.rs` |

> `T11` 与 W1 的 `T03` 同文件 ⇒ 必须分波（已分）。`T13` 与 W0 的 `T05` 同文件 ⇒ 已分。

### W3 —— 请求类型 + 估算器 + 收口落地

| 任务 | 依赖 | 涉及文件 |
|---|---|---|
| T07 | T06 | `model-openai-compatible/src/request.rs` |
| T12 | T05 | `ys-loop/src/token.rs` |
| T14 | T13 | `ys-tui-coding/{app,transcript}.rs`、`apps/coding-agent/{sessions,subagent}.rs` |

> `T14` 与 W1 的 `T20t` 都触 `transcript.rs`/`app.rs` ⇒ 分波（已分）。

### W4 —— 门控 + 其余 `ys-loop` 文件 + 工具

| 任务 | 依赖 | 涉及文件 |
|---|---|---|
| T08 | T01 | `model-openai-compatible/src/compat.rs` |
| T15 | T13 | `ys-loop/{summarize,compact}.rs` |
| T16 | T05 | `ys-loop/src/turn_budget.rs` |
| T10 | T06 | `tools-basic/{view_image.rs(新), lib.rs}`、`apps/coding-agent/src/toolset.rs` |
| T18 | T05 | `apps/coding-agent/src/app_loop.rs` |

> `ys-loop` 三个文件（`summarize`/`compact`/`turn_budget`）互不相交；`basic.rs` 在 W2 已用过 ⇒ 不冲突。

### W5 —— K2 两半（承重）

| 任务 | 依赖 | 涉及文件 |
|---|---|---|
| T09 | T07, T08 | `model-openai-compatible/src/lib.rs` |

> 单任务独占一波：`lib.rs` 是 `T06`（W2）与 `T17`（W6）也触的文件，必须串行。

### W6 —— 序列化塔存

| 任务 | 依赖 | 涉及文件 |
|---|---|---|
| T17 | T09 | `model-openai-compatible/src/lib.rs`、`tests/retry.rs` |

### W7 —— 渲染收尾 + 文案 + 变异脚本

| 任务 | 依赖 | 涉及文件 |
|---|---|---|
| T20 | T05, T13, T14 | `ys-tui-coding/{transcript,app}.rs`、`apps/coding-agent/{subagent,main}.rs` |
| T19 | T10 | `tools-basic/src/ocr/mod.rs`、`apps/coding-agent/src/prompt.rs` |

> `T20` 与 W1 的 `T20t`、W3 的 `T14` 同文件 ⇒ 分波（已分）。`T19` 与 W1 的 `T04` 都触 `ocr/mod.rs` ⇒ 分波（已分）。

### W8 —— 变异脚本

| 任务 | 依赖 | 涉及文件 |
|---|---|---|
| T21 | 全部 | `scripts/vision-mutation.py`（新）、`scripts/verify.sh` |

> 注：W0–W8 共 **9 个波次**。

---

## `crates/ys-loop/` 的波次分布（同波内互不相交）

| 波次 | 文件 | 任务 |
|---|---|---|
| W1 | `basic.rs` | T03 |
| W2 | `basic.rs` | T11 |
| W3 | `token.rs` | T12 |
| W4 | `summarize.rs`、`compact.rs`、`turn_budget.rs` | T15、T16 |

`basic.rs` 在 W1 与 W2 各一次（不同波，满足）；W4 的三个文件互不相交，可并行。

---

## 关键顺序约束（违反即返工）

| # | 约束 | 原因 |
|---|---|---|
| S1 | `T02 → T03 → T04` 连续完成，中间不插任务 | `ToolResult` 加字段期间全 workspace 不编译，插任务会让验证信号失真 |
| S2 | `T20t`（红）**早于** `T20`（转绿） | design.md §11 步 4；红测试是消费点清单的活文档 |
| S3 | `T09` 的两半（`:1311` + `:1316`）**同一任务** | plan §4.3：只改一处「看起来像修过了」 |
| S4 | `T13` → `T14` / `T15` / `T20` | 三处都改调 `joined_text`；反序要改两遍 |
| S5 | `T17` **晚于** `T09` | 同文件；且 `body = None` 的失效点依赖 `:1311` 最终形态 |
| S6 | `T16` 只加注释与测试，**不改** `_ => 0` | C7：计入图片会让闸门无效循环 |
| S7 | `T21` **最后** | 它注入在各任务成品代码里；先写会大面积锚点失效 |
| S8 | `T07` 允许 `lib.rs` 短暂报错 | `Content` 的类型改在 `request.rs`，三处构造点由 `T09` 收口。若要求每波可编译，把 `T07` 并入 `T09` |

### 构造点计数不能用 grep（T02/T03/T04 的执行手段）

三个迁移任务的构造点计数**只能以编译器为准**，`grep` 在四个方向上都不可靠：

| 失真方向 | 例子 |
|---|---|
| **多计**变体声明/枚举构造 | `ContentBlock::ToolResult {`、`AgentEvent::ToolResult {` |
| **漏计**带前缀的绑定 | `let mut tool_result = ToolResult {`（`edit.rs:1127`） |
| **漏计**类型别名 | `crates/ys-loop/src/basic.rs` import 了 `ToolResult as CoreToolResult` ⇒ `grep "ToolResult {"` 在该文件命中 **0** 处，实际有 **12** 处生产构造点 |
| **误计**测试代码 | 各文件 `mod tests` 内的构造点行 |

**执行手段**：临时给 `ToolResult` 加一个探针字段（如 `pub __probe: ()`），跑
`./scripts/cargo-build.sh check --workspace --all-targets`，让 `E0063 missing field` 逐个点名，
补完后再删掉探针字段并加真正的 `media`。`cargo` 默认在 100 个错误处截断，必要时分批
（先 `-p <crate>`）看全。

这正是 plan §4.2「编译器守护」的第一次实际应用 —— 它是 T02/T03/T04 的**执行手段**，
而不只是「按报错逐条补」一句口号。同理，`ToolResult` 加字段期间的报错数**不是**
验收指标（探针字段与真实字段的报错数会因 `#[serde(default)]` 差异而不同）。

---

## 配对自检（21 impl ↔ 21 test）

| impl | 配对 test | 该 test 守护的承重结论 |
|---|---|---|
| T01 | T01t | 嗅探按**内容**判格式；`ys-core` 零 tokio |
| T02 | T02t | `skip_serializing_if` = 字节兼容契约 |
| T03 | T03t | 迁移未改语义（编译通过 ≠ 语义不变） |
| T04 | T04t | 编译单元关闭；`ocr` 既有用例不受影响 |
| T05 | T05t | 加变体**零编译错误** ⇒ golden/round-trip 是唯一字节守护 |
| T06 | T06t | 锁外 IO + 缓存上界 + **按编码后**判单图 |
| T07 | T07t | `Content` 不得改变纯文本字节 |
| T08 | T08t | 门控**只在一处判**、fail-closed |
| T09 | T09t | **K2 两半** + 六条占位 |
| T10 | T10t | 工具层**不编码**（D2） |
| T11 | T11t | 折叠仍是一条消息（消息数守恒） |
| T12 | T12t | K3：`token.rs` 计入 1024 |
| T13 | T13t | C6：穷尽 match 的**编译期**守护 |
| T14 | T14t | C6：五处收口、分隔符各自保真 |
| T15 | T15t | 摘要判定不被收口改动影响 |
| T16 | T16t | C7：**有意不计入**（把「不做」钉成要求） |
| T17 | T17t | C9：**不得**循环外算一次（回退分支要重算） |
| T18 | T18t | A 档：skill 展开不丢图 |
| T19 | T19t | C2：文案跨 crate 一致、不再声明独家 |
| T20 | T20t | 白名单先红后绿；replay == live |
| T21 | T21t | 变异脚本自身有效（锚点失效即失败） |

**无未配对 impl**。

---

## 测试优先级（来自 plan §7 的 C1–C27）

| 优先级 | 任务 | 条数 |
|---|---|---|
| **P0** | T01t、T02t、T05t、T06t、T07t、T08t、T09t、T10t、T11t、T12t、T13t、T14t、T16t、T18t、T19t、T20t、T21t | 17 |
| P1 | T06t 的缓存边界三条、T17t 的 429 复用 | 并入 |
| P2 | T03t、T04t、T15t | 3 |

---

## 完成判据（整体）

1. `./scripts/cargo-build.sh check --workspace --all-targets` 零 error。
2. `./scripts/par-test.sh` 全绿（**不用**裸 `cargo test --workspace`）。
3. `python3 scripts/vision-mutation.py` 全部 mutant 被抓住（≥15 条）。
4. `./scripts/verify.sh` 通过（第 2 阶段含 `vision-mutation`）。
5. 以下断言逐条成立：
   - `rg "the only way" adapters/ apps/` ⇒ **0** 命中（C2）；
   - `rg "base64" apps/coding-agent/tests/golden/` ⇒ **0**（G6）；
   - `apps/coding-agent/tests/stream_usage_fallback.rs:361` 保持绿（C9 的护栏）；
   - `rg "\bv: 1" crates/ys-session/src/meta.rs` ⇒ 命中，且**无** `v: 2`（C1：未改）；
   - `git status` 不含 `crates/ys-tui-coding/src/draw.rs`（E4）。
