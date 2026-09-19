# webfetch 总结能力 设计计划

## 背景

- **当前状态**：`webfetch` 抓取 URL → HTML 转 Markdown → 落盘 → 返回头部（2000 行 / 50KB）+ 指针。模型拿到的是**原文头部**，不知道「这一页对它当前的问题有没有用」。
- **问题**：模型需要的是**按意图抽取**（如「这个 API 的鉴权头怎么填」），而不是一整页原文。且头部每轮随会话重发，一个 50KB 的页面会长期占用上下文。
- **为什么需要这个变更**：ADR-0016 曾以「`Tool::call` 拿不到 `Model`，需注入 `Arc<dyn Model>` + 重排装配顺序」为由**否决**了 Claude Code 式的总结能力。该否决的两条实现路径理由经复核**偏重**（不必 `Arc`，也不必重排 —— `RuntimeContext` 每回合已带 `model`，`ToolContext` 本就每回合新建），故由 ADR-0018 显式重开。
- **上游**：`docs/arch/webfetch-summarize/{context,design,review}.md` + `docs/adr/0018-webfetch-summarize.md`。

## 设计

### 整体方案

`webfetch` 新增**可选** `prompt` 参数：

- **给了 `prompt`** ⇒ 抓取 + 转 MD + 落盘后，调一次模型（同一模型 + 固定系统提示词）按该意图做摘要，返回**摘要 + 指针**。
- **没给 / 空串 / 全空白** ⇒ 行为与今天**逐字节相同**，且**不发生任何模型调用**（核心不变量）。

### 架构决策

| # | 决策 | 理由 |
|---|---|---|
| D1 | trait 形状 `async fn summarize(&self, content: &str, prompt: &str) -> Result<String, String>`，落 `ys-tool` | `ys-model` 依赖 `ys-tool`（为 `ToolSpec`）⇒ 反向成环 ⇒ `ToolContext` 写不出 `&dyn Model`。窄 trait 是唯一出路，与 `ApprovalHandler` 同形。`&str` 避免 ≤50KB 拷贝；`String` 错误因为三种失败处置完全相同 |
| D2 | 调用参数：固定 system；`tools: vec![]`；`max_tokens: 1024`；`temperature: None` | ⚠️ **`max_tokens`/`temperature` 在 coding-agent 装配下均被 `config.X.or(request.X)` 静默覆盖为 4096/0.7**（`main.rs:610-611`）—— 两个值只在库路径生效，如实记录，不作线上保证 |
| D3 | 内容封顶**复用头部的 `MAX_LINES`/`MAX_BYTES`（2000 行 / 50KB）** | **保守性**论证（不是覆盖率）：今天无 prompt 时同一模型本就要吃 ≤50KB 头部 ⇒ 不引入今天没有的窗口/400 风险 |
| D4 | 摘要超时 `SUMMARIZE_BUDGET = 60s`，独立于 `FETCH_BUDGET` | 30s 是网络账、60s 是推理账；不设内层超时的话，挂死的调用会撞外层 `bash_timeout`，外层只回 `Command timed out`，**正文与指针一起丢** |
| D5 | 取消用 `select! { biased; wait_aborted, timeout(60s, summarize) }` **真取消** | 复用 `net.rs` 既有模式（`fetch_capped:225` 已是同款）；drop future 即可取消 in-flight 请求，**零侵入 `ys-model`** |
| D6 | 取消走**独立分支**：短消息、不回落头部 | 照 `FetchError::Aborted`（`fetch.rs:271-276`）的既有形状。abort 后本回合会被轮边界结束，≤50KB 头部只会白写一次 session |
| D7 | 三分 `Ok` / `Skipped` / `Failed` | `skipped` = 我们**主动没调**（退化保护），`failed` = **调了但挂了**。处置相同、语义相反，合并会让「形态不适合」伪装成故障且测试钉不住 |
| D8 | 覆盖率 notice 报**字节**而非行 | `head` 是面向行的截断器，单行正文下会报「lines 1-3 of 4」（75%）而正文覆盖 0% |
| D9 | 退化保护：正文**一个字节都没进**摘要输入时不发调用 | 结构性判据 `head.hit.is_some() && shown_lines <= 3`，非魔数百分比 |
| D10 | 不新建 ADR 之外的能力；无缓存、无配置项、无事件、无分档 | ADR-0014 判据 |

### 数据流

