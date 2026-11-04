# OCR 工具 — 架构上下文

> 日期：2026-11-04
> 状态：需求已澄清，设计已成文（[design.md](./design.md) v2，2026-11-04）。
> **本文件是探索期产物**：记录实测数据与判断形成过程。当前契约形态以 `design.md` 为准 ——
> 下文凡与它冲突处，均已就地标注。
> 基线：`master` @ `4f1142e`。
> 缘起：模型无法看到 png / jpg 等图片里的内容。

## 概述

给 agent 一个 `ocr` 工具：把图片里的文字连同**位置信息**取成文本，交给模型。

它不是一个顺手加的小工具 —— 在当前仓库里，它是**唯一**能让模型看到图片内容的通道
（见下文「为什么必须做成工具」）。

---

## 现有架构

### 模块边界

接入位置由 `docs/extension-points.md` 的判断链定死：**Tool**。

> 判断链第 2 条：「是模型决定何时做，还是无论模型怎么想都必然发生？」—— OCR 只在模型
> 拿到图片路径时才该跑，且跑不跑由模型按任务判断 ⇒ **模型决定 ⇒ Tool**。停。

三个必须触碰的层：

| 层 | 文件 | 职责 |
|---|---|---|
| 工具实现 | `adapters/tools-basic/src/ocr.rs`（**新建**） | 参数解析、spawn tesseract、解析 TSV、渲染、落盘 |
| 装配 | `apps/coding-agent/src/toolset.rs:88` `build_tools` | 唯一装配接缝 |
| 提示词派生 | `apps/coding-agent/src/prompt.rs:249` `tool_snippet` | 工具清单文案 |

### 核心抽象（复用，不新建）

| 抽象 | 位置 | 本需求怎么用 |
|---|---|---|
| `Tool` trait | `crates/ys-tool/src/lib.rs:31` | 实现 `spec` / `call` / `is_concurrency_safe` / `timeout` |
| `exec::run_bounded` | `adapters/tools-basic/src/exec.rs:213` | **外部二进制的唯一调用原语**：有界输出、独立进程组、`kill_on_drop`、`stdin`→`/dev/null`、deadline |
| `search::{BYTE_CAP, DEADLINE}` | `adapters/tools-basic/src/lib.rs:192` 起 | 1 MiB / 60 s，与 grep/find/ls 同口径 |
| `truncate` 三档 | `crates/ys-tool/src/truncate.rs` | `MAX_BYTES` 50 KiB、`SOFT_RESULT_BYTES` 8 KiB |
| `workspace_path::absolute_notice` | `adapters/tools-basic/src/lib.rs:32` 起 | 模型传 workspace 内绝对路径时追加一条事实 |
| `test_support::{test_dir, ctx, guarded, fake_bin}` | `adapters/tools-basic/src/lib.rs:249` 起 | 测试约定：临时目录含 pid + 原子序号、挂起即 panic |
| `webfetch` 的 store 范式 | `adapters/tools-web/src/store.rs` | 落盘：FNV-1a 命名（`DefaultHasher` 跨版本不稳定）、原子写、**绝不碰 workspace** |

### 关键数据流

```text
模型: ocr(path = "shot.png")
  │
  ├─ ① 校验路径存在、扩展名在白名单内（png/jpg/jpeg/webp/gif/bmp/tiff）
  │
  ├─ ② 选后端（启动期探测一次，结果缓存）
  │      macOS 且 `swift` 在 PATH ⇒ Vision
  │      否则                      ⇒ tesseract
  │      └─ 两者都无 ⇒ ToolError::Execution，文案给出两平台安装命令
  │
  ├─ ③ 调外部进程（`exec::run_bounded`，一次调用拿全文本 + 坐标）
  │      Vision    ⇒ swift <脚本> <img>     → JSON
  │      tesseract ⇒ tesseract <img> - -l <lang> tsv → TSV
  │
  ├─ ④ 归一：两个后端都折成 `OcrWord { text, x, y, w, h, conf }`
  │      TSV：level 4 = 行框、level 5 = 词框（含 conf）；丢弃 level 1/2/3
  │      Vision JSON：脚本已把坐标换成像素 / 左上原点
  │
  ├─ ⑤ 渲染（默认行级）
  │      <line_no> (<x>,<y>,<w>,<h>) conf_min:<conf> <text>
  │
  ├─ ⑥ 预算判定
  │      ├─ ≤ 8 KiB  ⇒ 直接作为 ToolResult 返回
  │      └─ > 8 KiB  ⇒ 落盘，结果给路径 + 行数 + 前若干行
  │
  └─ ⑦ word_boxes=true ⇒ 改渲染词级并**强制落盘**（永不直接返回正文）
```

