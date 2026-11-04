# 图像输入（Vision Input）— 架构上下文

## 概述

让 Agent 能收到**像素**而不是「图片转成的文字」：把图片作为多模态内容送进模型，
由模型自身理解图片（截图、图表、UI 缺陷图），而不是先经 OCR 引擎压成文本。

现状基线：仓库**没有**原生图片输入能力，模型「看到」图片的唯一通道是 `ocr` 工具
（`adapters/tools-basic/src/ocr/`，ADR-0068），输出是文本 + 坐标。

触发点：DeepSeek 上线了 `deepseek-flash` 的图像理解能力
（<https://api-docs.deepseek.com/zh-cn/guides/vision>），其 chat completions 端点接受
OpenAI 兼容的 `content` 块数组 —— 本仓的 provider 默认端点之一即可原生读图，
上游无阻塞。

## 现有架构

### 模块边界

```
用户输入(TUI/-p/--json) → apps/coding-agent → ys-loop(BasicLoop) → ys-model::Model
                                   ↓                                    ↓
                            ys-session(JSONL 落盘)          adapters/model-openai-compatible
                                   ↓                                    ↓
                            ys-core::Message/ContentBlock ←── translate_request ──→ ChatMessage{content: Option<String>}
                                   ↑
                         tools-basic::ocr（图片 → 文本，唯一现存读图通道）
```

- **`ys-core`**：`Message` / `ContentBlock` 是全会话的唯一内容表示，跨 crate 契约。
- **`ys-model`**：`ModelRequest.messages: Vec<Message>` —— 请求里带的就是 core 类型，
  适配器负责翻译成 provider 方言。
- **适配器**：`translate_request`（`adapters/model-openai-compatible/src/lib.rs:1233`）
  把块数组**扁平化**成单个字符串。

### 核心抽象

```rust
// crates/ys-core/src/message.rs:17-39
pub struct Message { pub role: Role, pub content: Vec<ContentBlock> }

#[non_exhaustive]
pub enum ContentBlock {
    Text { text: String },
    ToolUse { id: ToolCallId, name: String, arguments: serde_json::Value },
    ToolResult { tool_call_id: ToolCallId, content: String, is_error: bool },
}
```

**没有任何承载二进制 / 引用外部资源的块变体** —— 这是本轮改动的主战场。

`Role` 有 4 个值（`User` / `Assistant` / `System` / `Harness`），`Harness` 在线上按 `user` 发送
（`crates/ys-core/src/message.rs:10-14`）—— 与 DeepSeek「图片仅限 user 消息，
system/assistant 带图返回 400」的限制直接相关。

### 关键数据流

**当前（1）图片 → 文本 → 模型**

```
用户: "看看这张图 /tmp/a.png"
  → 模型调用 ocr(path=/tmp/a.png)  → 工具进程（tesseract / mac Vision）
  → ToolResult{content: "WxH\n<TSV 或文本>"}  → 模型读到文本
```

**目标（2）图片 → 模型（原生）**

```
用户: "看看这张图 /tmp/a.png"
  → 用户消息 content = [Text{...}, Image{...}]   ← 新变体落在这里
  → adapter: content 数组（不再扁平化）+ base64 data URL
  → deepseek-flash 直接读图
```

**现成可复用的入口特征**：`ocr` 工具已经确立了「工具参数里给的是**路径**，
由工具自己读文件」的模式；用户输入侧同样是「用户敲了路径」。这暗示 image 块
天然以**本地路径**为锚，而非由 UI 粘贴二进制（见「未澄清问题」Q1）。

**入口侧现状（已穷尽核实）**：从用户敲字到 `Message` **没有任何承载多模态载荷的中间类型**
—— 三个入口都在就地造一个只含单个 `Text` 块的 Message，之后原样透传：

