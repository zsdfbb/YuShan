# 图像输入（Vision Input）设计计划

> 日期：2026-11-05
> 上游（权威设计）：`docs/arch/vision-input/design.md`（下文引作「design.md §N」）
> 现状与卡点：`docs/arch/vision-input/context.md`（K0–K7b、已裁决 A1/A2）
> 质量分析：`docs/arch/vision-input/review.md`（R1–R8 已全部吸收进 design.md）
> **与 design.md 的偏离共 13 条**：E1–E4 为核实修正（结论不变，只修数字与清单）；
> C1–C9 为**已裁决**的调整。其中 **C1 推翻 §11 第 11 步、C6 推翻 D12、C7 推翻 D8 的一半、
> C8 修正 §3.4 的限额口径**；C9 是 design.md 未覆盖的新增项（重试循环的序列化次数）。全部集中在 §5。
> 性质：**跨 crate 的增量特性**。`ys-core` 加数据、工具层加一个工具、适配器加一条 IO 通道、
> 三处消费点补渲染。不改任何既有 trait 签名，不引新 crate（只显式声明一条既有依赖）。
> 范围：`crates/ys-core`、`crates/ys-loop`、`crates/ys-session`、`crates/ys-tui-coding`、
> `adapters/tools-basic`、`adapters/model-openai-compatible`、`apps/coding-agent`、`scripts/`。
> 执行计划：`docs/exec-plans/2026-11-05-vision-input.md`（本文的「单元」在那里拆成「任务」）。

## 1. 目标与非目标

### 1.1 目标

| # | 目标 | 可验收形态 |
|---|---|---|
| G1 | 图片以**像素**进入模型上下文，不再先经 OCR 压成文本 | 出站请求体出现 `{"type":"image_url","image_url":{"url":"data:image/png;base64,…"}}` |
| G2 | 只有**工具**这一条生产者：模型调 `view_image(path)` 取图 | 工具返回 `media: [Image{path}]`，`ys-loop` 折叠成 `ContentBlock::Image` |
| G3 | 无图像能力的 provider / 模型上**必有文本占位**，不静默丢弃 | 固定句 `image content omitted because you do not support image input`（**不带路径**）出现在出站消息里 |
| G4 | 路径失效（删除/改名/不可读）**不打断回合**，原因可见 | 占位串含 `No such file`；回合 `StopReason` 不变 |
| G5 | `ocr` 工具**保持不变**，与 `view_image` 并存 | `ocr` 的全部用例零改动通过；两个工具都在父表里 |
| G6 | 像素字节**永不跨 crate 边界**，会话里只有路径 | 会话文件全文不含 `base64`；`Image` 块落盘形如 `{"Image":{"path":…}}` |

### 1.2 非目标（明确不做，见 design.md §1「非目标」、§10）

- TUI 剪贴板粘贴 / 拖拽 / 缩略图 —— TUI 不接受图片输入（用户已裁决）。
- base64 进会话、Files API（`file_id`）、远程 URL 图片。
- 本地缩放 / 裁剪 / 转码（不引 `image` crate）⇒ 超限图片**拒绝并说明**，不静默缩。
- 视频 / 音频 / 图片生成。
- 用户输入侧的路径解析：`看看这张图 /tmp/a.png` 里的路径**不**自动变成 `Image` 块（design.md §4）。
- **`SessionMeta.v → 2`**：design.md §7/§8/§11 第 11 步的这条**不做**，`v` 保持 1（理由见 §5 C1）。
- 工具文案不声明「唯一通道」：`ocr` 与 `view_image` 并存后，「唯一能看图的通道」变成假话（见 §5 C2）。
- 发送去重（同一张图多回合各占 1024 token 预算，design.md §10 第 4 条）。
- 老版本读新会话的兼容逻辑（改不了老二进制，design.md §7）。

### 1.3 与既有能力的分工

| | `ocr`（ADR-0068，不动） | `view_image`（本设计新增） |
|---|---|---|
| 输出 | 文本 + 像素坐标 | 像素本身 |
| 适用 | 版面调试、定位元素 | 看懂截图、图表、UI 缺陷 |
| 成本 | 约 0.3–0.8 K token/图（行级渲染已压到约 1/7） | 至多 1024 token/图 |
| 落盘 | 结果可落 `<temp>/yushan-ocr/` | 不落任何衍生文件 |

成本量级接近，不是数量级差异（context.md 实测底数）；新增的理由是「无需猜 OCR 歧义、可多轮追问」。

## 2. 现状锚点（实施前的实测基线）

以下每一条都是**当前**事实，实施时必须逐条对得上。反过来说，本设计的正确性也建立在它们之上。

| # | 事实 | 指针 |
|---|---|---|
| A1 | 图像输入**完全未实施** | `ImageDetail` / `sniff_image` / `MediaAttachment` / `supports_image` 在 `crates/ adapters/ apps/` 零命中；`crates/ys-core/src/media.rs`、`adapters/model-openai-compatible/src/media.rs`、`adapters/tools-basic/src/view_image.rs`、`scripts/vision-mutation.py` 均不存在 |
| A2 | `ContentBlock` 是 `#[non_exhaustive]`，只有 `Text` / `ToolUse` / `ToolResult` 三变体 | `crates/ys-core/src/message.rs:24-39` |
| A3 | `ToolResult` 是普通结构体字面量，**无**便利构造 | `crates/ys-core/src/tool.rs:13-17` |
| A4 | 适配器把块数组**扁平化**成单个字符串，对未知变体静默跳过 | `adapters/model-openai-compatible/src/lib.rs:1233` `translate_request`，`:1311` `_ => {}`，`:1316` 推送条件 |
| A5 | `ChatMessage.content` 只接受 `Option<String>`（K1 结构性阻塞） | `adapters/model-openai-compatible/src/request.rs:51-61` |
| A6 | 能力门控的成熟骨架：provider 级能力位 + 模型 id 可识别性 | `adapters/model-openai-compatible/src/compat.rs:166` `supports_effort` / `:186` `effort_for` |
| A7 | **两个**独立的 token 估算器，各有 `_ => 0` | `crates/ys-loop/src/token.rs:12`、`crates/ys-loop/src/turn_budget.rs:38` |
| A8 | 工具结果折叠点 | `crates/ys-loop/src/basic.rs:1410` |
| A9 | TUI 渲染白名单（`match` + 通配臂）与同型守卫用例 | `crates/ys-tui-coding/src/transcript.rs:157` / `:163`，用例 `:538` `p2_3_only_text_and_tool_use_produce_lines` |
| A10 | `message_text` | `crates/ys-tui-coding/src/app.rs:908` |
| A11 | subagent 报告把未知块渲染为占位串 | `apps/coding-agent/src/subagent.rs:1824` |
| A12 | `message_bytes` 只计 `Text` | `apps/coding-agent/src/main.rs:673`（`:679` 是 `_ => 0`） |
| A13 | `expand_skill_invocation` 用 `vec![Text]` **重建** Message（静默丢图） | `apps/coding-agent/src/app_loop.rs:1803`，重建点 `:1821` |
| A14 | 工具注册点 | `apps/coding-agent/src/toolset.rs:107`（`:26` 是 import） |
| A15 | `SessionMeta.v` 字段已存在、缺省 1 | `crates/ys-session/src/meta.rs:36`、`:71` |
| A16 | `set_meta` → `flush_to_file`（全量 tmp+rename）；`append` 走 `O_APPEND` 单次 `write_all`、**不碰 meta**；`flush_to_file` 是 meta 行的唯一写入实现 | `crates/ys-session/src/jsonl.rs:246`、`:260`、`:197-200` 注释 |
| A17 | 格式嗅探的现成实现（PNG/GIF/BMP/WebP/TIFF/JPEG 魔数）可上提到 `ys-core` | `adapters/tools-basic/src/ocr/mod.rs:523` `image_dims`、`:538` `dims_from_header`、`:77` `DIM_HEAD_BYTES` |
| A18 | `ys-core` 零 tokio（`[dependencies]` 只有 serde / serde_json / thiserror） | `crates/ys-core/Cargo.toml` |
| A19 | golden 基线首行是 `{"source":"agent","turn":1,"event":{"UserMessage":{"message":{"role":"User","content":[{"Text":{"text":"hi"}}]}}}}` | `apps/coding-agent/tests/golden/json_no_debug_baseline.jsonl:1` |
| A20 | `base64 0.22.1` 已在锁文件（经 `reqwest`/`hyper-util` 传递），但适配器未直接声明 | `Cargo.lock:44`、`adapters/model-openai-compatible/Cargo.toml` |
| A21 | 变异脚本的模板形态：`Mutant` dataclass + 精确文本替换 + `cargo test -p <pkg>` + 断言指定用例变红 | `scripts/cancellation-mutation.py`（509 行） |
| A22 | `verify.sh` 第 2 阶段的变异脚本清单 | `scripts/verify.sh:55`（`cancellation-mutation wheel-mutation selection-mutation theme-mutation`） |
| A23 | `SessionMeta.v` **只写不读**：全仓只有声明与赋值，唯一读取方是一条测试断言 | `crates/ys-session/src/meta.rs:36` / `:71`；`apps/coding-agent/src/wiring.rs:593` |
| A24 | `ocr` 的 description 与父表 fragment **逐字重复**，且有逐字断言守着 | `adapters/tools-basic/src/ocr/mod.rs:138`、`apps/coding-agent/src/prompt.rs:279`、`:913`；`:273-278` 的前提注释已被本功能推翻 |

## 3. 架构决策摘要（design.md §2 的 D1–D12）

| # | 决策 | 理由 | 在实施中的落点 |
|---|---|---|---|
| **D1** | `ContentBlock::Image { path, detail }` 只带路径；出站现读并 base64 内联进**请求** | 会话体积与文本可读性；base64 只存在于 wire 形态 | U3（`message.rs`）；U5/U6（`media.rs`） |
| **D2** | **唯一的读盘 + base64 点：适配器出站** | 依赖方向：只有适配器认识 wire 格式；像素字节不出适配器 | U5；U7 的 `view_image` **严禁**读全文件 |
| **D3** | 格式嗅探是 `ys-core` 里的**纯函数**（`&[u8] → Option<(ImageFormat, u32, u32)>`） | 纯数据无 IO；`ocr::dims_from_header` 的同构实现 | U1（上提 A17 的逻辑） |
| **D4** | 能力门控**两层**：`supports_image_input`（provider）+ `supports_image(model)`（模型） | 照搬 effort 的既有骨架（A6）；门控只在 `supports_image` 一处判 | U6 |
| **D5** | 不支持图像时**替换为文本占位**，不是丢弃 | 治 K2 `_ => {}` 的语义层 | U6 |
| **D6** | 路径读不到 ⇒ **文本占位 + 继续回合**（不 fatal） | 回合不该因一张图而死 | U6 |
| **D7** | 适配器内按 `(path, mtime, len)` 缓存 base64 结果；128 MiB / 64 条 LRU；**锁外读盘** | 否则每回合重读重编码 | U5 |
| **D8** | token 估算**有意不对称**：`token.rs` 计入（1024/图，治 K3）；`turn_budget.rs` **不计入** | `plan_elisions` 的唯一手段是截断 `ToolResult` 文本，对 Image 块返回 `None` ⇒ 计入只会每回合空转且预算仍超（C7） | U8（两个文件，前者改时后者加注释与反向测试） |
| **D9** | `ys_core::ToolResult` 增 `media: Vec<MediaAttachment>`（`#[serde(default, skip_serializing_if = "Vec::is_empty")]`） | 让工具能交出图片，同时**不改** `ContentBlock::ToolResult` 字节 | U2 |
| **D10** | K2 的修复是**两处**：`:1311` 的显式 Image 臂 **+** `:1316` 的推送条件 | 只改前者时，仅含 Image 的消息仍整条不出场（`"messages":[]`） | U6（**不得拆成两个单元**） |
| **D11** | 占位**只生成在出站临时副本**，绝不写回 `ContentBlock` | 写回则永久固化，原图补齐后再也不会重试送图 | U6 |
| **D12** | **取文本逻辑收口到 `ys-core`**（`ContentBlock::as_text()` 的穷尽 match）；跨 crate 消费点一律调它，不各自写白名单 | `#[non_exhaustive]` **只在跨 crate 时**禁穷尽匹配；定义 crate 内部穷尽匹配合法且**有守护力**（C6 已实证） | U13 + 每处消费点改调 |
| **D13** | `ImageDetail` 的 serde 用 `#[serde(rename_all = "lowercase")]`，`Default = High` | 会话 JSONL 里应是小写；`high` 是「我要看清这张图」的合理缺省 | U1 |
| **D14** | `view_image` 的 `detail` 只接受 **`high` \| `original`**；协议层枚举仍保留 `Low` / `Auto` | `low` 对「我要看清这张图」没有意义 ⇒ 工具层收窄，协议层完整 | U7 |
| **D15** | `ContentBlock::Image.detail` 的 wire 形态是**小写**字符串，`None` 时该键不出现 | 与 `ImageDetail` 的 serde 属性一致；老变体字节不变 | U1 / U3 |
| **D16** | 只统一**「哪些块算文本」**，**不**统一分隔符；分隔符显式枚举为 `TextJoin` | 分隔符是调用方的真实语义差异（界面回显要换行、标题要空格、摘要要无缝） | U13 |
| **D17** | `as_text()` 的 match **永久保持穷尽、无 `_` 臂**；`TextJoin` 不用裸 `&str` | 后者会让分隔符再次各自漂移（正是 C6 要治的病） | U13 |
| **D18** | 单图限额按 **base64 编码后**字节数判（≤ 32 MiB）；编码前先按 `原图 len × 4/3 + 余量` 预估 | 厂商口径「单图 ≤32 MiB（base64/URL）」指的是**编码后**（C8） | U5 |
| **D19** | **请求体 48 MiB 是真正的上限**：`translate_request` 维护累计计数器，超限时**当前及之后**的图降为占位 | 两张 32 MiB 的图就是 64 MiB > 48 MiB，两条限额不加协调就自相矛盾（C8） | U6 |
| **D20** | 请求体**序列化结果塔存**：由调用方在每次 `send` 前给字节，只在 `stream_options` 被剥掉后失效 | 图片使请求体达 MB 级；缓存 data URL 挡不住 `.json()` 的重复序列化（C9） | U14 |