### 外部依赖

| 依赖 | 用途 | 平台 | 备注 |
|---|---|---|---|
| `tesseract` 5.x | OCR 引擎（**两平台共用**） | Ubuntu + macOS | 外部二进制，不链进 agent 进程 |
| `tesseract-ocr-chi-sim` | 中文语言包 | Ubuntu | 单独包；macOS 在 `tesseract-lang` 里 |
| `swift` + Vision | macOS 首选引擎（中文更好） | macOS | 解释执行，见下「`swift` 的启动代价」 |
| `exec::run_bounded` | 调用封装 | 本仓 | 已有，直接复用 |

**零新增 Rust 依赖** —— 这是关键约束。TSV 是制表符分隔的纯文本，手写解析即可；
不需要 `image` / `tesseract` / `ocrs` 之类的 crate（那会把引擎或解码器链进进程）。

### 后端形态：Rust 调两个外部进程

```
OcrTool（Rust，adapters/tools-basic/src/ocr.rs）
  │
  ├─ 探测（启动期一次，照 SearchBackends::detect_from 范式）
  │    macOS：swift 在 PATH ⇒ Vision；否则 ⇒ tesseract
  │    其它 ：tesseract
  │
  ├─ Vision  → run_bounded("swift", [脚本路径, 图片路径])
  │              脚本由 include_str! 内嵌，首次调用落到 <tmp>/yushan-ocr-<hash>.swift
  │              Swift 侧做「归一化坐标 → 像素 / 左下原点 → 左上原点」换算，输出 JSON
  │
  └─ tesseract → run_bounded("tesseract", [img, "-", "-l", lang, "tsv"])
                 TSV 到 stdout，Rust 直接解析（零脚本）
```

两后端归一到一个内部结构，**渲染与预算判定只有一份**：

```rust
struct OcrWord { text: String, x: u32, y: u32, w: u32, h: u32, conf: f32 } // conf: 0–100
```

**坐标一律像素、左上原点**（与 tesseract 原生口径一致 ⇒ 该后端无需换算）。
Vision 给的是归一化 0–1、**左下原点**，换算全部在 Swift 脚本内完成 —— Rust 侧不知道有几个引擎。

### 脚本为什么内嵌（自包含）

`include_str!` 把 `.swift` 编进二进制（本仓已有 `include_str!` 用法，如
`adapters/tools-web/tests/mapper.rs:8`），首次调用写临时文件后复用。

理由：**工具不依赖 `~/.yushan/skills` 是否装过**，与「自包含」定位一致；
临时文件名含内容 hash ⇒ 升级二进制不会跑到旧脚本。

### `swift` 的启动代价（唯一的性能短板）

`swift script.swift` 是**解释执行**，每次调用都走一遍编译前端 —— 对 ~100 行脚本约
**1–3 s**，远大于 OCR 本身。这对单次调用可接受（`DEADLINE` 60 s 够用），
但**批量场景会被放大**。

不为此改设计：真正的修法是预先 `swiftc -O` 出二进制并缓存，属**后续优化**，
不是首版必需（首版量的是「能不能读对」，不是「批量吞吐」）。

### 为什么必须做成工具（而不是 skill）

`~/.yushan/skills/pdf-analysis/` **已经有 OCR 能力**：`scripts/setup.sh --ocr` 会装
`tesseract` / `ocrmypdf`，SKILL.md 写了 `pdftoppm` 转图再 OCR 的流程。

但它是**给模型看的操作手册**：模型得自己拼命令行、自己判后端、自己落盘、自己处理
多页。做成工具有三处实质差异：

1. **触发可靠性**：`prompt-delivery/notes.md:66` 自己记的是 skill 目录行「触发**不可靠**」。
   OCR 恰是最易漏的一步 —— 模型 `read` 图片拿到 `stream did not contain valid UTF-8`，
   很可能直接把错误报给用户；工具表里有 `ocr` 则描述就在眼前。
2. **坐标格式稳定**：行级渲染形状 `<line_no> (<x>,<y>,<w>,<h>) conf_min:<conf> <text>`
   是**契约**，模型据此回填位置。skill 靠模型每次手拼 `tesseract … tsv | awk`，列序与精度每次可能不同。
   （唯一真值与逐字节样例见 design.md §1。）
3. **可测**：代码有单测 + 变异测试；skill 的脚本只能靠端到端跑。

