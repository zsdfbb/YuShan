# 图像输入（Vision Input）— 设计方案

> 上游：`docs/arch/vision-input/context.md`（现状、卡点 K0–K7b、已裁决 A1/A2）
> 参照实现：OpenAI Codex（源码在 `tmp/codex/`），关键文件在下文逐条标注

## 0. 先回答架构问题：功能不做进 TUI

**TUI 只负责渲染一行标记，不承载任何图像逻辑。** 功能落在三层：

```
┌──────────────────────────────────────────────────────────────┐
│ ys-core        纯数据：Image 块 / ImageDetail / 格式嗅探(纯函数) │  ← 零 IO、零 tokio
├──────────────────────────────────────────────────────────────┤
│ tools-basic    view_image 工具：校验路径、产出"附件"             │  ← 同步校验，不编码
│ ys-loop        把工具附件折叠成 Image 块（块结构转换）            │  ← 不碰字节
├──────────────────────────────────────────────────────────────┤
│ model-openai-  唯一的 IO 点：读盘 → 嗅探 → base64 → data URL     │  ← 编码关在这里
│   compatible   + 能力门控 + 缓存 + 降级                        │
├──────────────────────────────────────────────────────────────┤
│ ys-tui-coding  只渲染 "[image: /path]"，无 IO、无编码            │  ← 最薄的一层
└──────────────────────────────────────────────────────────────┘
```

**解耦的核心不变量：像素字节永不跨 crate 边界，只有路径跨。**

- `ContentBlock::Image { path }` 里**没有字节** ⇒ 会话、事件、TUI、压缩、token 估算
  看到的都是一个字符串。
- base64 是**wire 细节**，只存在于适配器的一次函数调用栈里，函数返回即丢。
- 由此：`ys-core` / `ys-tool` / `ys-tui-coding` 都不需要知道 Base64、MIME、data URL 是什么。

对比 Codex：它把 `LocalImage { path }` 存在协议层、把字节化推迟到「请求序列化」
（`tmp/codex/codex-rs/protocol/src/user_input.rs:36-43` 注释原文：*will be converted to an
`Image` variant (base64 data URL) during request serialization*）。本设计取同一形态，
但**更进一步**：Codex 序列化后把 data URL **留在历史里**（`snapshot_local_user_input`
原地替换），本设计**从头到尾不留字节**。

## 1. 目标与非目标

**目标**

1. 两条生产者都能把图片送到模型面前，形态统一为同一种块：
   - **用户**：在输入里给路径（`看看这张图 /tmp/a.png`）→ 模型直接看到像素。
   - **模型**：调 `view_image(path)` 工具 → 下一轮看到像素。
2. 无图像能力的 provider / 模型上**必有明确降级**（文本占位），不得静默丢弃。
3. 路径失效（文件被删/移/改名）**不打断回合**，但要留下可见原因。
4. `ocr` 工具保持不变，与之并存：`ocr` 给「文字 + 像素坐标」，`view_image` 给像素。

**非目标**（明确不做）

- TUI 剪贴板粘贴图片 / 拖拽 / 缩略图（用户已裁决：TUI 无法直接接受图片输入）。
- base64 进会话、Files API（`file_id`）、远程 URL 图片。
- **本地图像缩放 / 裁剪 / 转码**（无 `image` crate）⇒ 超限图片**拒绝并说明**，不静默缩。
- 视频 / 音频。
- 图片生成。

## 2. 决策总览

