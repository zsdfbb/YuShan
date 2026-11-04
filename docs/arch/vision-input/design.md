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
| **D1** | `ContentBlock::Image { path, detail }` —— **只带路径，字节不进会话**；出站时现读并 base64 内联进**请求**（见 §3.7） | 会话体积与文本可读性；base64 只存在于 wire 形态 |
| **D2** | **唯一的读盘 + base64 点：适配器出站** | 依赖方向：只有适配器认识 wire 格式；像素字节不出适配器 |
| **D3** | 格式嗅探是 `ys-core` 里的**纯函数**（`&[u8] → Option<ImageFormat>`） | 纯数据无 IO；`ocr::image_dims` 的同构实现，宿主零依赖 |
| **D4** | 能力门控**两层**：`supports_image_input`（provider）+ `supports_image(model)`（模型） | 照搬 `effort` 的既有骨架（`docs/design-final/effort.md` §3，ADR-0042） |
| **D5** | 不支持图像时**替换为文本占位**，不是丢弃 | 照抄 Codex `strip_images_when_unsupported`（`core/src/context_manager/normalize.rs:346`）；治本仓 K2 `_ => {}` |
| **D6** | 路径读不到 ⇒ **文本占位 + 继续回合**（不 fatal） | 照抄 Codex `local_media_error_placeholder`（`protocol/src/models.rs:1814`）；回合不该因一张图而死 |
| **D7** | 适配器内按 `(path, mtime, len)` **缓存 base64 结果**，128 MiB / 64 条 LRU 上界，锁外读盘 | 否则每回合重读重编码（Codex 靠 provider 侧 file_id 缓解，本仓没有；见 issue #28316） |
| **D8** | token 估算：每个 Image 块 **1024** 记（`token.rs` 与 `turn_budget.rs` **两个**估算器都要） | DeepSeek 官方「每图 token 上限 1024」；治 K3 `_ => 0` |
| **D9** | `ys_core::ToolResult` 增 `media: Vec<MediaAttachment>`（`#[serde(default, skip_serializing_if)]`） | 让工具能交出图片，同时**不改 `ContentBlock::ToolResult` 字节** |
| **D10** | K2 的修复是**两处**：`:1311` 的 `_` 臂 **+** `:1316` 的推送条件 | 实测：只改 `:1311` 时，仅含 Image 的消息仍会整条不出场（`"messages":[]`） |
| **D11** | 占位**只生成在出站临时副本**，绝不写回 `ContentBlock` | 写回则永久固化，原图补齐后再也不会重试送图 |
| **D12** | `ContentBlock` 的变体穷尽性靠**测试**守，不靠编译器 | `#[non_exhaustive]` 反而保证有 `_` 臂 ⇒ 加变体零编译错误（实测） |

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

/// 用户/模型对图片精度的**意图**。它是数据而非 wire 值：适配器负责翻译成
/// `low|high|original|auto`，provider 不认识时**丢弃**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageDetail { Low, High, Original, Auto }

impl ImageFormat {
    /// DeepSeek 声明支持的四种；其余由适配器按 provider 决定接受与否。
    pub fn is_vision_supported(self) -> bool { matches!(self, Self::Png|Self::Jpeg|Self::Gif|Self::WebP) }
    pub fn mime(self) -> &'static str { /* "image/png" … */ }
}

/// 按**内容**判格式（不认扩展名 —— DeepSeek 明确要求按内容判断）。
/// 与 `adapters/tools-basic/src/ocr/mod.rs:527` 的 `dims_from_header` 同源逻辑。
pub fn sniff_image(head: &[u8]) -> Option<(ImageFormat, u32, u32)>;

// crates/ys-core/src/tool.rs —— 工具能交出图片（D9）
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    /// 附件：**只有路径**。`skip_serializing_if` 让既有事件 JSON 逐字节不变。
    ///
    /// **不变量：`is_error` 为真时本字段恒为空。** 折叠点（`ys-loop/basic.rs`）为每个附件
    /// 追加一个 `ContentBlock::Image` 而不看 `is_error`，若二者共存，模型会同时收到
    /// ⌈失败了⌋与那张图。工具层在失败时只报事实，不给附件。
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

### 3.1b `ToolResult` 加 `media` 的迁移成本