> **成本不是判据（修正）**：skill 目录行每轮约 **6 token**（命中缓存），正文只在触发时
> 载入（`skills.rs:15` 启动期每份只读前 4 KB）⇒ **skill 比工具更省 token**。
> 选工具的理由是上面三条，与成本无关。

> **与 skill 的分工**：本工具只做**图片**。扫描版 PDF 仍走 `pdf-analysis` skill 的
> `pdftoppm` + 页范围控制路径 —— 那需要页区间、DPI 这类参数，塞进本工具会让参数面
> 变复杂，且 skill 已经跑通。

### 多模态现状：为什么这是唯一通道

本仓**完全没有**图片输入能力，且是结构性的（2026-11-04 核实）：

| 层 | 现状 | 证据 |
|---|---|---|
| `ContentBlock` | 只有 `Text` / `ToolUse` / `ToolResult`，无 image 变体 | `crates/ys-core/src/message.rs:25-38` |
| JSONL 契约 | 外部标签形态 `{"Text":{"text":...}}`，加变体会改字节 | `apps/coding-agent/tests/golden/json_no_debug_baseline.jsonl:1` |
| `ModelRequest` | 无多模态字段 | `crates/ys-model/src/request.rs:9-36` |
| 适配器 wire 类型 | `content: Option<String>`（**不是数组**），无 `image_url` | `adapters/model-openai-compatible/src/request.rs:50-61` |
| 适配器转换 | `_ => {}` 显式吞掉未来变体 | `lib.rs:1311` |
| `ProviderCompat` | 6 个能力位全是推理/流式/思考维度，**无 vision** | `compat.rs:17-41` |
| `read` 读图片 | `read_to_string` 失败 ⇒ `"stream did not contain valid UTF-8"` 工具错误 | `adapters/tools-basic/src/read.rs:83-85` |
| TUI | markdown 图片降级为 `[img]` 文字 | `crates/ys-tui-coding/src/styles.rs:191-196` |

⇒ **把图片送给模型需要同时改五处**（含碰 JSONL 字节契约），不属本需求范围。
在可预见的将来，OCR 是模型「看到」图片的唯一手段。

---

## 约束

### 技术

- **引擎不进进程**：`tesseract` 与 `swift` 都经 `exec::run_bounded` spawn，零 Rust 依赖。
- **平台差异只体现在探测层**：macOS 有 `swift` 则走 Vision（中文更好），否则与 Ubuntu
  一样走 `tesseract`。**归一化之后无平台分支** —— 渲染 / 预算 / 落盘共用一份代码。
  - Ubuntu 安装：`apt-get install tesseract-ocr tesseract-ocr-chi-sim`
  - macOS 安装：`brew install tesseract tesseract-lang`（仅 Vision 不可用时需要）
- **交给平台处理的部分**：`read` 对图片的 UTF-8 错误文案留给 `read` 自己改（另一项），
  本工具不碰 `read`。
- **`is_concurrency_safe = true`**：只读，且每次调用写的是独立文件（命名含内容哈希）。

### 性能

- **一次 spawn 拿全**：`tesseract <img> - -l <lang> tsv` 与 Vision 单次调用都同时产出
  文本与坐标，**不需要二次 OCR**。
- `DEADLINE` 60 s、`BYTE_CAP` 1 MiB（复用 `search` 常量）。
  tesseract 一张图通常 < 3 s；**Vision 要加上 swift 解释执行开销**（见上），合计约 2–4 s。
- 结果预算与既有纪律对齐：`SOFT_RESULT_BYTES` 8 KiB 触发落盘，`MAX_BYTES` 50 KiB 硬截断。

### 演进

- **参数面保持窄**：`path` / `lang` / `word_boxes` / `psm` 四个。
  `lang` / `psm` 仅对 tesseract 后端生效；Vision 自动识别语种，忽略二者（但接受参数，不报错）。
  页范围、DPI、区域裁剪属扫描 PDF 邻域，留给 skill。
- **坐标格式一旦定型就是契约**：模型会据此回填位置，改格式等于改语义。
  首版就要把「行级默认 / 词级按需」写死。
- **新增工具会动工具表指纹**（见下「已知代价」），这是每次加工具都要付的。

### 组织

- 单 crate 内新增一个文件 + 一处内嵌脚本 + 三处装配，无跨 crate 改动。
- 测试约定沿用 `test_support::fake_bin`（假 `tesseract` / 假 `swift` 脚本），
  开发机不装任何引擎也能跑全部用例（Linux CI 不可能有 `swift`）。
  ⇒ **Vision 路径的测试必须靠注入假 `swift`**，不能依赖真机。