| # | 决策 | 理由 / 依据 |
|---|---|---|
| **D1** | `ContentBlock::Image { id, path, detail }` —— **默认只带路径**；字节按需快照到旁路文件（见 §3.7） | 保留可复现性，同时不让 base64 进主 JSONL（体积/重写问题见 §3.7） |
| **D2** | **唯一的读盘 + base64 点：适配器出站** | 依赖方向：只有适配器认识 wire 格式；像素字节不出适配器 |
| **D3** | 格式嗅探是 `ys-core` 里的**纯函数**（`&[u8] → Option<ImageFormat>`） | 纯数据无 IO；`ocr::image_dims` 的同构实现，宿主零依赖 |
| **D4** | 能力门控**两层**：`supports_image_input`（provider）+ `supports_image(model)`（模型） | 照搬 `effort` 的既有骨架（`docs/design-final/effort.md` §3，ADR-0042） |
| **D5** | 不支持图像时**替换为文本占位**，不是丢弃 | 照抄 Codex `strip_images_when_unsupported`（`core/src/context_manager/normalize.rs:346`）；治本仓 K2 `_ => {}` |
| **D6** | 路径读不到 ⇒ **文本占位 + 继续回合**（不 fatal） | 照抄 Codex `local_media_error_placeholder`（`protocol/src/models.rs:1814`）；回合不该因一张图而死 |
| **D7** | 适配器内按 `(path, mtime, len)` **缓存 base64 结果** | 否则每回合重读重编码（Codex 靠 provider 侧 file_id 缓解，本仓没有；见 issue #28316） |
| **D8** | token 估算：每个 Image 块 **1024** 记 | DeepSeek 官方「每图 token 上限 1024」；治 K3 `_ => 0` |
| **D9** | `ys_core::ToolResult` 增 `media: Vec<MediaAttachment>`（`#[serde(default, skip_serializing_if)]`） | 让工具能交出图片，同时**不改 `ContentBlock::ToolResult` 字节** |

## 3. 类型改动（逐 crate）

### 3.1 `ys-core`（纯数据，零 IO、零 tokio）

```rust
// crates/ys-core/src/message.rs —— 新增变体（外部标签，旧变体字节不变）
#[non_exhaustive]
pub enum ContentBlock {
    Text { text: String },
    ToolUse { id: ToolCallId, name: String, arguments: serde_json::Value },
    ToolResult { tool_call_id: ToolCallId, content: String, is_error: bool },
    /// 图片引用。**只存路径，不存字节** —— 字节在适配器出站时现读（D1/D2）。
    Image {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<ImageDetail>,
    },
}

// crates/ys-core/src/media.rs —— 新模块（纯函数，无依赖）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageFormat { Png, Jpeg, Gif, WebP, Bmp, Tiff }

impl ImageFormat {
    /// DeepSeek 声明支持的四种；其余由适配器按 provider 决定接受与否。
    pub fn is_vision_supported(self) -> bool { matches!(self, Self::Png|Self::Jpeg|Self::Gif|Self::WebP) }
    pub fn mime(self) -> &'static str { /* "image/png" … */ }
}

/// 按**内容**判格式（不认扩展名 —— DeepSeek 明确要求按内容判断）。
/// 与 `adapters/tools-basic/src/ocr/mod.rs:537` 的 `dims_from_header` 同源逻辑。
pub fn sniff_image(head: &[u8]) -> Option<(ImageFormat, u32, u32)>;

// crates/ys-core/src/tool.rs —— 工具能交出图片（D9）
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    /// 附件：**只有路径**。`skip_serializing_if` 让既有事件 JSON 逐字节不变。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub media: Vec<MediaAttachment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MediaAttachment {
    Image { path: String, detail: Option<ImageDetail> },
}
```

**为什么 `detail` 在 core 里**：它是**数据**（用户/模型的意图），不是 wire 值。适配器负责把它
翻译成 `low|high|original|auto` 或**丢弃**（provider 不认识时）。

### 3.2 `ys-tool`（接口零改动）

`Tool` trait 签名**不变**（仍返回 `ys_core::ToolResult`）—— 只是这个返回类型多了 `media`。
`ys-tool` 运行时仍零 tokio（新变体是纯数据）。

### 3.3 `adapters/tools-basic`（新增 `view_image`）