`ToolResult` 是**结构体字面量**（不是 `#[non_exhaustive]` 枚举），加字段 ⇒ 每个构造点
**必然编译失败**。实测当前共 **168 处**：生产 **77 处**、测试 **91 处**。

这正是本设计想要的守护 —— 与 `ContentBlock` 的情形**恰好相反**（见 §5 与 §8 的对比）：
结构体字面量是穷尽的，编译器会逐个点名；`#[non_exhaustive]` 枚举反而保证每个 match
都有 `_` 臂、新变体被静默吞掉。

配套 `ToolResult::text(content, is_error)` 便利构造（`media` 取空 `Vec`）以降低噪声，
但**不**用它掩盖漏改：77 处生产点应一次性改完，与加字段同批提交。

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

### 锁边界（承重，不得挪）

**读盘与 base64 全部在锁外，锁内只做查表与插入。**

```
1. 锁内：查 (path, mtime, len) → 命中则 Arc::clone 返回，立即释放锁
2. 锁外：读盘 → 嗅探 → base64 编码      ← 可能几毫秒到几百毫秒
3. 锁内：插入（或发现前人已插入同键 ⇒ 丢弃自己那份，用已有的）
```

三条理由：

- **锁内不做 IO**：一次 32 MiB 读盘 + 编码在锁内会串行化所有并发看图（子 agent 并发时叠加）。
- **`std::sync::Mutex` 就够**：锁内无 `.await`，故不需 `tokio::sync::Mutex`，也不存在
  guard 跨 await 的 `Send` 问题。
- **第 3 步的重复插入是正常的**：两个并发请求可能同时未命中并各自算一遍，这是可接受的
  浪费（结果相同），换来不用在锁内做重活。

**base64 编码不 `spawn_blocking`**：单次最大 32 MiB，编码为纯 CPU 且无阻塞点；
本仓并发度低（单 TUI + 有限子 agent），换取少一个运行时依赖。若后续出现子 agent 大批
看图，再改为 `spawn_blocking` —— 届时只需改 `media.rs` 内部。

### 缓存上界

缓存键含 `mtime` + `len` ⇒ 同一路径被改写会**新增**条目而非替换，陈旧条目必须淘汰：

- 总字节上限 **128 MiB**（base64 后约为原图的 1.37 倍，即约 93 MiB 的原图存量）
- 条目数上限 **64**
- 超出任一上限 ⇒ 按最近最少使用（LRU）淘汰；条目自带 `last_used: Instant`

不设上界的后果：32 MiB 的 4K 截图 base64 后约 **41 MiB**，几张就能吃掉数百 MB。

### 3.5 `ys-loop`（结构转换，不碰字节）

一处改动：`basic.rs:1410` 把工具结果折成块时，**先**产出 `ToolResult` 文本块，
**再**为每个 `MediaAttachment::Image` 追加一个 `ContentBlock::Image`
（追加进**同一条** `Role::User` 消息）。

### 3.6 `ys-tui-coding`（仅渲染）

`transcript.rs:143` 的 `replay_lines`（`match` 在 `:157`，通配臂 `:163`）与
`app.rs:905` 的 `message_text` 各加一条 `Image` 分支：
渲染成 `[image: /path (1920×1080)]`（有嗅探结果时带尺寸）。**无 IO，只显示路径字符串。**

### 3.7 图片的持久化：只存路径

`ContentBlock::Image` **只存路径**（D1/D2），图片字节不随消息进历史。
字节在**适配器出站时**从磁盘现读、编码成 data URL —— 这是全链路唯一的 IO 点。

会话文件里因此只有一个路径字符串：

```json
{"Image":{"path":"/tmp/a.png","detail":"high"}}
```

**这个选择依赖 B（append-only）已落地**：`JsonlSession::append` 现在是
`O_APPEND` 单次 `write_all`，代价与已落盘条数无关（`docs/design-final/session-append-only.md`）。
若写路径仍是全量重写，内联任何体积可观的内容都会把每次 append 变成 O(会话总字节)。

### 已知代价：路径是引用，不是快照

原图被改名/删除/改写后，**历史里的那张图就复原不出来**。这不是待修缺陷，是本节
明确接受的取舍：

- 出站时读不到 ⇒ **文本占位**（含原因），回合不失败（§5）
- 原图被改写 ⇒ 下次出站送的是**新内容**，历史与线上不再逐字节对应