### 3.1 承重数值（写死在实现里，不得静默调整）

| 参数 | 值 | 出处 / 理由 |
|---|---|---|
| 单图体积上限 | **32 MiB，指 base64 编码后** | 厂商口径是「单图 ≤32 MiB（base64/URL）」= 编码后（C8）；按原图判则 24 MiB 原图会实际超限 |
| 请求体预算 | **48 MiB（编码后累计）** | 这是真正的上限：超限时当前及之后的图降为占位（C8） |
| 单请求图片数上限 | **600** | 同上 |
| 缓存总字节上限 | **128 MiB** | base64 后约为原图 1.37 倍 ⇒ 约 93 MiB 原图存量 |
| 缓存条目上限 | **64** | 与上一条任一超出即按 LRU 淘汰 |
| 每图 token 估算 | **1024**，但**仅 `token.rs` 计入** | 治 K3；`turn_budget.rs` 有意不计入（C7 / D8） |
| 嗅探读盘量 | **256 KiB**（`DIM_HEAD_BYTES`） | 与 `ocr` 同口径；TIFF 的 IFD0 可能离文件头较远 |
| 会话格式版本 | **保持 v = 1**（不升 2） | 见 §5 C1：升 2 只能白付一次全量重写，挡不住老版本跳过该行 |
| `view_image` 的 `content` 体积 | 一行元信息，**有界**（不含 base64） | 见 §5 C4：几 MB 的 data URL 不得进文本或日志 |

### 3.2 锁边界（承重，不得挪 —— design.md §3.4）

三步必须保持这个顺序，且**锁内不做 IO**：

```
1. 锁内：查 (path, mtime, len) → 命中则 Arc::clone 返回，立即释放锁
2. 锁外：读盘 → 嗅探 → base64 编码        ← 可能几毫秒到几百毫秒
3. 锁内：插入（若前人已插入同键 ⇒ 丢弃自己那份，用已有的）
```

- 用 `std::sync::Mutex` 即够（锁内无 `.await`；不存在 guard 跨 await 的 `Send` 问题）。
- 第 3 步的重复插入是**正常且可接受**的浪费（结果相同），换来锁内不做重活。
- base64 编码**不** `spawn_blocking`：单次 ≤32 MiB、纯 CPU、本仓并发度低。
  若后续出现大批并发看图，只改 `media.rs` 内部即可，调用方不受影响。

## 4. 四条承重结论（实现者必须先读这一节）

### 4.1 加变体在**跨 crate** 消费点零编译错误 ⇒ 两件事都必须做（C6 修正后）

`#[non_exhaustive]` 的语义方向**正好相反**：它禁止**跨 crate** 穷尽匹配，即**保证**每个
外部 `match` 都写 `_` 臂 —— 新变体恰好被 `_` 臂静默吞掉。实测：给 `ContentBlock` 加 `Image`
变体后 `check --workspace --all-targets` **零 error**，39 个测试二进制全部照过。

**但这条只对一半**（C6 的最小 crate 实证）：

| 位置 | 穷尽 match（无 `_` 臂） | 加新变体时 |
|---|---|---|
| **外部 crate**（adapters / apps / 其他 crates） | ❌ `error[E0004]: non-exhaustive patterns` | —（外部根本写不了） |
| **`ys-core` 内部**（定义 crate） | ✅ 合法、编译通过 | ✅ **编译失败**，逐个点名 `pattern &ContentBlock::Audio { .. } not covered` |

即：`#[non_exhaustive]` **只禁跨 crate 穷尽匹配，在定义 crate 内部不生效**。
⇒ 本设计**不采用**「白名单测试作为唯一防线」，而是把取文本逻辑收口进 `ys-core`，
在那里写穷尽 match（U13 的 `ContentBlock::as_text()`），让编译器成为真正的守护。

⇒ **实施纪律**：

1. **取文本不透反射**：任何「只要 `Text` 块」的地方都调 `as_text()` / `joined_text()`（D12），
   不得自建白名单。新变体到时只在 `as_text()` 一处编译失败，改完就覆盖了全部文本消费点。
2. `as_text()` 的 match **永久保持穷尽**（D17）：哪怕某个新变体显然不贡献文本，
   也必须写显式的 `Self::X { .. } => None`，不得折成 `_ => None`（那等于自废守护）。
3. **非文本类消费点仍靠测试**：渲染、翻译、折叠这些**不是简单取文本**的地方（它们对每个
   变体有各自的语义）无法收口到一个函数。design.md §8 的逐文件清单仍是这些地方的
   **唯一清单**；漏一行 = 一处静默丢失。U4 的白名单用例继续守着它们。
4. 任何「我加了变体，编译器没说啥，应该没事」的判断都是错的。

| | 加变体/字段后 | 守护力 |
|---|---|---|
| `ContentBlock`，**外部 crate** 的 `_` 臂 | 零 error，静默吞掉 | ❌ **无**（靠 U4 白名单测试） |
| `ContentBlock`，**`ys-core` 内** 的穷尽 match | 逐个点名报错 | ✅ **有**（这就是 U13 的理由） |
| `ToolResult`（普通 struct 字面量） | 逐个点名报错 | ✅ **有** |

### 4.2 `ToolResult` 加字段**必然编译失败** ⇒ 编译器逐个点名，这是好事

`ToolResult` 是**结构体字面量**（不是 `#[non_exhaustive]` 枚举），加 `media` 字段后每个构造点
都必然报 `missing field 'media'`。

⇒ **实施纪律**：

1. **用编译器驱动迁移**，不要靠 grep 自造清单：以 `cargo check --workspace --all-targets`
   的报错为准，逐个补齐。本文给出的分布（见 E1）是**工作量估算的起点，不是终点**。
2. 配套 `ToolResult::text(content, is_error)` 便利构造（`media` 取空 `Vec`）以降噪，但
   **不**用它掩盖漏改；生产构造点与加字段**同批**落地（同一个编译单元，见 §6 U2）。
3. 迁移期间工作区**不编译**是预期的。验收以「编译单元关闭后 `check --all-targets` 零 error」为准。

### 4.3 K2 的修复是**两处**，只改一处「看起来像修过了」

`lib.rs:1316` 当前的推送条件：

```rust
if !text_content.is_empty() || !tool_calls.is_empty() {
    messages.push(ChatMessage { .. });
}
```

`_ => {}`（`:1311`）只是把一个块丢掉；而一条**只有 Image 块**的消息因两个容器都空，
**整条消息根本不 push** —— 实测出站为 `"messages":[]`。

⇒ **实施纪律**：

| 只做哪一步 | 结果 | 错觉 |
|---|---|---|
| 只改 `:1311` | 图片仍然消失，`"messages":[]` | ✅「我加了 Image 分支」 |
| 只改 `:1316` | 推送了一条空消息（`content: None`, `tool_calls: None`） | ✅「我改了推送条件」 |
| **两处都改** | `content: [ {text}, {image_url} ]` | 正确 |

守护手段：design.md §9 的「**消息数守恒**」用例（入站 N 条非空消息 ⇒ 出站消息数 ≥ 入站非空数），
以及 `scripts/vision-mutation.py` 里针对**两半各一个**的 mutant。

## 5. 与设计文档的偏差（E1–E4 核实修正 + C1–C8 已裁决调整）

十二条偏差分两类。**E 类**是核实后的事实修正，设计结论不变，只修数字与清单。
**C 类**是**已裁决**的调整，其中四条**直接推翻 design.md 的结论**：
C1（删实施步骤）、C6（推翻 D12）、C7（推翻 D8 的一半）、C8（修正限额口径）。

| # | 偏差 | 性质 | 对 design.md 的影响 |
|---|---|---|---|
| E1 | `ToolResult` 构造点计数：43/28 而非 168/77/91 | 核实修正 | 只改工作量估算，论据不变 |
| E2 | §8 清单漏了 `model-openai-compatible/Cargo.toml`（需加 `base64`） | 核实补充 | 新增一条 build 改动 |
| E3 | `sessions.rs:182 user_text` 明确不改 | 核实澄清 | 从改动清单里排除 |
| E4 | `crates/ys-tui-coding/src/draw.rs` 实测 0 处通配臂 | 核实纠正 | context.md 该条失真，不改该文件 |
| **C1** | **不做 `SessionMeta.v → 2`** | **推翻 §7/§8/§11 步 11** | 删掉一条实施步骤 |
| **C2** | `ocr` 文案改为陈述作用域，不再声明「唯一」 | 修改 §3.3 相关文案 + §8 漏的文件 | 新增 3 个文件（含 `prompt.rs`） |
| **C3** | 占位文案照 Codex 形态；「模型不支持图像」一条**不带路径** | 修改 §5 表格 | 四条文案重写 |
| **C4** | `view_image.content` 一行元信息 + 钉「不含 base64」断言；`detail` 收窄为 `high\|original` | 修改 §3.3 + §9 | 新增一条断言 |
| **C5** | `ImageDetail` 的 serde 小写 + `Default = High` | 细化 §3.1 | 类型定义补属性 |
| **C6** | **取文本逻辑收口到 `ys-core`**（`as_text()` + `TextJoin` + `joined_text()`） | **推翻 D12** | 7 个文件改调，5 处自建白名单全书 |
| **C7** | `turn_budget` **不计入**图片（与 `token.rs` 不对称） | **推翻 §6 + D8 的一半** | 只加注释 + 反向测试 |
| **C8** | 单图 32 MiB 指**base64 后**；48 MiB 累计预算超限降级 | 修正 §3.4 / §5 | 新增计数器 + 2 个错误变体 |
| **C9** | 请求体序列化结果塔存，仅在 `stream_options` 被剥后失效 | design.md 未覆盖 | 新增 1 个单元（U14） |
| **C8** | 单图 32 MiB 指**编码后**；48 MiB 累计预算超限时降级 | 修正 §3.4 口径 | 新增预算计数器与两个 `MediaError` 变体 |

---

### E1 —— `ToolResult` 构造点计数：**43 处生产 / 28 处测试**（不是 168 / 77 / 91）

| 口径 | 数字 | 说明 |
|---|---|---|
| design.md §3.1b / §8 写的 | 168 处（生产 77、测试 91） | 实际是 `grep "ToolResult {"` 的**原始命中数**，含 `ContentBlock::ToolResult`（48 处）与 `AgentEvent::ToolResult`（3 处）等，不等于需要加字段的结构体字面量 |
| 核实值 | **生产 43 / 测试 28** | 只统计 `ys_core::ToolResult` 的结构体字面量 |

生产 43 处的分布：

| 文件 | 处数 |
|---|---|
| `crates/ys-loop/src/basic.rs` | 11 |
| `adapters/tools-web/src/search.rs` | 7 |
| `adapters/tools-web/src/fetch.rs` | 6 |
| `adapters/tools-basic/src/read.rs` | 5 |
| `adapters/tools-basic/src/grep.rs` | 3 |
| `adapters/tools-basic/src/ls.rs` | 3 |
| `adapters/tools-basic/src/bash.rs` | 2 |
| `adapters/tools-basic/src/find.rs` | 2 |
| `adapters/tools-basic/src/ocr/mod.rs` | 2 |
| `adapters/tools-basic/src/write.rs` | 1 |
| `crates/ys-core/src/message.rs` | 1 |

**口径说明**：上表最后一行 `crates/ys-core/src/message.rs:34` 的那一处是
`ContentBlock::ToolResult` 的**变体定义**，不是构造点 —— 它**不需要改动**（D9 要求其字节形态不变）。
因此实际要补 `media` 的构造点是 **42 处生产 + 28 处测试 = 70 处**。

**论据不变**：这是普通结构体字面量 ⇒ 编译器逐个点名（§4.2）。偏差只影响工作量估算，
不影响「编译器当清单」的实施方式。

### E2 —— design.md §8 清单**漏了** `adapters/model-openai-compatible/Cargo.toml`

`base64 0.22.1` 已在 `Cargo.lock:44`（经 `reqwest`/`hyper-util` 传递），但
`adapters/model-openai-compatible/Cargo.toml` **未直接声明**它。在 `media.rs` 里
`use base64::…` 会因「非直接依赖」而不可见。

⇒ 必须显式加一条依赖：

```toml
base64 = "0.22"
```

这是 **B 档（build）改动**，落在 U5，与 `media.rs` 同批。context.md 已预先裁决：
这算「显式依赖新增」，**不是**新增 crate（版本已在锁文件里，无新下载、无新编译单元）。

### E3 —— `sessions.rs:182 user_text` **明确不改**

`crates/ys-session/src/jsonl.rs` 之外，`apps/coding-agent/src/sessions.rs:182` 的 `user_text`
只服务**会话标题**（唯一调用点 `apps/coding-agent/src/app_loop.rs:561`）。

`user_text` 的语义是「纯 `ToolResult` 的 user 消息不算用户文本」；`Image` 块出现在
**工具结果折叠后的 user 消息**里（design.md §4 生产者 A），与标题无关。

⇒ **语义不改**（`Image` 不进标题），但**实现形式改由 C6 统一**：取文本部分改为
`msg.joined_text(TextJoin::Space)`。`Role::User` 检查与「非空 Text」过滤**仍留在 `sessions.rs`**，
只是不再自己遍历块。理由写进 as-built：

- 标题素材必须是**用户敲的字**，图片路径不是。若把 `Image.path` 拼进标题，`/resume` 列表里
  会出现一屏 `/tmp/xxx.png`。
