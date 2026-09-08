# TUI 改进架构 — 架构质量分析报告

## 分析范围

- **对象**：`docs/arch/tui-resident-status/design.md` + `adr-tui-resident-status.md` 提议的实施方案
- **维度**：全量（可行性 / 可维护性 / 可理解性 / 性能与可靠性），重点关注**解耦质量**
- **来源**：design.md + context.md + improvements-p1-p2-p3.md + 对现有源码的事实核查

## 事实核查（先于打分）

| design.md 声明 | 事实 | 影响 |
|----------------|------|------|
| "`Agent::tool_names()` 返回 `Vec<&'static str>`" | ❌ `ToolSpec.name` 是 `String`，**不是 `&'static str`**（`crates/agent-tool/src/spec.rs:7`） | 类型签名需改为 `Vec<String>` 或 AppView 内 clone String |
| "`Agent::tool_names()` 实现为 `self.registry.names()`" | ❌ `ToolRegistry` **没有** `names()` 方法（`crates/agent-tool/src/registry.rs:18-55`） | 需新增 `pub fn names(&self) -> Vec<&str>` 或 `Vec<String>` |
| "暴露 `cancel()` 而非 `&mut CancelToken`" | ✅ 正确——`CancelToken` 是 `Arc<AtomicBool>`，Clone 廉价 | 决策正确 |
| "AgentBuilder::cancel_token 已公开" | ✅ `builder.rs:62-65` | 实施时 TUI 可注入同一 token 而非用 Agent.cancel() |
| "`format::print_footer` 接受原始数据" | ⚠️ review 阶段二次核查：design.md 主线一致——line 307「已吃 Config」指改造**前**的 print_banner/render_status；line 309「改为只吃 AppView」指改造**后** | 无需修正 |
| "CommandContext 加 `view: &mut AppView` 字段供命令读" | ✅ 与 ADR §6 一致 | 决策正确 |

**3 处事实性错误需修正 design.md**（见末尾「易修复」）。

---

## 各维度判断

### 1. 可行性

| 检查项 | 判断 | 关键发现 |
|--------|------|----------|
| 1.1 技术可实现性 | 🟢 | `AppView::from_sources` 是普通函数 + owned 数据，零运行时成本；`CommandContext<'a>` 加字段是 Rust 标准模式；rustyline Completer 是标准 trait 实现 |
| 1.2 依赖成熟度 | 🟢 | 新增依赖仅 `rustyline = "14"`（成熟，14k+ stars）；其它都是 std + 现有 crate |
| 1.3 实现周期 | 🟢 | 6 个独立 PR × 0.3-1d = ~4d；与 design.md 估算一致 |

### 2. 可维护性

| 检查项 | 判断 | 关键发现 |
|--------|------|----------|
| 2.1 模块边界 | 🟢 | **核心收益**：render 层（format/tui/completer）零 source 耦合；commands 仍直接改 sources（避免双写）；StateStore 与 ProviderRegistry 职责分明 |
| 2.2 接口稳定性 | 🟢 | Agent 公开 API 收敛（3 getter + 1 action），未来加 footer 字段 0 API 变更；AppView 是应用层 struct，crate 解耦清晰 |
| 2.3 测试难度 | 🟢 | format.rs 单测只需构造 `AppView::default()`；StateStore 单测用 `path_override`；CmdCompleter 单测 mock entries |
| 2.4 错误传播 | 🟢 | `&mut W: Write` 返回 `io::Result<()>`，错误向上传播；`StateStore::save` 返回 `Result<(), String>`（与现有 `ProviderRegistry::save_auth` 一致）；`from_sources` 不会失败（纯聚合函数） |
| 2.5 并发安全 | 🟢 | 单线程 REPL；`view_dirty` 是局部 `bool`；`CancelToken` 是 `Arc<AtomicBool>` 跨线程安全 |
| 2.6 资源管理 | 🟢 | `AppView` 全部 owned（PathBuf/String/Vec），无引用 / 无泄漏；rustyline 自动管 history 文件 |
| 2.7 演进收敛性 | 🟢 | design.md 给出「未来加 footer 字段改动范围表」——加 git branch/theme/active skill 都不需要 Agent API 变更 |

### 3. 可理解性