要「完全可复现」就得把字节留在会话里（base64 内联进消息行）或旁路存一份，
两者都会让会话体积随图片线性增长，且与本仓「文本可读、可 diff」的会话形态冲突。
**不做**。（用户已裁决：走 base64 内联进请求，不落会话。）

> 注意区分两个「base64 内联」：**请求**里是 `data:image/png;base64,…`（wire 形态，
> 适配器构造，瞬时存在）；**会话**里永远是路径。前者是本节要的，后者是本节拒绝的。

### 压缩与磁盘

压缩丢弃 Image 块 ⇒ 该路径从历史消失。因为字节从未落进会话文件，
**磁盘与 context 本来就同步**，不需要 GC、不需要清理命令。

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
ContentBlock::Image{path, detail}
   ↓ compat 门控（D4）
   ├─ 不支持 → Text { "[image omitted: model has no image input] /path" }   ← D5
   └─ 支持
       ↓ 按 path 读盘
           ↓ 失败 ⇒ Text 占位（含原因）                        ← D6
       ↓ sniff_image 按内容校验 → 非图片/不支持格式 ⇒ Text 占位
       ↓ 体积超限 ⇒ Text 占位
       ↓ MediaCache 查 (path + mtime + len) → 命中则零重编码    ← D7
       ↓ base64 → data:{mime};base64,·
       ↓ ChatMessage { role: "user", content: [ {type:"text"}, {type:"image_url"} ] }
```

缓存键是 `(path, mtime, len)`：同一路径内容被改写时键自然变化，不会送回陈旧字节。

### ⚠️ 消息推送条件必须同时改（承重，易漏）

`lib.rs:1316` 现在的推送条件是：

```rust
if !text_content.is_empty() || !tool_calls.is_empty() {
    messages.push(ChatMessage { .. });
}
```

**这就是 K2 的真正危害层**：`_ => {}`（`:1311`）只是把一个块丢掉；而一条**只有 Image 块**
的消息，因 `text_content` 与 `tool_calls` 都为空，**整条消息根本不 push**。

实测（向 `translate_request` 注入一条仅含 `Image` 块的消息）：

```
出站 JSON = {"model":"test","messages":[],"stream":true,...}
```

`"messages":[]` —— 消息人间蒸发。因此本设计的适配器改动是**两处**，不是一处：

1. `:1311` 的通配臂 → 显式 `Image` 分支（累积到新的 `image_parts: Vec<_>`）
2. `:1316` 的推送条件 → 加上 `|| !image_parts.is_empty()`

只做第 1 步、不改第 2 步 ⇒ 图片仍然消失，而且**看起来像修过了**。§9 的「消息数守恒」
用例专盯这一点。

## 5. 失败与降级矩阵（全部**不静默**）

| 情形 | 行为 | 用户/模型看到的 |
|---|---|---|
| provider / 模型不支持图像 | 替换文本占位 | `[image omitted: model has no image input] /path` |
| 文件不存在 / 不可读 | 替换文本占位 | `[image unreadable: /path (No such file …)]` |
| 不是图片 / 格式不支持 | 替换文本占位 | `[image unsupported: /path (detected: bmp)]` |
| 超过体积上限 | 替换文本占位 | `[image too large: /path (35.1 MiB > 32 MiB)]` |

**关键点**：占位不是空串、不是丢块 —— 模型能看见「这里本来有一张图，为什么没有」，
从而能调整策略（改用 `ocr` 或换模型）。这正是 Codex 的做法。

### ⚠️ 占位**不得写回** `ContentBlock`（承重）

占位只生成在**出站时的临时副本**上，`ContentBlock::Image` 本身保持原样。

若把占位文本替换进消息体并落盘，则永久固化：用户把图补回去、恢复文件后，
历史里已经是死文本，**永远不再重试**；原图被改写时更是永远送旧的占位。

判定方式（也即 §9 的测试）：

- 原图删掉 ⇒ 出站降级为占位，**但会话 JSONL 里仍是 `{"Image":{"path":…}}`**
- 把图放回同一路径 ⇒ **下一回合能正常送图**（占位不作数）

### ⚠️ `#[non_exhaustive]` 不是编译期守护（实测证伪）

`#[non_exhaustive]` 的语义方向**正好相反**：它禁止跨 crate 穷尽匹配，
即**保证**每个 `match` 都写 `_` 臂 —— 于是新变体恰好被 `_` 臂静默吞掉。它从来不提供
「加变体时提醒你更新消费点」的机制。

