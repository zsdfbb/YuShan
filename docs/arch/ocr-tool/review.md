# OCR 工具 — 架构质量分析（review）

> 日期：2026-11-04
> 对象：`docs/arch/ocr-tool/design.md`（255 行）+ `context.md`（557 行）+ `docs/adr/0068-ocr-tool.md`
> 方法：把设计里每条「代码事实」断言回查一遍源码；再按可行性 / 可维护性 / 可理解性 / 可靠性四个维度评分。
> 结论：**方案本身可落地，实测数据质量高于本仓平均水平；但有 1 处契约级自相矛盾、1 处装配面清单漏项、3 处运行期语义未定义。**

---

## 总评

| 维度 | 判定 | 一句话 |
|---|---|---|
| 可行性 | 🟢 绿 | 关键判定全部有实测（7 图 CER / psm / 字节数 / OSD 失败率），无未验证假设 |
| 可维护性 | 🟡 黄 | 后端抽象（enum + 穷尽 match）与 TSV 统一是对的；但装配面清单漏 4 处断言 + explore 子表决策缺失；探测与编译缓存的位置/归属未定 |
| 可理解性 | 🔴 红 | **坐标格式是「契约」，而 design 内部对它的写法自相矛盾**；context 状态行过期；ADR 与 design 的「加后端改动面」算术不一致 |
| 性能与可靠性 | 🟡 黄 | deadline 与外层 `tool_budget` 的余量关系未定义；`<temp>` 落盘无清理且 resume 后会悬空；解码内存无上限 |

---

## 一、可行性 🟢

值得肯定的部分（这些是本设计的真正资产，不应在后续迭代中被削弱）：

- **语种默认值不是拍的**。`chi_sim+eng` 7/7 不劣于 `eng`、纯 `chi_sim` 才伤拉丁文本、`psm 7`
  在多行图静默吐空（CER 100%）——三条结论分别否定了三个候选方案的默认选择。这类「用实验推翻
  自己候选」的记录在本仓不常见，值得保留。
- **零新增依赖、零 `ys-*` crate 改动**与「最小核心」一致；`run_bounded` 已是外部二进制的唯一
  原语（`adapters/tools-basic/src/exec.rs:213`），复用点选得对。
- **刷新「不做 OSD」的论证是闭合的**：失败率 3/8 且失败只能回退到新默认值 ⇒ 增量确为零。
- 注入式探测（`detect_from(PATH)`，先例 `exec.rs:76`）、纯函数 `tsv.rs`、假 `tesseract`/假
  `swift` 的测试策略，都使「开发机不装引擎也能跑全量用例」成立 —— 这对 Linux CI 尤其关键。

### 可行性上的两个黄点

1. **解码内存无上限**。`metadata` 只卡 20 MB **文件**大小（design.md:41），不卡**解码后**像素。
   PNG 是压缩格式，20 MB 的 PNG 可以解出 8000×8000 以上（≈256 MB RGBA）。`run_bounded` 限时间
   （`DEADLINE`）不限 RSS，`tesseract` 在这种图上会先吃满内存再超时。context.md:265 写「超大图
   ⇒ 触发 deadline 或 `BYTE_CAP`」，但 `BYTE_CAP` 卡的是**输出**，不是输入。
2. **`swiftc` 首次编译（+2–4 s）与 OCR 在同一次 `call` 里串行**，会与工具预算争夺同一份
   deadline。见下文「可靠性 R1」。

---

## 二、可理解性 🔴

### I1（红）坐标格式契约自相矛盾 —— 这是本次 review 最该先修的一条

`context.md:142` 明确写：「坐标格式稳定：行级渲染形状 `12 (98,66,821,24) conf_min: 文本`
**是契约**」。但 `design.md` 里三种写法并存：

| 位置 | 写法 |
|---|---|
| design.md:68-71（内联输出示例） | `1 62,49,890,28 第三章 配置文件说明` —— **无 `conf_min:`、无括号** |
| design.md:204-207（否决 C 的裁剪） | 「保留完整的 `x,y,w,h`」+ 保留 `conf_min:` 标签 |
| context.md:220 / :67 | `conf_min: <text>` |

