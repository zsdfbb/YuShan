# webfetch 总结能力 — 架构质量分析报告

> 产出：`/arch-validate`。分析对象：[`design.md`](./design.md) + [`../../adr/0018-webfetch-summarize.md`](../../adr/0018-webfetch-summarize.md)。
> 方法：源码逐条核实（不信文档自述）+ 一轮独立对抗性复核（其结论已逐条复验，**一处误报已剔除**，见 §5）。

---

## 分析范围

- **对象**：设计文档 + ADR（尚未实施，故评的是**设计**而非代码）
- **维度**：可行性 / 可维护性 / 可理解性 / 性能与可靠性（全量）
- **不做**：对既有 `tools-web` 代码本身的评审（那是 ADR-0016 的范围）

---

## 各维度判断

| 维度 | 判定 | 一句话 |
|---|---|---|
| 可行性 | 🟢 | 机制全部可落地，源码事实逐条核实为真，零新依赖 |
| 可维护性 | 🟡 | 骨架干净，但有**一个第二真值**与两条重复的 notice 组装路径 |
| 可理解性 | 🟡 | 概念一致性好；**ADR 的承重证据措辞过强**（最严重的一处） |
| 性能与可靠性 | 🟠 | 延迟成乘性且**全程零可观测**；`max_tokens` 的「假话」只抓了一半 |

### 可行性 🟢

源码事实**逐条核实为真**：`ToolContext` 无 model（`context.rs`）；`ys-model`→`ys-tool` 成环（`ys-model/src/request.rs:3`）；`basic.rs:308` 是唯一生产接线点；`ToolContext::new` 15 处；`wait_aborted(None)` 是 `std::future::pending()`（`net.rs:285-288`）⇒ **`select!` 无「永远走取消分支」的致命 bug**；`net.rs:222-230` 的 `fetch_capped` 已是同款 `select!` 写法。

一处**降级为黄的点**：设计 §2.1 说「照抄 `basic.rs:473-478` 的 `NoopModelEventSink`」—— 但 `ys-model` **不导出**任何 sink 实现，所以这是**同 crate 内的第二份 5 行拷贝**，不是「复用」。§5 只为 ADR-0017 之后的 `text_of()` 规划了去重，漏了这一个。

另一处：测试清单第 8 条要「用记录型 `Model` 断言 `tools.is_empty()` / `system` / `max_tokens`」—— **该基建不存在**：`MockModel::complete` 的形参是 `_request`（`mock.rs:65`），**完全不看请求**，且只有 `push_*` 三个方法（`mock.rs:33,41,49`）、**无计数出口**（见 §3 风险 R8）。

### 可维护性 🟡

**好的部分**：窄 trait 与 `ApprovalHandler` 同形；`with_summarizer` 与 `with_boundary` 同形；`new()` 签名不变 ⇒ 15 处调用点零改动；依赖方向零新边；错误用 `String` 的论证成立（三种失败处置完全相同）。

**问题一：`cap` 与 `head` 是两次同参调用 —— 第二个真值（易修复）**

设计 §2.3 的 `cap = ys_tool::head(&file_content, ys_tool::MAX_LINES, ys_tool::MAX_BYTES)` 与既有 `fetch.rs:334` 的 `head` **同参、同为纯函数 ⇒ 结果必定相等**。而 `summary_pointer(&cap, saved)` 要的 `Truncation{content, shown_lines, total_lines}` 在既有 `head` 里**全都有**。

⇒ 设计 §3 依据 4「不产生第二个真值」在**值**上成立、在**调用点**上不成立：两处必须永远同参，今天靠常量兜住；一旦有人动 `MAX_LINES`，或 ADR-0017 的「`min(50KB, f(window))`」落地，两处会**静默分叉**（一个按显示预算截、一个按窗口截），而分叉**只会在指针上暴露**。附带：`ys_tool::head` 会对 ≤5 MiB 的 `file_content` 做第二次全量 `lines().collect::<Vec<_>>()`（`truncate.rs:119`）。

**问题二：两条腿的 notice 组装重复且顺序不一致（需讨论）**

- 成功腿 = `[既有三条] + [覆盖/指针]`（指针在**末位**，与 `saved_notice` 同位）
- 失败腿 = `[Summary failed] + [既有三条] + [saved_notice]`（新句在**首位**）