---

## 需求范围

### 范围内

- 图片格式：`png` / `jpg` / `jpeg` / `webp` / `gif` / `bmp` / `tiff`
- **默认行级坐标**（完整形状与 golden fixture 见 design.md §1）：
  ```
  <line_no> (<x>,<y>,<w>,<h>) conf_min:<conf> <text>
  ```
- `word_boxes=true` ⇒ 词级（tesseract 用 `level 5`，Vision 退化为行框按空格切词），**强制落盘**
- 语言经 `lang` 参数（默认 `chi_sim+eng`；拼装时校验字符集防注入），**仅 tesseract 生效**
  —— 故 Vision 下头行打 `lang=n/a` 且不入缓存键（design.md §2.3）
- macOS 走 Vision（中文更好）；Vision 运行期失败则降级 tesseract，**必带 notice 不静默**
  （探索期的「静默回退」已被否决 —— 静默降级会让模型把降级结果当原生结果）
- 超 8 KiB 自动落盘，结果给路径 + 行数 + 前 N 行（落盘**必是全量**，见 design.md §4.3）
- 后端缺失时给出两平台安装命令

### 范围外（明确不做的）

- **多模态**：把图片字节送进模型（需改 JSONL 契约 + 五层，非本需求）
- **扫描 PDF**：留给 `pdf-analysis` skill（需页范围 / DPI 参数）
- **图片显示**：TUI 渲染图片（`gap-analysis` 已列 TIER 4 跳过）
- **RapidOCR**：Debian/Ubuntu 从未打包（sources.debian.org 无记录、snap 无），
  且其 CLI 不吐文本与坐标，需自写包装 + venv。详见「已决策」。
- **版面还原**（表格结构、阅读顺序重排）：tesseract 的 `level 3` 段落信息不足以可靠
  还原表格，做一半比不做更误导
- **改 `read` 的二进制错误文案**：另一件事

### 关键场景

- **场景 1（截图问内容）**：用户贴 `error.png`，模型 `ocr` 得文本 + 行框，
  回答「第 3 行的报错是什么」。
- **场景 2（定位）**：UI 截图，模型需要说「按钮在右上角 (812,40)」⇒ 行框坐标直接可用。
- **场景 3（词级精确定位）**：用户要求「把那个坐标填回脚本」⇒ `word_boxes=true`，
  落盘后 `read` 精确取词框。
- **场景 4（中文）**：默认 `chi_sim+eng`，中英混排一页。
- **场景 5（长文档截图）**：整页代码截图，输出 > 8 KiB ⇒ 落盘，模型按需 `read` 续读。

### 异常/边界

- **后端未安装** ⇒ `ToolError::Execution`，文案含两平台命令，**不静默降级**
- **文件不存在 / 不是图片** ⇒ `ToolError::InvalidInput`，早失败
- **图片里没有文字** ⇒ 返回明确的「未识别到文本」（**不是**空结果 —— 空结果会被误读成
  「文件为空」）
- **语言包缺失** ⇒ tesseract 非零退出，原样透传 stderr（它自己会说 `Failed loading language`）
- **超大图**（如 8000×8000）⇒ 触发 deadline 或 `BYTE_CAP`，按既有 `run_bounded` 语义收敛
- **`lang` 含 shell 元字符** ⇒ 参数校验拒绝（虽然不走 shell，但防误用）

---

## 实测数据（2026-11-04，tesseract 5.5.0 + chi_sim/eng）

> 方法：`apt-get download` 六个 deb → `dpkg-deb -x` 到 `/tmp`，设 `LD_LIBRARY_PATH` /
> `TESSDATA_PREFIX` 运行（**不动系统**，无需 sudo）。测六张图：三张 PIL 合成
> （中文正文页 / 深色代码截图 / UI 按钮），三张真实项目截图。

### 字节数：行级渲染把原始 TSV 压到 1/7 左右

| 图 | 原始 TSV | 行级渲染 | 行数 |
|---|---:|---:|---:|
| cn_page（1200×900，中文正文） | 9,036 B | **1,253 B** | 12 |
| code_dark（900×420，深色代码） | 2,256 B | **400 B** | 7 |
| ui_zh（700×260，UI） | 1,602 B | **147 B** | 6 |
| real1（1200×630，真实首页） | 803 B | **153 B** | 3 |
| real2（1088×814，真实图表） | 1,927 B | **325 B** | 6 |
| real3（1200×855，真实 logo） | 428 B | **36 B** | 2 |