| 入口 | 构造点 | 形态 |
|---|---|---|
| TUI 输入 | `crates/ys-tui-coding/src/commands.rs:387` `prompt_message` | `Message{Role::User, vec![Text]}` |
| TUI 提交 | `crates/ys-tui-coding/src/events.rs:844` `submit()` 取 `app.input.take()`（纯 `String`） | 同上 |
| `-p` / `--json` | `apps/coding-agent/src/main.rs:2320` / `:2409` → `ys-loop/src/input.rs:14` `AgentInput::text` | 同上 |
| 接线器 | `apps/coding-agent/src/wiring.rs:379` | 同上 |
| skill 展开 | `apps/coding-agent/src/app_loop.rs:1799` `expand_skill_invocation`（只看首块 Text，命中则**重建** Message） | 同上 —— ⚠️ **会重建**，新增块若不处理会被丢掉 |

关键：`ys_protocol::Request::Prompt(Message)`（`crates/ys-protocol/src/request.rs:14`）的载荷
**已经是 `Message`** ⇒ protocol 层无需改动；`AgentInput`（`crates/ys-loop/src/input.rs:4`）只有
`message: Message` 一个字段，无 attachments/parts 粘滞位 ⇒ **不需要"新增中间类型"**，
多模态可以全程走 `ContentBlock` 一个面。

**TUI 侧现状**：无任何附件 / 拖拽 / 图片粘贴机制。所有粘贴路径（bracketed paste、`Event::Paste`、
Ctrl+V 剪贴板）都落到**纯字符串插入输入框**（`crates/ys-tui-coding/src/events.rs:157`、
`app.rs:193` 的 `input_drag` 是鼠标拖选**文本**，不是文件拖放）；剪贴板是文本双向通道
（读 `run.rs:1100`、写 `/copy` 走 OSC 52）。⇒ 若 Q1 要求粘贴图片，**TUI 输入通路需新增能力**，
不是改接线就能得到。

### 关键卡点（已核实，逐条带指针）

| # | 位置 | 现状 | 对图像的后果 |
|---|------|------|--------------|
| K1 | `adapters/model-openai-compatible/src/request.rs:50-61` | `ChatMessage.content: Option<String>` | **结构性阻塞**：无法承载块数组。需引入 `Content` 枚举（`String` \| `Vec<Block>`）并保持纯文本时线上字节不变 |
| K2 | `adapters/model-openai-compatible/src/lib.rs:1311` | `_ => {}` 静默丢弃未知块 | 新增变体**编译不报错、运行时静默丢图** —— 最坏的失败模式（模型收到空上下文却不报错） |
| **K0** | **全仓**（见下表「爆炸半径」） | **没有任何一处对 `ContentBlock` 做无 `_` 兜底的 `match`** —— `#[non_exhaustive]` 让下游一律写 `_` | **本轮最重要的结论**：新增 `Image` 变体**一个编译错误都不会触发**，所有遗漏点都表现为「静默跳过」。⇒ 编译器不会当保镳，必须靠穷尽式清单 + 变异测试守。这与 ADR-0068「``enum Kind` + 穷尽 `match`，漏臂就是编译错误」的枚举安全前提**恰恰相反** |
| K3 | `crates/ys-loop/src/token.rs:7-14` | `_ => 0` 估算 0 token | 图片按 0 计 ⇒ `basic.rs:625` 的压缩触发**永不因图片而提前** ⇒ 长会话必踩上下文溢出。DeepSeek 侧每图上限 1024 token，是可计算的实数 |
| K4 | `crates/ys-loop/src/compact.rs:293-311` | `merge_summary_into` 逐字保留 content | 合并摘要安全（`_` 分支插 Text 块在前），但**被摘要掉的图片**没有任何回收策略 |
| K5 | `crates/ys-session/src/jsonl.rs` | `serde_json::to_string(message)` 整条落盘（`:129`）；**反序列化失败该行直接跳过**（`:91-102`，计入 `skipped_lines`，不报错） | **最险的一条**：老版本二进制读到含 `Image` 块的会话 ⇒ 该**整条消息被静默丢弃**（不是拒绝文件）。base64 进历史另加 JSONL 体积爆炸（单图 ≤32 MiB 量级）|
| K5b | `crates/ys-session/src/jsonl.rs:57-60` | 判别靠「`Message` 无 `type` 字段」 | `ContentBlock` 无 `#[serde(tag=…)]` / `untagged` / `deny_unknown_fields` ⇒ 新增变体是**普通外部标签**，旧变体字节不受影响（这一点安全）|
| K6 | `crates/ys-core/src/message.rs:24` | `#[non_exhaustive]` | 变体加得进；但 `Role` **不是** `non_exhaustive`，若为图片加角色会破契约 |
| K7 | `crates/ys-loop/src/compact.rs:131`、`jsonl.rs:206+`；`main.rs:676` `message_bytes`；`ys-tui-coding/src/app.rs:905` `message_text`；`sessions.rs:182` `user_text` | 多处 `match` 只认 `Text`/`ToolUse`/`ToolResult`，兜底分支静默跳过（`_ => 0` / `_ => None`） | 与 K2 同类但程度不一：「不影响」与「丢数据」需逐个判定；`message_bytes` 返回 0 会让**输入字节统计失真** |