⇒ 不能是「一条链 + 一个插入点」，而是两个近乎重复的组装点。`saved_notice(&head, saved)` 在 `fetch.rs:353` 是在 `head.content` 被 move **之前**调用的（`fetch.rs:355`）—— 这个「先算 notice 再 move」的顺序约束要在两条腿上各写一遍，且设计用 `head.content.clone()` 绕开（≤50KB 的无谓拷贝，与设计在 Q1 里「`&str` 避免 ≤50KB 拷贝」的自我要求相矛盾）。

**并发安全**：无新锁。`select!` drop future 的安全性经核实**干净**：`model-openai-compatible/src/lib.rs::complete` 只读 `&self.config`，全文件 grep `Mutex|RwLock|Cell<|static |lazy` **零命中**（累积器是局部变量）⇒ 中途 drop 不留不一致状态、不毒化锁、不影响后续调用。

**资源管理**：无泄漏路径。但设计把它描述为「更好只要一行可见性改动」——**漏了成本面**：请求**已经发出**，drop 只关连接，prompt 与已生成前缀通常**照样计费**。（另一面：工具调用是串行 for 循环，无并发资源竞争。）

**问题三：取消与摘要失败被混成同一条路径（需讨论）**

同 crate 对「用户取消」的既有处置是**短消息、不回落**：`net.rs:225` → `FetchError::Aborted` → `fetch.rs:271-276` 返回 `"Fetch aborted by user."`（`is_error: false`，**不塞任何正文**）。
设计的 Ctrl+C 却走 `Err("cancelled")` → `Summary failed: cancelled — showing the page head instead` + **塞回最多 50KB 头部进 session**。语义不一致：用户喊停，得到的是「摘要失败」外加一份大文本。

### 可理解性 🟡

**概念一致性 🟢**：`Summarizer`/`ApprovalHandler`、`with_summarizer`/`with_boundary`、`is_error:false`/ADR-0001、复用 `head`/「截断的唯一实现」—— 全部与既有词汇对齐。

**问题四：ADR 的承重证据把「样本里没有」写成了「现实中不存在」（最严重）**

ADR 决策 3 与设计 §1 都写：「**0/5 撞闸** ⇒ 真实页面都放得下 ⇒ 任何更大的封顶都是在为**实测中不存在的场景**预付 token」。

但 `tests/golden/README.md:63-70` 自己写明：这 5 份夹具是按**元素覆盖**挑的，且设计原话就要求「5 类真实页面**各截到 ≤200KB**」——`blog.html` 被**按字节截断**（262,605 → 204,800），`github-readme.html` 未截断的理由是「截到 200KB 会把 README 连同表格与代码块一起切掉」。**⇒ 样本是被工程化到小尺寸的，不是体积代表样本。**

更硬的反例：夹具 HTML 是 110 KB–415 KB，而抓取上限是 **5 MiB**（`fetch.rs:56`）—— 覆盖率 **0/8** 的 `[415KB, 5MiB]` 区间。按实测转换比（2.25×–32×），5 MiB 页面转 MD 后是 **160 KB–2.3 MB**。

**还有一条转化比的适用域被越过**：2.25×–32× 全是 **HTML→MD** 的比值，而 `webfetch` 对 `text/*`、`application/json`、`application/xml` 是**原样透传**（`fetch.rs:313-315` 的 `} else { decoded }`）。这条路径的比值是 **1.0×** —— 一份 5 MiB 的 JSON/日志，50 KB 闸覆盖 **~1%**。这是设计声明支持的**真实**场景。

**问题五：line-based 覆盖率在单行正文上读数失真**

`head_lines` 的「首行无条件保留」规则（`truncate.rs:78`）意味着一份**不含换行**的正文（JSON / minified / 纯文本）：

```
file_content = "<!-- untrusted … -->\nSource: <url>\n\n{一整行 5MiB 正文}"
lines() → ["H", "Source: …", "", "{正文}"]   (4 行)
→ 留 H / Source / "" 后在正文那行 break  ⇒ shown_lines=3, total_lines=4
```