实测：给 `ContentBlock` 加 `Image` 变体后，`check --workspace --all-targets` **零 error**，
35 个文件 / 184 处引用、39 个测试二进制全部照过。

对比两者，这是本设计最重要的形态区别：

| | 加变体/字段后 | 守护力 |
|---|---|---|
| `ContentBlock`（`#[non_exhaustive]` enum） | 零 error，静默吞掉 | ❌ **无** |
| `ToolResult`（普通 struct 字面量） | 77 处生产构造点逐个点名报错 | ✅ **有** |

所以：**`ContentBlock` 的穷尽性必须靠测试守，不能靠编译器。** 具体机制见 §9 的
「块集合白名单」与「消息数守恒」两条用例。

## 6. token 估算与压缩

**两个估算器都要改** —— 它们互相独立：

- `crates/ys-loop/src/token.rs:12` 的 `_ => 0`（`estimate_tokens`）⇒ 增
  `ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE`（**1024**，常量旁写 DeepSeek 口径）。
  这是 `basic.rs:626` 压缩触发的依据 ⇒ 治 K3。
- `crates/ys-loop/src/turn_budget.rs:38` 的 `_ => 0` ⇒ 同口径补上，否则回合预算低估图片。

- 压缩：`compact.rs::merge_summary_into` 已是 `_` 分支插 Text 在前（安全）；
  摘要文本**不包含图片内容**（`summarize.rs` 只拼 Text），这是可接受的 ——
  但**被摘要掉的图片**必须显式决定去留：**随消息一起丢弃**（路径仍在 JSONL 历史里，
  但不再上送）。理由：保留图片会突破上下文预算，且「最近 N 张」的 N 无处可推断。

## 7. 会话格式与兼容

- **旧变体字节不变**：`Image` 是新增外部标签 `{"Image":{"path":…}}`。
  既有 golden 基线（`apps/coding-agent/tests/golden/json_no_debug_baseline.jsonl:1`）不受影响。
- **老版本读新会话**：`jsonl.rs:170` 的 `from_str::<Message>` 失败 ⇒ 落到 `SessionMeta`
  分支 ⇒ 再失败则 `skipped_lines += 1` 跳过（`:179`）⇒ 该条消息消失。
  缓解：写入首个 `Image` 块时把 `SessionMeta.v` 升为 **2**（`meta.rs:36` 已有该字段，
  注释称「廉价前瞻」）。**本次不改老版本的读取逻辑**（改不了），但新版本可据 `v` 提示
  「此会话含旧版本不认识的图像块」。
- **`ModelRequest` 与线格式无关（已无 serde 派生）**：`ModelRequest` 不派生
  `Serialize`/`Deserialize`（ADR-0071）。`ContentBlock` 的变体只影响**会话 JSONL**
  与适配器内部的 `ChatCompletionRequest`，不影响请求体字节契约。

## 8. 改动清单（逐文件）