#### 爆炸半径：新增 `Image` 变体的全部消费点（已穷尽）

按后果分三档。**没有任何一档会编译失败**。

**A 档 —— 直接丢数据 / 错数据（必须改）**

| 位置 | 行为 |
|---|---|
| `adapters/model-openai-compatible/src/lib.rs:1311` | `_ => {}` → **图根本不上线路**（致命） |
| `crates/ys-loop/src/token.rs:7-14` | `_ => 0` → 上下文预算不计图（致命，见 K3） |
| `adapters/model-openai-compatible/src/lib.rs:1673-1683` `final_text` | `filter_map Text` → 响应侧文本收集（图片本不应出现在 assistant，但需显式拒绝） |
| `apps/coding-agent/src/app_loop.rs:1821` `expand_skill_invocation` | **重建整条消息为 `vec![Text]`** → skill 展开时**静默丢图** |
| `crates/ys-session/src/jsonl.rs:91-102` `load_from_file` | 整行解析失败 ⇒ **整条消息跳过**（卡点 K5） |

**B 档 —— 不渲染 / 统计失真（需给用户可见降级，不得静默）**

| 位置 | 现状 |
|---|---|
| `crates/ys-tui-coding/src/app.rs:905` `message_text` | 只拼 `Text`，图被丢弃 ⇒ `events.rs:897` `note_steered` / `begin_local_turn` 里图消失 |
| `crates/ys-tui-coding/src/transcript.rs:155-166` `replay_lines` | 只拼 `Text`；**代码注释已明写**「加变体时必须同步本函数与 `message_text`，漏了就静默不渲染」 |
| `apps/coding-agent/src/subagent.rs:1819` `transcript_body` | `_ => out.push_str("[unknown content block]\n")` —— 有占位但不渲染图 |
| `apps/coding-agent/src/main.rs:676` `message_bytes` | `_ => 0` ⇒ 输入字节统计失真 |
| `apps/coding-agent/src/sessions.rs:182` `user_text` | 只取 `Text`，用**空格**拼接（与其他处 `\n` 不同） |

**C 档 —— 语义上不应受影响（仍建议逐个过目）**

`ys-loop/src/turn_budget.rs:70-93` `elide_message`（`other => other.clone()`，保留块）、
`turn_budget.rs:32-43` `message_tool_result_tokens`、`compact.rs:128` `contains_tool_result`、
`compact.rs:293` `merge_summary_into`、`compact.rs:374` + `summarize.rs:105`（摘要**看不见**非 Text 块 ——
若图需要进摘要，这是断层）、`ys-session/src/jsonl.rs:201-300`（工具配对修复，只认 ToolUse/ToolResult）、
`basic.rs:926/938`（正文与工具收集）、`subagent.rs:1758/1769`（`tool_text_of` / `text_of`）、
`app_loop.rs:1829` `first_text`（首块非 Text ⇒ 不展开 skill —— 与 A 档那条联动）。