⇒ **单图行级输出最大仅 1.3 KB**，远低于 `SOFT_RESULT_BYTES` 8 KiB。
**对单张图片，「超阈值落盘」几乎永不触发** —— 落盘只为 `word_boxes` 与整页密集长文存在。
原先按「300–800 token/页」推算的预算紧张度**高估了**，阈值不用调。

### 语言选择是质量的主导因素（`chi_sim+eng` 7/7 不劣于 `eng`）

限定**中英文**范围后，用 CER（字符错误率，只比字母/数字/汉字，IgnoringCase）逐图量化。
同一张图换 `-l`，差异是**事故级**的：

| 图 | 内容 | `-l eng` | `-l chi_sim` | `-l chi_sim+eng` |
|---|---|---:|---:|---:|
| cn_prose | 9 行中文正文 | **86.5%**（乱码） | 0.0% | **0.0%** |
| mixed | 4 行中英混排 | **41.0%** | 0.9% | **0.0%** |
| line_cn | 单行中文 | **58.3%** | 0.0% | **0.0%** |
| en_prose | 3 行英文 prose | 0.0% | 3.7% | **0.0%** |
| block_en | 3 行英文短块 | 0.0% | 6.3% | **0.0%** |
| line_code | 单行代码 | 0.0% | 5.3%（`E6368`） | **0.0%** |
| code_block | 7 行代码块 | 0.0% | 3.4% | **0.0%** |

⇒ 四条硬结论：

1. **纯 `eng` 遇中文/混排不是变差，是乱码**（86.5% / 41.0%），**不可作默认**。
2. **`chi_sim+eng` 对拉丁文本无损** —— 英文 prose、短块、单行代码、代码块**全部 0.0%**。
   这是先前版本的最大误判：曾以为它对代码页有系统性损伤。
3. **纯 `chi_sim` 才是伤拉丁文本的那个**（3.4%–6.3%，`eng`/`usize` 被认错）。
4. ⇒ **默认 `chi_sim+eng`**。在中英文范围内它**没有一处劣于 `eng`**，是唯一的支配解。

#### 唯一的例外：密集代码截图，两败俱伤但不改变结论

`code_dark`（900×420，带 `^^^^` 波浪线与 `\|` 边框）行级 CER：

| `-l eng` | `-l chi_sim+eng` |
|---:|---:|
| 44.8% | 46.6% |

逐行 `diff` 只有两处不同：

- `error[E0308]` → `error[E9308]`（1 个字符）
- `\| ----~ expected` → `\| “人 expected`（`^^^^` 下划线行，两者都是垃圾）

⇒ 差距 **2 个百分点，且两边的绝对值都在 45% 量级** —— 这类图的瓶颈是
`^`/`\|`/缩进这些**本就不是文本的字符**，不是语种。**不足以支持为它牺牲中文能力**。

#### 纯英文图的代价：慢 1.9×，但仍在 1 s 内

| 图 | `-l eng` | `-l chi_sim+eng` | 差 |
|---|---:|---:|---:|
| en_prose | 186 ms | 357 ms | +171 ms |
| code_dark | 291 ms | 714 ms | +423 ms |
| cn_page | 491 ms | 919 ms | +428 ms |
| real2 | 373 ms | 716 ms | +343 ms |

⇒ 纯英文图慢约 **1.9×**（绝对值 +0.2–0.4 s）。这是为了中文/混排正确的必要代价，
且仍远低于 `DEADLINE`。需要压这一档时由模型显式传 `lang="eng"`。

### 为什么不做 OSD（三次实测后推翻「OSD 先行」）

先前决策是「OSD 判 script → 选 `-l`」。**实测否定这条路**，三个独立理由：

**① OSD 从未判错，只是失败 —— 但失败率 3/8**

| 图 | 尺寸 | OSD | 实际 |
|---|---|---|---|
| cn_prose / cn_page | 1000×480+ | `Han` ✓ | 中文 |
| mixed | 980×300 | `Han` ✓ | 中英混排 |
| code_dark | 900×420 | `Latin` ✓ | 代码 |
| en_prose | 880×200 | `Latin` ✓ | 英文 |
| block_en | 760×120 | `Latin` ✓ | 英文 |
| real1 / real2 | 1200×630+ | `Latin` ✓ | 拉丁 |
| **ui_zh** | 700×260 | **失败** | 中文 |
| **real3** | 1200×855 | **失败** | 拉丁 |
| **line_cn / line_code** | 900×60 / 700×44 | **失败** | 中文 / 代码 |