- `Image` 块对「用户原文」的贡献是 `None`，这一点由 `as_text()` 的穷尽 match 表达（C6），
  不再靠本函数里的 `_ => None` 臂 —— 后者在加变体时不会报错，前者会。

（早期核实结论「`user_text` 完全不动」已被 C6 覆盖：**语义不动、实现改调**。）

### E4 —— `crates/ys-tui-coding/src/draw.rs` **0 处** `ContentBlock` 通配臂

context.md 称该文件有 6 处 `ContentBlock` 通配臂。实测 **0 处**（该文件不 match `ContentBlock`）。

⇒ 该条**失真**，`draw.rs` 不在改动清单里，也不写白名单用例。B 档的渲染改动只落在
`crates/ys-tui-coding/src/transcript.rs`（A9）与 `crates/ys-tui-coding/src/app.rs`（A10）。

---

### C1 —— **不做 `SessionMeta.v → 2`**（推翻 design.md §7 / §8 / §11 步 11）

**裁决**：`v` 保持 **1**；design.md §11 第 11 步（`SessionMeta.v = 2`）**删除**，原 U11 任务作废。

**理由（三条，均已核实）**：

1. **`v` 目前只写不读**。全仓只有 `crates/ys-session/src/meta.rs:36` 的声明与 `:71` 的赋值，
   唯一的读取方是 `apps/coding-agent/src/wiring.rs:593` 的一条测试断言（`assert_eq!(meta.v, 1)`）。
   没有任何版本分支消费它 ⇒ 升 2 **不带来任何行为**。
2. **升版本的代价是全量重写**。`append` 走 `O_APPEND` 单次 `write_all`、**完全不碰 meta**
   （A16）；meta 的唯一写入实现是 `flush_to_file`，它是 **tmp + rename 的全量重写，含全部 messages**
   （`crates/ys-session/src/jsonl.rs:197` 注释契约）。「首个 Image 块出现时升 v」= 一次
   **O(会话总字节)** 的重写，恰好是本仓刚花力气消掉的那类开销（B 档 append-only 落地的前提）。
3. **它挡不住本来要挡的东西**。老版本读新会话时该行仍被 `jsonl.rs:170` 的 `skipped_lines`
   跳过 —— 加版本号**不改变这个事实**。design.md §7 那句「本次不改老版本的读取逻辑（改不了）」
   本身已承认了这一点。

**Codex 对照**：它的 `SessionMeta`（`tmp/codex/codex-rs/protocol/src/protocol.rs:3361+`）
**没有任何格式版本字段**；新字段一律用 `#[serde(default)]` + `skip_serializing_if` 做**字段级容错**。
本设计的 `ContentBlock::Image` 已是新增外部标签，老版本解析该行失败即跳过 —— 字段级容错
已到位，版本号是多余的一层。

**连带**：`ToolResult.media` 与 `ContentBlock::Image` 的 `#[serde(default)]` 承担了全部兼容职责 ——
这两处的 serde 属性因此是**承重契约**，不能为了「写法好看」去掉。

---

### C2 —— `ocr` 文案改为「陈述作用域」，不再声明独家

**裁决**：`ocr` 与 `view_image` 并存后，「唯一能看图的通道」变成**假话**，必须改。

**两处必改**（design.md §8 **漏了 `prompt.rs`**）：

| 位置 | 现状 | 目标形态 |
|---|---|---|
| `adapters/tools-basic/src/ocr/mod.rs:138` `spec()` 的 description | `… — the only way to see what an image contains here; set word_boxes for per-word coordinates` | 陈述两者分工（见下） |
| `apps/coding-agent/src/prompt.rs:279` `tool_snippet` 的 `"ocr"` 分支 | 同上（且 `:273-278` 的注释以「`ContentBlock` 只有 `Text`/`ToolUse`/`ToolResult`」为前提，**该前提被本功能推翻**） | 同上去掉「唯一」，并重写那段注释 |

**目标文案**（两处逐字一致）：

```
Extract the text in an image together with its pixel positions — use this when you need the
      text and where it sits; use view_image to see the picture itself. Set word_boxes for
      per-word coordinates
```

**连带必改（测试变红是预期的，不是回归）**：

- `apps/coding-agent/src/prompt.rs:913` `search_list_and_web_snippets_are_verbatim` 里有
  `tool_snippet("ocr")` 的**逐字断言**。文案一改它就红，必须同步更新断言与它上方
  （`:900-903`）说明「改文案等于改行为，须过显式评审」的注释。
- `apps/coding-agent/src/prompt.rs:273-278` 那段注释的前提「`ContentBlock` 只有
  `Text`/`ToolUse`/`ToolResult`，`ChatMessage.content` 是 `Option<String>`」**两句都不再成立**
  （本功能加 `Image` 变体、把 `content` 改成 `Option<Content>`）⇒ 必须重写为当前事实，
  并保留「为什么文案要点明分工」的理由。
- 新增 `view_image` 的 `tool_snippet` 分支（否则父表里它显示为 `Custom tool`），
  并在同一逐字用例里钉住。

**Codex 对照**：它的工具描述只陈述自己的适用场景，**从不声明独家**
（`core/src/tools/handlers/view_image_spec.rs:44`：`View a local image file from the filesystem
when visual inspection is needed. Use this for images already available on disk.`）。
Codex 没有 ocr 工具，故无对照文案；借的是这条**原则**。

---

### C3 —— 占位文案照 Codex 的形态；「模型不支持图像」一条**不带路径**

**裁决**：design.md §5 的四条占位按下列形态落地（前三条带路径与原因，第四条不带路径）。

| 情形 | 占位文本（逐字） | 依据 |
|---|---|---|
| 文件不存在 / 不可读 | `` yushan could not read the local image at `{path}`: {error} `` | Codex `protocol/src/models.rs:1797-1810` `local_media_error_placeholder` |
| 不是图片 / 内容非法 | `` Image located at `{path}` is invalid: {error} `` | Codex `:1882-1892` `invalid_image_error_placeholder` |
| 格式不支持 | `` yushan cannot attach image at `{path}`: unsupported image `{mime}`. `` | Codex `:1894-1900` `unsupported_image_error_placeholder` |
| **模型 / provider 不支持图像** | `image content omitted because you do not support image input` —— **不带路径** | Codex `core/src/context/unsupported_media.rs:19` |
| 体积超限（本仓独有） | `` yushan cannot attach image at `{path}`: too large ({size} > 32 MiB). `` | 与「格式不支持」同形；本仓无缩放能力 |

**第四条为什么故意不带路径**（照抄 Codex 的取舍）：模型对「你没法看图」这件事**无能为力**，
逐个图重复路径纯属浪费 token —— `strip_images_when_unsupported` 会给每张图各贴一次，
十张图就是十行同样的路径。前三条不同：模型可以据此**改策略**（换路径、改用 `ocr`），
所以路径与原因是有效信息。

**实现约束**：

- 逐字断言这五条（`translate_image.rs` 里各一条），文案是模型行为的输入。
- 第四条在**同一批**里只发一次（按消息或按请求去重），不是每图一次。
- `yushan` 前缀照 Codex 用自己的名字（`Codex could not read…`），本仓用 `yushan`。

---

### C4 —— `view_image.content` 必须是**一行元信息**，且测试钉住「不含 base64」

**裁决**：`content` 的体量是有界的一行元信息；`design.md §9` 的
「view_image 产出 media，且不读全文件」那条**扩成两条断言**。

**形态**（`content`，不是 data URL）：

```
image: /tmp/a.png (image/png, 1920×1080, 1.4 MiB) — attached as pixels, not text
```

**必须断言的**（同一用例内）：

1. `content` **不含** `base64`、不含 `data:`；
2. `content.len()` 有界（路径 + 百字节量级）；对一个 **40 MiB** 的合法 PNG 头 + 垃圾尾部的夹具，
   这个界不能被突破；
3. `media` 恰有一个 `Image { path, detail }`。

**Codex 对照**：`ViewImageOutput::log_output()`
（`core/src/tools/handlers/view_image.rs:231-233`）返回 `` `<image data URL omitted: {} bytes>` `` ——
显式防止几 MB 的 data URL 灌进日志；真正的 data URL 走 `to_response_item()` 的 `InputImage`，
**不进文本**。本仓的等价形态就是这个一行元信息。

**另一项收窄**：`detail` 的 schema 只允许 **`high` \| `original`**（`view_image_spec.rs:20` 的
`JsonSchema::string_enum(vec![json!("high"), json!("original")], …)`），**不是**
design.md §3.3 写的 `low|high|original`。`low` 对「我要看清这张图」没有意义。
`ImageDetail` **枚举本身**仍保留 `Low` / `Auto`（协议层完整，将来别的生产者可能用），
**工具层**收窄。缺省 `high`（D13）。

---

### C5 —— `ImageDetail` 的 serde 形态照 Codex

**裁决**：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    Auto,
    Low,
    #[default]
    High,
    Original,
}
```

- **必须加 `#[serde(rename_all = "lowercase")]`**：design.md §3.1 没写这个属性 ⇒ 会落成
  `"Low"` / `"High"`（首字母大写）。会话 JSONL 里应是小写：`{"Image":{"path":…,"detail":"high"}}`。
- **`Default` = `High`**（Codex 的 `DEFAULT_IMAGE_DETAIL`，`protocol/src/models.rs:967`）。
- 变体**序**不影响 serde（外部标签按名字），但保持 `Auto, Low, High, Original` 与
  Codex 一致以便对照阅读。
- `ImageFormat` 同理用 `#[serde(rename_all = "lowercase")]`（`"png"` / `"jpeg"` …）——
  它出现在占位文案 `(detected: bmp)` 里，小写形态才是用户看到的样子。

**Codex 对照**：`protocol/src/models.rs:957-967`。

---

### C6 —— **取文本逻辑收口到 `ys-core`**（推翻 D12：`#[non_exhaustive]` 的守护力取决于位置）

**裁决**：不采用「块集合白名单测试作为唯一防线」（design.md D12 / §5），改为把
**「哪些块算文本」**收口成 `ys-core` 里的**穷尽 match**，让编译器当守护（A 方案）。

#### C6.1 design.md §5 的断言只对了一半

| 位置 | 穷尽 match（无 `_` 臂） | 加新变体时 |
|---|---|---|
| **外部 crate**（`adapters` / `apps` / 其余 crates） | ❌ `error[E0004]: non-exhaustive patterns` | —（外部不允许写） |
| **`ys-core` 内部**（定义 crate） | ✅ 合法、编译通过 | ✅ **编译失败**，逐个点名 `pattern &ContentBlock::Audio { .. } not covered` |

即：**`#[non_exhaustive]` 只禁止跨 crate 穷尽匹配，在定义 crate 内部不生效**。
因此「证伪编译期守护」的结论成立的前提是「消费点都在外部 crate 且写了 `_` 臂」——
在 `ys-core` 内部写穷尽 match 是**有守护力**的，这正是本设计采用 `as_text()` 的理由。

（本条修正已同步进本文 §4.1 与 D12。）

#### C6.2 待统一的 5 处实现（当前各不相同）

| 位置 | 取什么 | 分隔符 | 过滤 |
|---|---|---|---|
| `crates/ys-tui-coding/src/app.rs:905` `message_text` | Text 块 | `\n` | 否 |
| `apps/coding-agent/src/sessions.rs:182` `user_text` | 仅 `Role::User` + Text | **空格** | 跳过空白块 |
| `apps/coding-agent/src/subagent.rs:1769` `text_of` | Text 块 | `\n` | 否 |
| `crates/ys-tui-coding/src/transcript.rs:152`（assistant 分支） | Text 块 | **``（空）** | 否 |
| `crates/ys-loop/src/summarize.rs:106` 与 `compact.rs:374` | Text 块 | **``（空）** | 否 |

五处对「哪些块算文本」的答案目前**碰巧都对**，但那是因为它们那个时代只有三个变体。
加 `Image` 后每处都得重新答一遍 —— 这才是 D12 想解决的问题，而把它收口成一个函数是
比五份白名单更硬的解法。

#### C6.3 目标形态

```rust
// crates/ys-core/src/message.rs
impl ContentBlock {
    /// 该块对**文本消费者**的贡献。`None` = 不是文本块。
    ///
    /// 这里是**穷尽 match、无 `_` 臂**：定义 crate 内部 `#[non_exhaustive]` 不生效
    /// ⇒ 新增变体必然在此编译失败，各文本消费点据此逐个点名。绝不改成 `_ => None`。
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text } => Some(text),
            Self::ToolResult { .. } => None,
            Self::Image { .. } => None,
        }
    }
}

/// 分隔符是**调用方的语义选择**，故显式枚举（不用裸 `&str`，避免再次各自漂移）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextJoin { Newline, Space, None }