**测试已完成的部分保护**：`crates/ys-tui-coding/src/transcript.rs:538-560` 就是专测「白名单漏变体
静默不渲染」的用例；`apps/coding-agent/tests/resume_cli.rs:162-167` 把**外部标签**形态写死
（`{"Text":{"text":…}}`，并据此手写 JSONL）；字节级基线在
`apps/coding-agent/tests/golden/json_no_debug_baseline.jsonl:1`。⇒ 新增变体**不破**旧格式，
但**改成块数组会破**（K1）。

### 外部依赖（本轮新增能力的上游约束）

| 依赖 | 用途 | 约束 |
|------|------|------|
| DeepSeek `/chat/completions`，模型 `deepseek-flash` | 图像理解后端 | 块数组格式；`image_url` base64 / 外部 URL / `file` 块（`file_id` 或 `file_data`）；可选 `detail: low\|high\|original\|auto` |
| 同上 | 格式与限额 | JPEG/PNG/GIF/WebP（**按内容判断**，不认扩展名与声明 MIME）；请求体 ≤48 MiB；单图 ≤32 MiB（base64/URL）；单请求 ≤600 图；单边 ≤8192 px（≥15 图时 4096） |
| 同上 | 位置限制 | **图片仅限 `user` 消息**，`system`/`assistant` 带图 ⇒ `400` |
| 同上 | 计费 | 自动缩放至约 1300×1300；**每图 token 上限 1024**；`low` 缩到 512×512 |
| 同上（未确认） | 流式 | 文档页未提流式；本仓主链路走 SSE 流式，**该路径需实测**（未澄清 Q4） |

### 已有可复用件（本轮不必新造）

| 件 | 位置 | 可复用到什么程度 |
|---|---|---|
| **图片格式嗅探** | `adapters/tools-basic/src/ocr/mod.rs:512` `image_dims` / `:537` `dims_from_header`（读前 256 KiB，按**魔数**分派 PNG/GIF/BMP/WebP/TIFF/JPEG） | 直接给出「是不是图片、什么格式、多大」—— 正是 DeepSeek 要求的「按内容判断格式」。当前是 `tools-basic` 私有函数，复用需上提或复制（试探：上提到 `ys-tool` 或独立小 crate） |
| **两层能力门控骨架** | `adapters/model-openai-compatible/src/compat.rs:166` `supports_effort` + `:186` `effort_for` | 结构可照搬：`supports_*` 判身份、`*_for` 出最终值、「门控只在一处判」。新增一个能力位的**完整改动面**已确认：`compat.rs`（字段 + `Default` + 各构造器 + 门控 ×2 + 单测）、`request.rs`（wire 字段）、`lib.rs:translate_request`、`provider.rs:181-253`（内置表）、`config.rs`（用户开关时）、`main.rs:2904`（构造 config） |
| **provider 解析入口** | `apps/coding-agent/src/provider.rs:303` `find_provider` / `:308` `compat_for`（未知 → `standard()`）/ `:667` `provider_for_model` | 按名或 api_base 都能定位；`/model` 改 `config.provider` 与 `config.model` 后，compat 由 `config.rs:602` `current_compat()` **每次重新取** ⇒ 切模型后能力位自动跟随，无需额外失效逻辑 |
| **路径即参数的工具范式** | `adapters/tools-basic/src/ocr/mod.rs:135-166`（`required: ["path"]`） | 用户侧输入「图片路径」与工具侧一致；也说明本仓既有心智是「给路径、由实现读盘」 |

### 已知不可复用 / 硬阻塞（复核后）