| crate / 文件 | 改动 | 风险 |
|---|---|---|
| `crates/ys-core/src/media.rs`（新） | `ImageFormat`、`ImageDetail`、`sniff_image`、`mime()` | 纯函数，零依赖 |
| `crates/ys-core/src/message.rs` | +`Image { path, detail }` 变体（**无 `id`**） | 新变体，旧字节不变 |
| `crates/ys-core/src/tool.rs` | `ToolResult` +`media` + `ToolResult::text()` | ⚠️ **168 处构造点（生产 77）全部编译失败** —— 这是**好事**，逐个点名 |
| `adapters/tools-basic/src/view_image.rs`（新） | 工具实现 + spec | 新文件 |
| `adapters/tools-basic/src/lib.rs` | 注册工具 | — |
| `adapters/model-openai-compatible/src/media.rs`（新） | 读盘 / 嗅探 / base64 / 缓存（锁外 IO，LRU 上限） | **唯一 IO 点** |
| `adapters/model-openai-compatible/src/request.rs:54` | `ChatMessage.content: Option<Content>`（`Content = String \| Vec<ContentPart>`）+ `ContentPart` | ⚠️ K1，最需要 golden 测试 |
| `adapters/model-openai-compatible/src/compat.rs` | `supports_image_input` + `supports_image(model)` + `image_detail_for(model, detail)` | 照搬 effort 骨架 |
| `adapters/model-openai-compatible/src/lib.rs:1311` | 替换 `_ => {}` 为**显式 Image 分支**（累积 `image_parts`） | ⚠️ 治 K2 的**一半** |
| **`adapters/model-openai-compatible/src/lib.rs:1316`** | ⚠️ **推送条件加 `\|\| !image_parts.is_empty()`** | ⚠️ **K2 的另一半，极易漏** —— 只改 `:1311` 图片仍会整条消失 |
| `adapters/model-openai-compatible/src/lib.rs:1673` | `final_text`：响应侧不应有 Image，显式忽略并记日志 | — |
| `crates/ys-loop/src/basic.rs:1410` | 工具结果折叠时追加 Image 块 | — |
| `crates/ys-loop/src/token.rs:12` | Image 臂 1024 | ⚠️ 治 K3 |
| **`crates/ys-loop/src/turn_budget.rs:38`** | ⚠️ **第二个 token 估算器，同口径补 Image 臂** | 易漏 |
| `crates/ys-loop/src/compact.rs` | 确认 `merge_summary_into`（`:301`）/ `contains_tool_result` 语义 | — |
| `crates/ys-tui-coding/src/transcript.rs:157`（`match`）+ `:163`（通配臂） | 渲染 `[image: path]` | ⚠️ 治 B 档 |
| `crates/ys-tui-coding/src/app.rs:905` | `message_text` 加 Image 分支 | ⚠️ 治 B 档 |
| `apps/coding-agent/src/subagent.rs:1824` | `[unknown content block]` → 明确渲染 | ⚠️ 治 B 档 |
| `apps/coding-agent/src/main.rs:673` `message_bytes` | Image 计入（路径长度即可） | — |
| `apps/coding-agent/src/app_loop.rs:1821` | ⚠️ `expand_skill_invocation` **重建 Message 会丢 Image 块** —— 必须保留非 Text 块 | **A 档，静默丢数据** |
| `crates/ys-session/src/meta.rs:36` | v→2（首个 Image 块出现时） | — |

> **为何本表必须完整**：`ContentBlock` 是 `#[non_exhaustive]`，加变体**不会**产生编译错误
> （见 §5 实测）。本表就是「哪些消费点需要人工更新」的唯一清单；漏一行就是一处静默丢失。

## 9. 测试矩阵与变异点

**必须有的形态测试**

| 测试 | 位置 | 变异（去掉即应变红） |
|---|---|---|
| 纯文本请求的**线上字节不变** | 适配器 golden | 把 `Content` 改成总是数组 ⇒ 红 |
| **消息数守恒**：入站 N 条非空消息（含仅 Image 的）⇒ 出站消息数 ≥ 入站非空数 | 适配器 | 只改 `:1311` 不改 `:1316` 推送条件 ⇒ 红（**K2 的另一半**） |
| **块集合白名单**：断言当前已处理的 `ContentBlock` 变体集合 | `ys-tui-coding`（照 `transcript.rs:538` 的 `p2_3` 形态）、适配器各一份 | 加变体不进白名单 ⇒ 红（**替代 `#[non_exhaustive]` 守护**） |
| `ToolResult` 无 media 时事件 JSON 不变 | `ys-core` 序列化 | 去掉 `skip_serializing_if` ⇒ 红 |
| Image 块 → `image_url` 块数组 + 正确 mime | 适配器 | 魔数判断换扩展名 ⇒ 红（造一个 `x.png` 实为 JPEG 的夹具） |
| 不支持图像 ⇒ 文本占位（**不是丢块**） | 适配器 | 恢复 `_ => {}` ⇒ 红（K2 变异） |
| 路径不存在 ⇒ 占位含原因、回合不失败 | 适配器 | 改成 `Err` 直接失败 ⇒ 红 |
| **占位不写回会话**：原图删掉 ⇒ 出站占位，但 JSONL 仍是 `{"Image":{"path":…}}`；把图放回 ⇒ 下一回合能送图 | 适配器 + `ys-session` | 把占位写回 `ContentBlock` ⇒ 红 |
| token 估算含图片 | `token.rs` | 恢复 `_ => 0` ⇒ 红（K3 变异） |
| **`turn_budget` 估算含图片** | `turn_budget.rs` | 恢复 `_ => 0` ⇒ 红 |
| view_image 产出 media，且**不读全文件** | `tools-basic` | 让工具自己 base64 ⇒ 断言失败（体积巨大） |
| TUI 渲染 Image 块 | `transcript.rs` | 恢复白名单 ⇒ 红（`transcript.rs:538` 已有同型用例） |
| skill 展开保留 Image 块 | `app_loop` | 恢复 `vec![Text]` ⇒ 红（A 档变异） |
| 老会话（无 Image）可正常回放 | `ys-session` | — |
| **图片字节不进会话文件**：含 Image 块的会话落盘后，文件里出现的只有路径字符串，且全文不含 `base64` | `ys-session` | 把 `Image.path` 改成 `Image.data`（内联字节）⇒ 红（**B 与 C 的接缝护栏**） |
| **出站时才读盘**：Image 块入历史后删原图，构造请求仍不报错，降级为占位 | 适配器 | 在入历史时就读盘并缓存字节 ⇒ 红（会变成入历史即失败） |
| **原图改内容 ⇒ 送出字节跟着变**：缓存键含 mtime/len，改写后不再命中旧缓存 | 适配器 | 缓存键只用 path ⇒ 红 |
| **缓存有上界**：灌入超 128 MiB 的条目后，总字节不超限 | 适配器 | 去掉 LRU 淘汰 ⇒ 红 |