模型看到的**第一份真实输出**来自 design.md 的示例段落，而 `conf_min:` 是「conf 可信度」的
唯一信号位（context.md:442 说「绝不按 conf 过滤，`conf: 0` 的行是真文本」—— 这条规则的存在
前提就是 conf 出现在输出里）。**示例与论述不一致 ⇒ 实现者照示例写就丢掉 conf，照论述写就与示例冲突。**
按「坐标格式一旦定型就是契约，改格式等于改语义」（context.md:141），这不是文档瑕疵，是接口未定。

> 修法：以一句"唯一真值"段落钉死完整形状（含括号、分隔符、`conf_min:` 是否恒出现、词级形态），
> 内联示例与落盘示例都从它派生。

### I2（红）`lang` 在 Vision 路径上是「静默无效的参数」

- context.md:224：「语言经 `lang` 参数…**仅 tesseract 生效**」
- design.md:66：「输入 schema 只有 3 个字段：`path` / `lang` / `word_boxes`」，且头行是
  `# shot.png 1200x900 lang=chi_sim+eng via=tesseract`

在 macOS 上（默认走 Vision），模型传 `lang="eng"` 会被接受、出现在头行、进入缓存键
（`fnv1a64(image‖lang‖level)`），但**不改变任何行为**。三处后果：
① 头行变成谎报；② 同一张图按 lang 落多份内容完全相同的文件；③ 模型无法从输出判断自己的参数被忽略。
ADR-0068「负面」里写的是「纯英文图慢 1.9× —— 可由模型显式传 `lang="eng"` 规避」，这条规避
**只在 Linux 成立**，文档未标注。

### I3（黄）ADR 与 design 的「加第三后端改动面」算术不一致

- ADR-0068 决策一：`1 变体 + 3 臂 + **1 个解析函数** + 1 行顺序表`
- design.md:305（对比矩阵）：`1 变体 + 3 臂 + **1 行表**`（无解析函数）

决策三刚把「线上格式统一为 TSV、Rust 侧只留唯一解析器」定为理由，那么新后端**不该**再带一个
解析函数。ADR 的算术与自己的决策三矛盾，是两处笔误之一（另一处见 I4）。两者会被后来者当
「改动面估算」引用，需要对齐。

### I4（黄）context.md 的状态行与「静默回退」措辞已过期

- context.md:4：`状态：需求已澄清，待方案设计（下一步 arch-design）` —— design 已成文，状态未更新。
- context.md:224：「`swift` 不可用时**静默回退** tesseract」
- design.md:227：「**失败必定带 notice**（不静默）」

「静默/不静默」是正交的行为契约，两份交付物说法相反。探索期文档可以保留当时的判断，但**当前
交付形态**必须唯一（按 AGENTS.md：静态断言只能引用 `design-final/` 或代码；两份设计文档互相
矛盾时，读者无从判断哪份是 now）。

### I5（黄）缺「不做什么」的显式反向清单

`word_boxes=true` 在 mac 退化为行框（design.md:250）已登记为风险，但**没有登记为接口语义**：
schema 里 `word_boxes: boolean` 的描述是否声明「mac 上不保证词级」？若不声明，模型会把退化结果
当词框去算偏移。这属于「参数意图 vs 引擎能力」的边界，值得像 §「参数是意图，不是引擎旋钮」那样
单独一句钉住。

---

## 三、可维护性 🟡

### M1（红/黄）装配面清单不完整 —— 4 处断言 + 1 个未决策的子表

design.md:242 只列了 `EXPECTED_ORDER` 9→10 与两个指纹锚（`fp_tools` / `fp_parent`）。实际还有：

| 位置 | 现有断言 |
|---|---|
| `apps/coding-agent/src/toolset.rs:228` | `assert_eq!(tools.len(), 9, "工具表应为 9 个")` |
| `toolset.rs:318` / `:495` / `:630` | 另外三处 `9`（后者是「基础表…子 agent 的表」） |
| `apps/coding-agent/src/assembly.rs:145` | 注释与逻辑：`子 agent 的工具表 = 基础 9 元表` |
| `docs/design.md` §12 | 多处「9 基础 + 3 追加 = 12」的元数契约描述 |