impl Message {
    /// 拼接全部 `Text` 块。`TextJoin::None` 用于「摘要/流式响应」这类必须无缝的场景。
    pub fn joined_text(&self, join: TextJoin) -> String { … }
}
```

**只统一「哪些块算文本」，不统一分隔符**（D16）—— 后者是各调用方的真实语义差异：
界面回显要换行、标题要空格、摘要要无缝。

#### C6.4 五处调用的映射

| 调用点 | 改为 | 备注 |
|---|---|---|
| `app.rs:905` `message_text` | `msg.joined_text(TextJoin::Newline)` | 函数体变成一行转发（保持 `pub(crate)` 签名不变） |
| `sessions.rs:182` `user_text` | `msg.joined_text(TextJoin::Space)` | **`Role::User` 检查与空白过滤仍留在 `sessions.rs`**，只把取文本改为调新函数 |
| `subagent.rs:1769` `text_of` | `m.joined_text(TextJoin::Newline)` | |
| `transcript.rs:152` | `message.joined_text(TextJoin::None)` | |
| `summarize.rs:106` / `compact.rs:374` | `response.message.joined_text(TextJoin::None)` | 两处各自的「空/全空白响应 = 失败」判定**不动** |

#### C6.5 对既有测试的影响（预期变化，不是回归）

| 用例 | 处置 |
|---|---|
| `crates/ys-tui-coding/src/app.rs:2695` `test_message_text_joins_text_blocks` | **保留**，改为经由新函数验证（断言 `joined_text` 行为） |
| `crates/ys-tui-coding/src/transcript.rs:538` `p2_3_only_text_and_tool_use_produce_lines` | **保留但重新定位**：它不再是「替代编译器的唯一防线」，而是**行为回归测试**（渲染结果是否正确）。**不要删**。注释里「加变体时必须同步本函数与 `app::message_text`」那句在收口后可删 —— 两份白名单已合一。 |
| `apps/coding-agent/src/sessions.rs:453` `user_text_skips_non_text_and_non_user` | **保留**，语义不变 |

#### C6.6 新增的穷尽性测试（U13 的验收）

在 `crates/ys-core/src/message.rs` 的 `mod tests` 里加一条钉住 `as_text` 的**变体全集**：
逐变体断言返回值（`Text` ⇒ `Some`，其余 ⇒ `None`），并声明加的断言不能因变体增加而失效。
这条测试的作用是**钉住「不得改成 `_` 臂」**—— 一旦有人把它写成 `_ => None`，本用例的
逐变体断言仍会过，所以还要加一句注释明确「全臂展开是该函数的契约」；真正的守护是编译器。

#### C6.7 与 C2 的关系

**无依赖，可并行**。C6 改 `crates/ys-core/src/message.rs` + 5 个消费文件；
C2 改 `adapters/tools-basic/src/ocr/mod.rs` + `apps/coding-agent/src/prompt.rs` —— 文件集不相交。

---

### C7 —— `turn_budget` **不计入**图片（推翻 design.md §6 与 D8 的一半）

**裁决**：`crates/ys-loop/src/turn_budget.rs:38` 的 `_ => 0` **保持不变**，只补注释与一条反向测试。
与 D8「两个估算器都要改」的偏差是**有意的不对称**，不是漏改。

#### C7.1 实测依据

`crates/ys-loop/src/turn_budget.rs:67-95` 的 `elide_message` 只对 `ContentBlock::ToolResult` 做头部截断：

```rust
.map(|block| match block {
    ContentBlock::ToolResult { content, .. } if needs_elision(content) => { /* 缩成头部 + 指针 */ }
    other => other.clone(),          // ← Image 块走这里，原样保留
})
// :93
if changed { Some(...) } else { None }   // ← 只含 Image 的消息 ⇒ None
```

而 `plan_elisions`（`:108-130`）逐条尝试降级直到压进预算。若超预算的原因是**图片**：

1. 每个 Image 块都降不掉（`elide_message` 返回 `None`）
2. 循环跑完整个区间，**一条未降**
3. 下一回合再来一遍

⇒ **花力气但预算永远压不下来**：它的手段（缩文本）与超支来源（图片）不匹配。

#### C7.2 注释条文（写进 `turn_budget.rs:38`，不用「暂不实现」这类措辞）

> `Image` 块**有意不计入**本闸门：`plan_elisions` 的唯一手段是截断 `ToolResult` 文本。若把
> 图片 token 计入，超支原因落在图片上时 `elide_message` 对它返回 `None`，循环逐条尝试后
> 一条也降不掉 ⇒ 每回合空转且预算仍超。图片的体积治理归 `compact`（压缩丢弃 Image 块），
> 不归本闸门。

#### C7.3 两个估算器的不对称（维护者必须一眼看懂）

| 估算器 | 图片是否计入 | 理由 |
|---|---|---|
| `crates/ys-loop/src/token.rs:12` | ✅ **计入 1024/图** | 它是「上下文占用」的度量，图片**确实**占 token ⇒ 不计就是错报（K3）。消费者是压缩阈值。 |
| `crates/ys-loop/src/turn_budget.rs:38` | ❌ **不计入** | 它是「可腾空间」的度量。图片不可腾（不可降级）⇒ 计入会让闸门陷入无效循环。 |

两者的区别是**问的问题不同**（占了多大 vs 能腾多少），不是「一处修了一处忘了」。
U8 必须在两个文件里各留一段指向对方的注释，否则下一个维护者必定把它们「修复」成对称。

#### C7.4 防「顺手补上」的反向测试

```rust
#[test]
fn image_blocks_do_not_affect_tool_result_tokens() {
    // 含 Image 的消息不增加本闸门的分母：
    // 它不可降级，计入只会让 plan_elisions 空转。
    assert_eq!(tool_result_tokens(&[msg_with_image()]), 0);
    assert!(plan_elisions(&[msg_with_image()], 0, 1).is_empty());
}
```

这条用例的意义是**把「不做」钉成要求**：后来人若「顺手」补上 `Image => 1024`，它就变红。

---

### C8 —— 体积上限按 **base64 后**判定，请求体累计不超 48 MiB（修正 §3.4）

**裁决**：单图 32 MiB 指**编码后**；请求体 48 MiB 是**真正的上限**，超限时当前及之后的图降级。

#### C8.1 实测依据

`docs/arch/vision-input/context.md:159` 原文：单图 ≤32 MiB 是 **base64/URL**（即**编码后**），
请求体 ≤48 MiB。design.md §3.4 写「单图 32 MiB」却未说明判断对象，§5 占位示例写 `35.1 MiB > 32 MiB`。

若按**原图**大小判 32 MiB：原图 24 MiB → base64 后约 32.9 MiB ⇒ **实际超限**。
且两张 32 MiB 的图 = 64 MiB > 48 MiB 请求体上限 —— **两条限额互相矛盾**，原设计未处理。

#### C8.2 两条限额的职责划分

| 限额 | 判据 | 超出时的行为 | 占位文案 |
|---|---|---|---|
| 单图 **32 MiB** | **base64 编码后**的字节长度 | 该图降级 | ``[image too large: /path (32.9 MiB > 32 MiB)]`` |
| 请求体 **48 MiB** | 按消息顺序**累计**已编码图片 | **当前及之后**的图全部降级 | ``[image too large: /path (would exceed the 48 MiB request budget)]`` |
| 单请求 **600 图** | 计数 | 超出部分降级 | ``[image too large: /path (over the 600 images per request limit)]`` |

#### C8.3 预估先行（省一次大内存分配）

编码前先按 `原图 len × 4/3 + 余量` 预估：明显超 32 MiB 的**直接不进编码**（避免为了一张
注定被拒的图先分配一个 40+ MiB 的 `String`）。预估是个**快速败类路径**，不是第二套判据 ——
精确判定仍以实际编码后的长度为准（预估漏网的由编码后长度二次判定）。

#### C8.4 预算计数器与 K2 同处

累计字节计数器在 `translate_request` 里维护，判定写在 `:1311` 的 Image 臂内——
**与 K2 的修复同一处，不得写成两套逻辑**。顺序：

```
:1311 Image 臂
  ├─ 门控不支持 → 占位（C3 第四条，不带路径）
  ├─ 单图超 32 MiB（编码后） → SingleTooLarge 占位
  ├─ 计数 > 600 → 图数超限占位
  ├─ 累计 + 本图 > 48 MiB → RequestBudgetExceeded 占位     ← 计数器在这里累加
  └─ 否则 push image_url 块
```

计数器**只在真正 push 时**累加（降级的图不占预算）。

#### C8.5 两个新增的 `MediaError` 变体

```rust
pub enum MediaError {
    NotFound { path: String },
    Io { path: String, source: String },
    UnsupportedFormat { path: String, detected: String },
    SingleTooLarge { path: String, bytes: u64, limit: u64 },        // C8
    RequestBudgetExceeded { path: String, limit: u64 },             // C8
    ImageCountExceeded { path: String, limit: usize },              // C8
}
```

后两个变体由 `:1311` 的臂产生（不在 `media.rs` 内部判）—— `media.rs` 只负责单图，
请求级预算属于翻译层。

#### C8.6 测试不得真读 20 MiB

累计预算用例**不要求真造大文件**：把预算改小到 1 KiB（测试可经 `OpenAICompatibleConfig`
或测试专用钩子注入小上限），用两张 1.5 KiB 的小图验证「第二张降级」。逻辑与真实尺寸无关。

---

### C9 —— 请求体序列化结果塔存，仅在 `stream_options` 被剥后失效

**性质**：design.md **未覆盖**的新增项（不是推翻，不是修正）。但它对图片特性是**必要**的：
不处理则代价随图片体量线性放大。

#### C9.1 问题（实测定位）

`adapters/model-openai-compatible/src/lib.rs:313-327`：

```rust
async fn send(&self, url: &str, chat_request: &ChatCompletionRequest) -> … {
    let mut request = self.client.post(url)…;
    request.json(chat_request).send().await      // ← :326 每次尝试都重新序列化整个结构体
}
```

重试循环在 `:459`（`max_attempts = self.retry.max_attempts + 1`），每次尝试都重新 `.json()`。
加图片后这一步被放大到 MB 级：一张 5 MiB 原图 → base64 约 6.8 MiB；4 次尝试 ⇒ 约 27 MB 的
临时字符串反复分配。

**关键**：`MediaCache`（D7）能挡住「读盘 + base64 编码」，但**挡不住 JSON 序列化**——它缓存的
是 data URL **字符串**，而 `.json()` 每次都要把这个字符串重新写进 JSON 缓冲。这不是缓存能解决的问题。

#### C9.2 先核实：`chat_request` 到底会不会变（**已核实**）

全仓 `chat_request` 的写入点**只有一处**：

```
adapters/model-openai-compatible/src/lib.rs:654    chat_request.stream_options = None;
```

它在 `attempt()` 的 **stream_options 一次性回退**分支内（`:624-660`）：端点对
`stream_options.include_usage` 返回 400 且响应体指明该字段不支持时，剥掉它并**当场重发一次**
（`match self.send(url, chat_request)`），同时 `meta.fallback_retry = true`。

⇒ **不得**把序列化提到重试循环外一次算完（那样会让回退分支发出带陈旧 `stream_options` 的请求，
`apps/coding-agent/tests/stream_usage_fallback.rs` 会红）。

#### C9.3 方案：惰性塔存 + 单一失效点

```rust
// `body` 的有效性绑定「chat_request 自上次序列化以来未被修改」。
// 唯一的修改点是 stream_options 剥除，故在那里把它置回 None。
let mut body: Option<Arc<Vec<u8>>> = None;
```

1. `attempt()` 内每次 `send` 前：`body` 为 `None` 则序列化（`serde_json::to_vec`），结果存回 `body`。
2. `:654` 剥掉 `stream_options` 之后**紧跟一句** `body = None;`，使回退重发前重新序列化。
3. 外层重试进入下一次 `attempt` 时 `body` 仍持有上一次的值 ⇒ 直接复用（`chat_request` 在此期间未变）。

**效果**：外层重试 N 次 ⇒ 序列化 **1 次**；走了回退分支 ⇒ **2 次**（改前 + 改后，必须的）。
现状最坏 N×2 次。

#### C9.4 三个实现细节

1. **`Content-Type` 必须显式**：`.json()` 会自动带，`.body()` 不会。`:322` 已有该 header，保留即可
   —— 但改写时必须逐字确认它还在。
2. **用 `Arc<Vec<u8>>`，不引 `bytes`**：`.body((*body).clone())` 只复制 `Vec` 内容一次（`Into<Bytes>`
   会拷贝）。`bytes::Bytes` 可避免这次拷贝，但它同样需要**显式声明依赖**（与 `base64` 同理）
   ⇒ **不新增第二条显式依赖**，选 `Arc<Vec<u8>>`。 请求体本来就是 MB 级，“复制一次”比“新增一条依赖”便宜。
3. **序列化失败映射到既有 `ModelError` 变体**，不新增变体；错误语义与 `attempt` 之外的失败一致。

#### C9.5 测试要求

| # | 断言 | 现有覆盖 |
|---|---|---|
| C24 | **回退路径两次请求体不同**：第一次含 `stream_options`、第二次不含 | **已覆盖**：`apps/coding-agent/tests/stream_usage_fallback.rs:361` `endpoint_rejecting_stream_options_falls_back_once`（`:402-410` 已逐字断言两体不同）。本单元**必须让它保持绿**。 |
| C25 | **重试间复用**：429 触发外层重试 ⇒ 最终成功的请求体与不重试时**逐字节相等** | 新增（行为断言，不需观测序列化次数） |
| — | 既有 `adapters/model-openai-compatible/tests/retry.rs` 全部不得回归 | 已存在 |

**不要把 C24 重写成新用例** ：它已存在且恰好就是「防止复用陈旧 body」的护栏。本单元只需
确保它仍绿 —— 若它变红，说明 `body = None` 漏了或放错了位置。

## 6. 分步实施（design.md §11 的 12 步 → 14 个可验收单元）

单元编号 `U1`…`U14`；每个单元给出「目标 / 非目标 / 涉及文件 / 关键约束 / 验收」。
`docs/exec-plans/2026-11-05-vision-input.md` 是它的任务级细化（`T01`…）。

步→单元的对应：design.md §11 的第 1–10 步对应 U1–U10；第 11 步（v→2）**删除**并由 U11（工具文案）
取代；第 12 步是 U12。**U13（C6 取文本收口）与 U14（C9 序列化塔存）是新增的，
不属于 design.md §11 的任何一步。**

### 依赖拓扑

```
U1 (media 纯函数)
 ├── U2 (ToolResult.media)  ──┬── U3 (ContentBlock::Image) ── U4 (白名单红测试)
 │                            │        │
 │                            │        ├── U5 (适配器 media.rs + Content 枚举)
 │                            │        │     └── U6 (门控+占位+:1316，K2 两半)
 │                            │        │           └── U14 (序列化塔存 + 失效点)
 │                            │        ├── U7 (view_image)
 │                            │        │     └── U11 (工具文案：ocr + prompt.rs + view_image snippet)
 │                            │        └── U8 (折叠 + token.rs 计入 / turn_budget.rs 不记)
 │                            │
 │                            ├── U13 (as_text/joined_text 收口) ── U9 (TUI / subagent / message_bytes)
 │                            │
 └────────────────────────────┴─────────────────── U10 (expand_skill_invocation)