`cap.content` ≈ **90 字节（只有两行 header）**，而 `summary_pointer` 会写「Summary covers lines 1-3 of 4」—— 行读数 75%，**正文覆盖 0%**。这正是质量目标禁止的「覆盖不全却看起来权威」，而这类页面恰恰走**非 HTML 透传**路径，是 5 份**全是 HTML** 的夹具**结构上覆盖不到**的形态。设计测试清单第 6 条（断言 `len() ≤ MAX_BYTES`）在这种输入上**会通过**，给出虚假信心。

### 性能与可靠性 🟠

**问题六：`max_tokens: 1024` 在生产里同样是句假话 —— 与设计抓到 temperature 的那条完全同构，设计只抓了一半**

设计 D2/Q2 用「`lib.rs:416` 是 `config.temperature.or(request.temperature)`，app 侧写死 `Some(0.7)` ⇒ 设了会被静默覆盖，是句假话」论证 `temperature: None`。**紧邻的上一行是同一个形状**：

```rust
max_tokens: config.max_tokens.or(request.max_tokens),   // lib.rs:415
temperature: config.temperature.or(request.temperature), // lib.rs:416
```

app 侧 `main.rs:610` 写死 `max_tokens: Some(4096)` ⇒ **摘要请求实际发出去的是 4096，不是 1024**（compaction 的 `basic.rs:487` 早有同款）。

后果具体：**Q1 拒绝 `truncated` 标志的理由之一是「提示词已把输出钉在 <200 词、`max_tokens=1024`」—— 下半句不成立。** 且设计测试清单第 8 条「断言 `max_tokens == 1024`」会**全绿**而线上是 4096 —— 正是本项目反复警告的「测试全绿但事实相反」。

**问题七：延迟是乘性的，且全程零可观测**

`basic.rs` 对同一条 assistant message 里的多个 tool call 是**顺序 for 循环**（`basic.rs:273`），每个各自套一个 `bash_timeout`。设计只量化单次（典型 ~18s / 最坏 60s）；模型一轮发 3 个带 prompt 的抓取 ⇒ 上限 **3 × (30 + 60) = 270s 的全程零输出**。

可观测性覆盖：**零**。无 `AgentEvent`、无 metric、无日志（`tools-web` crate 级禁 print；`logging.rs` 是 app 私有且依赖方向单向）。设计与 ADR 都把「将来一次 `emit`」记为升级路径，**但没有把「今天完全没有」列为本次的可靠性缺口**。

**问题八：`token` 代数的呈现不可复核（**不是**数学错误）**

> ⚠️ 对抗复核员判「`R*` 代数不成立/不可复现」—— **该判定是误报**，源于把 KB 与 token 混用。我用 token 重算**完全复现**：
> `R*(C=50KB) = (12800+1400)/(12800−350) = 1.14`；`R*(C=200KB) = (51100+1400)/12450 = 4.22`。**代数本身正确。**

但**呈现**有三个真问题：① 设计正文**没给单位、没给推导** ⇒ 读者无法复核（复核员自己就栽在这里）；② **漏了 prompt cache 的影响**：头是稳定前缀、会被缓存 ⇒ `k<1`，而 `R* = 1 + [(C+ρS)/(H−S) − 1]/k` 在 `k=0.1` 时从 **1.14 升到 2.41**（缓存让摘要**更不划算**，设计没说）；③ 没点明公式对功能本身的反噬：`R*≈1.14` 意味着**「抓一次就作答」的单轮回合上，摘要在任何 C 下都净亏**。

---

## 风险排序（影响 × 可能性）

