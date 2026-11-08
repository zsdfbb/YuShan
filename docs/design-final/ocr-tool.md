# OCR 工具 — As-built

> 性质：**实际建成记录**。设计见 [`docs/arch/ocr-tool/design.md`](../arch/ocr-tool/design.md)，
> 决策见 [`docs/adr/0068-ocr-tool.md`](../adr/0068-ocr-tool.md)，
> 执行计划见 [`docs/exec-plans/completed/2026-11-04-ocr-tool.md`](../exec-plans/completed/2026-11-04-ocr-tool.md)。
> 日期：2026-11-04

## 一句话

`ocr` 把图片里的文字**连同像素坐标**取成文本，补上了仓库唯一的图片输入通道
（`ContentBlock` 只有 `Text`/`ToolUse`/`ToolResult`，`ChatMessage.content` 是
`Option<String>`，均无多模态字段）。macOS 上 Vision 优先、`tesseract` 兜底；其余平台只有
`tesseract`。范围限定**中英文**，PDF 不做（留给 `pdf-analysis` skill）。

## 落点

```
adapters/tools-basic/src/ocr/mod.rs      1232 行  流水线编排 + spec + notice + 头尺寸读法
adapters/tools-basic/src/ocr/tsv.rs       542 行  纯函数：parse_tsv / filter / render / render_at
adapters/tools-basic/src/ocr/engine.rs    514 行  Kind / OcrEngine / Spill（探测 + argv + 落盘）
adapters/tools-basic/src/ocr/vision.swift 239 行  macOS Vision 后端，include_str! 内嵌
adapters/tools-basic/src/lib.rs                    pub mod ocr; + pub use ocr::OcrTool;
apps/coding-agent/src/toolset.rs                  注册进 base_tools + EXPECTED_ORDER + 4 处 len()
apps/coding-agent/src/assembly.rs                 explore 准入（补集推导 + 注释约定）
apps/coding-agent/src/prompt.rs                   tool_snippet 的 "ocr" arm
apps/coding-agent/src/debug_log.rs                is_command_tool / command_fingerprint / summarize_ocr
docs/design.md                                    §10 默认工具表与 §12 元数契约
```

`ys-tool` / `ys-core` / `ys-session` **一行未改**。**零新增 Rust 依赖**——引擎一律 spawn
外部进程，图像尺寸只读文件头（不解码）。

### 四个文件的分工

| 文件 | 职责 | 关键抽象 |
|---|---|---|
| `ocr/mod.rs` | spec、参数校验、流水线编排、notice 拼装、可观测性字段。**不含后端细节**，不 spawn 之外的 IO 语义 | `OcrTool`、`Args` |
| `ocr/tsv.rs` | TSV → `Vec<OcrLine>` → 过滤 → 渲染。**纯函数、零 IO、零 tokio** | `parse_tsv`、`filter`、`render`、`render_at`、`Granularity`、`FilterStats`、`Header` |
| `ocr/engine.rs` | 后端探测、argv 构造、候选链、落盘。**同步、零 tokio**（不 spawn、不解析） | `Kind`、`OcrEngine`、`Spill` |
| `ocr/vision.swift` | Vision 调用 → 归一化坐标 → 同列序 TSV | — |

## 流水线（`OcrTool::call` 的实际步序）

```
① 参数解析 (serde) → validate（空 / 不存在 / 不是常规文件 三态）
② engine.candidates() 纯路径扫描，链空 ⇒ Execution + 两平台安装命令
③ 逐候选 run_bounded spawn，成功即停；全失败 ⇒ 拼接**每个**候选的原因
④ parse_tsv → filter → visible_rows → 组 Header → render_at
⑤ 落盘判定：word_boxes 或 > 8 KiB ⇒ Spill::write 全量；回显再走 head(2000 行 / 50 KiB)
⑥ notice：空结果三分 + 落盘临时性 + 截断说明 + 语言包缺失
```

`is_error` 只在 ①②③ 的失败上置 `true`；④⑤⑥ 一律 `is_error=false`（「图里没字」是**有效
观测**，不是工具故障）。