```rust
// 新文件 adapters/tools-basic/src/view_image.rs
// spec: view_image(path: string, detail?: "low"|"high"|"original")
// 行为：解析路径 → 存在性 + 是文件 + 嗅探格式 → 返回
//       ToolResult { content: 一行元信息, media: vec![Image{path, detail}] }
// 硬约束：**不读全文件、不 base64、不缩放** —— 字节是适配器的事（D2）
//       （读取前若干字节做嗅探即可；与 ocr 的 DIM_HEAD_BYTES 同口径）
```

工具描述（对齐 Codex `view_image_spec.rs:44`，但要**说明何时该用本工具、何时该用 ocr**）：

> View a local image file from the filesystem when visual inspection is needed.
> Accepts a filesystem path. Use `ocr` instead when you need text with pixel
> coordinates (layout debugging, locating an element).

**为什么它是工具而不是隐式行为**：模型知道「我现在该看图了」这个信息比 harness 猜得准；
且工具调用在事件流里**可观测**（用户看得到模型在做什么）。

### 3.4 `adapters/model-openai-compatible`（唯一 IO 点）

```rust
// 新文件 adapters/model-openai-compatible/src/media.rs
/// 出站时把 Image 块解析成 data URL。**本仓唯一读图字节的地方。**
pub(crate) fn resolve_image(path: &str, cache: &MediaCache)
    -> Result<Arc<str>, MediaError>;   // Ok(data URL) / Err(带原因)

pub(crate) struct MediaCache { /* Mutex<HashMap<PathBuf, Entry>>，键含 mtime+len */ }
// Entry: Arc<str>（data URL）；命中即零拷贝复用
```

**硬上限**（DeepSeek 口径，超出即拒绝而非缩小）：单图 32 MiB、请求体 48 MiB 预算、
单请求 600 图。超限 → `MediaError::TooLarge` → 走 D5/D6 的占位路径。

### 3.5 `ys-loop`（结构转换，不碰字节）

一处改动：`basic.rs:1409` 把工具结果折成块时，**先**产出 `ToolResult` 文本块，
**再**为每个 `MediaAttachment::Image` 追加一个 `ContentBlock::Image`
（追加进**同一条** `Role::User` 消息）。

### 3.6 `ys-tui-coding`（仅渲染）

`transcript.rs::replay_lines` + `app.rs::message_text` 各加一条 `Image` 分支：
渲染成 `[image: /path (1920×1080)]`（有嗅探结果时带尺寸）。**无 IO，只显示路径字符串。**

## 3.7 图片的持久化：三个方案，以及为什么选混合

用户问到了关键一点：**base64 保留才能复现之前的运行结果**。这个诉求是对得，
但直接存 base64 有三个代价，其中一个与本仓现有实现碰上就是致命的。

### 代价 1（致命）：全量重写 × 体积

`crates/ys-session/src/jsonl.rs:167-170`：`append()` 就是 `messages.push` + `flush_to_file()`，
而 `flush_to_file`（`:112-141`）**每次都把整个会话重写成临时文件再 rename**。
叠加 base64 后的真实数字（本机实测一张 906 KiB 截图）：

| 项 | 数值 |
|---|---|
| 单图 base64 | ≈ **1.18 MiB** |
| 10 张图 | ≈ **11 MiB** |
| 一条消息的 append 开销 | 重写**全部** 11 MiB |
| 一次 1000 条消息的会话 | ≈ **11 GB 磁盘写入** + 1000 次全文件解析 |

这不是理论担忧：它把每回合 O(1) 的 append 变成 O(会话总字节)，且随图片数线性放大。

### 代价 2：context 与存储被混为一谈

压缩（compaction）的目的就是**把旧内容移出 context**。若旧图在 JSONL 里仍以 base64 存在，
那么「压缩后的会话文件」与「压缩前的」尺寸几乎一样 —— 用户看到 `/compact` 成功，
磁盘却一点没小。这与 `docs/design-final/compaction.md` 的预算模型也矛盾。