更要紧的是一个**未决策项**：`ocr` 插进 `base_tools` 后，explore 子表由
`assembly.rs:152` 的 `filter(!matches!("write" | "edit"))` 派生 ⇒ **`ocr` 会自动进入 explore 表
（7→8 元）**。而 explore 是「只读调查、硬预算 8 轮 / 300 s」的子 agent，让它有能力 spawn
OCR 进程是**产品决策**，不是装配副作用。design 与 context 均未提及。`docs/design.md` §12 里
explore 条目明确写「explore 子表 7 元且不含 write/edit」—— 加 ocr 会让这条断言与语义一起变。

### M2（黄）后端探测函数的复用方式未定

design.md:46 说「`detect_from(PATH)`（启动期一次，纯路径扫描，不 fork）」，但
`find_in_path` 是 `adapters/tools-basic/src/exec.rs:113` 的**私有** `fn`。要么提到 `pub(crate)`
（改既有模块可见性），要么在 `ocr/engine.rs` 复制 10 行。design 的模块图只画了
`engine.rs → exec.rs::run_bounded`，没画这条；而「不新增第二条路径探测实现」正是
`SearchBackends` 当初被提取的理由（`exec.rs:52-60` 注释）。**复制=第二个真值**，需明说选哪个。

### M3（黄）「按调用 stderr 惰性发现语言包」的状态存哪没写

design.md 的已知风险写「`--list-langs` 要 1161 ms ⇒ 按调用 stderr 惰性发现」。但
`Tool::call(&self, …)` 是 `&self` 共享，`OcrTool` 被 `Arc`/`&dyn` 复用；「发现」的结果要么
`OnceCell`/`Mutex` 缓存（引入内部可变状态，且与 `is_concurrency_safe = true` 的并发前提发生交互），
要么每次重扫 stderr（零状态）。**两者语义不同**：前者只报一次缺包，后者每次都报。design 未定，
实现者会随手选一个 ⇒ 行为不可预测。

### M4（黄）`swiftc` 产物目录未定，且与 S5 的「后置切片」边界不清

design.md:246：「`include_str!` → 哈希命名 → 原子写 → 缺则重编」——**写在哪**？候选有
`<temp>/yushan-ocr/`（与结果同目录，会被系统清空 ⇒ 重编 2–4 s）与 `~/.yushan/`（持久，
但需要新目录语义）。这个选择直接决定「首次调用成本是一次性还是每次重启一次」，而 design 只给
了「缓存」的效果数字（250–650 ms），没给**失效条件**。`store.rs` 的 FNV 手写理由（跨版本稳定）
在这里同样适用，值得显式引用一次。

### M5（绿/黄）`enum Kind` + 穷尽 `match` 的判据补丁写得很好，建议同步回 `backends.rs`

design.md:150 那段「差异若落在有界输入→有界输出的纯函数里，仍按数据差异处理」是**比现状更精确**
的判据，而 `tools-web/src/backends.rs:3-10` 的注释只有「差异全是数据不是算法」。判断标准应当只有
一份真值 —— 否则下次就又出现「同一判据两种表述」的漂移。

### M6（绿）测试策略可维护

`tsv.rs` 纯函数 + golden fixture 钉 Swift↔Rust 契约 + `#[ignore]` 真编译 e2e，三层分工清楚，
且明确承认「Linux CI 覆盖不了编译路径」（design.md:245）。这是诚实的边界声明。

---

## 四、性能与可靠性 🟡

### R1（黄/红）内层 deadline 与外层 `tool_budget` 的余量关系未定义

这是本设计**最容易埋雷**的一处，且仓里已有现成的教训：

- `bash.rs:21` `DEADLINE_MARGIN = 5s`，注释写明了原因：外层预算到点是 `tokio::time::timeout`
  把工具 future **drop**，届时只有 `kill_on_drop` 生效（**只杀直接子进程**，进程组不清理）。
- `grep` / `find` 传 `search::DEADLINE`（60 s，`lib.rs:196`）且**不实现 `timeout()`** ⇒ 外层
  回落 `bash_timeout`（默认 300 s）⇒ 内层严格更早，余量天然成立（`basic.rs:113`）。
- design.md:20 说「复用 `search::{BYTE_CAP, DEADLINE}`，与 grep/find/ls 同口径」——但**没写
  OCR 的 `Tool::timeout()` 返回什么**。