## 输出格式契约

以 [`docs/arch/ocr-tool/design.md`](../arch/ocr-tool/design.md) §1 为准，本节照抄其定义。
**实现与设计逐字节一致**，由 `tsv.rs` 的两条 golden 测试守住
（`golden_fixture_row_level` / `golden_fixture_word_level`）。

**头行**（三种粒度恒有，`#` 开头避免与坐标行混淆）：

```text
# <basename> <W>x<H> via=<tesseract|vision> lang=<生效值> lines=<N> filtered=<M>[ degraded=<原因>]
```

- `via=` 是**实际成功**的后端；`lang=` 是**实际生效**的语种参数。
- Vision 后端下 `lang=n/a`（`OcrEngine::header_lang` 单点决定，`mod.rs` 不另写）。
- `degraded=` 只在发生降级时出现，原因三选一：`vision-failed` / `lang-missing` /
  `word-boxes-unsupported`。**as-built 只产出 `lang-missing`**（由 stderr 的
  `Failed loading language` 惰性判定，不进进程状态）。

**行级**（默认，`word_boxes=false`）：

```text
<line_no> (<x>,<y>,<w>,<h>) conf_min:<conf> <text>
```

**词级**（`word_boxes=true`）：

```text
<line_no>.<word_no> (<x>,<y>,<w>,<h>) conf:<conf> <text>
```

逐字节 golden fixture（`tsv.rs` 内联，改动即红）：

```text
# shot.png 1200x900 via=tesseract lang=chi_sim+eng lines=2 filtered=0
1 (62,49,890,28) conf_min:96 第三章 配置文件说明
2 (61,116,906,26) conf_min:91 应用程序启动时会依次读取以下三个位置的配置文件，
```

```text
# shot.png 1200x900 via=vision lang=n/a lines=1 filtered=0 degraded=word-boxes-unsupported
1.1 (62,49,890,28) conf:0 第三章
```

`line_no` 是**本结果序号**（从 1 递增），不是 tesseract 的 `line_num`。坐标为**原图像素、
左上原点**（Vision 的归一化左下坐标在 Swift 侧一次性换算，Rust 不做二次变换）。
**内联与落盘格式完全相同**——落盘只换投递方式，不换格式。

## 关键 as-built 决策

以下各条是**实现期定下的**，设计文档里没有、或形态不同。

### 1. `OcrTool::with_engine(OcrEngine)` —— 测试注入

生产走 `OcrTool::new()` 里的 `OcrEngine::detect()`（进程 `PATH` 纯路径扫描，不 fork、不跑
`--version`）。测试经 `with_engine` 注入指向假后端目录的 `OcrEngine::detect_from(path)`，
配合 `test_support::fake_bin` 造假的 `tesseract` / `swift` ⇒ **开发机不装任何引擎也能跑全部
流水线用例**，Linux CI 不可能有 `swift` 也不再是障碍。

选构造器注入而非 env：无 `env_lock`、无并行污染、无 `EnvRestore`。

### 2. `OcrTool::with_workspace(PathBuf)` —— 相对路径解析根

与 `ReadTool::new(workspace)` / `LsTool` 同形。`validate` 里 `workspace.join(raw)`
（`Path::join` 对绝对路径原样返回）⇒ 绝对与相对两条路径共用一次解析。
`workspace` 与 `engine` 都是私有字段，装配端只经这两个构造器设置。
读图像头时给足 `DIM_HEAD_BYTES = 256 KiB`（TIFF 的 IFD0 可能离文件头较远）。

### 3. 头行 `WxH` 只读文件头，**不解码像素**

`image_dims` 读前若干字节 → `dims_from_header` 纯函数按魔数分派：PNG（IHDR，大端 u32）、
JPEG（扫 SOFn 段）、GIF（逻辑屏幕，小端 **u16**）、BMP（`BITMAPINFOHEADER`）、WebP（VP8/VP8L/VP8X
三形态）、TIFF（`BigTIFF` 与否 + 字节序）。