### 代价 3：base64 是 wire 细节，进 core 就是污染

`ys-core` 是不认识 provider 方言的。把 base64 放进去，等于让每个读 `ContentBlock` 的地方
（TUI 渲染、token 估算、压缩、事件流）都能看见一个几 MB 的字符串 —— 而它们只需要路径。

### 三个候选

| 方案 | 图片字节存哪 | 可复现 | 文件增长 | 结论 |
|---|---|---|---|---|
| **S1 只存路径** | 不存 | ✗ 文件删/改即失真 | 零 | 不满足你的诉求 |
| **S2 base64 进 JSONL** | 消息行内 | ✓ 完全 | 线性且**每次 append 全量重写** | ✗ 代价 1 致命 |
| **S3 旁路快照（选定）** | **内容寻址的独立文件** | ✓ 完全 | 线性但**永不重写主文件** | ✓ 见下 |

### S3：内容寻址的旁路快照

```
ContentBlock::Image {
    id: String,            // 内容指纹：sha256(字节) 的前 16 hex
    path: String,          // 人类可读的原始路径（失效时仍能告知用户）
    detail: Option<ImageDetail>,
}
```

字节落点：`<sessions_dir>/media/<id>.<ext>`

- **只在首次入历史时写一次**（`tokio::fs::write`，原子 rename）。
- 主 JSONL 只多一个 16 字节十六进制串 —— **append 开销仍然 O(1)**，代价 1 消失。
- 出站时：**先看 id 旁路文件是否存在**，存在则直接用（快、且与原始文件脱钩）；
  不存在则回落到 `path` 现读（兼容没快照的老会话 / 手工构造的请求）。
- `path` 失效但 `id` 命中 ⇒ **仍能复现**（这正是你要的能力，且比 S2 更强：
  原图被改后 S2 给的是旧内容，S3 也是*旧内容*但**能被识别出来**——id 不匹配就开新 id）。
- 清理：删会话时删它的 `media/` 目录；未引用文件由启动期 GC（按引用计数扫）。

### 与 Codex 的对比

Codex 把 base64 **留在历史里**（`snapshot_local_user_input` 原地替换成 `UserInput::Image`）。
它付得起这个代价，因为：(a) 它不重写整个会话文件；(b) 它的 32 MiB 传输上限是
**WebSocket 单消息**限制，不是文件限制。
**本仓两个前提都不同** ⇒ 不照抄它的存储形态，只照抄它的**行为契约**（读者不该
因为图而失败、失败要可见）。

### 这也修了压缩语义（代价 2）

压缩丢弃 Image 块 ⇒ 主 JSONL 里那行本来就没了；旁路文件成为**无引用** ⇒ 启动 GC 回收。
**磁盘与 context 同步收缩**，除非用户从 `/resume` 回滚到更早的会话（那时引用仍在）。

## 4. 数据流

**生产者 A：用户给路径（首轮）**

```
用户输入 "看看这张图 /tmp/a.png"          ← 文本里就是路径，TUI 不做任何特殊处理
   ↓  Message { User, [Text] }             ← 路径留在文本里，模型自己决定要不要调 view_image
   ↓  模型调 view_image("/tmp/a.png")
   ↓  ToolResult { content, media: [Image{path}] }
   ↓  ys-loop 折叠 → Message { User, [ToolResult, Image] }
适配器出站（唯一 IO）→ 读盘 → 嗅探 → base64 → data URL → content 块数组
```

> **注意**：**用户输入侧不解析路径**。`看看这张图 /tmp/a.png` 里的路径不自动变成
> `Image` 块 —— 否则要把「文本里的哪个子串是图片」这个歧义问题引入解析层（也与
> `expand_skill_invocation` 会重建 Message 的既有行为冲突）。让**模型**调
> `view_image` 拿图，路径歧义归模型处理，且调用过程可观测。
> 这是与 Codex 唯一的实质差异：它有 `UserInput::LocalImage`（TUI 明确 attach），
> 本仓 TUI 不做 attach（用户已裁决），所以**只有工具这一条生产者**。