**变异测试**：新增 `scripts/vision-mutation.py`，照 `scripts/cancellation-mutation.py` 的形态，
注入 ≥8 处 bug（含 K2/K3/A 档三处静默点）。

## 10. 已知边界（写进 as-built）

1. **路径是引用，不是快照**：原图改名/删除后再回放 ⇒ 出站降级为文本占位，回合不失败；
   原图被改写 ⇒ 下次出站送的是**新内容**，历史与线上不再逐字节对应。
   「完全可复现」不在本设计范围内（见 §3.7）。
2. **会话体积不因图片增长**：Image 块只有路径，压缩丢弃它即彻底释放。
   代价是依赖原图仍在其路径上 —— 这是如实写明的边界，不是待修项。
3. **不做本地缩放**：超 32 MiB 的图直接拒绝。大截图（如 4K Retina 全屏）可能触线，
   用户需自己压。Codex 有 `image` crate 做 2048 缩放，本仓不引。
4. **不做发送去重**：同一张图在多个回合里各占 1024 token 预算。
   Codex 同样没有（issue #28316 即此问题）。
5. **TUI 不能粘贴图片**：用户已裁决；`view_image` 需要路径，截图用户需先存盘。
6. **模型级门控靠 id 识别**：`/v1/models` 无能力标记，只能按 id 判（如 `deepseek-flash`
   含 `flash`）。**具体判据待实现时按实测收紧**；无法识别 ⇒ fail-closed（不发图，走占位）。

9. **占位不落盘**：降级生成的占位只存在于出站临时副本，JSONL 里永远是原路径。
   因此原图补齐后下一回合能正常送图 —— 这是「路径是引用」的一个正面副作用。
10. **缓存有上界**（128 MiB / 64 条，LRU）：长会话不会因看图而无限涨内存。

## 11. 实施顺序

1. **纯数据先行**：`ys-core::media`（嗅探 + 单测，零 IO）—— 可独立 TDD，无依赖。
2. `ToolResult.media` + `ToolResult::text()`（改完全部 168 处构造点，与加字段同批）。
3. `ContentBlock::Image`（纯数据变体）。
4. **块集合白名单测试先行**：先写红测试列出当前消费点，再逐个治（替代 `#[non_exhaustive]` 守护）。
5. `model-openai-compatible::media`（锁外读盘/编码/LRU 缓存/上限）+ `Content` 枚举 + golden 测试。
6. 门控 + 降级占位 + **`:1316` 推送条件** + 「消息数守恒」测试（K2 的两半一起做）。
7. `view_image` 工具 + 注册。
8. `ys-loop` 折叠 + 两个 token 估算器（`token.rs` 与 `turn_budget.rs`，含 K3 变异）。
9. TUI / subagent / message_bytes 渲染（B 档）。
10. `expand_skill_invocation` 保留非 Text 块（A 档）。
11. `SessionMeta.v = 2`。
12. `scripts/vision-mutation.py`：注入 ≥8 处 bug，含 K2 的两半、K3、A 档。