- **工具无法返回图片**：`ys_tool::Tool::call` 返回 `ys_core::ToolResult { content: String, is_error: bool }`
  （`crates/ys-core/src/tool.rs:13-17`），到 `ContentBlock::ToolResult` 的转换在
  `crates/ys-loop/src/basic.rs:1409` —— **没有非文本返回路径**。
- **`read` 读不了图**：`tokio::fs::read_to_string` 对二进制直接 `ToolError::Execution`
  （`adapters/tools-basic/src/read.rs:83-86`），历史上图片在 TUI 里降级成 `[img]`。
- **仓库内零基础**：`image_url` / `base64` / `mime` / 图片 token 估算的既有实现**一处都没有**
  （只有 `ocr::image_dims` 的魔数嗅探）。
- **ADR-0068 的原始论据需正面回应**：`docs/adr/0068-ocr-tool.md:9-14` 把「新增 image 变体
  要穿透 model adapter、provider compat、事件流三层，与『最小核心』冲突」作为**走 ocr 工具的理由**；
  `docs/arch/ocr-tool/context.md:157-176` 更细：结论是「要同时改五处（含碰 JSONL 字节契约）」。
  本轮若反转，这两处是必须逐条回答的对手盘。

## 约束

- **技术**
  - **零新增运行时依赖**是本仓一贯偏好（ADR-0068 明确写过）：base64、图片格式嗅探、
    mime 判定应尽量用已有依赖或最小手写，不引 `image` crate 做解码。
    **base64 已在 `Cargo.lock:44`（`base64 0.22.1`，来自传递依赖）**，但
    `adapters/model-openai-compatible/Cargo.toml` **未直接声明** —— 要用必须显式加，
    这算一条**显式依赖新增**（虽是已有 crate，不增编译单元）。
  - `ys-core` 零 tokio、只装数据与纯 trait ⇒ 图片块**不能**带「读文件的 IO 行为」，
    只能是**声明性数据**（路径或已编码字节），读盘发生在适配器 / 上层。
  - **会话格式兼容**：`ContentBlock` 的 serde 是用变体名做外部 tag（历史落盘形如
    `{"ToolUse":{...}}`），新增变体不得改变既有变体的字节形态。
  - **provider 门控已是成熟模式**：`ProviderCompat`（`adapters/model-openai-compatible/src/compat.rs`）
    的两层门控（provider 级能力位 + 模型 id 可识别性）可直接照搬，
    结构见 `docs/design-final/effort.md` §3 —— 如 `supports_effort()` / `effort_for()`
    的「门控只在一处判」不变量。
  - **模型级门控的现实**：DeepSeek 的图像能力绑在 `deepseek-flash` 上（旧名
    `deepseek-v4-flash-vision-exp` 已下线但请求仍被承接）。目录来自 `/v1/models`
    端点（`apps/coding-agent/src/provider.rs:188`），模型名**不写死** ⇒ 无法靠种子列表
    「认识」哪些模型支持图像，需从 id 判别（未澄清 Q3）。
- **性能**
  - 图片以 base64 进请求体 ⇒ **每回合重放**同一张图都要重付传输与 token 成本
    （每图 ≤1024 token）。压缩前的长会话里，图会反复上送。
  - **与 OCR 的成本对比有实测底数**：`docs/arch/ocr-tool/context.md:272-286` —— 行级渲染把
    原始 TSV 压到约 1/7，**单图行级最大 1,253 B**（≈300 token 量级），且原文自评「8 KiB 软
    阈值几乎永不触发，原先按 300–800 token/页 的预算紧张度被**高估**了」。
    ⇒ 单图：OCR 约 0.3–0.8 K token vs 原生图 **至多 1 K token**（`low` 档更少）。
    **成本量级接近，不是数量级差异**；原生图换来的是「无需猜 OCR 歧义、可多轮追问」。
  - 真正的成本风险在**体积与重放**：base64 使单图进入历史后每回合重传（≤32 MiB/图），
    而 OCR 文本一次生成后永不重传。这一条决定 Q2（存路径 vs 存 base64）。
