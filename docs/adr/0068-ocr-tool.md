# ADR-0068: OCR 工具的后端抽象与语种策略

## 状态

已接受

## 上下文

仓库当前完全没有图片输入能力：`ContentBlock` 只有 `Text`/`ToolUse`/`ToolResult`
（`crates/ys-core/src/message.rs:24-38`，`#[non_exhaustive]`），`ModelRequest` 无多模态字段，
`ChatMessage.content` 是 `Option<String>` 而非数组。新增 `image` 变体要穿透 model adapter、
provider compat、事件流三层，与「最小核心」冲突。

⇒ 走 `ocr` 工具，让**模型唯一能「看到」图片的通道**是一个文本工具。

硬约束（用户裁决）：

- 支持 **Ubuntu 与 macOS**
- OCR 必须是**独立工具 / 独立进程**——引擎不进 agent 进程
- **零新增 Rust 依赖**
- 范围限定**中英文**；PDF 不做（留给 `pdf-analysis` skill）

三个候选架构按「最小复杂度 / 可扩展 / 性能」三种倾向并行产出，均不写文件。
共同点：都否决 `trait OcrBackend`，都用 `enum Kind` + 穷尽 `match`（形态同
`adapters/tools-web/src/backends.rs`）。分歧集中在**语种策略**与**输出格式**。

## 方案对比

| 方案 | 描述 | 优点 | 代价 |
|------|------|------|------|
| A 最小复杂度 | 3 文件；TSV 统一线上格式；**不做 OSD**，默认 `eng` + 低产出提示；落 `<temp>` | 最小实现面（~550 Rust）；唯一 TSV 解析器；后端差异退化为纯数据 | 默认 `eng` 对中文页是**乱码**（实测 86.5% CER） |
| B 可扩展 | 6 文件；`enum Kind` + 三臂；**OSD 提升为 `LangPolicy` 数据位**；元信息头行；mac 运行期降级链；落 `~/.yushan/ocr` | 加后端漏臂 = 编译错误；platform 规则可在 Linux 断言；Vision 挂掉有出路 | 默认依赖 OSD，而 OSD **失败率 3/8**、开销 +43~87% |
| C 性能 | 4 文件 + asset；**不做 OSD**；行级渲染；`swiftc -O` 缓存；去 `w,h` | mac 1.1–3.4 s → 250–650 ms；最省 token | 默认 `eng`（同 A 的问题）；`w,h` 裁剪在本仓库阈值下是**优化不存在的问题** |

三方对**后端抽象**的判断一致，对**语种**的判断都错——实测数据推翻了 A/C 的「默认 `eng`」
与 B 的「OSD 先行」。

## 决策

选择 **[A 的形态] + [B 的降级链与头行] + [C 的 `swiftc` 缓存]，并将默认语种改为 `chi_sim+eng`、删除 OSD。**

### 决策一：`enum Kind` + 三条穷尽 `match`，不建 trait

先例判据要补精确一条（写进模块头注释）：

> 差异若落在「有界输入 → 有界输出的纯函数」里，即使代码形状不同，仍按数据差异处理
> （表 + 穷尽 `match`）；只有当差异跨出纯函数（持状态 / 需签名 / 需 crate 外实现 /
> 需在单次调用内多态派发）时，才升级为 trait。

OCR 的两个解析器同签名、零状态、唯一调用点。`dyn` 换来的是**丢掉穷尽性检查**——
`Vec<Box<dyn>>` 漏填静默少一个后端，`match` 漏臂是编译错误。

加第三后端（如 RapidOCR）的实际改动面：**1 个枚举变体 + 3 个 match 臂 + 1 个解析函数 +
1 行顺序表**，参数面与渲染零改动。

### 决策二：默认 `lang="chi_sim+eng"`，**不做 OSD**

逐图 CER 实测（中英文范围，7 张图）：