| # | 风险 | 影响 | 可能性 | 级别 |
|---|---|---|---|---|
| **R1** | **ADR 的承重证据被样本偏差削弱**：`0/5 撞闸` 来自**刻意截到 ≤200KB、按元素覆盖挑选**的夹具；`[415KB, 5MiB]` 区间 0 覆盖 | 高（一份 accepted ADR 的成文依据） | 高（已证实） | 🔴 **P0** |
| **R2** | **`max_tokens` 假话未记录**：摘要实际发 4096；Q1 拒绝 `truncated` 的理由一半不成立；「断言 1024」会全绿而线上 4096 | 中高 | 高（已证实） | 🔴 **P0** |
| **R3** | **非 HTML 透传路径上封顶覆盖 ~1%**（5 MiB JSON/日志，比值 1.0×）—— 摘要「看起来有答案其实没读到」 | 高 | 中 | 🟠 P1 |
| **R4** | **单行正文的 line-based 覆盖率失真**：报 75%、正文 0%；且测试第 6 条在此形态上假绿 | 高 | 中 | 🟠 P1 |
| **R5** | **多抓取乘性延迟 + 零可观测**：3 个带 prompt 的抓取 ⇒ 上限 270s 全程无输出 | 中高 | 中 | 🟠 P1 |
| **R6** | `cap`/`head` 两次同参调用 = **第二个真值**，分叉只在指针上暴露 | 中 | 中 | 🟡 P2 |
| **R7** | **取消语义不一致**：abort 时照 `FetchError::Aborted` 该给短消息，实则塞回 ≤50KB 头部 | 中 | 高 | 🟡 P2 |
| **R8** | **测试基建缺口**：`MockModel` 不看请求（`mock.rs:65`）且无计数；真接 `ModelSummarizer` 会**吃掉主循环的队列响应**（队列有余量⇒静默错位；耗尽⇒`remove(0)` panic） | 中 | 中 | 🟡 P2 |
| **R9** | `NoopModelEventSink` 同 crate 第二份拷贝（设计自称「照抄」） | 低 | 高 | 🟢 P3 |
| **R10** | 摘要产物进 session ⇒ 成为 compaction 的输入，compaction 的提示词只说「保留关键决策与事实」，**untrusted 标注可能被洗掉** | 低 | 中 | 🟢 P3 |
| **R11** | 第二条腿用 `head.content.clone()`（≤50KB 无谓拷贝），与 Q1「`&str` 避免拷贝」的自我要求矛盾 | 低 | 高 | 🟢 P3 |

> **R8 的一处澄清（设计未提但值得记住）**：今天 `apps/coding-agent/tests/integration.rs` 躲过 MockModel 队列错位纯属**结构上的侥幸** —— 那里的模型是手写的、按 `has_tool_results` 分支的无状态实现（`integration.rs:44-52`），而不是队列式 `MockModel`。真正用队列式 `MockModel` 的是 `ys-loop/src/lib.rs` 与 `ys-runtime/tests/*`，今天不碰 webfetch，但往后的用例很容易踩。

---

## 改进建议

### 易修复（低风险快速改进）

1. **删掉第二次 `head()`** —— 用既有的 `head` 当 `cap`，顺带消掉 `head.content.clone()`（先定 `content`、再 move，或把指针串先算好）。一并消掉 R6、R11。
2. **ADR 措辞降级**（R1）—— 把「实测中不存在的场景」改成可辩护的表述：
   > 「在 5 份**按元素覆盖挑选、且多数刻意截到 ≤200KB** 的 HTML 夹具上 0/5 撞闸；`[415KB, 5MiB]` 区间与**非 HTML 透传路径（比值 1.0×）未被测量**。封顶取同头部预算的理由是『不引入今天没有的窗口风险』，而非『已覆盖真实分布』。」
3. **如实记录 `max_tokens` 同样被 config 覆盖为 4096**（R2），并据此**重估 Q1 拒绝 `truncated` 的理由** —— 要么重新论证（去掉「钉在 1024」这半句），要么把 `truncated` 加回来。
4. **notice 的覆盖率从「行」改「字节」**（R4）—— 例如「Summary input was capped at 50KB — 读数 X / 共 Y 字节（约 Z%）」。行读数在单行正文上是假的。
5. **`NoopModelEventSink` 提成 `ys-loop` 的 `pub(crate)`**（R9）—— 一行。
6. **代数补单位、推导与 `k<1` 的情形**（R8 呈现）—— 或直接删公式，改成可证的定性表述：「喂进摘要器的输入不得超过无 prompt 时本就要发的头部；超出部分的风险与收益均未被测量」。

### 需讨论（需要团队决策）