**认不出或读不到一律降级 `0x0`**，绝不失败——尺寸只是给坐标系一个参照，
用结果坐标的上界冒充原图尺寸会让「右上角」这类判断静默错位，但拿不到尺寸也不该让整个
OCR 失败。解码像素要引图像库（违零新增依赖），而解压炸弹的防线另有其道（见下「与设计的偏差」）。

扩展名白名单与 `spec` 的 `png/jpg/jpeg/webp/gif/bmp/tiff` 同集。

### 4. 落盘失败 ⇒ 退回内联 + notice，**不报 error**

`spill_if_needed` 返回 `None` 有两条路径：本来就不该落盘（行级且 ≤ 8 KiB），以及
**`Spill::write` 失败**。后者推一条
`could not write the full result to <dir>: <e>` 的 notice，然后仍走内联——此时内联就是
**唯一**一份产物，故回显照旧受 `head` 截断，截断本身可见（`record_truncation`）。

落盘是**投递方式**，它失败不等于工具失败。同理 `image_bytes` 读不到时**退化为空字节**而非
报错：所有同类失败共用一个名字，覆盖写仍然安全（写的本来就是结果本身）。

### 5. `stop != Eof`（超时 / 中止 / 超限）一律算失败，且分开措辞

`attempt_error` 按 `Bounded.stop` 四分支给原因：

| `stop` | 措辞 |
|---|---|
| `TimedOut` | `<label> timed out after 60s` |
| `Aborted` | `<label> was aborted` |
| `Cap` | `<label> produced more than 1024 KiB of TSV` |
| `Eof` + `code == 0` | `None`（成功，采用本后端） |
| `Eof` + 非 0 / 信号 | `<label> (<prog>) failed: exit <code>: <stderr 单行摘要>` |

理由：`stop != Eof` 是**我们杀的**，此时退出码不承载语义；笼统报「引擎坏了」会让模型
在超时后反复重试同一张图。stderr 摘要压空白并截到 240 字符——多行原文会把错误正文撑成一页。

### 6. `Spill` 哈希：`image_bytes ‖ 0 ‖ lang ‖ 0 ‖ granularity`

`fnv1a64` 手写（8 行），不用 `DefaultHasher`：后者跨 Rust 版本**不保证稳定**，升级工具链后
同一张图会落到另一个文件，目录里堆满「同一张图的另一份」。

- 哈希对象是图像**内容**而非路径 ⇒ 同一张图换个文件名不多一份。
- `lang` 只在它真正生效的后端上参与（`OcrEngine::lang_affects_cache`：Tesseract `true`、
  Vision `false`）⇒ Vision 下同图不同 `lang` 落同一文件。
- 一个 `\0` 在两处：`image_bytes` 与 `lang` 之间、`lang` 与 `granularity` 之间（`granularity`
  取单字节 `w` / `l`）。**分隔符不可省**——否则 `lang="a", word_boxes=true` 与
  `lang="a\0w"` 这类拼接会撞到同一个字节流。
- 命名 `<stem>-<16hex>.txt`；先写唯一 tmp（`<name>.<pid>.<进程内原子序号>.tmp`）再 `rename`
  ⇒ 读者永不见半个文件，这是 `is_concurrency_safe = true` 的前提。

### 7. `command_fingerprint` 与落盘哈希的**已知不一致**

debug 日志的 `command_fp` 对 `ocr` 取
`path ‖ 0 ‖ lang ‖ 0 ‖ granularity`（`debug_log.rs::command_fingerprint`），与
`engine.rs::file_name` 声称「同一算法、同一输入 ⇒ 一个数字双重索引」。

**实际上只有「同路径同参数」时两者才相等**：

| 侧 | `path` 分量 |
|---|---|
| debug 指纹 | **路径字符串**（脱敏后参数的 `path` 原值） |
| 落盘哈希 | **图像内容字节**（`image_bytes`） |

⇒ 同一张图换名（`a.png` → `b.png`）或同一路径内容变了，两个数字**不同**；同路径同参数时
才逐位相等，此时「一个数字同时索引重复输入与落盘产物」才成立。