- **演进**
  - 本轮若否决原生输入，`ocr` 路线需保持在图片场景下继续可用（不能因新变体破坏旧路）。
  - `#[non_exhaustive]` 的变体一旦落盘，任何**读不懂该变体**的旧版本会话回放都需要
    明确策略（拒绝 / 降级为占位文本）。
- **组织**
  - 单一主题、可由本轮一次做完；但涉及 `ys-core` + 适配器 + loop 三处，
    属于「穿透式」改动，需先定形态再动手。
  - 文档纪律：ADR-0068 用「最小核心」论证过**不做** image 变体。本轮若反转，
    必须写新 ADR 正面回应那条论据，而不是悄悄加变体。

## 需求范围

### 范围内

- 用户在**输入里表达图片意图**（路径形态优先），图片随该条消息送进模型。
- DeepSeek provider 上的端到端可用：`deepseek-flash` 读本地图片（JPEG/PNG/GIF/WebP）。
- 无图像能力的 provider / 模型上有**明确降级**：不发图片块、给出可理解的提示，
  且不得静默丢内容。
- 图片在会话中的生命周期有定义：落盘形态、压缩时的处理、token 估算口径。
- **"爆炸半径"清单全部有结论**：A 档 5 处全改，B 档 5 处给可见降级，C 档逐条判定；
  关键路径上要有**变异测试**证明漏改会被抓到（因为编译器不会报错，见 K0）。

### 范围外（明确不做的）

- **不做** TUI 剪贴板粘贴二进制 / 拖拽截图（除非 Q1 裁决要）——输入面先限制在路径。
- **不做** Files API（`file_id` 上传与复用、64 MiB 大图）。
- **不做** 把图片以 base64 **存进会话**（A2 已定：只存路径）。
- **不做** 输入框内**渲染缩略图 / 内嵌图**（A1 已定：只显示路径）。
- **不做** 视频 / 音频输入。
- **不做** 本地图像解码、裁剪、缩放、格式转换（`detail` 交给 provider 侧）。
- **不替换** `ocr` 工具：其「文字 + 像素坐标」输出对定位类任务是图像理解不可替代的，
  两者并列。
- **不做** 生成图片（只有输入）。

### 关键场景

- **场景 1｜截图排障**：用户贴一张 UI 截图路径 + 「这个布局错在哪」→ 模型直接看图回答，
  无需先 OCR 再猜。
- **场景 2｜图表读数**：贴一张折线图 → 模型读出趋势与数值（OCR 路线在此几乎不可用）。
- **场景 3｜多图对比**：同一请求里 2–3 张图（DeepSeek 单请求 ≤600 图）。
- **场景 4｜降级**：当前 provider 是 `glm` / `mimo` 等无图像能力端点 ⇒ 明确提示
  「当前模型不支持图像输入」，并建议改模型或用 `ocr` 工具。
- **场景 5｜压缩前后的长会话**：图进入历史后被压缩掉 / 保留，都不能让会话变成不可发送。

### 关键异常/边界

- 路径不存在、不是图片、格式不支持（按**内容**判，非扩展名）。
- 图片超过 32 MiB；多图总大小超限。
- 图片出现在 `system` 或 `assistant` 消息里（DeepSeek 直接 400）。
- 会话回放：历史里有 Image 块但当前模型无图像能力。
- **拷贝的图来自其它机器 / 文件已被删**：`data:` URL 是从**出站那一刻**读盘得到的 ⇒
  路径失效时该条消息**无法发送**（A2 的引用模型必然代价）。
- **剪贴板里同时有多种格式**：文件引用 vs 位图 vs 文本，优先级决定用户拿到什么
  （建议：文件引用 > 位图落盘 > 文本，理由：前两者保留信息量，文本分支是旧行为）。
- 子 agent（`subagent_async`）继承主模型时是否也带图片能力。
- **skill 展开丢图**：`expand_skill_invocation`（`app_loop.rs:1821`）重建 `vec![Text]` ⇒
  「贴图 + 调用 skill」的组合会静默丢图。