| 检查项 | 判断 | 关键发现 |
|--------|------|----------|
| 3.1 概念一致性 | 🟡 | 🟡 **AppView 字段将持续增长**——v0 14 字段；如果未来加 theme/active skill/group 等可能突破 25 字段临界点。design.md 提到「review 控制粒度」「超过 20 字段重构为分组 struct」——但**没有给出具体分组方案** |
| 3.2 抽象层次 | 🟢 | render 层（format.rs）只读 AppView → 单向；tui.rs 是协调层；commands 改 sources 是执行层。三层清晰 |
| 3.3 文档完整度 | 🟢 | design.md + ADR 详尽；future 路径给出明确表格 |
| 3.4 新人上手 | 🟡 | 🟡 新人需理解「为什么 commands 仍直接改 Config/Agent 而不是改 view」——这是反直觉的，需要 doc comment 解释（避免新人加命令时走错路径） |

### 4. 性能与可靠性

| 检查项 | 判断 | 关键发现 |
|--------|------|----------|
| 4.1 性能模型 | 🟢 | `view_dirty` 避免每轮 rebuild；5 次 render/turn × 15μs = 75μs/turn，**vs LLM 调用秒级背景完全不可观测** |
| 4.2 故障模式 | 🟡 | 🟡 **view_dirty 漏设风险**——如果某个新加的命令忘了设 dirty，下一次 print footer 会显示 stale 数据。design.md 提「靠 review 抓」——但更稳妥是**自动化测试**：每个改 sources 的命令加 dirty 单测 |
| 4.3 退化策略 | 🟢 | state.json 缺失 → Default；auth.json 缺失 → 空 HashMap；rustyline 失败 → `Result<_, ReadlineError>` 向上传播 |
| 4.4 可观测性 | 🟢 | view_dirty 标志可扩展为「计数器」，未来可加 tracing |

### 5. 与硬约束的契合度

| 硬约束 | 满足？ | 备注 |
|--------|-------|------|
| 不引入 TUI 框架 | ✅ | rustyline 是行编辑库，不算框架 |
| 不跨动态库边界传 trait/Tokio 类型 | ✅ | `CancelToken` 是 value type（Arc<AtomicBool>），Clone 廉价 |
| 最小核心（CLAUDE.md） | ✅ | render 层抽象限定在 `apps/coding-agent/`，不渗透到 `crates/` |
| 演进收敛 | ✅ | Agent API 收敛；AppView 字段可分组重构 |

---

## 解耦质量专项评估

user 特别强调「解耦」。从 5 个维度量化：

| 维度 | 评分（1-5） | 论证 |
|------|----------|------|
| **format.rs 独立** | ⭐⭐⭐⭐⭐ | 只读 `&AppView`，不依赖 Config/Agent 任何字段 |
| **tui.rs 协调层** | ⭐⭐⭐⭐ | 持有 view + view_dirty + 多个 source，但通过 AppView 抽象 |
| **Agent 公开 API 收敛** | ⭐⭐⭐⭐ | 3 getter + 1 action，本轮后不再扩张 |
| **StateStore 独立** | ⭐⭐⭐⭐⭐ | 与 auth.json 完全分离；不同文件、不同权限、不同演进 |
| **commands 改 sources 路径** | ⭐⭐⭐ | 不通过 view 反向 sync 是对的，但**新人易误解**——需 doc |

**总体解耦质量**：⭐⭐⭐⭐（4/5）

---

## 风险排序

| # | 风险 | 影响 | 可能性 | 优先级 | 建议 |
|---|------|------|--------|--------|------|
| **R1** | design.md 类型签名错误（`Vec<&'static str>` 应为 `Vec<String>`） | 低（编译时才发现） | 高 | P1 | ✅ **已修正** — design.md §解耦评估表格 + ADR §决策4 都已改为 `Vec<String>` |
| **R2** | design.md §4.1 写 `self.registry.names()` 但该方法不存在 | 低 | 高 | P1 | ✅ **已修正** — design.md §解耦评估表格改为两种实现路径（clone specs 或新增 `ToolRegistry::names()`）；§关键文件清单新增 `crates/agent-tool/src/registry.rs` 的 MODIFY 项 |
| **R3** | design.md 内部不一致（方案 A 段说「原始数据」、方案 C 段说「`&AppView`」） | 中（实施时混乱） | 高 | P1 | ✅ **已修正** — review 二次核查发现 design.md 主线一致；review.md 自身更新描述 |
| **R4** | view_dirty 漏设某处 → footer 显示 stale 数据 | 中（用户体验受损） | 中 | P2 | PR review checklist + 单测覆盖 |
| **R5** | AppView 字段超 20 后维护性下降 | 中（长期） | 中 | P3 | 实施时按「identity/session/capabilities」分组；未来重构 |
| **R6** | commands 改 sources 但忘 save state.json | 中 | 中 | P2 | 在 LoginCommand/ModelCommand/LogoutCommand 加单测：检查 state.json 是否被正确写入 |
| **R7** | rustyline `readline` 与原 `read_line` 的错误处理语义不同 | 低 | 低 | P3 | 文档化新行为（Ctrl-D = exit / Ctrl-C = continue with hint） |
| **R8** | Agent::cancel() 与 CancelToken.clone() 共存导致 CancelToken 被多处持有 | 低 | 低 | P3 | 文档化 cancel 语义：调用 cancel 后当前 turn 优雅退出，下一轮 turn 需新建 |
| **R9** | `view.commands: Vec<CommandMeta>` 与 `CommandRegistry` 数据源可能漂移 | 低 | 低 | P3 | `from_sources` 接收 `commands: Vec<CommandMeta>` 作为参数而非内部构造 |