| 图 | `-l eng` | `-l chi_sim` | `-l chi_sim+eng` |
|---|---:|---:|---:|
| 9 行中文正文 | **86.5%** | 0.0% | **0.0%** |
| 4 行中英混排 | **41.0%** | 0.9% | **0.0%** |
| 单行中文 | **58.3%** | 0.0% | **0.0%** |
| 英文 prose / 短块 / 单行代码 / 代码块 | 0.0% | 3.4–6.3% | **0.0%** |

**`chi_sim+eng` 7/7 不劣于 `eng`，对拉丁文本无损。** A 与 C 的「默认 `eng`」前提实测不成立
——「`chi_sim+eng` 伤代码」是**纯 `chi_sim`** 的行为。

唯一例外：密集代码截图（`^^^^` + `|` 边框）`eng` 44.8% vs `chi_sim+eng` 46.6%，逐行 `diff`
只差 `E0308`→`E9308` 一个字符，两边绝对值都在 45% 量级（瓶颈是非文本字符）。不足以牺牲中文能力。

**删 OSD 的三个理由**：

1. **失败率 3/8**（小图/单行即失败），失败只能回退到 `chi_sim+eng` = 新默认值，毫无增量。
2. **开销 +43%~87%**；中文页判 `Han` ⇒ 选 `chi_sim`（比 `+eng` 更差）⇒ 800 ms 完全白花。
3. **混排页原理上无解**：OSD 只报一个主导 script，必然丢一半；而 `chi_sim+eng` 在混排上 0.0%。

删掉后省一次 spawn、一条 `osd.traineddata` 安装依赖、一条失败回退分支——**这是零代价**。

### 决策三：线上格式统一为 TSV

Vision 原生是行级、tesseract 是词级。让 Swift 造同列序 TSV（+约 10 行），Rust 侧只留
**一个**解析器。否则是两个可能漂移的真值。TSV 是瞬态不进上下文，字节代价为零。

### 决策四：`--psm` 固定 6，不进参数面

`psm 7`（整图当一行）在多行图上**静默吐空**：9 行中文页 CER 100%、7 行代码块 96.4%、
3 行短块 100%——而在单行图上与 `psm 6` 逐字相同（换不来收益）。这是读图工具最坏的失败模式：
模型判定「图里没字」而非「参数错了」。

`psm 3` vs `6` 差异在噪声级（< 2 个百分点且方向不稳定）。固定 6 的理由是「对单行图安全、
对多行图不崩」，不是「更准」。

## 后果

**正面**：

- 中文与中英混排正确（此前任何单语种默认都会在某一档崩到 41–86% CER）
- 单次 spawn、无 OSD，Linux 单图 270–950 ms
- 加第三后端只需 1 变体 + 3 臂 + 1 解析函数 + 1 行表，参数面零改动
- `ys-core` / `ys-event` / `ys-protocol` / `ys-tool` 一行不改；零新增 Rust 依赖

**负面**：

- 纯英文图慢 1.9×（+0.2–0.4 s）——可由模型显式传 `lang="eng"` 规避
- `word_boxes=true` 在 mac 退化为行框（Vision 无词框），须附 notice
- `swiftc` 缓存路径无 CI 覆盖（Linux 跑不了真编译），靠纯函数测路径 + golden fixture 钉契约
- 工具表指纹变更：`EXPECTED_ORDER` 9→10，`fp_tools` / `fp_parent` 两个锚须重算，
  且新工具必须插在 `base_tools` 段内（子表须为父表 JSON 的字节前缀，`toolset.rs:673`）

**已知运维面**：

- 缺语言包时 `-l chi_sim+eng` **`rc=0`**（静默降级到仅 eng），stderr 有 `Failed loading language`
  ⇒ 成功分支必须扫 stderr，不能只判退出码
- `--list-langs` 要 1161 ms ⇒ 绝不能在启动期探测，改为按调用 stderr 惰性发现

## 关联

- [design.md](../arch/ocr-tool/design.md)
- [context.md](../arch/ocr-tool/context.md) — 全部实测数据与方法
- `adapters/tools-web/src/backends.rs` — 「常量表 + 穷尽 match」的先例
- ADR-0015 — 后端探测不在启动期 fork