U12 (vision-mutation.py) 依赖 U6 / U7 / U8 / U9 / U11 / U13 / U14
```

**U13 必须在 U9 之前**：`message_text`（`app.rs:905`）与 `transcript.rs:152` 都交给 U9 改渲染，
而它们也是 U13 的收口对象。先在 U13 把取文本合成 `joined_text`，U9 就只需处理「非文本类」
渲染（`Image` 块要产一行 `[image: …]`，这与取文本无关）。反序做会重复改同一段代码两次。

**U13 对 U3 的依赖是软的**：`as_text` 的 `Self::Image { .. } => None` 臂需要 `Image` 变体先存在。
若想并行，可先把 `as_text` 写成对现有三变体的穷尽 match，U3 再加一臂 —— 但那样 U3 会改两个
文件，波次更碎。**建议串行**（U3 → U13）。

**已删除的单元**：原 U11（`SessionMeta.v → 2`）**作废**，见 §5 C1。`crates/ys-session` 因此
**零生产改动** —— 只剩下测试（C14 / C15 落在 `jsonl.rs` 的 `mod tests` 里，验证「字节不进会话」）。

---

### U1 —— `ys-core::media`：格式嗅探纯函数（步 1）

- **目标**：`ImageFormat`（Png / Jpeg / Gif / WebP / Bmp / Tiff）、`ImageDetail`
  （Low / High / Original / Auto）、`sniff_image(head: &[u8]) -> Option<(ImageFormat, u32, u32)>`、
  `ImageFormat::mime()` / `is_vision_supported()`。
- **非目标**：任何 IO、任何 base64、任何 error 类型。**`ys-core` 零 tokio**（A18）—— 本单元
  不能引入 `std::fs` 调用，只能吃 `&[u8]`。
- **涉及文件**：
  - `crates/ys-core/src/media.rs`（新）
  - `crates/ys-core/src/lib.rs`（`mod media;` + `pub use media::*;`）
- **关键约束**：
  - **按内容判格式，不认扩展名**（DeepSeek 明确要求）。`x.png` 实为 JPEG ⇒ 判 `Jpeg`。
  - 嗅探逻辑与 `adapters/tools-basic/src/ocr/mod.rs:538 dims_from_header` **同源**（A17）：
    PNG 的 `IHDR`、GIF 的逻辑屏幕、BMP 的 DIB 头、WebP 的 VP8/VP8L/VP8X、TIFF 的 IFD0、
    JPEG 的 SOFn 扫描。`ys-core` 不能依赖 `tools-basic`（依赖方向）⇒ **把逻辑上提**，
    而不是让 `tools-basic` 反向依赖。`ocr` 侧**不改**（保留自有实现，两者各自单测）。
  - 头不足 / 长度异常 ⇒ `None`，**绝不 panic**（所有切片用 `get(..)` 形式）。
  - `ImageDetail` 是**数据**（用户/模型的意图），不是 wire 值；provider 不认识时由适配器丢弃。
  - 变体序与 serde 形态即契约（**D13 / C5**）：`ImageDetail` 与 `ImageFormat` 都加
    `#[serde(rename_all = "lowercase")]` ⇒ 落盘为 `"high"` / `"png"` 而不是 `"High"` / `"Png"`。
    `ImageDetail` 的 `Default` 是 **`High`**（`#[default]` 标在 `High` 上）。
    理由：占位文案里 `(detected: bmp)` 与 JSONL 里的 `"detail":"high"` 都是模型/用户看到的样子，
    小写才是它该有的形态；大写的默认 derive 形态是一处会被写进会话的意外契约。
  - `ImageFormat::mime()` 返回 `image/png` / `image/jpeg` / `image/gif` / `image/webp` /
    `image/bmp` / `image/tiff`；`is_vision_supported()` 只对前四种为真（DeepSeek 口径）。
- **验收**：
  - `cargo test -p ys-core` 全绿。
  - 六种格式各一条正例 + 四条反例（空头、截断、非图片、头长度不足）；`x.png` 实为 JPEG 的夹具。
  - `cargo tree -p ys-core | grep -c tokio` ⇒ 仅 dev-dependencies 里出现（零 tokio 不破）。

### U2 —— `ToolResult.media` + `ToolResult::text()` + 全部生产构造点（步 2）

- **目标**：
  ```rust
  pub struct ToolResult {
      pub content: String,
      pub is_error: bool,
      #[serde(default, skip_serializing_if = "Vec::is_empty")]
      pub media: Vec<MediaAttachment>,
  }
  pub enum MediaAttachment { Image { path: String, detail: Option<ImageDetail> } }
  impl ToolResult { pub fn text(content: impl Into<String>, is_error: bool) -> Self { … } }
  ```
  加字段后**把全部生产构造点改完**（同批）。
- **非目标**：不改 `ContentBlock::ToolResult`（D9 要求其字节形态不变）；不加别的便利构造
  （`error(...)` 之类不在设计里）。
- **涉及文件**：
  - `crates/ys-core/src/tool.rs`（`media` 字段 + `MediaAttachment` + `text()`）
  - `crates/ys-loop/src/basic.rs`（11 处）
  - `adapters/tools-web/src/search.rs`（7）/ `fetch.rs`（6）
  - `adapters/tools-basic/src/read.rs`（5）/ `grep.rs`（3）/ `ls.rs`（3）/ `bash.rs`（2）/
    `find.rs`（2）/ `ocr/mod.rs`（2）/ `write.rs`（1）/ `edit.rs`（1）/ `bg_tools.rs`（1）
- **关键约束**：
  - **这是唯一的「编译器守护」单元**（§4.2）：不做任何 grep 驱动的猜测，按 `cargo check`
    的报错逐条补。`--all-targets` 必须包含测试点（28 处）。
  - `skip_serializing_if = "Vec::is_empty"` 是**契约**：无 media 时事件 JSON / 会话 JSONL
    **逐字节不变**（守护 A19 的 golden 与 `AgentEvent::ToolResult` 的既有形态）。
  - 本单元开启一个**跨任务的编译单元**：`tool.rs` 改完到全部构造点补齐之间，工作区不编译。
    这是预期状态，不是故障。
  - 语义零变化：既有工具一律 `media: vec![]`，行为与字节都不变。
- **验收**：
  - `./scripts/build-baseline.sh` 之外，直接 `./scripts/cargo-build.sh check --workspace --all-targets`
    零 error。
  - `./scripts/par-test.sh` 全绿（既有用例零改动通过 ⇒ 语义未变）。
  - 序列化断言：`serde_json::to_string(&ToolResult::text("x", false))` 恰为
    `{"content":"x","is_error":false}`（**无** `media` 键）。

### U3 —— `ContentBlock::Image` 变体（步 3）

- **目标**：
  ```rust
  Image {
      path: String,
      #[serde(default, skip_serializing_if = "Option::is_none")]
      detail: Option<ImageDetail>,
  }
  ```
- **非目标**：**无 `id` 字段**（design.md §8 明确）；不带字节；不带 mime（mime 由适配器嗅探得出）。
- **涉及文件**：`crates/ys-core/src/message.rs`
- **关键约束**：
  - 落盘形态 `{"Image":{"path":"/tmp/a.png","detail":"high"}}`；`detail: None` 时该键**不出现**。
    小写形态由 U1 的 `#[serde(rename_all = "lowercase")]` 保证（D15 / C5）；本单元只负责
    外层外部标签，不要在这里再写一遍 rename。
  - **既有变体字节不变**（A19 的 golden 首行必须逐字节相同）。
  - 加完变体后 `check --workspace --all-targets` **仍然是零 error**（§4.1 的地基）——
    如果这里出现了 error，说明有消费点不是 `_` 臂而是显式穷尽，需记入 as-built。
- **验收**：
  - `cargo test -p ys-core` 全绿。
  - serde round-trip：`Image` 块落盘 → 读回相等；`detail: None` 无该键。
  - `apps/coding-agent/tests/debug_log.rs`（golden 守着 A19）全绿。

### U4 —— 块集合白名单测试**先写红**（步 4）

- **目标**：把「当前已处理的 `ContentBlock` 变体集合」写成**断言**，让新增变体无法静默通过。
  这是 D12 的落地：`#[non_exhaustive]` 不提供守护，只能靠测试。
- **非目标**：不改任何生产渲染逻辑（本单元只加红测试）。不新增测试框架。
- **涉及文件**：
  - `crates/ys-tui-coding/src/transcript.rs`（照 `:538` `p2_3_only_text_and_tool_use_produce_lines`
    的形态扩一条「已处理块集合」用例）
  - `crates/ys-tui-coding/src/app.rs`（`message_text` 的块集合用例）
- **关键约束**：
  - 本单元**必须先红**（此时 `Image` 变体已存在但渲染未实现）。红 = 白名单不符。
  - 白名单的语义是「**我们已经决定怎么处理**这些变体」，不是「编译器只允许这些」。
    将来加变体时这个用例变红 = 提醒更新消费点，这正是目的。
  - 用例必须断言**块的种类集合**，不是渲染出来的字符串 —— 否则改文案会误红。
- **验收**：`cargo test -p ys-tui-coding` ⇒ **指定用例红**（这是本单元唯一正确的
  结束状态）。在执行计划里 B 档的 U9 完成后它转绿。

### U5 —— 适配器 `media.rs` + `Content` 枚举 + golden（步 5）

- **目标**：
  1. 新文件 `adapters/model-openai-compatible/src/media.rs`：`resolve_image(path, &MediaCache)
     -> Result<Arc<str>, MediaError>`、`MediaCache`（`std::sync::Mutex<HashMap<…>>` + LRU）。
  2. `request.rs`：`ChatMessage.content: Option<Content>`，`Content` 是
     「字符串 | 块数组」的两种 wire 形态（K1 的解法）。
  3. `Cargo.toml` 显式声明 `base64 = "0.22"`（E2）。
  4. golden 测试：纯文本请求的**线上字节不变**。
- **非目标**：不做门控（U6）、不碰 `:1311`/`:1316` 的翻译逻辑（U6）、不注册工具（U7）。
- **涉及文件**：
  - `adapters/model-openai-compatible/src/media.rs`（新）
  - `adapters/model-openai-compatible/src/request.rs`
  - `adapters/model-openai-compatible/Cargo.toml`
  - `adapters/model-openai-compatible/src/lib.rs`（`mod media;` + 三处 `ChatMessage {}` 构造点
    的类型适配）
- **关键约束**：
  - **锁边界**（§3.2）三步顺序不得挪；`std::sync::Mutex`（不是 `tokio::sync::Mutex`）。
  - **单图 32 MiB 是 base64 编码后的字节数**（C8 / D18）：先按 `原图 len × 4/3 + 余量` 预估，
    明显超限的直接不进编码；精确判定仍以实际编码后的长度为准。
  - 请求体 48 MiB 与 600 图**不在本单元判**（它们需要跨消息累计，属 `translate_request`，U6）。
    本单元只提供 `MediaError::SingleTooLarge { bytes, limit }`。
  - 缓存 **128 MiB / 64 条** LRU。
  - 缓存键含 `mtime` + `len`（原图改写后不得命中旧条目）。
  - 嗅探读盘量 256 KiB（`DIM_HEAD_BYTES` 同口径）；
  - `Content` 的 serde **不得**改变纯文本请求的字节：纯文本必须序列化成 `"content":"…"`
    而不是 `[{"type":"text",…}]`。这是 golden 用例守的那条线。
  - 编码不 `spawn_blocking`（design.md §3.4 末段）。
  - `MediaCache` 需可注入上限（测试用）：否则累计预算的用例要么真造 20 MiB 文件，要么测不了（C8.6）。
- **验收**：
  - `cargo test -p ys-model-openai-compat` 全绿。
  - golden：同一 `ModelRequest` 在改动前后的 `ChatCompletionRequest` 序列化**逐字节相同**。
  - 缓存上界：灌入超 128 MiB 的条目 ⇒ 总字节 ≤ 128 MiB 且条目数 ≤ 64。
  - 锁外 IO：并发两个同键请求都能完成（不死锁），允许重复计算。
  - **D18 反向用例**：原图 24 MiB（base64 后 ≈32.9 MiB）⇒ 判超限，而不是因「原图 < 32 MiB」放过。

### U6 —— 门控 + 占位 + `:1316` 推送条件（K2 的两半一起做，步 6）

- **目标**：
  1. `compat.rs`：`supports_image_input`（provider 能力位）+ `supports_image(model)`
     （模型 id 可识别性）+ `image_detail_for(model, detail)`；门控**只在一处判**（照 `:186`）。
  2. `lib.rs:1311`：`_ => {}` → 显式 `Image` 分支，累积 `image_parts`，并在此处判
     **请求体 48 MiB 累计预算**与 **600 图上限**（C8 / D19）。
  3. `lib.rs:1316`：推送条件加 `|| !image_parts.is_empty()`。
  4. 降级矩阵（design.md §5 + §5 C3 / C8）：不支持 / 读不到 / 非图片 / 单图超限 /
     累计超预算 / 图数超限 ⇒ **六种**文本占位。
  5. 「消息数守恒」用例。
- **非目标**：不实现 `view_image`（U7）；不改响应侧解析（`:1673` 属 U6 的同文件邻近改动，
  见执行计划 T09）；不写回占位（D11）。
- **涉及文件**：
  - `adapters/model-openai-compatible/src/compat.rs`
  - `adapters/model-openai-compatible/src/lib.rs`（`translate_request` 签名接入 `&MediaCache`、
    `:1311`、`:1316`、以及全部调用点）