7. **取消语义**（R7）—— abort 时是否照 `FetchError::Aborted` 返回**短消息**（`"Fetch aborted by user during summarization."`，不带正文），而不是塞回 ≤50KB 头部？理由是 ADR-0013 里 Abort 会中止本回合，这份头部大概率永远用不上，却要付一次 session 写入与后续轮次的重复发送。
8. **非 HTML 正文是否该分档封顶**（R3）—— 单一字节闸横跨两种内容形态（HTML 2.25–32× vs 非 HTML 1.0×）是 R3/R4 的共同根因。最小选项：按 `ContentKind` 分两个常量；或对 `Text` 类内容直接把封顶调小（因为它没有转换收益，摘要也更难「抽取」）。
9. **退化保护**（R4）—— 对 `hit == Some(Bytes) && shown_lines < 5` 这类「只截到头几行」的情形，照「宁可不总结」直接跳过摘要 + 明说。这与质量目标一致，代价是一个早返回分支。
10. **测试策略写死**（R8）—— ① 带 prompt 的用例**一律注入独立假 `Summarizer`**，绝不让 `ModelSummarizer` 接 `MockModel`；② 若确需断言请求形状，新建「记录型 `Model`」测试基建（设计 §2.1 的文件清单要补上）；③ 可选：给 `MockModel` 加计数 + 队列耗尽返回 `Err` 而非 panic。
11. **把「今天零可观测」列为本次的可靠性缺口**（R5），而不是只写升级路径 —— 至少让风险章节承认「270s 无输出」是需要在发布说明里告知用户的行为。

### 架构级（影响面大，需要跨 phase 规划）

12. **「工具 → 事件」通道的缺失开始产生真实代价**。本切片之前，工具都是秒级或十几秒级、且不需要中途反馈；摘要把「一个工具调用」拉到典型 ~18s / 乘性 270s 的**零反馈**区间。设计正确地**没有**在本切片里动这条通道（那会破坏「工具无事件出口」的结构不变式，且与「不新增 `AgentEvent`」抵触），但**这个缺口的性价比拐点可能已经到了**。建议：本切片完成后，用真实使用数据回看「用户是否把 270s 无输出误判为卡死」，再决定是否值得开一条窄的进度通道。
13. **封顶常量与 ADR-0017 的窗口配置的关系需要一个明确的所有者**。设计把「`min(50KB, f(window))`」列为将来的一行改动，但**没有指明谁在什么时候做**。ADR-0017 落地时会同时引入 `~/.yushan/config.json` 与 `RunLimits.context_window` —— 那正是把 `SUMMARY_MAX_BYTES` 接上去的自然时点。建议在 0018 的 Consequences 里写死这个触发条件（照 0017 自身的「留口子」写法）。

---

## 总体评价

**机制 🟢 / 论据 🟡 / 可观测性 🔴。**

设计的**骨架是扎实的**：窄 trait + `ToolContext` 注入确实绕开了 ADR-0016 当年想象的 `Arc<dyn Model>` + 重排装配，且这个结论经源码逐条核实为真（`RuntimeContext.model` 每回合都在、15 处调用点零改动、`select!` 复用了同文件既有模式）。三处「非默认」选择（窄签名、真取消、同头部预算）我都复核后认可。

但**它最响亮的那句论据站不住**：`0/5 撞闸` 来自一批**按元素覆盖挑选、且多数刻意截到 ≤200KB** 的夹具，而抓取上限是 5 MiB、非 HTML 路径的转换比是 1.0×。结论（封顶取同头部预算）**仍然可以成立**，理由是「不引入今天没有的窗口风险」——但那是一个**保守性**论证，不是**覆盖率**论证，ADR 把两者混为一谈了。这是本报告认为**实施前必须先修**的一处，因为它会以「已实测」的面目进入仓库。

其次，设计**自己抓到了 `temperature` 被静默覆盖，却漏了紧邻一行的 `max_tokens`** —— 同一个 `or` 形状、同一个后果，而且它直接架空了 Q1 拒绝 `truncated` 的一半理由。这条属于「测试会全绿而事实相反」，正是本项目最警惕的失败模式。

**建议**：先做 §易修复的 1–6（工作量小、全是文本与删代码），再对 §需讨论的 7–10 做一次决策，即可进入实施；§架构级 12–13 记入 backlog 与 0018 的 Consequences，不在本切片处理。

---

## 整改记录（2026-09-19）

`arch-validate` 提出的风险项**逐条处置**。**本节的存在使本报告与 `design.md` / ADR-0018 一致** —— 读上方风险表时请对照本节的现状列。