---

## 改进建议

### 易修复（低风险快速改进）

1. **修正 design.md §4.1 类型签名**
   ```rust
   // 改前（错）：
   pub fn tool_names(&self) -> Vec<&'static str> {
       self.registry.names()
   }
   // 改后（对）：
   pub fn tool_names(&self) -> Vec<String> {
       self.registry.specs().iter().map(|s| s.name.clone()).collect()
   }
   ```
   或新增 `ToolRegistry::names()` 返回 `Vec<&str>`（基于 specs 缓存，零分配）

2. **修正 design.md 内部不一致**
   - 删除方案 A 段关于「format 接受原始数据」的描述
   - 统一为方案 C：所有 print_* 接 `&AppView`

3. **补 ADR §6 文档 comment**
   在 `commands/mod.rs::CommandContext` 字段上加注释：
   ```rust
   /// Commands modify sources (Config/Agent/StateStore) directly.
   /// view is for READ ONLY — to avoid stale-data bugs, never sync back from view.
   pub view: &'a mut AppView,
   ```

4. **补 PR review checklist**
   - 任何修改 `Config` / `Agent` / `StateStore` 的命令 → 必须设 `view_dirty = true`
   - 任何修改 `Config.model` 的命令 → 必须 `state.save()`

### 需讨论（需要团队决策）

5. **AppView 字段是否现在就分组**
   建议：**不分组**——v0 14 字段可控；分组会增加 `view.identity` / `view.session` 等嵌套，未来重构时一次性切分更稳。当前在 `from_sources` 加注释提示「如果字段超过 20，按下表分组」即可。

6. **rustyline 的 Ctrl-D 退出语义**
   当前规划：`Ctrl-D → break`（退出 REPL）。
   建议确认：是否要 `Ctrl-D` 退出？还是用 `Ctrl-D = EOF + 用户显式 `exit`/`quit` 命令？

### 架构级（影响面大，需要跨 phase 规划）

无。当前设计在 v0 范围内已是最简 + 最解耦。

---

## 总体评价

### 健康度

**整体 🟢 优秀**。解耦质量 4/5，演进收敛性已证明（Agent API 本轮后不再扩张），format 模块化为纯函数层可独立单测。

**3 处事实性错误已全部修正**：
- ✅ 类型签名 `Vec<&'static str>` → `Vec<String>`（design.md §解耦评估 + ADR §决策4）
- ✅ `ToolRegistry::names()` 路径建议（design.md §关键文件清单新增 registry.rs MODIFY）
- ✅ 「format 接受原始数据 vs &AppView」二次核查无矛盾（review.md R3 更新描述）

### 最大风险点

**R4 view_dirty 漏设**——纯代码纪律问题，靠 review 抓不够稳。**建议实施时为每个改 sources 的命令加单测**：「执行命令后 view_dirty 应为 true 且 view 字段反映新值」。

### 推荐下一步

1. **立即**（P1）：修正 design.md 的 3 处事实错误
2. **实施**（按 design.md 分组 A→F）：先做 A（view.rs + format.rs 改签名），让 format 改为吃 `&AppView`
3. **每个改 sources 的命令加单测**：login/model/logout/state.json save 验证
4. **PR review 时检查 view_dirty 标志是否设**

### 解耦设计的核心收益（最终验证）

| 收益 | 是否达成 |
|------|---------|
| format.rs 零 source 耦合 | ✅ 通过 AppView 屏障 |
| 加新字段改动最小化 | ✅ AppView 字段 + from_sources 收集 + format 读（3 处） |
| headless 模式零成本复用 | ✅ format 是 `Writer + AppView` 纯函数 |
| Agent API 收敛 | ✅ 本轮后不再扩张（已论证） |
| StateStore 与 ProviderRegistry 独立 | ✅ 不同文件、不同权限、不同演进 |

**建议**：修正 3 处事实错误后，按 6 组独立 PR 实施。