- **关键约束**：
  - **K2 两半同批**（§4.3）。任何把 `:1316` 留到「下一批」的拆分都违反 D10。
  - 占位串必须与 §5 C3 + C8.2 的**六条**逐字一致（模型据此判断「该改用 `ocr` 还是换路径」）：
    - 读不到：`` yushan could not read the local image at `{path}`: {error} ``
    - 非法：`` Image located at `{path}` is invalid: {error} ``
    - 格式不支持：`` yushan cannot attach image at `{path}`: unsupported image `{mime}`. ``
    - 单图超限（**编码后**）：`` yushan cannot attach image at `{path}`: too large ({size} > 32 MiB). ``
    - 累计超预算：`` [image too large: /path (would exceed the 48 MiB request budget)] ``
    - 图数超限：`` [image too large: /path (over the 600 images per request limit)] ``
    - **模型不支持图像**：`image content omitted because you do not support image input`
      —— **不带路径**（模型对此无能为力，逐图重复路径只是烧 token）；同一批内只发一次。
  - **占位不写回**（D11）：只存在于出站临时副本；`ContentBlock::Image` 保持原样。
  - 门控 fail-closed：id 无法识别 ⇒ 不发图，走占位（design.md §10 第 6 条）。
  - 门控只在 `supports_image` 判一次；`image_detail_for` 与 `:1311` 分支**不得**各自再判。
  - **预算计数器与 K2 同一处**（C8.4）：判定顺序是「门控 → 单图 → 图数 → 累计 → push」，
    写成两套逻辑就会漏。计数器**只在真正 push 时**累加（降级的图不占预算）。
- **验收**：
  - `cargo test -p ys-model-openai-compat` 全绿。
  - **消息数守恒**：入站 N 条非空消息（含仅 `Image` 的）⇒ 出站 `messages.len() ≥ N`。
  - 变异（手动验证一次，脚本化在 U12）：把 `:1316` 的条件改回原样 ⇒ 守恒用例红。
  - 不支持模型的请求体里**没有** `image_url`，且含占位文本。
  - **累计预算**（C8.6）：把预算改小到 1 KiB，两张 1.5 KiB 的图 ⇒ 第二张降级为占位，
    出站序列化体 < 1 KiB **不需要真造 20 MiB 文件**。

### U7 —— `view_image` 工具 + 注册（步 7）

- **目标**：`adapters/tools-basic/src/view_image.rs`：spec `view_image(path, detail?)`；
  行为 = 解析路径（相对路径以 workspace 根为根，与 `read`/`ocr` 同口径）→ 存在性 + 是文件 +
  嗅探格式 → 返回 `ToolResult { content: 一行元信息, media: vec![Image{path, detail}] }`。
- **非目标**：**不读全文件、不 base64、不缩放**（D2）。不做像素解码。
- **涉及文件**：
  - `adapters/tools-basic/src/view_image.rs`（新）
  - `adapters/tools-basic/src/lib.rs`（`pub mod view_image;` + `pub use view_image::ViewImageTool;`）
  - `apps/coding-agent/src/toolset.rs`（`:26` import、`:107` 注册，紧跟 `OcrTool` 之后）
- **关键约束**：
  - 只读文件头 ≤256 KiB 做嗅探（`DIM_HEAD_BYTES` 同口径）。
  - 失败（不存在 / 不是文件 / 非图片 / 超 32 MiB）⇒ `is_error: true` + 可读原因；
    **不**返回 `media`（降级在适配器，工具层只报事实）。
  - 工具描述**陈述自己的适用场景，不声明独家**（照§5 C2 的原则，逐字）：
    > View a local image file from the filesystem when visual inspection is needed.
    > Use this for images already available on disk.

    第二句（`Use this for images already available on disk.`）照 Codex
    `view_image_spec.rs:44` —— 它同时承担一个歧义消解：模型常拿它试 URL，而本工具只吃本地路径。
  - `detail` 参数的 schema **只允许 `high|original`**，缺省 `high`（D14 / C4）。`low` 对
    「我要看清这张图」没有意义；`ImageDetail` 枚举本身仍保留 `Low`/`Auto`（协议层完整）。
  - **`content` 是一行有界元信息，绝不是 data URL**（C4）：
    `image: /tmp/a.png (image/png, 1920×1080, 1.4 MiB) — attached as pixels, not text`。
  - 工具是**只读**的 ⇒ `is_concurrency_safe()` 可返回 true（与 `read`/`ls` 同组）。
- **验收**：
  - `cargo test -p ys-tools-basic` 全绿。
  - 「不读全文件」+「不含 base64」断言（**同一用例三条**，C4）：造一个 **40 MiB** 的合法 PNG 头
    + 垃圾尾部的夹具，断言 ① `content` 不含 `base64` / `data:` ② `content.len()` 有界
    （路径 + 百字节量级）③ `media` 恰有一条。
  - `detail` schema 的枚举值恰为 `["high","original"]`；传 `low` ⇒ `InvalidInput`。
  - `cargo run -p ys-coding-agent -- --json "看下 /tmp/a.png"` 的父表里出现 `view_image`。

### U8 —— `ys-loop` 折叠 + **两个估算器不同处理**（步 8，C7 修正）

- **目标**：
  1. `basic.rs:1410`：先 push `ContentBlock::ToolResult`，再为每个 `MediaAttachment::Image`
     追加一个 `ContentBlock::Image`（追加进**同一条** `Role::User` 消息）。
  2. `token.rs:12`：`_ => 0` 之前**新增** `Image { .. } => IMAGE_TOKEN_ESTIMATE`（1024）。
  3. `turn_budget.rs:38`：**保持 `_ => 0` 不变**，只补注释（C7）。
- **非目标**：不碰字节（本层不认识 base64/MIME）；不改压缩策略（`compact.rs` 的 `_` 分支已安全）；
  **不把图片 token 纳入 `turn_budget`**。
- **涉及文件**：
  - `crates/ys-loop/src/basic.rs`（`:1410`）
  - `crates/ys-loop/src/token.rs`（`:12`）
  - `crates/ys-loop/src/turn_budget.rs`（`:38`，**仅注释 + 反向测试**）
- **关键约束**：
  - **两个估算器有意不对称**（D8 / C7）：`token.rs` 问「占了多少」，`turn_budget` 问「能腾多少」。
    后者手段只有截断 `ToolResult` 文本，对 Image 返回 `None` ⇒ 计入只会每回合空转。
  - **两个文件各留一段指向对方的注释**，否则下一个维护者必定把它们「修复」成对称。
  - 常量定义一处、两边引用（`turn_budget` 只是不用它），旁写「DeepSeek 官方每图上限 1024」。
  - 折叠顺序：`ToolResult` 在前、`Image` 在后 —— 模型先看到工具的文本说明再看图。
  - 折叠后仍是**一条**消息（不额外 push），否则消息数守恒与既有会话形态都会变。
  - 压缩时被摘要掉的图片**随消息一起丢弃**（design.md §6）：不在 `summarize.rs` 里拼图片。
- **验收**：
  - `cargo test -p ys-loop` 全绿。
  - 折叠：工具返回 1 个附件 ⇒ 该 user 消息 `content.len()` 比无附件时多 1，且最后一块是
    `Image`，且只 push 了一条消息（消息数守恒）。
  - `token.rs`：含 1 个 `Image` 块的 100 字符消息，估算值比不带图时恰多 1024。
  - **`turn_budget.rs` 反向用例**（C7.4）：含 Image 的消息 `tool_result_tokens(...) == 0`，
    且 `plan_elisions(...)` 返回空。这条把「不做」钉成要求。

### U9 —— TUI / subagent / `message_bytes` 渲染（B 档，步 9）

- **依赖**：U13（取文本已收口）、U3。
- **目标**：四处消费点补 `Image` 语义：
  | 位置 | 形态 | 与 U13 的分工 |
  |---|---|---|
  | `crates/ys-tui-coding/src/transcript.rs:157`（`match`）+ `:163`（通配臂） | `[image: /path]` 产一行 | U13 只交了取文本；**产行逻辑**仍在本单元 |
  | `crates/ys-tui-coding/src/app.rs:908` `message_text` | `[image: /path]` 接在文本后 | U13 已将函数体改成 `joined_text(Newline)`；本单元改为「`joined_text` 结果 + 逐个 Image 块的标记行」 |
  | `apps/coding-agent/src/subagent.rs:1824` | 明确渲染，不再是 `[unknown content block]` | 同理（子 agent 报告页） |
  | `apps/coding-agent/src/main.rs:673` `message_bytes` | Image 计入（路径长度即可） | 与 U13 无关（它不取文本，只算字节） |
- **非目标**：**无 IO、无编码、无缩略图**（design.md §0 的最薄一层）。`draw.rs` **不动**（E4）。
  不重写取文本逻辑（那是 U13 的活）。
- **涉及文件**：
  - `crates/ys-tui-coding/src/transcript.rs`
  - `crates/ys-tui-coding/src/app.rs`
  - `apps/coding-agent/src/subagent.rs`
  - `apps/coding-agent/src/main.rs`
- **关键约束**：
  - TUI **只显示路径字符串**，不 stat、不读文件（否则 `replay_lines` 变成 IO 函数，
    回放路径与会话内容耦合）。
  - 尺寸只在**已有嗅探结果**时显示；`replay` 侧不补嗅探（会引 IO）。
  - U4 的白名单用例在本单元**转绿**（若在 U13 之后：U13 已消除了它替代编译器的那半个理由，
    它现在只守着「渲染行为正确」；转绿仍是本单元的验收）。
  - `message_bytes` 只加路径字节：它是「回合输入字节数」的观测值，不是 token 估算。
  - **不得**为了让 `message_text` 显示图片而绕过 `joined_text` 再建一份白名单（那就是把 C6 拆了）。
- **验收**：
  - `cargo test -p ys-tui-coding -p ys-coding-agent` 全绿。
  - U4 的用例转绿（含义转为行为回归）；`p2_3_only_text_and_tool_use_produce_lines` 绿且未被删。
  - replay 与 live 对含 `Image` 块的会话**逐行相等**（`transcript.rs` 既有回放等价用例的扩展）。

### U10 —— `expand_skill_invocation` 保留非 `Text` 块（A 档，步 10）

- **目标**：`apps/coding-agent/src/app_loop.rs:1821` 的重建点不再 `vec![ContentBlock::Text { text: expanded }]`，
  而是「替换首块 Text 为 expanded，**保留**其余块」。
- **非目标**：不改 `/skill-name` 的展开语义、不改 `first_text` 的判定、不改缓存策略。
- **涉及文件**：`apps/coding-agent/src/app_loop.rs`
- **关键约束**：
  - 这是**静默丢数据**的 A 档缺陷：当前一旦用户消息带图（`Image` 块）且首块是 `/skill-x`，
    展开会把图整块丢掉，且没有任何日志。
  - 保留顺序：展开后的 Text 在**原首块位置**，其后块按原序跟。
  - `first_text` 仍是「首块是 Text 才展开」（`Role::User` 之外的输入不展开，语义不变）。
  - 日志里补一条「保留了 N 个非 Text 块」，使这条路径可观测。
- **验收**：
  - `cargo test -p ys-coding-agent` 全绿。
  - 用例：`Message { User, [Text("/skill-x 看图"), Image{path}] }` ⇒ 展开后 `content.len() == 2`
    且第二块仍是同路径的 `Image`。
  - 变异：把实现改回 `vec![Text]` ⇒ 该用例红。

### U11 —— 工具文案：`ocr` 去掉「唯一」+ 父表 fragment + `view_image` snippet（步 11，取代原 v→2）

> **本节取代 design.md §11 的第 11 步**。原步「`SessionMeta.v = 2`」已作废（§5 C1）。
> `crates/ys-session` 因此**零生产改动**：C14 / C15 只是测试，落在 `jsonl.rs` 的 `mod tests` 里。

- **目标**：让两个工具的描述各自陈述作用域，并把被本功能推翻的注释改成当前事实。
- **非目标**：不改工具行为（只改文案）；不改 `ocr` 的任何执行逻辑；不改其它工具的 fragment。
- **依赖**：U7（`view_image` 的名字与描述是同一批契约，得先存在才能写进父表）。
- **涉及文件**：
  - `adapters/tools-basic/src/ocr/mod.rs`（`:138` 的 `spec()` description）
  - `apps/coding-agent/src/prompt.rs`（`:279` 的 `"ocr"` 分支、`:273-278` 的前提注释、
    新增 `"view_image"` 分支、`:913` 的逐字断言、以及 `:900-903` 的说明注释）
- **关键约束**：
  - **两处文案必须逐字一致**（`spec()` 与 `tool_snippet` 是同一句话的两个副本，
    既有纪律要求它们同步）。目标文案见 §5 C2。
  - `:273-278` 的注释现在写着「`ContentBlock` 只有 `Text`/`ToolUse`/`ToolResult`，
    `ChatMessage.content` 是 `Option<String>`」—— **两句都被本功能推翻**（A24）。
    必须重写为当前事实，但**保留**「为什么文案要点明分工」的理由（不写变更叙事）。
  - `:913` 的逐字断言变红是**预期**，必须同步改断言而不是删断言。
  - 新 `"view_image"` 分支的文案与 `ViewImageTool::spec().description` 逐字一致。
- **验收**：
  - `cargo test -p ys-tools-basic -p ys-coding-agent` 全绿。
  - `tool_snippet("ocr")` 与 `OcrTool::new().spec().description` **相等**（跨 crate 一致性断言）。
  - `tool_snippet("view_image")` 不再是 `"Custom tool"`。
  - `rg -c "the only way" adapters/ apps/` ⇒ **0**（旧文案彻底消失）。

### U13 —— 取文本逻辑收口到 `ys-core`（C6 新增，不属于 §11 的任何一步）

- **依赖**：U3（`Image` 变体得先存在，否则 `as_text` 的穷尽 match 少一臂）。
- **目标**：
  1. `crates/ys-core/src/message.rs` 加 `ContentBlock::as_text()`（**穷尽 match、无 `_` 臂**）、
     `TextJoin { Newline, Space, None }`、`Message::joined_text(join)`。
  2. 五处自建白名单（参 §5 C6.2）全部改为调 `joined_text`。
  3. 加一条逐变体断言的穷尽性测试。