这是**必须写明的已知限制**，不是可修的 bug：debug 侧在 `summarize_tool_args` 里拿不到文件
内容（只拿得到 JSON 参数，且脱敏在前），读内容会让可观测性路径产生 IO 依赖。三处注释
（`debug_log.rs:1101`、`debug_log.rs:2601`、`engine.rs::file_name`）都描述了这条
「同一算法」的意图，实际语义应按上表理解。

### 8. 其余实现期定死的小项

- `DEFAULT_LANG = "chi_sim+eng"`，`word_boxes` 缺省 `false`；`path` **无默认**（`required` 在
  解析层同样成立）。
- `--psm` 固定 `6`，`--oem` / `dpi` 不暴露。`psm 7` 在多行图上**静默吐空**，是读图工具最坏
  的失败模式。
- **`argv` 必须在 image 后紧跟 `stdout`**：tesseract 是
  `tesseract imagename outputbase [options...] [configfile...]`，第二个位置参数是**输出基名**。
  缺了它，`-l` 被当成基名 ⇒ 结果写进 `./-l.tsv`、其余参数被当 configfile、语言退化成默认，
  而 **exit 仍是 0 且 stdout 全空**。`stdout` 放进 options 之后同样无效（实测四种形态，
  只有 `img stdout -l L --psm 6 tsv` 与 `-l L --psm 6 img stdout tsv` 有输出）。由
  `engine::tests::tesseract_stdout_is_the_second_positional` 守住。
- **引擎零输出 ⇒ 失败，不是「图中无字」**：stdout 里没有任何非空白字节时按 `is_error` 报错
  并附 stderr 单行摘要，**整份替换**渲染结果（否则会先冒出误导性的 `lines=0 filtered=0` 头行）。
  判据**按后端限定**：tesseract 即使面对纯白图也仍吐头行 + `level 1` 记录，零输出只可能是
  引擎故障；`vision.swift` 不吐头行、空图本就零字节，故 Vision 的零输出仍是正常空结果。
  不能用 `exit code` 区分（两者都是 0）。
- `Tool::timeout()` **刻意不实现**：`None` ⇒ 外层回落 `RunLimits::bash_timeout`（300s），
  内层 `search::DEADLINE`（60s）+ `BYTE_CAP`（1 MiB）。实现时必须从 `ctx.tool_budget` 推导
  内层（照 `bash.rs::effective_deadline`），绝不能返回与内层相等值——内外层相等 ⇒ 外层先赢 ⇒
  只有 `kill_on_drop` 生效 ⇒ `swiftc` 编出的 `swift-frontend` 孙进程漏网成孤儿。
- `parse_tsv` 容错：列数不足 12 的行、前 11 列解析不出的行、词记录出现在任何行记录之前的行
  **一律跳过不报错**（引擎吐的脏行不该让整个工具失败）；`text` 是第 12 列起的全部剩余内容
  重新 `join("\t")`（引擎若漏了 `\t` 这里兜住）。
- `conf` 解析：真机 tesseract 吐**浮点**（如 `96.4`）和页/块级的 `-1`；`-1` 折算为 `0`，
  `>100` 拒绝，浮点取整。
- `filter` 只丢**纯空白**与 **`w < 6` px**。**`conf == 0` 不是过滤条件**——实测 `conf: 0`
  的行是真文本，按置信度过滤会静默吞掉整段结果。被滤行数进 `filtered=`，它是空结果判定的
  唯一判据。
- 头行的 `lines=` 是**本粒度实际渲染的行数**（`visible_rows` 按粒度重数一遍），而不是
  `FilterStats::kept`。`tsv::Header::lines` 的字段注释写的是「即 `FilterStats::kept`」——
  该注释只对「行级且无词记录」的情形准确；词级下 `kept` 把两种粒度一起计数，直接用它会让
  头行的 `lines=` 与下文行数对不上。以 `mod.rs` 的 `visible_rows` 为准。
- 落盘目录恒为 `std::env::temp_dir()/yushan-ocr/`，**不建 `YUSHAN_OCR_DIR`**。
- 结果文件的临时性在 notice 里自述（`TEMP_FILE_WARNING`）：不说这句，`--resume` 之后 `read`
  该路径得到 `not found`，模型无法区分「被系统清理」与「OCR 失败」。