| # | 议题 | 原级别 | 处置 | 落在哪 |
|---|---|---|---|---|
| R1 | ADR 承重证据被样本偏差削弱 | 🔴 P0 | **已修**：删去「实测中不存在的场景」这一过度声明，改为「**保守性**」论证，并写明样本偏差（按元素覆盖挑选 + `blog.html` 刻意截到 204,800）、`[415KB, 5MiB]` 零覆盖、非 HTML 透传比值 1.0× | ADR 决策 3 / design §1 ① / §3 |
| R2 | `max_tokens` 假话未记录 | 🔴 P0 | **已修**：补记 `max_tokens` 与 `temperature` **同形**（`lib.rs:415-416` 的 `config.X.or(request.X)`）且同被覆盖为 4096/0.7；据此**修正 Q1 拒绝 `truncated` 的理由** | design §0 D2 / Q1 / Q2 / ADR 决策 4 |
| R3 | 非 HTML 透传路径覆盖 ~1% | 🟠 P1 | **已决策**：**不分档封顶**（第二档无独立证据，ADR-0014 判据）；危害由**字节覆盖率读数**消解 | design §8 |
| R4 | 单行正文 line-based 覆盖率失真 | 🟠 P1 | **已决策 + 已设计**：覆盖率改**字节**；加**结构性**退化保护（正文贡献 0 字节 ⇒ 不发调用）；`skipped` 与 `failed` 措辞分开 | design §2.3 / §2.4 A、B1、B2 |
| R5 | 零可观测性 | 🟠 P1 | **已决策：接受**，不加事件通道（会破坏「工具无事件出口」的结构不变式）；缓解 = description 措辞 + 发布说明；backlog 触发条件已写死 | ADR Consequences |
| R6 | `cap`/`head` 两次同参调用 | 🟡 P2 | **已修**：删掉第二次 `head()`，复用既有 `head`，顺带消掉 `head.content.clone()` | design §2.2 / §2.3 |
| R7 | 取消语义与 `FetchError::Aborted` 不一致 | 🟡 P2 | **已决策 + 已设计**：取消走**独立分支**，短消息、不回落头部 | design §2.3 / §2.4 D |
| R8 | 测试基建缺口 | 🟡 P2 | **已决策**：① 带 prompt 的用例一律注入独立假 `Summarizer`（写死成纪律）；② 新建「记录型 `Model`」测试基建；③ 改 `MockModel` 记为独立 backlog（防不住错位，只把 panic 变 Err） | design §8 / exec-plan T4、T6 |
| R9 | `NoopModelEventSink` 第二份拷贝 | 🟢 P3 | **已决策**：提成 `ys-loop` 的 `pub(crate)`，`generate_summary` 一起用 | design §8 / exec-plan T3 |
| R10 | 摘要被 compaction 二次摘要，untrusted 标注可能被洗掉 | 🟢 P3 | **已决策：记录，本切片不改**（那是 ADR-0017 的领地） | ADR Consequences |
| R11 | `head.content.clone()` 无谓拷贝 | 🟢 P3 | **已修**（随 R6 一并消掉） | design §2.3 |
| — | 代数呈现不可复核 | 🟡 | **已修**：补单位与推导；并补 `k<1`（prompt cache）下 `R*` 从 1.14 升到 2.41、以及「单轮回合净亏」 | design §1 ① / §3 / ADR 决策 3 |
| — | 工具→事件通道缺失 | 架构级 | **backlog**：触发条件 = 真实使用数据显示乘性延迟被误判为卡死 | backlog |
| — | `SUMMARY_MAX_BYTES` 与 ADR-0017 窗口配置接线 | 架构级 | **写死触发条件与 owner** = ADR-0017 的实施者 | ADR Consequences |

> **一处对复核员结论的更正（诚实记录）**：独立对抗复核曾判「`R*` 代数不成立 / 不可复现」——**该判定是误报**，
> 源于把 KB 与 token 混用。用 token 重算**完全复现**：`R*(C=50KB) = (12800+1400)/(12800−350) = 1.14`、
> `R*(C=200KB) = (51100+1400)/12450 = 4.22`。代数本身正确，问题只是**呈现**（缺单位与推导）。

**仍未闭合的一项**（不是缺陷，是待测量）：**Q3 封顶常量值**需 `prototype` 用 `tests/golden/` 五页实测「封顶 vs 摘要质量 vs token」。
本切片按 `ys_tool::MAX_BYTES` 落地；实测后可能只改一个常量。**机制已定，只有那个数待定。**