- **压缩看不见图**：摘要只拼 `Text`（`compact.rs:374`、`summarize.rs:105`）⇒ 图不会进摘要；
  且被摘要掉的图无回收策略（Q6）。

## 已裁决

### A1（原 Q1）输入面：允许拷贝图片，TUI 只显示路径

**用户裁决**：输入面可以「拷贝图片」的方式输入；但 TUI 输入框里**只显示路径**，不内嵌图片。
即剪贴板是**入口**，路径是**存储与展示形态**。

**可行性实测（macOS 本机，2026-xx）** —— 三种剪贴板形态行为**完全不同**，不能只靠一层文本读：

| 剪贴板形态 | 产生方式 | `pbpaste`（现有唯一读取器） | `osascript` 取路径 | 结论 |
|---|---|---|---|---|
| **文本** | 复制路径字符串 | 得到路径字符串 ✅ | — | 现有通路即可 |
| **文件引用**（Finder `⌘C`，`«class furl»`） | `set the clipboard to (POSIX file …)` | **空串 + exit 0**（静默失败） | `POSIX path of (the clipboard as «class furl»)` → 真实路径 ✅ | **必须加 `osascript` 夹具**；`pbpaste` 会连「失败」都看不出来 |
| **原始位图**（截图 `⌃⇧⌘4`、网页复制图，`«class PNGf»`） | `read … as «class PNGf»` | 空串 | **报错 `-25133`（无路径可取）** | **根本不存在路径** ⇒ 必须落盘成临时文件才能满足「TUI 显示路径」 |

Linux 对应能力：`wl-paste --type image/png`、`xclip -selection clipboard -t image/png -o`、
`xsel` **无类型选项**（读不了图）—— 与 `run.rs:1163` `clipboard_readers()` 的现有顺序同构。
macOS 额外注意：`pngpaste` 是第三方工具（本机未装），**不引入它**。

> 推论：`run.rs::read_system_clipboard()` 当前的契约是「返回 `Option<String>`，命令缺失/非 UTF-8 ⇒ `None`」。
> 新需求要让它多一条分支：优先取**文件引用路径**，否则若剪贴板是位图则**落盘为临时 PNG
> 并返回该路径**，否则回落现有文本行为。这一步是纯 IO，仍在 UI 线程的子进程（现有设计已如此），
> 但**写临时文件**是新增副作用，需定目录与清理策略（建议沿用 `YUSHAN_*` 覆盖惯例，落
> `<temp>/yushan-clip/`，按会话清理）。

### A2（原 Q2）存储形态：只记路径

**用户裁决**：以**路径**记录。

由此**直接定型**的几条（无需再问）：

- `ContentBlock` 的 image 块**只携带路径**（+ 可选 `detail`），**不带 base64**。
  ⇒ JSONL 不会因图而膨胀，每行仍是 KB 量级。
- **base64 编码发生在出站那一刻**（`translate_request` 内），从磁盘读文件 → base64 → `data:` URL。
  这样「一个路径」只在内存与 wire 上的一次请求里变成字节，**永不进会话**。
- **代价必须写进设计**：历史是**引用而非快照**。文件被改/删/移后，回放时看到的是**另一张图或报错**。
  旧会话在另一台机器上无法回放。压缩后若只留路径，摘要也不包含图内容。
  ⇒ 设计需给出**失效行为**：出站时读不到文件 ⇒ 明确错误还是剔除该块继续？
  （建议：**明确错误**并指名路径，因为静默剔图正是本轮要消灭的失败模式。）
- 因存路径，**「按内容判断格式」可在读文件时顺便做**（魔数嗅探已有 `ocr::image_dims`），
  比存 base64 时更自然。
- 因存路径，`ContentBlock` 的图片变体与 `ocr` 工具的**多轮复用天然契合**：同一路径既能看图又能 OCR。

## 未澄清问题