## 与设计的偏差

1. **`swiftc` 缓存（design §3.5）未实现**。`vision.swift` 是**源码资材**，经
   `include_str!` 内嵌进 Rust 二进制的意图仍在（这是「源码即资材、产物按源码哈希命名」的
   设计前提），但 as-built **未落地编译缓存**：`VISION_NAMES = ["vision", "swift"]` 是纯
   路径扫描，故 mac 上真正的 Vision 通路依赖**环境里已有**的 `vision` 二进制或 `swift`
   解释器直跑源码。首次编译失败也不在此判定（无编译步骤 ⇒ 无此分支）。
   `engine.rs:50` 的注释「结果与**将来的** `bin/`」如实保留了这一状态。
2. **40 MP 解压炸弹防线未实现**。design §3.3 列了「像素 > 40 MP ⇒ `InvalidInput`」，as-built
   的 `validate` 只做三态（空 / 不存在 / 不是常规文件），**无尺寸上限、无 20 MB 文件上限、
   无扩展名白名单校验**。理由：像素上限要求解码头（`WxH` 恰好只读了头），而文件大小上限
   会挡掉合法的大截图；真正的资源防线是内层 `DEADLINE` + `BYTE_CAP`。这是一处**有意的
   简化**，不是漏项。
3. **`degraded=vision-failed` / `word-boxes-unsupported` 未产出**。as-built 只产
   `lang-missing`（`DEGRADED_LANG_MISSING` 是 `mod.rs` 里唯一的 `degraded=` 常量）：候选链失败
   走「拼接每个候选的原因」的 error 路径（而非降级到下一个后端并打标记）；Vision 无词框时
   在 **Swift 侧**退化成「整行当一个词框」并向 **stderr** 出一行 warn（`vision.swift:212`），
   **不写** `degraded=` 字段。fixture 里的 `word-boxes-unsupported` 是 `render` 的头行**渲染
   能力**测试，由手工构造的 `Header` 驱动，不是流水线产出的。
   另注意：Vision 的词框来自 `enumerateSubstrings(options: .byWords)`（按 Unicode 词边界），
   不是简单地按空格切。
4. **explore 准入实现为「补集推导 + 注释约定」**，与 design §5 的表述一致但在代码里是
   `assembly.rs` 的过滤器而非显式允许集（design 也承认这个取舍）。
5. **`is_concurrency_safe = true`**：只读本地输入 + 原子落盘 ⇒ 与 `read`/`grep`/`find`/`ls`
   同组，批内全只读才并行。
6. **行记录是行锤点，`text` 允许为空**。tesseract 5.x 的 `level == 4` 记录**text 列恒为空**
   （文字只在 `level == 5` 词记录里），Vision 的两种记录都带 text。故 `parse_tsv` **不按空
   text 丢弃行记录** —— 丢它等于丢掉锤点，后续词记录全部无处可挂，结果恒空。锤点自带 text
   为空时，行文本在解析末尾由该行的词拼接（`join_words`）：相邻词至少一个非 CJK ⇒ 插空格；
   两个都是 CJK ⇒ 通常不插（tesseract 把中文逐字切词），但字间距达到两词较大行高的 `2/3`
   时视为原文有空格 —— 真机中英混排样本因此拼出 `第三章 配置文件说明` / `Hello OCR test 2026`。
   行记录自身的 `conf`（tesseract 给 `-1` ⇒ 0）**不参与**行级最小值的聚合，无词记录的锤点
   行 `conf` 为 0。

## 测试矩阵

### `ys-tools-basic` 侧：52 条 ocr 用例