```
url → vet_url → fetch_capped → 分派/解码/htmd → file_content → 落盘     【全部不动】
    → head = ys_tool::head(&file_content, 2000行/50KB)                   【不动】
    ├─ prompt 缺省/空/全空白 ──► 现状：head + notices（0 次模型调用）
    └─ prompt 有
         ├─ 已置位 Abort ──► "Fetch aborted by user during summarization."（无 notice、无正文）
         ├─ head_is_header_only ──► Skipped ⇒ head + "Summary skipped: …"
         └─ 正常
              select! { biased; wait_aborted → Failed("cancelled"),  ← 注：此处由 D6 前置探针覆盖
                        timeout(60s, summarize) }
              ├─ Ok(text)  → "## Summary of <url> (… untrusted …)\n\n{text}" + 字节覆盖率/指针
              └─ Err(why)  → head + "Summary failed: {why} — showing the page head instead"
         两条回落腿 is_error = false（ADR-0001）
```

### 接口变更

**新增（`crates/ys-tool/src/summarizer.rs`）**

```rust
#[async_trait]
pub trait Summarizer: Send + Sync {
    async fn summarize(&self, content: &str, prompt: &str) -> Result<String, String>;
}
```

**修改（`crates/ys-tool/src/context.rs`）** —— `new()` 签名**不动**（15 处调用点零改动）

```rust
pub struct ToolContext<'a> {
    pub boundary: Option<&'a dyn BoundarySource>,
    pub cwd: PathBuf,
    pub workspace_root: PathBuf,
    pub summarizer: Option<&'a dyn Summarizer>,   // 新增
}
pub fn with_summarizer(mut self, s: &'a dyn Summarizer) -> Self { … }   // 新增
```

**新增（`crates/ys-loop/src/summarize.rs`）** —— `SUMMARIZE_SYSTEM` 常量 + `pub(crate) struct ModelSummarizer<'a>(&'a dyn Model)` + `pub(crate) struct NoopModelEventSink`（从 `basic.rs` 提取）

**修改（`crates/ys-loop/src/basic.rs:308`）** —— 唯一生产接线点

```rust
let summarizer = ModelSummarizer { model: ctx.model };
let tool_ctx = ToolContext::new(ctx.boundary, ctx.cwd.clone(), ctx.workspace_root.clone())
    .with_summarizer(&summarizer);
```

**修改（`adapters/tools-web/src/fetch.rs`）** —— schema 加可选 `prompt`、description 改写、`SUMMARIZE_BUDGET`、三分 `Outcome`、`summarize_bounded`、`head_is_header_only`、`summary_pointer`

**修改（`adapters/tools-web/src/net.rs`）** —— `wait_aborted` 私有改 `pub(crate)`（一行）

## 涉及文件

- **新增**：`crates/ys-tool/src/summarizer.rs`、`crates/ys-loop/src/summarize.rs`
- **修改**：`crates/ys-tool/src/context.rs`、`crates/ys-tool/src/lib.rs`、`crates/ys-loop/src/basic.rs`、`crates/ys-loop/src/lib.rs`、`adapters/tools-web/src/fetch.rs`、`adapters/tools-web/src/net.rs`、`adapters/tools-web/tests/tools.rs`、`adapters/tools-web/tests/contracts.rs`、`apps/coding-agent/src/prompt.rs`
- **删除**：无
- **明确不动**：`ys-runtime`、`ys-component`、`ys-protocol`、`ys-session`、`ys-tui-coding`、`apps/coding-agent/src/{toolset,main}.rs`、`EXPECTED_ORDER`（工具数仍 9）

## 测试策略

### 测试合同（Phase 3 审查依据）

#### N1: W4 核心不变量 —— 无 prompt 不调模型
- **验收方法**: `cargo test -p ys-tools-web` — 假 `Summarizer` 带 `AtomicUsize` 计数，无 `prompt` 时断言**计数 == 0**；变异（把调用提到 `if` 外）必须变红
- **优先级**: 必测

#### N2: prompt 空串/全空白等同未给
- **验收方法**: `cargo test -p ys-tools-web` — `prompt: ""` 与 `"   "` ⇒ 计数 == 0，输出与无 prompt **逐字节相同**
- **优先级**: 必测

#### N3: 成功路径输出形态
- **验收方法**: `cargo test -p ys-tools-web` — 输出含 `## Summary of` + 摘要正文 + untrusted 标注 + 指针 notice，且**不回灌正文行**
- **优先级**: 必测

#### N4: failed 腿
- **验收方法**: `cargo test -p ys-tools-web` — 假 summarizer 返 `Err` ⇒ `head` 内容 + `Summary failed:` notice + `is_error == false`，落盘文件逐字节不变
- **优先级**: 必测

#### N5: skipped 腿（退化保护）
- **验收方法**: `cargo test -p ys-tools-web` — >50KB 且**无换行**的正文 ⇒ **计数 == 0** + `Summary skipped:` notice + 回落头部；变异（把措辞写成 `Summary failed`）必须变红
- **优先级**: 必测