- **非目标**：不统一分隔符（D16）；不动 `user_text` 的 `Role::User` 检查与空白过滤；
  不把 `as_text` 用作渲染/翻译的万能入口。
- **涉及文件**：
  - `crates/ys-core/src/message.rs`（新类型 + `mod tests`）
  - `crates/ys-tui-coding/src/app.rs`（`:905` `message_text` 改为一行转发）
  - `crates/ys-tui-coding/src/transcript.rs`（`:152` assistant 分支）
  - `apps/coding-agent/src/sessions.rs`（`:182` `user_text` 的取文本部分）
  - `apps/coding-agent/src/subagent.rs`（`:1769` `text_of`；`text_of` 可整个删掉改调）
  - `crates/ys-loop/src/summarize.rs`（`:106`）
  - `crates/ys-loop/src/compact.rs`（`:374`）
- **关键约束**：
  - `as_text()` 的 match **永久穷尽**（D17）：新增变体必须写显式 `Self::X { .. } => None`，
    **绝不**折成 `_ => None` —— 那等于自废 C6 的全部收益。
  - `TextJoin::None` 与空串的区分：它是**枚举变体**，不是 `""` 字符串；这样调用方无法静默漂移。
  - `user_text` 的语义边界不伸延：`Role::User` + 「至少一个非空 Text 块」两个条件仍在
    `sessions.rs`，新增的 `Image` 块**不影响标题**（这是 E3 的结论，C6 不推翻它）。
  - **不动** `summarize.rs` / `compact.rs` 的「空或全空白响应 = 失败」判定（只换取文本的部分）。
- **验收**：
  - `cargo test -p ys-core -p ys-tui-coding -p ys-loop -p ys-coding-agent` 全绿。
  - 穷尽性测试：逐变体断言 `as_text` 的返回值（`Text` ⇒ `Some`，`ToolResult`/`Image` ⇒ `None`）。
  - `rg -c "ContentBlock::Text" crates/ys-tui-coding/src/app.rs apps/coding-agent/src/sessions.rs apps/coding-agent/src/subagent.rs crates/ys-loop/src/summarize.rs crates/ys-loop/src/compact.rs`
    ⇒ 只剩测试代码里的命中（生产路径不再各自 match `ContentBlock::Text`）。
  - `test_message_text_joins_text_blocks`（`app.rs:2695`）、`p2_3_...`（`transcript.rs:538`）、
    `user_text_skips_non_text_and_non_user`（`sessions.rs:453`）**全部保留且绿**（参 §5 C6.5）。
  - 变异（U12 里落地一条）：把 `as_text` 的 `Self::Image { .. } => None` 折成 `_ => None`
    ⇒ 本身不会变红（编译器也不报），所以**用另一条变异守**：把 `as_text` 的 `Text` 臂
    改成返回 `None` ⇒ 五处的回归用例全红。

### U14 —— 请求体序列化结果塔存（C9 新增，不属于 §11 的任何一步）

- **依赖**：U5、U6（先让图片进入请求体，才能看到序列化开销的真实量级；且 `:1311`/`:1316`
  与 `:654` 同在 `lib.rs`，必须串行）。
- **目标**：惰性塔存序列化字节（`Option<Arc<Vec<u8>>>`），在每次 `send` 前按需序列化；
  在 `:654` 剥掉 `stream_options` 后**置回 `None`**。
- **非目标**：不改重试策略、不改 429/400 分类、不改 `DeltaWatchingSink` 语义、
  **不新增 `bytes` 依赖**（用 `Arc<Vec<u8>>`，C9.4）。
- **涉及文件**：
  - `adapters/model-openai-compatible/src/lib.rs`（`:313-327` `send` 签名、`:446-465` 的循环、
    `:576-582` `attempt` 签名、`:601` 与 `:655` 两个 `send` 调用点、`:654` 失效点）
  - `adapters/model-openai-compatible/tests/retry.rs`（新增 C25 用例）
- **关键约束**：
  - **不变量**：`body` 的有效性绑定「`chat_request` 自上次序列化以来未被修改」。
    全仓唯一修改点是 `:654`，**必须在那行后紧跟一句 `body = None`**。
  - **不得**改成「循环外序列化一次」—— 那样回退分支会发出带陈旧 `stream_options` 的请求，
    `apps/coding-agent/tests/stream_usage_fallback.rs:361` 会红（已存在且正好盯这个）。
  - `.body()` 不自动带 `Content-Type`：`:322` 的显式 header 必须保留（改写时逐字确认）。
  - 序列化失败映射到**既有** `ModelError` 变体。
  - `chat_request` 仍保持 `&mut`（`attempt` 要改 `stream_options`），不因本单元改成 `&`。
- **验收**：
  - `cargo test -p ys-model-openai-compat -p ys-coding-agent` 全绿。
  - **C24 保持绿**：`apps/coding-agent/tests/stream_usage_fallback.rs:361`
    （回退路径两次请求体：第一次含 `stream_options`、第二次不含）。变红 = 失效点漏了/放错位。
  - **C25 新增**：429 触发外层重试 ⇒ 最终成功的请求体与不重试时**逐字节相等**。
  - **反向变异**（入 U12 清单）：删掉 `:654` 之后的 `body = None` ⇒ C24 红。

### U12 —— `scripts/vision-mutation.py` + `verify.sh` 追加（步 12）

- **目标**：照 `scripts/cancellation-mutation.py`（A21）的形态新建变异脚本，注入 **≥9 处** bug，
  断言指定用例变红；并把 `vision-mutation` 追加进 `scripts/verify.sh:55` 的清单。
- **非目标**：不改既有四个变异脚本；不把它加进 `verify.sh --tests-only` 路径（第 2 阶段本就被跳过）。
- **涉及文件**：
  - `scripts/vision-mutation.py`（新）
  - `scripts/verify.sh`（`:55` 一行追加）
- **关键约束**：
  - 注入用**精确文本替换**；锚点不存在或替换后文本不变 ⇒ **报错退出**（绝不允许
    「注入没生效 ⇒ 测试绿 ⇒ 假通过」）。
  - 每个 mutant 前快照、跑完还原；`finally` 兜底。
  - 必须覆盖三处静默点：**K2 的两半**（各一个 mutant）、**K3**（两个估算器各一个）、
    **A 档**（`expand_skill_invocation` 重建 Message）。
  - 至少还有一个 B 档（TUI/白名单）与一个缓存类 mutant（LRU 上界或缓存键去掉 mtime）。
  - **C4 新增**：把 `view_image` 的 `content` 改成内联 data URL（`content = data_url`）
    ⇒ 「`content` 不含 base64」那条用例必须变红。这是防止「把编码偷偷挪回工具层」的护栏（D2）。
  - **C6 新增**：把 `ContentBlock::as_text` 的 `Text` 臂改成返回 `None`
    ⇒ 五个消费方的回归用例全红（`message_text` / `user_text` / `text_of` / transcript / 摘要）。
  - **C7 新增**：把 `turn_budget.rs:38` 的 `_ => 0` 改成 `Image => 1024`
    ⇒ C7.4 的反向用例变红（守「有意不计入」）。
  - **C8 新增**：① 把单图判据从「编码后」改回 `metadata.len()` ⇒ C26 红；
    ② 删掉累计计数器 ⇒ C27 红。
  - **C9 新增**：删掉 `:654` 之后的 `body = None` ⇒ C24 红。
  - 共 **≥15 条**（原 ≥8 + C4 + C6 + C7 + C8×2 + C9 = 14，再加三条冗余覆盖）。
- **验收**：
  - `python3 scripts/vision-mutation.py --list` 列出 ≥15 条。
  - `python3 scripts/vision-mutation.py` 全部「被抓住」。
  - `./scripts/verify.sh` 第 2 阶段把 `vision-mutation` 也跑一遍且通过。
  - 单跑一个 mutant 时若锚点被上游改动破坏 ⇒ 脚本以非 0 退出并指出哪条锚点失效。

## 7. 测试合同（design.md §9 的 18 条 → 本文 C1–C27）

优先级：**P0** = 不做就无法验收（守护承重结论）；**P1** = 降级与边界；
**P2** = 渲染与兼容。变异方式一栏写「去掉/改坏什么会让它变红」——这也是 U12 的 mutant 来源。

> 本文的 `C1`–`C27` 是**测试合同编号**，与 §5 的**偏差编号** E1–E4 / C1–C9 是**两套不同命名空间**
> （同名但不同物，如「本文 C2 = 消息数守恒」与「§5 C2 = `ocr` 文案」）。引用时总是带 §7 或 §5 前缀。
> design.md §9 的 18 条对应本文 C1–C18；C19–C27 是本计划新增
> （C19/C20 = 锁边界与嗅探地基；C21 = 六条占位文案；C22/C23 = C6 收口；
> C24/C25 = 序列化塔存；C26/C27 = 编码后限额与累计预算）。

| # | 测试 | 测试位置 | 变异方式（变红的手段） | 优先级 |
|---|---|---|---|---|
| C1 | 纯文本请求的**线上字节不变** | `adapters/model-openai-compatible/tests/translate_golden.rs`（新，`translate_request` 是 `pub`） | 把 `Content` 改成「总是数组」⇒ 序列化出 `[{"type":"text"}]` | **P0** |
| C2 | **消息数守恒**：入站 N 条非空（含仅 Image）⇒ 出站数 ≥ N | `adapters/model-openai-compatible/tests/translate_image.rs`（新） | 只改 `:1311` 不改 `:1316` ⇒ 出站 `"messages":[]` | **P0** |
| C3 | **块集合白名单**（TUI） | `crates/ys-tui-coding/src/transcript.rs`（照 `:538` `p2_3` 形态）；`app.rs` 各一份 | 加变体不进白名单 ⇒ 红（替代 `#[non_exhaustive]` 守护） | **P0** |
| C4 | `ToolResult` 无 media 时事件 JSON 不变 | `crates/ys-core/src/tool.rs`（`mod tests`） | 去掉 `skip_serializing_if` ⇒ 出现 `"media":[]` | **P0** |
| C5 | Image 块 → `image_url` 块数组 + 正确 mime | `translate_image.rs` | 魔数判断换扩展名（`x.png` 实为 JPEG 的夹具）⇒ mime 错 | **P0** |
| C6 | 不支持图像 ⇒ 文本占位（**不是丢块**），且占位**不带路径** | `translate_image.rs` + `compat.rs` 单测 | 恢复 `_ => {}` ⇒ 占位消失；改成「每图带路径」⇒ 占位条数断言红 | **P0** |
| C7 | 路径不存在 ⇒ 占位含原因、回合不失败 | `translate_image.rs` | 改成 `Err` 直接失败 ⇒ `translate_request` 返回 Err | P1 |
| C8 | **占位不写回会话**：删原图 ⇒ 出站占位但 JSONL 仍是 `{"Image":{"path"…}}`；放回 ⇒ 下回合能送图 | `translate_image.rs` + `crates/ys-session/src/jsonl.rs`（`mod tests`） | 把占位写回 `ContentBlock` ⇒ 第二段送图断言红 | **P0** |
| C9 | token 估算含图片（`token.rs`） | `crates/ys-loop/src/token.rs`（`mod tests`） | 恢复 `_ => 0` ⇒ 差值断言红 | **P0** |
| C10 | **`turn_budget` 有意不计入图片**（反向用例） | `crates/ys-loop/src/turn_budget.rs`（`mod tests`） | 「顺手补上」`Image => 1024` ⇒ 红（C7 / D8 的不对称） | **P0** |
| C11 | `view_image` 产出 media，**不读全文件**，且 `content` **不含 base64**、体积有界 | `adapters/tools-basic/src/view_image.rs`（`mod tests`） | `content` 改成内联 data URL ⇒ 三条断言同时红（U12 的一条 mutant） | **P0** |
| C12 | TUI 渲染 Image 块 | `crates/ys-tui-coding/src/transcript.rs`（与 C3 同族，一条产行一条白名单） | 恢复白名单（去掉 Image 支）⇒ 无行 | P2 |
| C13 | skill 展开保留 Image 块（A 档） | `apps/coding-agent/src/app_loop.rs`（`mod tests`） | 恢复 `vec![Text]` ⇒ `content.len()==1` | **P0** |
| C14 | 老会话（无 Image）可正常回放；无图会话首行仍 `"v":1` | `crates/ys-session/src/jsonl.rs`（`mod tests`） | 若加了 `v→2` 的条件提升 ⇒ 「无图仍 v=1」断言红（反过来说：**C1 把这条实现删了，这条用例就是「不做」的钉子**） | P2 |
| C15 | **图片字节不进会话文件**：全文不含 `base64`，只有路径 | `crates/ys-session/src/jsonl.rs`（`mod tests`） | 把 `Image.path` 换成 `Image.data`（内联字节）⇒ 全文含 `base64` | **P0** |
| C16 | **出站时才读盘**：入历史后删原图，构造请求仍不报错 | `translate_image.rs` | 在入历史时就读盘 ⇒ 入历史即失败 | P1 |
| C17 | **原图改内容 ⇒ 送出字节跟着变**（缓存键含 mtime/len） | `adapters/model-openai-compatible/src/media.rs`（`mod tests`） | 缓存键只用 path ⇒ 第二次仍是旧 data URL | P1 |
| C18 | **缓存有上界**：灌入超 128 MiB 的条目后总字节不超限 | `media.rs`（`mod tests`） | 去掉 LRU 淘汰 ⇒ 总字节超限 | P1 |
| C21 | **六条**占位文案**逐字**（含 C3 的第四条不带路径与 C8 的两条预算占位） | `translate_image.rs` | 改任一条的措辞/换回 design.md 原表 ⇒ 逐字断言红 | **P0** |
| C22 | `ContentBlock::as_text()` 的**变体全集**（`Text` ⇒ `Some`，其余 ⇒ `None`） | `crates/ys-core/src/message.rs`（`mod tests`） | 把 `Text` 臂改成 `None` ⇒ 红；真正的守护是编译期（见下） | **P0** |
| C23 | 五处消费点的**行为回归**（`joined_text` 各分隔符） | `app.rs:2695` / `transcript.rs:538` / `sessions.rs:453` 等（均为**已有用例**） | 改 `as_text` 或 `joined_text` ⇒ 五处同时红 | **P0** |
| C24 | **回退路径两次请求体不同**（防复用陈旧 body） | **已存在**：`apps/coding-agent/tests/stream_usage_fallback.rs:361`（`:402-410`） | 删掉 `:654` 的 `body = None` ⇒ 第二次请求仍带 `stream_options` | **P0** |
| C25 | **重试间复用**：429 重试后成功的请求体 ≡ 不重试时（逐字节） | `adapters/model-openai-compatible/tests/retry.rs`（新增） | 序列化时误带时间戳/顺序不稳定 ⇒ 逐字节不等 | P1 |
| C26 | **单图按编码后判**：24 MiB 原图（base64 后 ≈32.9 MiB）⇒ 拒 | `media.rs`（`mod tests`） | 改用 `metadata.len()` 直接判 ⇒ 放过该图 | **P0** |
| C27 | **累计预算**：预算调小到 1 KiB，两张 1.5 KiB 图 ⇒ 第二张降级 | `translate_image.rs` | 不重置/不检查计数器 ⇒ 两张都上图 | **P0** |