| 分组 | 条数 | 覆盖 |
|---|---|---|
| `ocr::tsv::tests` 纯函数 | 18 | 两条 golden fixture（行级 / 词级逐字节）、`parse_tsv` 正常 / 畸形 / 中文 / 真机 tesseract 5 输出、**真机 12 行 TSV 全文（行级 text 恒空）端到端**、行锤点不丢后续词、多行词归属、Vision 形态行 text 不被覆盖、`conf` 四种形态（`-1` / 浮点 / `>100` / 边界）、`filter` 三例（保留 `conf:0` / 丢空白与窄碎片 / 全滤）、`render_at` 粒度选择 |
| `ocr::engine::tests` | 14 | `file_name` 形状（stem + 16 hex）、空 PATH 无候选、探到 tesseract、探测**不执行**后端、Tesseract argv 定形、**`stdout` 必须是第二个位置参数**、Vision argv 忽略 `lang`、`lang` 作用域三处一致、落盘幂等、`lang` 只对 Tesseract 改路径、粒度进哈希、逐字节保真、不留半文件、目录缺失自建 |
| `ocr::tests` 流水线 | 20 | schema（必填 `path`、不提 `image`）、缺 `path` / 空串 / 不存在 / 目录四态、默认 `lang`、`is_concurrency_safe`、`timeout()` 为 `None`、两行记录的完整头行、`word_boxes` 恒落盘且文件是全量、无后端可用的安装提示、全后端失败拼每一条原因、空输出成功且说「没字」、**引擎零输出算失败而非无字**、**纯空白输出同样算零输出**、**Vision 零字节仍是正常空结果**、全滤时建议 `word_boxes`、缺语言包降级、超硬闸门时「文件全量 + 回显截断」、软硬闸门之间的落盘 |

> 计数说明：`cargo test -p ys-tools-basic --lib -- --list` 给出 `ocr::` 共 52 条，其中
> `ocr::tsv::tests` 18 / `ocr::engine::tests` 14 / `ocr::tests` 20。设计阶段的目标分组是
> 9 / 13 / 17；实现与真机阶段在这三组分别增了 `conf` 四形态 + 真机 12 行 fixture + 行锤点四例（tsv）、
> `stdout` 位置回归（engine）、以及引擎零输出三例（流水线）。

### `ys-coding-agent` 侧

- `toolset::tests` 的 4 处 `assert_eq!(tools.len(), 10)` / `specs.len(), 10`：
  工具表构造、装配不受影响、装配顺序、基础表指纹。
- `prefix_fingerprint_is_anchored_and_stable`：基础表 10 元、`fp_tools` 与 `fp_parent`（13 元）
  两个锚已随 `ocr` 平移。
- `EXPECTED_ORDER` 顺序断言：`ls` → `ocr` → `websearch` → `webfetch`。
- `is_concurrency_safe` 准入表断言：`vec!["read", "grep", "find", "ls", "ocr"]`。
- `prompt.rs::tool_snippet` 的逐字断言（`ocr` 的「唯一能看图」与「词级坐标只能靠
  `word_boxes` 换」两句）。
- `debug_log.rs::test_ocr_fingerprint_indexes_the_input_triple`：`ocr` 是唯一没有 `command`
  键的命令类工具；三个分量任一变化换指纹；两个 `\0` 的拼接歧义；`word_boxes` 进指纹。
- `summarize_tool_args` 对 `ocr` 的投影（`summarize_ocr`）。

`ys-coding-agent` 全量 **556 条**（含上述）。

### 变异测试（design §8 的 6 条）