若实现者顺手写 `fn timeout() -> Some(60s)`（看起来"更严谨"），内外层就**相等**：内层不早于外层，
外层先 drop ⇒ `swiftc` 编译出的 `swift-frontend` 孙进程漏网（`exec.rs:186-200` 的杀组逻辑只在
内层超时路径上跑）。要么明确「不实现 `timeout()`，沿用 60 s + 300 s 的既有组合」，要么按
`bash.rs:37` 的形态从 `ctx.tool_budget` 推导内层 deadline。**这两个选择行为不同，必须写进设计。**

### R2（黄）`<temp>/yushan-ocr` 无清理 + resume 后路径悬空

- design.md:216-222 给了选 temp 的三条理由，但**v1 不做 sweep**，命名按**内容哈希**（同图同参
  幂等）—— 幂等只对「同一张图」成立；一个会话里 OCR 50 张不同截图就是 50 个文件，且跨会话累积。
- 更关键：工具把**落盘路径**交给模型，模型会在**后续回合**甚至 `--resume` 后去 `read` 它。
  macOS 的 `<temp>` 位于 `/var/folders/…`，系统可随时清理；Linux 的 `/tmp` 常被 tmpfiles 清。
  ⇒ 会话恢复后 `read` 该路径得到 `not found`，而模型无法区分「文件被清」与「OCR 失败」。
- 对照：`tools-web` 为 webfetch 付了 HOME 档的代价，理由正是「用户想长期留存」。OCR 的差别是
  「下一步就消费」，但**没有给失效一个可见语义**（例如 notice 里带「临时文件，可能已被清理」）。

### R3（黄）「未识别到文本」与「渲染后为空」的判定口径未定

context.md:253 要求「图片里没有文字 ⇒ 返回明确的『未识别到文本』，**不是**空结果」。但
design.md:47 的流水线第 ④ 步是「滤纯空白 + `width < 6`」——**过滤发生在渲染前**。于是三种输入
在实现上不可区分：
① 图里真没字（tesseract 无 level 4 行）；② 有字但全部被 `width < 6` 滤掉（细笔画/竖排单字）；
③ 语言包缺失导致 `rc=0` 且行全空。

②尤其危险：context.md:440 的实测里 `real3` 从 62 B 降到 36 B，说明窄碎片确实成片存在。若某张
图的行几乎都是窄碎片，工具会报「未识别到文本」——**这就是读图工具最坏的失败模式**（模型判定
「图里没字」而非「过滤太狠」）。需要一条「滤掉的行数 > 保留行数 ⇒ 附 notice」的兜底。

### R4（黄）缺可观测性条目

`docs/design.md` §12 要求工具调用进 debug 日志带 `command_fp`（命令类工具给数字）。OCR 是
`spawn` 外部进程的命令类工具，但 design 与 context 都没提 `command_fp` / 耗时拆分 / `via=` 的
debug 记录。加上 `swiftc` 这一档（首次编译 vs 命中缓存）差异达 10×，**没有读数就无法验证
「−75%」这个收益是否在真实会话里兑现**。

### R5（绿）并发安全的论证成立

`is_concurrency_safe = true` 的前提写清楚了：内容哈希命名 + 原子写（唯一 tmp 名 + rename）⇒
读者永不见半文件，同图同参幂等覆盖。补一句即可闭合：`tesseract` 只读 `TESSDATA`、无共享可写状态
（先例 `read`/`grep`/`find`/`ls` 均为 `true`，`bg_tools` 因 spawn 有副作用而恒 `false` —— 这里
判 `true` 的理由是「读图 + 幂等落盘」，与 `bash_bg` 不同，值得在 spec 旁注明一句免得被后人误改）。

---

## 风险排序（影响 × 可能性）