**生产者 B：模型主动看图（后续任意轮）**

同上，只是路径来自模型参数而非用户文本。

**出站细节（唯一 IO 点）**

```
ContentBlock::Image{id, path, detail}
   ↓ compat 门控（D4）
   ├─ 不支持 → Text { "[image omitted: model has no image input] /path" }   ← D5
   └─ 支持
       ↓ 先查 sideline: <sessions>/media/<id>.<ext>          ← D7 快照优先
       ├─ 存在 → 直接用（与原文件已脱钩）
       └─ 不存在 → 回落到 path 现读
           ↓ 失败 ⇒ Text 占位（含原因）                        ← D6
       ↓ sniff_image 校验 → 非图片/不支持格式 ⇒ Text 占位
       ↓ 体积超限 ⇒ Text 占位
       ↓ MediaCache 查 (id 或 path+mtime+len) → 命中则零重编码    ← D7
       ↓ base64 → data:{mime};base64,·
       ↓ ChatMessage { role: "user", content: [ {type:"text"}, {type:"image_url"} ] }
```

## 5. 失败与降级矩阵（全部**不静默**）

| 情形 | 行为 | 用户/模型看到的 |
|---|---|---|
| provider / 模型不支持图像 | 替换文本占位 | `[image omitted: model has no image input] /path` |
| 文件不存在 / 不可读 | 替换文本占位 | `[image unreadable: /path (No such file …)]` |
| 不是图片 / 格式不支持 | 替换文本占位 | `[image unsupported: /path (detected: bmp)]` |
| 超过体积上限 | 替换文本占位 | `[image too large: /path (35.1 MiB > 32 MiB)]` |
| 上层**没有**解析器（无适配器支持） | 编译期不可达（`ContentBlock` 有 `#[non_exhaustive]`，但适配器必须显式处理 —— 见 §8 变异测试） | — |

**关键点**：占位不是空串、不是丢块 —— 模型能看见「这里本来有一张图，为什么没有」，
从而能调整策略（改用 `ocr` 或换模型）。这正是 Codex 的做法。

## 6. token 估算与压缩

- `crates/ys-loop/src/token.rs::estimate_tokens` 增一臂：
  `ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE`（**1024**，常量旁写 DeepSeek 口径）。
- 由此 `basic.rs:625` 的压缩触发**会**因图而提前 —— 治 K3。
- 压缩：`compact.rs::merge_summary_into` 已是 `_` 分支插 Text 在前（安全）；
  摘要文本**不包含图片内容**（`summarize.rs` 只拼 Text），这是可接受的 ——
  但**被摘要掉的图片**必须显式决定去留：**随消息一起丢弃**（路径仍在 JSONL 历史里，
  但不再上送）。理由：保留图片会突破上下文预算，且「最近 N 张」的 N 无处可推断。

## 7. 会话格式与兼容

- **旧变体字节不变**：`Image` 是新增外部标签 `{"Image":{"path":…}}`。
  既有 golden 基线（`apps/coding-agent/tests/golden/json_no_debug_baseline.jsonl:1`）不受影响。
- **老版本读新会话**：`jsonl.rs:91-102` 会整行跳过 ⇒ 该条消息消失。
  缓解：写入首个 `Image` 块时把 `SessionMeta.v` 升为 **2**（`meta.rs:34-36` 已有该字段，
  注释称「廉价前瞻」）。**本次不改老版本的读取逻辑**（改不了），但新版本可据 `v` 提示
  「此会话含旧版本不认识的图像块」。
- **`ModelRequest` 序列化字节**：`ContentBlock` 变体不影响 `ModelRequest` 的
  `serde(skip)` 字段契约（`crates/ys-model/src/request.rs:34,52`）。