| # | 注入的 bug | 会红的用例 |
|---|---|---|
| 1 | 头行去掉 `filtered=` | `golden_fixture_row_level` + `all_rows_filtered_suggests_word_boxes` |
| 2 | `command_fingerprint` 对 `ocr` 返回 `None` | `test_ocr_fingerprint_indexes_the_input_triple` |
| 3 | `Tool::timeout()` 返回 `Some(60s)` | `timeout_is_not_declared` |
| 4 | 落盘改成「先截断再写」 | `word_boxes_always_spills_and_file_is_the_full_render`、`oversized_result_truncates_inline_but_spills_everything` |
| 5 | Vision 缓存键保留 `lang` | `lang_changes_path_for_tesseract_but_not_for_vision` |
| 6 | 过滤改成 `width < 6 \|\| conf == 0` | `golden_fixture_row_level`、`filter_keeps_conf_zero_nonempty_lines` |
| 7 | `parse_conf` 退回 `parse::<u8>` | `parse_conf_boundaries`、`parse_conf_folds_minus_one_to_zero`、`parse_conf_rounds_real_float_output`、`parse_tsv_accepts_real_tesseract_5_output`、`line_anchor_without_text_keeps_following_words`、`parse_tsv_attributes_words_to_their_own_line`、`parse_tsv_real_tesseract_rows_have_empty_text`（实测 **7 条红**：浮点与 `-1` 一旦解析不出，全部含词记录的 fixture 连带崩） |
| 8 | 行级 text 空 ⇒ `continue`（改回旧行为） | `parse_tsv_real_tesseract_rows_have_empty_text`、`line_anchor_without_text_keeps_following_words`、`parse_tsv_attributes_words_to_their_own_line` |
| 9 | `join_words` 短路返回空串 | 同上三条 —— 证明行文本拼接本身是被测的，不是陪衬 |
| 10 | `argv` 删掉 `stdout` | `tesseract_argv_is_fixed_shape`、`tesseract_stdout_is_the_second_positional` |
| 11 | 引擎零输出归入「图中无字」 | `silent_engine_with_no_output_is_an_error_not_an_empty_image` |

7-11 是**真机冒烟阶段新增的**回归防线，对应该阶段暴露的三个 P0（详见「真机验证」节）。

## 装配面 8 项（design §5）逐项确认

| # | 位置 | 状态 |
|---|---|---|
| 1 | `toolset.rs::build_tools` 在 `base_tools` **段内**追加 `OcrTool::new().with_workspace(...)` | ✅ 位置 `ls` 之后、`websearch` 之前（`EXPECTED_ORDER` 同序） |
| 2 | `EXPECTED_ORDER` 9 元 → **10 元** | ✅ 第 8 项是 `"ocr"` |
| 3 | 4 处 `assert_eq!(…, 9)` → **`10`** | ✅ `:234` / `:324` / `:501` / `:636`（原计划的行号是 `228/318/495/630`，`ocr` 插入后整体下移 6 行） |
| 4 | `fp_tools` 锚重算 | ✅ 只覆盖基础 11 元 |
| 5 | `fp_parent` 锚重算 | ✅ 现 `0x0ca1d7f2e96de67d`，覆盖 10 + 3 = **13 元** |
| 6 | `assembly.rs` explore 准入 | ✅ 子表 10 元按 `write`/`edit` 过滤 ⇒ **8 元**；`ocr` 有意留（注释写明承重理由：explore 的典型任务就是「这张截图里的错误是什么」） |
| 7 | `prompt.rs::tool_snippet` 加 `"ocr" => …` | ✅ 加在 `ls` 与派发工具之间，两句承重语义逐字钉死 |
| 8 | `docs/design.md` §10 表 + §12 元数 | ✅ 工具清单加 `ocr` 行、顺序改 10 个、§12 全部元数改 10 / 13 / 8 |

外加 `is_command_tool`（`"bash" \| "bash_bg" \| "ocr"`）、`command_fingerprint` 的 `ocr`
分支、`summarize_tool_args` 的 `summarize_ocr` 三处可观测性改动，均已落地并由
`test_ocr_fingerprint_indexes_the_input_triple` 覆盖。

## 真机验证

环境（实测）：

- macOS **27.0.1**（Build 26A434）/ **arm64**
- Homebrew **5.1.14-3805-g86e082c**
- `tesseract 5.5.4`（leptonica **1.87.0**），可用语言：`chi_sim` / `chi_sim_vert` /
  `eng` / `osd`

结果（真 `OcrTool` → 真 `tesseract` 全链路）：

```
# ocr-real.png 1200x400 via=tesseract lang=chi_sim+eng lines=2 filtered=0
1 (63,52,363,38) conf_min:89 第三章 配置文件说明
2 (63,152,371,32) conf_min:95 Hello OCR test 2026
```

- 对 **1200x400 PNG**（中英混排）真机 OCR ⇒ `第三章 配置文件说明` 与
  `Hello OCR test 2026` 均正确识别。