#### N6: abort 腿
- **验收方法**: `cargo test -p ys-tools-web` — 预先 `push(Boundary::Abort)` ⇒ 正文**只有** `Fetch aborted by user during summarization.`，**无 notice、无头部**；变异（改成回落头部）必须变红
- **优先级**: 必测

#### N7: 工具契约
- **验收方法**: `cargo test -p ys-tools-web` — `contracts.rs` 断言 description 仍含 `saved` 与 `path`，且含 `prompt`；schema 的 `required == ["url"]`
- **优先级**: 必测

#### N8: 系统提示词同步
- **验收方法**: `cargo test -p ys-coding-agent` — `prompt.rs` 的 snippet/guideline 断言更新后通过，且**保留** `do not refetch the same URL` 子串断言
- **优先级**: 必测

#### N9: 无回归
- **验收方法**: `cargo test --workspace` — 基线 **636 passed**，改动后**只增不减**且既有用例零改动通过；`cargo clippy --all-targets` 零 warning
- **优先级**: 必测

#### N10: 摘要请求形状
- **验收方法**: `cargo test -p ys-loop` — 新建「记录型 `Model`」测试基建（放 `ys-loop` 的 `#[cfg(test)]`），断言 `tools.is_empty()`、`system` 含 untrusted 框架、请求 `max_tokens == 1024`（注释写明会被 app 的 4096 覆盖，**不得当成线上保证**）、空响应 ⇒ `Err`
- **优先级**: 必测

#### N11: 字节覆盖率读数
- **验收方法**: `cargo test -p ys-tools-web` — 覆盖率 notice 是**字节**读数；变异（改回行读数）在 N5 的单行正文用例上变红
- **优先级**: 可推迟

### 测试框架与命令

- 单元 + 集成测试：`cargo test --workspace`
- 单 crate：`cargo test -p ys-tools-web` / `-p ys-loop` / `-p ys-coding-agent`
- Lint：`cargo clippy --all-targets`
- 格式：`cargo fmt --check`
- e2e（需真实凭证，不纳入本切片）：`cargo test -p ys-coding-agent e2e -- --ignored`

### 测试纪律（不是建议）

带 `prompt` 的用例**一律注入独立的假 `Summarizer`**，**绝不**让 `ModelSummarizer` 接 `MockModel` —— `MockModel::complete` 忽略请求并按 FIFO 消费（`crates/ys-model/src/mock.rs:65-68`），接上去会**吃掉主循环的响应**：队列有余量 ⇒ 静默错位；耗尽 ⇒ `remove(0)` panic。

## 波及文档

- `docs/design-final/tools-web.md`（需新增「总结能力」一节，并说明与 ADR-0016 落盘+指针的关系）
- `docs/design-final/coding-agent-tui.md`（webfetch 描述变更若被引用需同步）
- `docs/CONTEXT.md`（可选：是否收录「摘要 / skipped / failed」术语，Phase 4 判定）
- `docs/arch/webfetch-summarize/review.md`（整改记录，Phase 2 已补）

## 风险与注意事项

1. **注入面上升是净负面**：摘要把不可信内容**改写成同级模型口吻的段落**。全部防御 = 标题行标注 + 系统提示词一条规则，**安慰剂级，不得宣传为防护**。
2. **延迟**：单次带 prompt 的抓取从 1–3s 变典型 ~18s（最坏 60s）；且 `basic.rs` 对一条 message 里的多个 tool call 是**顺序执行** ⇒ N 个抓取是**乘性**的（3 个 ⇒ 上限 270s）。**全程 TUI 无进度反馈**（工具无事件出口）。
3. **摘要不够用时反而更贵**：模型读摘要 → 觉得不够 → `read(path, offset=…)`，那一轮输入是整段会话。
4. **Usage 少算**：摘要调用的 token 不进 `TurnStats` —— 今天**没有任何能消费它的缝**（`tools-web` 绝不 print；`logging.rs` 是 app 私有且方向单向）。已知缺口，与 compaction 同源。
5. **`bash_timeout` 交互**：用户若调到 <90s，带 prompt 的抓取会被外层超时先杀，报误导性的 `Command timed out`。
6. **模型可能滥用 `prompt`**（「更努力」倾向），把「可选」变成事实必选。唯一闸门是描述与 guideline 的措辞。
7. **Q3 封顶常量值待实测**：机制已定，本切片按 `ys_tool::MAX_BYTES` 落地；`prototype` 实测后可能只改这一个常量。
8. **CJK 是更松的闸**：50KB 字节闸对中文 ≈ 25.6K token，实测夹具（49,285 B）已贴近。