⇒ 判对率高（成功的 5 张全对），但**图稍小或单行就失败**。而失败必须回退，
回退目标只能是 `chi_sim+eng` —— 也就是新的默认值。**于是 OSD 对失败的那一半毫无贡献。**

**② 开销 +43%~87%，且在成功路径上可能白付**

| 图 | OSD | OCR(`chi_sim+eng`) | OSD 占 OCR 比重 |
|---|---:|---:|---:|
| cn_page | 800 ms | 923 ms | 87% |
| en_prose | 251 ms | 357 ms | 70% |
| real2 | 364 ms | 730 ms | 50% |
| real1 | 208 ms | 468 ms | 44% |
| code_dark | 304 ms | 702 ms | 43% |

⇒ 中文页 OSD 判 `Han` ⇒ 选 `chi_sim`（**比 `+eng` 更差**）⇒ **800 ms 完全白花**。
只有拉丁页判 `Latin` ⇒ `eng` 这一段是真收益，而该档的损失仅前述 2 个百分点。

**③ 混排页原理上无解**

OSD 只报**一个主导 script**。`mixed`（2 中 2 英）无论报哪个，选单语种必然丢另一边；
而 `chi_sim+eng` 在它上面是 **0.0%**。

⇒ **决策：删除 OSD 整条路径。** 少一次 spawn、少一条 `osd.traineddata` 安装依赖、
少一次失败回退分支。换取的是零代价，不是妥协。

### 缺语言包时 `rc=0` 静默降级（必须查 stderr）

tesseract 在 `-l` 里有未安装语种时**不报错退出**：

```
tesseract img - -l chi_sim+eng --psm 6
  → 退出码 0，stdout 有文本（只剩 eng 的结果）
  → stderr: Failed loading language 'chi_sim'
```

对比 `-l chi_sim`（该语种是**唯一**语种）：`rc=1`、stdout 空 —— 这个会正常失败。

⇒ **只判 `code == 0` 会漏掉「用户以为在跑中文、其实只有英文」的静默降级。**
必须在成功分支上扫 stderr 的 `Failed loading language '<code>'`，命中则附 notice
说明哪个语种缺失、以及安装命令（`apt install tesseract-ocr-chi-sim`）。

### 深色背景不是问题（推翻了我原先的担心）

`tessedit_do_invert` **默认即自动反色**：`do_invert=1` 与默认结果逐字相同；
`do_invert=0` 才炸（`Cama 0scei:S ET eeytt te`）。
⇒ **不要碰这个参数**，也不要引入图像预处理。

#### 附：`--psm 1` 也不能「一次调用拿语种」（已实验证伪）

若将来想重新考虑语种探测，需要知道这条路不存在：`--psm 1` 是
「automatic page segmentation **with OSD**」，但它**只用 OSD 判方向，不切换语言**：

- `-l eng --psm 1` 跑中文页 ⇒ 仍是乱码（`Soe ACB HAA`），**没有自动变中文**
- `--psm 1` 的 stderr 只有 `OSD: Weak margin…` 与方向，**没有 `Script:` 行**
- `Script: Han` 只在 `--psm 0` 的显式输出里出现

⇒ 语种探测**必须**单独跑一次 `--psm 0`，这是真实的额外 spawn（208–800 ms）。
没有免费的两全法。

### `--psm`：固定 6；`psm 7` 是静默数据丢失（已实测证伪）

**`--psm 7`（「整图当作单独一行」）在多行图上直接吐空，不是变差。**

| 图 | 行数 | `psm 3` | `psm 6` | `psm 7` |
|---|---:|---:|---:|---:|
| cn_prose | 9 | 0.0% | 0.0% | **100%（空输出）** |
| mixed | 4 | 0.0% | 0.0% | **100%（空输出）** |
| block_en | 3 | 0.0% | 0.0% | **100%（空输出）** |
| code_block | 7 | 0.0% | 0.0% | **96.4%**（只剩 11 字符） |
| line_cn | 1 | 0.0% | 0.0% | 0.0% |
| line_code | 1 | 0.0% | 0.0% | 0.0% |

⇒ 两条：

1. **`psm 7` 只在单行图上与 `psm 6` 逐字相同** —— 换不来任何准确率收益。
2. **多行图上它静默返回空/极短输出**。这是读图工具最坏的失败模式：
   模型会判定「图里没字」，而不是「参数用错了」。

⇒ **`psm 7` 不进参数面，也不做任何「单行图自动切换」的启发**（那需要在 Rust 侧
解码 PNG/JPEG 取尺寸，会引入图像解码依赖，违反零新增依赖）。