| # | 风险 | 影响 | 可能性 | 等级 |
|---|---|---|---|---|
| I1 | 坐标格式契约自相矛盾（`conf_min:` 有无） | 高（改了就是语义变更，模型回填位置出错） | 高（两份文档冲突，必定踩） | **P0** |
| M1 | 装配面漏 4 处断言 + explore 子表未决策 | 高（CI 变红第一次就撞；explore 能力面被动扩大） | 高 | **P0** |
| R1 | 内层 deadline / 外层 `tool_budget` 余量未定义 | 中高（孙进程漏网、`swiftc` 被砍半） | 中（取决于实现者的直觉） | **P1** |
| I2 | `lang` 在 Vision 下静默无效 + 头行谎报 | 中（模型误判参数生效；缓存多份重复文件） | 高（mac 是主平台之一） | **P1** |
| R3 | 「未识别到文本」与「全被滤掉」不可区分 | 中高（最坏失败模式：假阴性） | 中 | **P1** |
| R2 | temp 落盘无清理 / resume 后悬空 | 中（一次恢复会话就撞） | 中 | **P2** |
| M3 | 语言包惰性发现的状态归属未定 | 中（并发下行为不可预测） | 中 | **P2** |
| M4 | `swiftc` 产物目录未定 | 中（每次重启多付 2–4 s 或永久脏目录） | 中 | **P2** |
| 可行性 1 | 解码内存无上限 | 中（大图 OOM 波及 agent） | 低中 | **P2** |
| I3/I4/I5 | 文档算术不一致 / 状态过期 / 反向清单缺失 | 低（误导读者，不改行为） | 高 | **P3** |
| M2 | `find_in_path` 复用方式未定 | 低（10 行复制，第二个真值） | 中 | **P3** |

---

## 改进建议

### 易修复（改文档即可，不动方案）

1. **[P0/I1]** 在 design.md 增一节「输出格式（唯一真值）」，给出**行级内联 / 行级落盘 / 词级落盘**
   三份逐字节示例（含 `conf_min:`、括号、分隔符、行号语义），并声明「本文档其余处的示例若与
   本节冲突，以本节为准」。同步修 design.md:68-71 的示例。
2. **[P0/M1]** 把装配面清单补全成表格：`EXPECTED_ORDER`、`fp_tools`、`fp_parent`、
   `toolset.rs` 4 处 `len()` 断言、`assembly.rs:145` 注释、`docs/design.md` §12 的元数描述；
   并显式回答「`ocr` 是否进 explore 子表」（若进 ⇒ 7→8 元，§12 与 `assembly.rs:152` 一起改）。
3. **[P1/I2]** 二选一并在 schema 描述里写明：*a)* mac 路径忽略 `lang`，头行改为不打印或打印
   `lang=n/a (vision)`，缓存键不含 lang；*b)* 接受 lang 但显式校验「Vision 下只支持
   `chi_sim+eng`」，其余值 `InvalidInput`。同时给 ADR 的「负面」补一句「该规避仅 Linux」。
4. **[P1/R1]** 在 design.md 写明 `Tool::timeout()` 的返回值（建议照 `grep`/`find` 的 **不实现**，
   沿用 60 s 内层 + 300 s 外层），并补一句「若将来实现 `timeout()`，必须保持内层严格早于外层，
   见 `bash.rs:DEADLINE_MARGIN`」。
5. **[P1/R3]** 加一条兜底：过滤后为空但过滤前有行 ⇒ 返回「识别到 N 个窄碎片但无有效行」+
   notice，而不是「未识别到文本」；三种空结果（无字 / 全滤 / 缺语言包）用不同文案。
6. **[P3/I3/I4]** 对齐 ADR 决策一与 design 对比矩阵的「加后端改动面」；更新 context.md 状态行；
   把「静默回退」统一为 design 的「失败必带 notice」。
7. **[P3/M2]** 明说 `find_in_path` 提到 `pub(crate)`（推荐）还是复制。

### 需讨论（有取舍，建议拍一下）

8. **[P2/R2]** 落盘目录：`<temp>/yushan-ocr`（现在的选择）vs `~/.yushan/ocr`。若保持 temp，至少
   在路径旁附「临时文件，可能已被系统清理」的 notice，并明确 v1 不做 sweep 的**可观测触发条件**
   （已有：「目录膨胀反馈或 >1000 文件」）。
9. **[P2/M3]** 语言包惰性发现：进程级 `OnceCell`（只报一次）vs 每次扫 stderr（无状态但重复）。
   倾向后者 —— 与 `is_concurrency_safe = true` 的无状态前提一致。