## 8. 改动清单（逐文件）

| crate / 文件 | 改动 | 风险 |
|---|---|---|
| `crates/ys-core/src/message.rs` | +`Image { id, path, detail }` 变体 | 新变体，旧字节不变 |
| `crates/ys-core/src/media.rs`（新） | `ImageFormat`、`sniff_image`、`ImageDetail` | 纯函数，零依赖 |
| `crates/ys-core/src/tool.rs` | `ToolResult` +`media` | ⚠️ **所有 `ToolResult{..}` 字面构造会编译失败** —— 这是**好事**（穷尽结构体字面量，编译器当保镳）。加 `ToolResult::text()` 辅助降噪 |
| `crates/ys-tool/src/lib.rs` | 若需 `ToolResult::text` 便利构造 | — |
| `adapters/tools-basic/src/view_image.rs`（新） | 工具实现 + spec | 新文件 |
| `adapters/tools-basic/src/lib.rs` | 注册工具 | — |
| `adapters/model-openai-compatible/src/media.rs`（新） | 读盘 / 嗅探 / base64 / 缓存 / 上限 | **唯一 IO 点** |
| `adapters/model-openai-compatible/src/request.rs` | `ChatMessage.content: Option<Content>`（`Content = String \| Vec<ContentBlock>`）；`#[serde(untagged)]` ⇒ **纯文本时线上字节不变** | ⚠️ K1，最需要 golden 测试 |
| `adapters/model-openai-compatible/src/compat.rs` | `supports_image_input` + `supports_image(model)` + `image_detail_for(model, detail)` | 照搬 effort 骨架 |
| `adapters/model-openai-compatible/src/lib.rs::translate_request` | 替换 `_ => {}`（`:1311`）为**显式 Image 分支** | ⚠️ 治 K2 |
| `adapters/model-openai-compatible/src/lib.rs::final_text`（`:1673`） | 响应侧不应有 Image；显式忽略并记日志 | — |
| `crates/ys-loop/src/basic.rs:1409` | 工具结果折叠时追加 Image 块 | — |
| `crates/ys-loop/src/token.rs` | Image 臂 1024 | ⚠️ 治 K3 |
| `crates/ys-loop/src/compact.rs` | 确认 `merge_summary_into` / `contains_tool_result` 语义 | — |
| `crates/ys-tui-coding/src/transcript.rs:155` + `app.rs:905` | 渲染 `[image: path]` | ⚠️ 治 B 档 |
| `apps/coding-agent/src/subagent.rs:1819` | `[unknown content block]` → 明确渲染 | ⚠️ 治 B 档 |
| `apps/coding-agent/src/main.rs:676` `message_bytes` | Image 计入（路径长度即可） | — |
| `apps/coding-agent/src/app_loop.rs:1821` | ⚠️ `expand_skill_invocation` **重建 Message 会丢 Image 块** —— 必须保留非 Text 块 | **A 档，静默丢数据** |
| `crates/ys-session/src/meta.rs` | v→2（首个 Image 块出现时） | — |
| `crates/ys-session/src/media_store.rs`（新） | 旁路快照：写、查、GC（内容寻址） | 新文件；主 JSONL 写入路径**不动** |

## 9. 测试矩阵与变异点

**必须有的形态测试**