- TSV 输出 **12 列**，列序与 `parse_tsv` 的契约一致；`conf` 为浮点数，走 `parse_conf` 的
  取整路径。

### 单元测试全绿而真机全错：三个 P0 与它们的共同根因

真机冒烟之前，`ocr` 的 **44 条单元测试全绿**，但真机上依次暴露出三个 P0，**每一个都会让工具
在真实图片上返回空结果**：

| # | 缺陷 | 单元测试为何没抓到 |
|---|---|---|
| 1 | `conf` 列按 `u8` 解析，而 tesseract 5.x 输出**浮点**（`96.748116`）⇒ 每条记录 `continue` | 测试 helper `tsv_row(..., conf: u8, ...)` 在**类型层面**把 `conf` 钉成整数，真机形态不可能进入解析器 |
| 2 | 行级（`level == 4`）记录在真机上 `text` **恒为空**，而代码对空 text 的行走 `continue` ⇒ 行锚点从不建立 ⇒ 后续全部词记录连带被丢 | fixture 的 `tsv_row(4, ..., "hello world")` 让行级恒带 text —— 那是 tesseract 不会产出的形态 |
| 3 | `argv()` 缺 `stdout`，tesseract 把 `-l` 当成 outputbase ⇒ 结果写进 `./-l.tsv`、**exit=0 且 stdout 全空** | 假引擎是 `cat <<'EOF'` 固定吐 TSV 的 shell 脚本，**完全忽略 argv**，所以参数怎么错都绿 |

**共同根因**：测试输入全部是**我们自己构造的**，没有一条来自真引擎的原始字节。假引擎不校验
argv，helper 的默认参数把「未知」当成「已知」，于是测试实际验证的是「我们以为的契约」，不是
「契约本身」。这类缺陷的全部用例都绿、没有任何断言写错 —— 错的是**被测输入从未覆盖真实形状**。

**由此确立的规则**（改本模块前先读）：

1. 解析外部引擎输出的模块，至少要有**一条 fixture 是从真引擎原样抄来的字节**，附版本 / 命令 /
   日期注释。手工 fixture 用于提高覆盖率，**不能替代**真机字节。
2. **假引擎必须校验 argv**：参数不符就以非零退出并写 stderr，让 argv 错误在单测阶段就暴露，
   而不是靠真机冒烟。
3. 生产形态的回归测试列在 `tsv.rs` 的
   `parse_tsv_real_tesseract_rows_have_empty_text`（真机 12 行 TSV 全文）与
   `engine.rs` 的 `tesseract_stdout_is_the_second_positional`（`stdout` 紧跟 image）。

第四条已确认的成因分档：**stdout 无任何非空白字节 ⇒ 引擎失败（`is_error = true`），不是「图
中无字」**。判据按后端限定 —— tesseract 即使面对纯白图也仍吐头行 + `level 1` 记录，零输出只
可能是引擎故障；`vision.swift` 不吐头行，空图本就零字节，故 Vision 的零输出仍是正常空结果。

⚠️ **已知环境坑**：`tesseract` 无法打开 `/tmp/` 下的图（leptonica `fopenReadStream` 失败），
换到 `$HOME` 下正常。这是**本机环境特性**（`/tmp` 的访问限制 / 沙箱），不是工具 bug ——
`validate` 的三态检查与 `image_bytes` 的退化路径都会如实反映成「读不到」，不会被误判成成功。

## 相关文档

- 设计：[`docs/arch/ocr-tool/design.md`](../arch/ocr-tool/design.md)
- 决策：[`docs/adr/0068-ocr-tool.md`](../adr/0068-ocr-tool.md)
- 执行计划：[`docs/exec-plans/completed/2026-11-04-ocr-tool.md`](../exec-plans/completed/2026-11-04-ocr-tool.md)
- 同族 as-built：[`docs/design-final/search-tools.md`](./search-tools.md)（三上限与
  `stop` 先于 `code` 的口径）、[`docs/design-final/tool-result-recoverability.md`](./tool-result-recoverability.md)
  （notice 与落盘的投递语义）