10. **[P2/M4]** `swiftc` 产物目录 = OCR 落盘目录（同生共死，可能被清）vs `~/.yushan/ocr/bin`
    （持久）。选后者则需定义 **source 哈希变更 ⇒ 重编** 的失效条件。
11. **[P2/可行性]** 是否需要输入像素上限（如 `>40 MP ⇒ InvalidInput`）。`metadata` 只看文件字节，
    挡不住解压炸弹。

### 架构级（建议单独立项或写进 ADR 后果）

12. **[P1/R4]** **给工具加一张「可观测性最小集」表**（`command_fp` / 三段耗时 / `via=` /
    `swiftc` 命中与否）。当前仓里 `subagent_*`、`bash_bg` 都各自补过这条，OCR 又是 10× 量级的
    性能差异工具 —— 没有读数，「−75%」永远只是实验室数字。
13. **[P3/M5]** 把 design.md:150 的判据「补丁」（纯函数内 ⇒ 数据差异；跨出纯函数 ⇒ trait）
    回写进 `tools-web/src/backends.rs:3-10` 的模块头，让判据只有一份真值。这是一个**跨需求**的
    整理动作，不建议塞进 OCR 这个切片。

---

## 处置记录（v1 review → design v2）

> 上文保留 v1 的审查原样，供回溯「当时看到了什么」。下表是逐条处置。

| 条目 | 处置 | 落在 |
|---|---|---|
| I1 坐标格式自相矛盾 | ✅ 已修：`§1 输出格式（唯一真值）` + 头行/行级/词级三形态 + 2 段逐字节 golden fixture | design §1 |
| M1 装配面漏项 | ✅ 已修：扩成 8 项表格（含 4 处 `len()` / `tool_snippet` / `docs/design.md` §12） | design §5 |
| M1 explore 子表 | ✅ 已决策：**进**（只读 + 幂等落盘 + 8 轮/300s 预算），并要求注释否认「补集推导」 | design §5 |
| R1 内外层超时 | ✅ 已修：明确「**不实现** `Tool::timeout()`」，写清为何相等即漏孙进程 | design §3.1 |
| I2 `lang` 在 Vision 下无效 | ✅ 已修：忽略 + 头行 `n/a` + **不入缓存键**；ADR 后果句加限定 | design §2.3 / ADR 修订 |
| R3 空结果不可分 | ✅ 已修：三分表（无字 / 全滤 / 缺语言包），头行增 `filtered=` 作判据 | design §1、§3.2 |
| R2 落盘路径悬空 | ✅ 已修：路径必附「临时文件，可能已被清」；sweep 触发条件仍登记 | design §3.4 |
| M3 语言包惰性发现的状态 | ✅ 已定：**不缓存**（无状态，与 `is_concurrency_safe` 前提一致） | design §7 |
| M4 `swiftc` 产物目录 | ✅ 已定：`<temp>/yushan-ocr/bin/`，**源码哈希即失效判据**（无需清理旧文件） | design §3.5 |
| 可行性 1 内存 | ✅ 已加：40 MP 像素上限（只读 header） | design §7 |
| R4 可观测性 | ✅ 已加：`is_command_tool` 认 `ocr` + `command_fingerprint` 改取值键（同一哈希双重索引）+ `summarize_tool_args` 加臂 | design §6 |
| M2 `find_in_path` 复用 | ✅ 已定：提为 `pub(crate)`，拒绝复制 | design 模块划分 |
| I3 改动面算术 | ✅ 已修：ADR 与 design 统一为「1 变体 + 3 臂 + 1 行表」，并补「无新解析函数」 | design §2.1 / ADR |
| I4 状态行过期 | ✅ 已修：context.md 头部改为「设计已成文」并声明「冲突处以 design 为准」 | context 头部 |
| I5 `word_boxes` 退化未声明 | ✅ 已修：schema 写明「Vision 下退化为行框」+ 头行 `degraded=word-boxes-unsupported` | design §1 |
| **新发现**：超长文本落盘成半个文件 | ✅ 已修：`§4.3` 只做头截断（`truncate::head`），落盘**必是全量**，`word_boxes` 恒落盘 | design §4.3 |
| 架构级 13（判据回写 `backends.rs`） | ⏸ **未做**：跨需求整理，不塞进本切片 | — |