| 测试 | 位置 | 变异（去掉即应变红） |
|---|---|---|
| 纯文本请求的**线上字节不变** | 适配器 golden | 把 `Content` 改成总是数组 ⇒ 红 |
| `ToolResult` 无 media 时事件 JSON 不变 | `ys-core` 序列化 | 去掉 `skip_serializing_if` ⇒ 红 |
| Image 块 → `image_url` 块数组 + 正确 mime | 适配器 | 魔数判断换扩展名 ⇒ 红（造一个 `x.png` 实为 JPEG 的夹具） |
| 不支持图像 ⇒ 文本占位（**不是丢块**） | 适配器 | 恢复 `_ => {}` ⇒ 红（K2 变异） |
| 路径不存在 ⇒ 占位含原因、回合不失败 | 适配器 | 改成 `Err` 直接失败 ⇒ 红 |
| token 估算含图片 | `token.rs` | 恢复 `_ => 0` ⇒ 红（K3 变异） |
| view_image 产出 media，且**不读全文件** | `tools-basic` | 让工具自己 base64 ⇒ 断言失败（体积巨大） |
| TUI 渲染 Image 块 | `transcript.rs` | 恢复白名单 ⇒ 红（`transcript.rs:538` 已有同型用例） |
| skill 展开保留 Image 块 | `app_loop` | 恢复 `vec![Text]` ⇒ 红（A 档变异） |
| 老会话（无 Image）可正常回放 | `ys-session` | — |
| **单图 append 不重写全文件**：1000 条消息 + 10 张图的会话，一次 append 写入字节数 < 2 KiB | `ys-session` | 把快照改成内联 base64 ⇒ 红（**代价 1 的护栏**） |
| **原图删除后仍能回放**：写快照 → 删原图 → resolve 仍得 data URL | 适配器 + session | 去掉快照优先查找 ⇒ 红（**可复现性的护栏**） |
| **原图改内容 ⇒ 新 id**：改字节后重建块，id 必须不同 | `ys-core` / session | 只用路径当键 ⇒ 红 |

**变异测试**：新增 `scripts/vision-mutation.py`，照 `scripts/cancellation-mutation.py` 的形态，
注入 ≥8 处 bug（含 K2/K3/A 档三处静默点）。

## 10. 已知边界（写进 as-built）

1. **路径是引用不是快照 —— 但字节有快照（S3）**：旁路文件一旦写出，原图改名/删除
   **不影响回放**；原图被**改内容**则 id 不匹配 ⇒ 开新 id、旧快照仍在（能被识别为两张图）。
   剩下真正的边界：快照文件被手工删掉后，回落路径又已失效 ⇒ 占位报错。
2. **旁路目录体积会线性增长**：每张入过历史的图永久占一份。启动 GC 只能回收
   无引用的；**有引用的旧图不会被自动清**。需一条用户可见的手动清理（`/cleanup` 或
   安装脚本），本次只写进 as-built 边界。
3. **不做本地缩放**：超 32 MiB 的图直接拒绝。大截图（如 4K Retina 全屏）可能触线，
   用户需自己压。Codex 有 `image` crate 做 2048 缩放，本仓不引。
4. **不做发送去重**：同一张图在多个回合里各占 1024 token 预算。
   Codex 同样没有（issue #28316 即此问题）。
5. **TUI 不能粘贴图片**：用户已裁决；`view_image` 需要路径，截图用户需先存盘。
6. **模型级门控靠 id 识别**：`/v1/models` 无能力标记，只能按 id 判（如 `deepseek-flash`
   含 `flash`）。**具体判据待实现时按实测收紧**；无法识别 ⇒ fail-closed（不发图，走占位）。

## 11. 实施顺序

1. **纯数据先行**：`ys-core::media`（嗅探 + 单测，零 IO）—— 可独立 TDD，无依赖。
2. `ContentBlock::Image` + `ToolResult.media`（含 `ToolResult::text()`，修全部构造点）。
3. `model-openai-compatible::media`（读盘/编码/缓存/上限）+ `Content` 枚举 + golden 测试。
4. 门控 + 降级占位（含变异测试 K2）。
5. `view_image` 工具 + 注册。
6. `ys-loop` 折叠 + token 估算（含变异测试 K3）。
7. TUI / subagent / message_bytes 渲染（B 档）。
8. `expand_skill_invocation` 保留非 Text 块（A 档）。
9. `SessionMeta.v = 2`。