**待用户确认**（下面每条都会决定设计空间的大小，请裁决或直接说「你定」）：

- [x] ~~**Q1 输入面到哪一步**~~ ⇒ **已裁决**：拷贝图片可输入，TUI 显示路径（见 A1）。
- [x] ~~**Q2 图片在会话里怎么存**~~ ⇒ **已裁决**：只存路径（见 A2）。
- [ ] **Q3 能力门控粒度**：按 provider 打能力位（deepseek=有），还是必须精确到
      **模型 id 识别**（`deepseek-flash` 才算有）？后者的「怎么从 id 认出支持图像」
      需要你给判据（现无 `/v1/models` 能力标记）。
- [ ] **Q4 流式路径是否已通**：vision 文档**整页未提流式**；本仓主链路是 SSE
      （`adapters/model-openai-compatible/src/stream.rs`）。需实测一次（要 `YUSHAN_API_BASE` +
      `YUSHAN_API_KEY` + 一个 `deepseek-flash` 可读的图）还是直接按「非流式兜底」设计？
- [ ] **Q5 与 `ocr` 的关系**：`ocr` 工具是否要在「模型有图像能力」时**自动让位**（提示模型
      优先用原生看图），还是完全并存、由模型自己选？
- [ ] **Q6 压缩策略**：被压缩掉的图片是**直接丢弃**（摘要里只留「曾有一张图」）还是
      **保留最近 N 张**？图片 token 按 1024/张计入估算的口径要写进压缩阈值吗？
- [ ] **Q7 「按内容判断格式」的落点**：`ocr::image_dims` 已在 `tools-basic` 里做了魔数嗅探。
      复用时是把它**上提**成共享件（改 crate 边界），还是适配器侧**复制一份**嗅探（几十行，
      避免 `ys-core` 依赖 `tools-basic`）？后者更符合依赖方向，但会产生第二份实现。

## 后续建议

- 建议用 `arch-design` 做方案设计。四个候选方向的**分歧点**已清楚，可直接作为候选切分：
  - **A 块变体**：给 `ContentBlock` 加 `Image`，适配器出站转块数组（正统、改面最大）。
  - **B 工具旁路**：不做变体，加一个 `view_image` 工具由 harness 侧把图片**回注**进
    下一轮请求（绕开会话格式，但等于给 loop 开后门）。
  - **C 路径即协议**：用户消息里只出现结构化标记（如 `![path]`），适配器出站时扫描
    并转成图片块（不改 `ContentBlock`，但把语义藏进字符串 —— 与 K2 同类风险）。
  - **D 维持 OCR**：否决原生输入，改为增强 `ocr`（例如把图直接喂给 deepseek-flash 做
    描述，工具内完成）—— 成本最低，但模型失去多轮追问图片的能力。
- **候选评估的硬指标（由 K0 得出）**：哪个方案能在不改 `ContentBlock` 的前提下
  也**不漏数据**？A 的风险是「散点式遗漏且编译器不管」，C 的风险是「语义藏在字符串里」。
  方案设计时必须给出**「怎么保证覆盖全部消费点」的机制**（穷尽式遍历单测 / 变异测试 /
  中心化访问器 `ContentBlock::is_visual()` 之类），而不是依赖自觉。
- 方案设计需同时回答「**降级语义**」（场景 4）：无图像能力的模型在会话里遇到 Image 块
  时是拒发、剔图继续、还是报错停下？这决定了 `translate_request` 的返回类型是否需要变成
  可失败（现为 `Result<_, ModelError>`，可复用）。
- 建议用 `prototype` 先验证 **Q4（流式）× 最简请求**：一条 base64 图片走现有 SSE 链路，
  确认解析路径是否需要改（`stream.rs` 对 `content` 的假设）。
- 定形后按仓库纪律：新 ADR 正面回应 ADR-0068 的「最小核心」论据 →
  `docs/arch/vision-input/design.md` → `docs/design-final/`。