### 7.1 不在 design.md §9 里、但实施时必须加的两条

| # | 测试 | 位置 | 理由 |
|---|---|---|---|
| C19 | `MediaCache` **锁外读盘**：并发两个同键请求都完成、不死锁 | `media.rs` | design.md §3.4 的锁边界是承重约束，没有用例守着它就只是一段注释 |
| C20 | `sniff_image` 按内容判格式（`x.png` 实为 JPEG） | `crates/ys-core/src/media.rs` | D3 的判据；也是 C5 的下层地基 |

### 7.2 `ImageDetail` / `ImageFormat` 的 serde 契约（C5 引入）

并入 C4（`tool.rs`）与 C20（`media.rs`），不另开编号：

| 断言 | 位置 | 变红手段 |
|---|---|---|
| `ImageDetail::High` 序列化为 `"high"` | `crates/ys-core/src/media.rs` | 去掉 `rename_all = "lowercase"` ⇒ `"High"` |
| `ImageDetail::default() == High` | 同上 | 把 `#[default]` 挪到 `Auto` ⇒ 红 |
| `ImageFormat::Png` 序列化为 `"png"` | 同上 | 同上 |
| `{"Image":{"path":…}}` 无 `detail` 键 | `crates/ys-core/src/message.rs` | 去掉 `skip_serializing_if` ⇒ 出现 `"detail":null` |

## 8. 风险与回滚

### 8.1 风险清单

| # | 风险 | 触发条件 | 影响 | 缓解 |
|---|---|---|---|---|
| **R1** | **只改 `:1311` 漏 `:1316`** | 把 K2 拆成两个批次 | 图片整条消失，且「看起来像修过了」 | C2 守恒用例 + U12 的两个独立 mutant + §4.3 的表格写进任务描述 |
| **R2** | 漏改某个 `ContentBlock` 消费点 | 靠 grep 自造清单 | 静默丢块/渲染 `[unknown content block]` | C3 白名单用例（先红）+ design.md §8 逐文件清单 + U9 的「白名单转绿」验收 |
| **R3** | 迁移期工作区长时间不编译 | U2 的编译单元跨 6 个任务 | 无法增量验证其它改动 | 同一波次并行做完（文件集互不相交）；验收按「编译单元关闭」算 |
| **R4** | `Content` 的 serde 让纯文本请求变成数组 | 直接给 `Content` 加 `#[serde(untagged)]` 而不看输出 | 所有 provider 的请求体形态变化，可能 400 | C1 golden 逐字节断言（U5 先写） |
| **R5** | 锁内含 IO | 为了「命中率」把读盘挪进临界区 | 子 agent 并发看图串行化，32 MiB 读盘在锁内 | §3.2 三步顺序 + C19 并发用例；review 时把 `media.rs` 的锁块单独看一遍 |
| **R6** | 占位写回会话 | 「省得每次重算」 | 原图补齐后永不重试送图 | C8 的两段式用例（删图 → 放回）+ D11 写进任务描述 |
| **R7** | 第二个 token 估算器漏改 | 只搜到 `token.rs` | 压缩不触发、回合预算低估；长会话被图片撑爆上下文 | C10 单独一条 + U8 的「两个文件同单元」+ U12 两个 mutant |
| **R8** | 32 MiB 图的 base64 撑爆请求 | 用户给 4K Retina 全屏截图 | 请求体数百 MB，provider 直接拒 | 32 MiB / 48 MiB / 600 图三道硬上限；超限走占位（**不静默缩**） |
| **R9** | 模型级门控误判 | 按 id 猜 `flash` 之类 | 不支持图像的模型收到图 ⇒ 400 | fail-closed（无法识别 ⇒ 不发图）+ `compat.rs` 单测钉住各 provider 的取值 |
| **R10** | TUI 回放变成 IO 函数 | 为了让尺寸显示去 stat | 回放依赖磁盘状态，`replay == live` 的等价性破坏 | 尺寸只用「已存在的嗅探结果」；`transcript.rs` 的 replay 等价用例守着 |
| **R11** | `expand_skill_invocation` 修成「保留所有块但顺序乱」 | 重建时把 Image 放到 Text 之前 | 模型先看图后看指令，skill 展开语义变味 | C13 断言「展开 Text 在原首块位置」而不只断言「块还在」 |
| **R12** | `ocr` 文案仍说「唯一能看图」 | 只改了 `spec()`、漏了 `prompt.rs` | 模型对着截图不去调 `view_image`，而是要用户复述 | U11 的跨 crate 一致性断言 + `rg "the only way"` ⇒ 0 |
| **R13** | `prompt.rs:273-278` 的旧前提注释留着 | 只改文案不改注释 | 下一个人据此以为「没有多模态字段」，把新代码写错 | U11 的非目标明确排除行为改动，但注释**必改**；review 时单看这个文件 |
| **R14** | `as_text()` 被后人折成 `_ => None` | 「反正全是 None，简化一下」 | C6 的编译器守护**静默失效**，下一个变体又变成靠人工 | C22 的逐变体断言 + `?` 函数上方的注释（「全臂展开是契约」）+ review 时单看这个函数 |
| **R15** | 收口只做了一半 | 改了 `message_text` 漏了 `compact.rs` | 两份白名单变四份，比改前更难维护 | `rg` 验收命令（U13）列出全部命中；长的一眼就能看出哪处没改 |
| **R16** | 收口引致分词符漂移 | 把 `user_text` 的空格错输成 `Newline` | 会话标题里出现换行，`/resume` 列表排版错 | C23 逐个调用点断言分隔符（标题用例已存在） |
| **R17** | **把序列化提到循环外一次算完** | 「重试时请求体又不变」的直觉 | stream_options 回退分支发出带陈旧字段的请求，端点持续 400 | C24（**已存在**的 `stream_usage_fallback.rs:361`）直接变红；C9.2 的核实结果写进任务描述 |
| **R18** | 单图限额按原图判 | design.md §3.4 未说明判断对象 | 24 MiB 原图每过（实际编码后 32.9 MiB），请求被端 400 | C26；D18 写进任务描述 |
| **R19** | 48 MiB 与 32 MiB 两条限额未协调 | 两张 32 MiB 就是 64 MiB | 请求体超预算，整条请求被拒 | C27 的累计计数器（D19）；预算判定与 K2 同处 |
| **R20** | 为「修复不对称」把图片计入 `turn_budget` | 维护者看到 `token.rs` 改了而 `turn_budget.rs` 没改 | `plan_elisions` 每回合无效循环，预算永远压不下来 | C7.4 反向用例（把「不做」钉成要求）+ 两个文件各留指向对方的注释 |

### 8.2 回滚策略

本特性是**纯增量**的：新变体、新字段（带 `skip_serializing_if`）、新文件、新工具。
因此回滚可以按单元粒度做，不必整体回退。

| 回滚点 | 操作 | 副作用 |
|---|---|---|
| U7（`view_image`）出问题 | 从 `toolset.rs:107` 移除注册 | 模型不再能取图；已落盘的 `Image` 块仍在历史里（由 U6 的占位兜住） |
| U6（门控/占位）出问题 | `supports_image_input` 全部置 `false` | 所有图降级为占位串；回合不失败，可继续用 `ocr` |
| U5（缓存单点）出问题 | 把 `MediaCache` 换成「每次重读」的空实现（保留类型） | 性能退化，功能不变 |
| U11（工具文案）出问题 | 只回灌文案，不动代码 | 描述回到「唯一能看图」⇒ 模型不再主动调 `view_image`（退回只靠 `ocr`） |
| **无需回滚**（C1） | `SessionMeta.v` 从未被改过 | 无回滚点：`v` 恒为 1（这正是删掉该步的主要收益之一） |
| 整体回退 | `git revert` 单一 PR（本特性应落在一个 PR 里） | 会话里已有的 `{"Image":…}` 行在旧二进制里 `skipped_lines += 1`（该条消息消失，design.md §7 已写明） |
| **U13（取文本收口）出问题** | 把五个调用点改回各自的内联 match | 依赖 `ys-core` 的 `as_text` 也可一并删（无外部使用者）；**但会永久失去 C6 的编译器守护**，不建议回退 |
| **U14（序列化塔存）出问题** | 把 `send` 改回接 `&ChatCompletionRequest` 并 `.json()` | 行为回到当前（正确但多付序列化）；风险低于「带着陈旧 body 发出请求」 |
| **C7（`turn_budget` 不计图）出问题** | 把 `Image => 1024` 加回去 | 只影响闸门效率，不影响会话内容；但会让 `plan_elisions` 做无效循环 |

**不可回滚的部分**：已经写进用户会话文件的 `{"Image":…}` 行。旧二进制读它时会跳过该条消息。
这是 design.md §7 明确接受的代价，写进 as-built。

### 8.3 发布顺序建议

1. U1–U4 合入（`ys-core` 纯数据 + 白名单红测试）——**此时功能不可见**，但白名单测试是红的，
   这是有意为之：它声明「已知有未处理的消费点」。若不允许长红，可把 U4 与 U9 合并成同一 PR。
2. U5–U8 合入（适配器 + 工具 + 循环）——**功能可用**，非图像模型自动降级。
3. U9–U14 合入（渲染 + skill 展开 + 工具文案 + 取文本收口 + 序列化塔存 + 变异脚本）——
   **可观测性与守护补齐**。

> U13（取文本收口）与 U14（序列化塔存）在发布顺序上均可**独立于本特性**先落到主分支：
> 前者带来的编译器守护对后续任何新变体都有益（包括不相关的变体）；后者在无图时也是纯收益
> （重试不再重复序列化）。若想缩小本 PR 的审阅体量，两者都可单独成 PR。

## 9. 影响面与工作量

| crate / 目录 | 文件数 | 性质 |
|---|---|---|
| `crates/ys-core` | 4（含 1 新建） | 纯数据 + 纯函数 + **取文本收口（C6）** |
| `crates/ys-loop` | 4 | 块结构转换 + 两处估算 + 摘要/压缩两处改调 |
| `crates/ys-session` | 2（**仅测试**） | 零生产改动（C1 删了 `v→2`）；C14/C15 落在 `mod tests` |
| `crates/ys-tui-coding` | 2 | 渲染（无 IO）+ `message_text` 改调 |
| `adapters/tools-basic` | 4（含 1 新建） | 新工具 + `ocr` 文案 + 构造点迁移 |
| `adapters/tools-web` | 2 | 构造点迁移 |
| `adapters/model-openai-compatible` | 5（含 1 新建 + `Cargo.toml`） | **唯一 IO 点** + 门控 + 翻译 + 序列化塔存 |
| `apps/coding-agent` | 7 | 渲染 + 注册 + A 档修复 + 工具文案 + `user_text` 改调 + 构造点 |
| `scripts/` | 2（含 1 新建） | 变异测试 |

**新增依赖**：1 条（`base64 = "0.22"`，已在 `Cargo.lock:44`，E2）。**新增 crate**：0。
**新增公开 API**：`ys_core::{ImageFormat, ImageDetail, sniff_image, MediaAttachment, TextJoin}`、
`ToolResult::text`、`ContentBlock::as_text`、`Message::joined_text`（共 8 个）。
**trait 签名变更**：0。

## 10. 本轮不做（写进 as-built 的已知边界）

1. 路径是**引用不是快照**：原图改名/删除/改写后，历史里的那张图复原不出来（design.md §3.7）。
2. 不做本地缩放：>32 MiB 直接拒绝，用户需自己压（§10 第 3 条）。
3. 不做发送去重：同一张图多回合各占 1024 token（§10 第 4 条）。
4. TUI 不能粘贴图片：`view_image` 需要路径（§10 第 5 条）。
5. 模型级门控靠 id 识别，判据待实测收紧；无法识别即 fail-closed（§10 第 6 条）。
6. 老版本读含 `Image` 的会话 ⇒ 该条消息被跳过（本设计改不了老二进制）；**不靠版本号区分**（C1）。
7. **图片 token 不进 `turn_budget` 闸门**（C7）：图片体积治理归 `compact`。
8. **累计预算超限时「当前及之后」全部降级**（C8）：不做「跳过大图保留小图」的贪心选图。
9. `docs/design-final/vision-input.md`（as-built）**不在本计划内**——本计划只交付设计与执行两份文档；
   as-built 在实现完成后按 `docs/design-final/` 的既有体例另写，需包含本文 §5 的 E1–E4 / C1–C9
   与本节的 9 条边界。