**`psm 3` vs `psm 6` 差异在噪声级**：字节差 < 10%，CER 差 < 2 个百分点，
方向还不稳定（中文页 `psm 3` 略好、代码块 `psm 6` 略好）。
**固定 `--psm 6`，不进参数面** —— 理由不是「更准」，而是「对单行图安全、对多行图不崩」。

注意 `code_dark` 这类密集代码图在三个 psm 下 CER 都在 45% 量级 —— 瓶颈是
`^^^`/`|`/缩进这些非文本字符，不是 psm。

### 噪声形态：窄列碎片，不是乱码

每行平均 100 B 已含噪声，主要来源是**宽度 < 6 px 的碎片**（行号竖线 `1`、引号）。
过滤「纯空白 + `width < 6`」可再省 5–20%（cn_page 1253→1252，code_dark 459→392，
ui_zh 147→146，real3 62→36）。**不要按 `conf` 过滤** —— 实测 `conf: 0` 的行
（`note: expected type`）是**真文本**，按 conf 丢会把有效内容删掉。

### 运行耗时

单图 **270–950 ms**，取决于语种与图大小（同一进程重复测的最小值）：

| 图 | `-l eng` | `-l chi_sim+eng` |
|---|---:|---:|
| en_prose (880×200) | 186 ms | 357 ms |
| code_dark (900×420) | 291 ms | 714 ms |
| real2 (1088×814) | 373 ms | 716 ms |
| cn_page (1200×900) | 491 ms | 923 ms |

远低于 `DEADLINE` 60 s，也无须调 BYTE_CAP（1 MiB 足够）。

**`--list-langs` 要 1161 ms —— 绝不能在启动期探测。**
它的 stdout 还带 `List of available languages in ... (N):` 头行需剥离。
语种可用性改为**按调用 stderr 惰性发现**（见上文 `rc=0` 静默降级）。

---

## 已知代价与硬约束

### 上下文证书：工具表指纹会变

新增工具让 `EXPECTED_ORDER` 从 9 元变 10 元，**两个指纹锚必须重算**：

| 锚 | 位置 | 现值 |
|---|---|---|
| `fp_tools` | `apps/coding-agent/src/toolset.rs:714` 附近 | `0xc3a96ba4de69115a` |
| `fp_parent` | `:723` 附近 | `0x4ff39b05aba679ef` |

**位置不能随便选**：子表必须是父表 JSON 的**字节前缀**（`:673` 断言）。
⇒ 新工具必须插在 `base_tools` 段内，**不能**加进 `assembly.rs:188-209` 的追加段
（那里只放 `bash_bg` / `subagent_async` / `subagent_send`）。

建议插在 `webfetch` 之前（本地能力优先于联网能力，符合既有分组语义），
退一步只要 `EXPECTED_ORDER` 与 `build_tools` 的 `vec!` 顺序人工一致即可。

### 需要同步的测试

| 文件:行 | 断言 |
|---|---|
| `toolset.rs:223,228,318,495,630,632` | 工具数 9 → 10、顺序 |
| `toolset.rs:647-651,664-671,673-676` | 父表 12 元 → 13 元、追加段不变、前缀关系 |
| `toolset.rs:714,723` | 两个指纹锚重算 |
| `prompt.rs:847` | `every_real_toolset_tool_has_non_fallback_snippet` —— 漏加 snippet 会红 |
| `assembly.rs:223` 起的断言 | 父表 12 → 13、子表 9 → 10 |
| `subagent.rs:2568,2576,2582,2649,2655` | Explore 7 元 → 8 元、General 9 → 10 |

> **`prompt.rs:847` 那条是好设计**：新工具忘写 `tool_snippet` 会静默退化成 `"Custom tool"`，
> 该用例专门拦这个。

### EExplore 子表要不要保留 ocr

默认**保留**（只读工具，调查任务本来就看截图）。
若要摘掉，必须**同时**改 `assembly.rs:155-159` 与 `subagent.rs:1415` 两处 ——
注释明说「装配逻辑只有一份，模式只是过滤器」。

---

## 已决策

| 项 | 裁决 |
|---|---|
| 后端 | **Ubuntu 用 tesseract；macOS 用 swift + Vision（tesseract 作 mac 兜底）** |
| 语言选择 | **默认 `-l chi_sim+eng`** —— 限定中英文后 7/7 样本 CER 不劣于 `eng`，且对拉丁文本无损；`lang` 参数可覆盖（纯英文图可由模型传 `eng` 省约一半耗时） |
| OSD | **不做** —— 失败率 3/8、开销 +43%~87%、混排页原理上无解；删掉整条路径与 `osd.traineddata` 依赖 |
| 缺语言包 | 成功分支必须扫 stderr 的 `Failed loading language` —— 该情形 **`rc=0`**，只判退出码会漏 |
| `--psm` | 内部固定 `--psm 6`，**不进参数面**；`psm 7` 在多行图上静默吐空（实测 CER 100%），明确不用 |
| 结果过滤 | 只丢**纯空白 + `width < 6`** 的碎片；**绝不按 `conf` 过滤**（`conf: 0` 的行是真文本） |
| 脚本形态 | **自包含**：`include_str!` 内嵌 Swift 脚本，首次调用落临时文件 |
| 输出 | **默认行级**（`<line_no> (<x>,<y>,<w>,<h>) conf_min:<conf> <text>`），`word_boxes=true` 走词级 |
| 用量控制 | 超 8 KiB 落盘给路径；词级**强制落盘** |
| PDF | **不做**，留给 `pdf-analysis` skill |
| 工具名 | 独立 `ocr` 工具（不扩 `read`） |
| RapidOCR | **不引入** —— Debian/Ubuntu 从未打包（sources.debian.org 无记录、snap 无），且其 CLI 不吐文本与坐标，需自写包装 + venv，代价大于收益 |

## 未澄清问题

- [ ] **Q1 落盘目录**：用 `.yushan/ocr/`（照 `pdf-analysis` skill 的 `.yushan/pdf/`），
      还是走 `$YUSHAN_FETCHED_DIR` 那套四档探测（`adapters/tools-web/src/lib.rs:307`）？
      前者简单但**落在用户仓库里**（`.gitignore` 有 `tmp/` 但未必有 `.yushan/`），
      后者成熟但多一套 env。
- [x] **Q2 默认语言（已定，2026-11-04 三次复测后修正）**：限定中英文范围后，
      `chi_sim+eng` 在 7/7 样本上 CER **不劣于** `eng`（中文页 86.5%→0.0%、
      混排 41.0%→0.0%、英文 prose/短块/代码全部 0.0%），代价是纯英文图慢 1.9×。
      ⇒ **默认 `lang="chi_sim+eng"`，不做 OSD。**
      先前「OSD 先行」的裁决已被推翻：OSD 失败率 3/8、开销 +43%~87%、
      且混排页只报一个主导 script 因而原理上无解。详见「为什么不做 OSD」。
- [ ] **Q3 行框坐标的单位与原点**：两后端已统一为像素、左上原点。
      是否在输出末尾声明一次（多约 10 token）？
- [ ] **Q5 空结果的文案**：确认用「no text detected」而非空串（见上文异常边界）。
- [ ] **Q6 是否给 `ocr` 加 guideline**：`prompt.rs:293` 的 `render_guidelines` 按「工具在场」
      门控。加一条「图片必须用 ocr 而不是 read」能减少误用，但每条 guideline 都是常驻
      token。**建议加**（很短），待定。

---

## 后续建议

- **首次运行需装引擎**：本机（Ubuntu 26.04）**无 tesseract、无 sudo 权限**。
  真实安装：`sudo apt install tesseract-ocr tesseract-ocr-chi-sim`。
  本次实测靠 `dpkg-deb -x` 解包到 `/tmp` + `LD_LIBRARY_PATH`，**未动系统**。

- **下一步用 `arch-design`** 产出方案：把「TSV 解析」「Vision JSON 解析」「渲染两档」
  「落盘」「后端探测与错误文案」拆成可独立落地的小切片，各自给「改哪里 + 测试 + 变异点」。
- **先量再改**：真跑一次 `tesseract <图> - -l chi_sim+eng tsv`，量出「一页真实截图的
  行级输出字节数」与「词级输出字节数」。上面的 300–800 / 15,000 token 估算是按典型
  密度推的，**未经实测** —— 而阈值（8 KiB / 50 KiB）直接决定落盘行为，值得先量。
- **建议先落地「行级 + 后端探测 + 落盘」**（无 `word_boxes`、无 `psm`），
  把工具表指纹改动收在一次提交里，再按需加参数。
- **`read` 的错误文案**建议同期单独修：现在模型 `read` 一张 png 得到的是
  `stream did not contain valid UTF-8`，看起来像 bug。改成「这是二进制文件，若是图片请用
  `ocr`」即可，改动极小但直接减少误用。
