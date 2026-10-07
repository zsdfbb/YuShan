# session-default-new — 架构质量分析报告

## 分析范围

- **对象**：设计方案 `docs/arch/session-default-new/design.md`（融合方案 A+C）+ 决策 `docs/adr/0026-session-default-new-and-resume.md`
- **维度**：全量（可行性 / 可维护性 / 可理解性 / 性能与可靠性）
- **来源**：design.md + context.md + research.md + **直接读码交叉核对**（`apps/coding-agent` / `crates/ys-session` / `crates/ys-tui-coding` / `crates/ys-protocol`）
- **日期**：2026-10-07

> 立场：对抗式审查。**🔴/P0 项建议在开工前改设计文档**（尤其是 T1）。
> 结论先行：**方向正确、结构克制**（不引入 trait、单文件 meta、默认新会话 + 显式 resume，均符合 AGENTS.md 的「最小核心」与「不做假想消费者」）；但设计自述的**「三模式统一持久化」在实现落点上只覆盖了 TUI**，且有两处会**静默削弱核心功能**的细节缺陷。

---

## 处置状态（2026-10-07 复盘后已修入文档）

| # | 处置 |
|---|---|
| **T1** | ✅ 修入 design §5.1/§5.4/§5.7 **C7**：新增公共 `finalize_turn_meta`，`app_loop` 与 `run_single_turn`（`-p`/`--json`）两条路径都调；补测试行 |
| **T2** | ✅ 修入 design §5.2/§5.7 **C6** + context **D6 修订**：`title` 改为**有界片段（500 字符）**，4 KiB 首行封顶由此成立 |
| T3 | ✅ 修入 design §5.6/§7.4：给出 `main()` 新顺序（`parse_args`/`resolve`/`pick` 提到模型解析之前） |
| T4 | ✅ 修入 design §5.4/§5.7 **C8**：空列表走默认新会话；picker 输出一律 stderr |
| T5 | ✅ 修入 design §5.5/§6：`split_leading_flags` **循环剥离、顺序无关** + 矩阵测试含两种顺序 |
| T6 | ✅ 记为**接受**（design §8）：+1 次全量写 ≈ `1/(N+1)`；不改「只改内存」以免丢标题；归入 §9 复访项 |
| T7 | ✅ 修入 design §5.6/§8 + context **D10 修订**：明确 `thinking_visible` 是显示开关，app 侧**进程级**缓存、`/new` 不重置，消漂移 |
| T8 | ✅ 修入 design §5.6：`meta.turns` 只播种 `TurnStats`；`Envelope.turn` 本进程回合序，resume 后从 1 起 |
| T9 | ✅ 记入 design §9：老会话显示「新会话」为 D9 有意为之，观察项 |
| T10 | ✅ 修入 design §5.4/§6：`persistent_new` 保留 `create_dir_all` |
| T11 | ✅ 修入 design §8：`Wiring::ephemeral` 保留但标 `#[cfg(test)]` |
| T12 | ✅ 修入 design §7.7：文档同步清单加 `architecture.md` **§10** + `wiring.rs`/`events.rs` 头注释 |

> 未采纳：无（T1–T12 全部采纳）。T2/T7 改动了原 D6/D10 的措辞，已在 `context.md` 就地记「修订」。
>
> 附（修复期顺带发现）：`research.md` §4/§5 残留初稿的「主题由模型生成」表述，与最终决策（机械派生，
> D6）矛盾 —— 已改为「机械派生、不调用模型」，并标注本仓 `thinking_visible` 是显示开关而非 Pi 的思考档位。

---

## 各维度判断

| 维度 | 判断 | 关键发现 |
|---|---|---|
| 1.1 技术可实现性 | 🟢 | 纯本地文件 + 已存在的 `serde` / `std::io::IsTerminal`（Rust 1.88，edition 2024，均可用）；无新依赖、无黑科技。三候选对比与 A+C 融合的取舍站得住 |
| 1.2 依赖成熟度 | 🟢 | **零新增依赖**；`meta` 复用 `serde_json`，交互面复用 stdin |
| 1.3 实现周期 | 🟡 | 增量路径清晰，但 **`-p`/`--json` 的 meta 写入点缺失**（T1）、**`main()` 模型解析顺序需重构**（T3）——两者都未被实施步骤覆盖 |
| 2.1 模块边界 | 🟢 | `sessions.rs` 只依赖 `ys-session` + std；`ys-tui-coding` 仍不依赖 `ys-runtime`；`Request::SetThinking` 是纯数据变体，依赖方向不破（C5 成立） |
| 2.2 接口稳定性 | 🟡 | `Session` 加默认方法对 `MemorySession` 零改动 ✅；但 `CodingView` 加字段、`Request` 加变体、**CLI 解析契约放宽**（`split_leading_flags`）三者都需要回归防线；meta 的 `v:1` 明确不背兼容（可接受，但应写进 `meta.rs` 的头注释） |
| 2.3 错误处理 | 🟡 | resume 目标缺失/损坏、空会话列表、picker 输出去向（stdout vs stderr）均未定义；`list` 遇不可读文件的降级未写 |
| 2.4 并发安全 | 🟢 | 无新增共享可变状态；`BoundarySource` 每回合新建的既有铁律未被触碰；多进程并发写已在范围外声明 |
| 2.5 资源管理 | 🟡 | **「首行读取封顶 4 KiB」与「title 存原文、不截断」自相矛盾**（T2）；`update_meta` 可能每回合额外全量重写（T6） |
| 3.1 概念一致性 | 🟡 | `thinking_visible` 在本仓是**显示开关**（ADR-0025），不是 Pi 的**模型思考档位**；把它写进会话 meta 属概念错位（T7）；`turns` 存在双计数（T8） |
| 3.2 上手成本 | 🟢 | 12 个改动文件职责单一；`sessions.rs` 纯函数、`meta` 首行判别规则直白；文档同步清单（design §7.7）基本到位 |
| 4.1 性能模型 | 🟡 | 默认启动零目录扫描是**真实收益**；但 `append` 的 O(n²) 之上再叠每回合 `update_meta` 一次全量写，写放大未量化 |
| 4.2 故障模式 | 🟡 | 模型无凭证降级已定 ✅；崩溃残留 `.tmp`、resume 到损坏/空文件、`/new` 后 UI 与 meta 的 thinking 漂移（T9）未覆盖 |
| 4.3 可观测性 | 🟢 | `-p`/`--json` 末尾 `session: <id>` 走 stderr 的决策正确（stdout 是数据流） |

---

## 风险排序

| # | 风险 | 影响 | 可能性 | 优先级 | 建议 |
|---|---|---|---|---|---|
| **T1** | **`-p`/`--json` 不经 `app_loop`，「首回合写 title/model/tokens」无落点** | 高 | 高（必现） | **P0** | 抽公共函数 `finalize_turn_meta`，三模式都调（见建议 1） |
| **T2** | **列表 4 KiB 封顶 vs meta.title 存原文不截断** → 长首消息标题静默丢失 | 中 | 高（首消息 > ~4 KiB 即现） | **P0** | meta 存「截断后标题」（如 200 字符）或读取不设 4 KiB 死上限 |
| T3 | `main()` 现状在 `parse_args` **之前**恢复 state.json 并 `resolve_limits`，元数据模型优先级（env > meta > state.json）无处安放 | 中 | 高 | P1 | 把 `parse_args` + `resolve`/`pick` 提前，或把 resume 结果注入配置解析（建议 4） |
| T4 | `--resume`（裸 flag）在**空会话目录**下行为未定义；picker 列表/提示的**输出流未指定** | 中 | 中 | P1 | 空列表 → 明确提示 + 开新会话；picker 一律 stderr |
| T5 | `split_leading_flags` 未规定 `--stats` / `--resume` 的**顺序无关**剥离 | 中 | 中 | P1 | 循环剥离 + 解析矩阵测试（含两种顺序） |
| T6 | `update_meta` 每次落盘可能再全量重写一遍会话 | 中 | 中 | P2 | 与下一次 append 合并落盘，或只在文件已存在时 flush |
| T7 | `thinking_visible` 是**显示偏好**却按**会话状态**持久化；`/new` 后 UI 与 meta 漂移 | 中 | 中 | P2 | 见建议 2；至少明确 `CodingView` 字段只作启动初值 |
| T8 | `turns` 双计数：`meta.turns` 播种 `TurnStats`，但 `app_loop` 的 `Envelope.turn` resume 后从 1 重来 | 低 | 高 | P2 | 明确各自语义并写进 meta.rs 注释 |
| T9 | 老会话（无 meta）在列表里**全部显示「新会话」**——可达但不可辨识 | 低 | 高 | P2 | D9 已接受；列表加「无标题」区分或显示文件时间已足够 |
| T10 | `persistent_new` 若不 `create_dir_all`，首次落盘 `File::create` 会因父目录不存在而失败 | 中 | 低 | P2 | 保留现状的 `create_dir_all` 并加断言 |
| T11 | `Wiring::ephemeral` 变为 test-only → 非测试构建 `dead_code` 告警 | 低 | 中 | P2 | `#[cfg(test)]` 或 `#[allow(dead_code)]` |
| T12 | 文档同步遗漏：`architecture.md` §10（明说 `-p`/`--json` 用 ephemeral）、`wiring.rs` 头注释、`events.rs`「本地动作不发请求」 | 低 | 高 | P2 | 纳入 §7.7 清单 |

---

## 关键发现（含证据）

### T1（P0）三模式统一，但 meta 写入只落在 TUI

设计的意图是「三模式共通」（design §5.4 流程 A），但：

- `app_loop::run`（写 title / 刷新 tokens/model 的位置）是 **TUI 专用**：`apps/coding-agent/src/app_loop.rs:72`、Prompt 分支 `:101`、`expand_skill_invocation` `:107`。
- `-p` / `--json` 走的是 **另一条路径**：`Mode::Print` / `Mode::Json`（`apps/coding-agent/src/main.rs:834`、`:857`）→ `run_single_turn`（`apps/coding-agent/src/main.rs:488`）——**不经 `app_loop`，也没有 `TurnStats`**（`run_print_mode` 直接丢弃 `RunResult.usage`）。

后果：D4 说 `-p`/`--json` 也要可被 resume，但它们落盘的文件**永远没有 title/model/tokens**，列表里全是「新会话」。设计 §7 第 4 步只提「三模式统一持久化 + stderr 打 id」，第 5 步只提 `app_loop` —— **写入点漏了**，测试计划（§6）也只列了 `app_loop.rs`。

### T2（P0）4 KiB 封顶与「存原文」互斥

- C4：`list` 只读首行、**封顶 4 KiB**（design §5.7）。
- §5.2：`title` **存原文、不截断**（援引 Codex 实测最长 4412 字符）。

首条用户消息一旦超过 ~4 KiB（粘贴长文很常见），meta 首行被截断 → `serde_json` 解析失败 → 落到「缺省『新会话』」。这正是 C4 想避免的「解析消息」，却因把**原始消息内容塞进 meta** 而复活。两条不变量必须二选一：meta 存**有界**标题，或列表读取**不设 4 KiB 死上限**（读到 `\n` 为止，仅对 meta 行）。

### T3（P1）模型恢复优先级与 `main()` 现有顺序冲突

现状顺序（`apps/coding-agent/src/main.rs`）：`state_store.load()` `:722` → 从 state.json 恢复 provider/model `:729` → `resolve_limits(&config)` `:771` → `parse_args` `:792`。

而 D10 / §5.6 要求 `env > meta > state.json`——**meta 只有在 `parse_args` 之后、`resolve/pick` 之后才拿得到**。要落实就得把 `parse_args`（及 resume 解析）提到配置恢复之前，或把「resume 目标」作为输入注入配置解析阶段。设计把这条列为「待端到端复现」，但它是**结构性顺序问题**，不是偶发验证项。

### T4（P1）空列表与输出流未定义

- `ys-coding-agent --resume` 且 `sessions/` 为空：列表为空 → 报错？还是开新会话？§5.4/§5.5/§9 均未写。
- picker 的打印走 stdout 还是 stderr 未写。若走 stdout，`-p --resume`（stdout 是模型文本）与 `--json --resume`（stdout 是逐行 `Envelope`）都会被污染。design §5.4 C 只对**非 TTY** 路径写了 stderr。

### T6（P2）写放大

`JsonlSession` 每个 `append` 已经全量重写（`crates/ys-session/src/jsonl.rs:93`），`replace_all` 单次落盘（`:110`）；一条回合内多次 append 已是多次全量写。若 `update_meta` 再触发一次 `flush_to_file`，就是**每回合额外一整遍会话**。建议 `update_meta` 只改内存（下次 `append` 一并落盘），或在回合末与最后一次写合并；否则应量化并与 O(n²) 复访项一起记录。

### T7（P2）`thinking_visible` 概念错位 + `/new` 漂移

- 本仓的 `/thinking` 是**内联思考的显示开关**（`crates/ys-tui-coding/src/app.rs:39`、ADR-0025），不是 Pi 的**模型思考档位**。research.md §4 用「Pi 存思考档位」为 D10 背书属概念映射偏差。
- `CodingView` 增加 `thinking_visible` 后，UI 侧 `App.thinking_visible`（运行期真值）与 app 侧 `CodingView.thinking_visible`（持久化镜像）**两份并存**；`App::new` 只消费一次。于是 `/new` 切到空 meta 的会话后，UI 仍保持旧开关、meta 却是空，**直到用户再 toggle 一次**才对齐。设计 §8 恰恰以「两份真值必然漂移」否决了替代方案，此处需自洽。

### T8（P2）`turns` 双计数

`meta.turns` 播种 `TurnStats`（显示累计），而 `app_loop` 的 `turns` 局部计数器（`apps/coding-agent/src/app_loop.rs:85`）决定 `Envelope.turn`。resume 后后者从 1 重来。两者是否应一致（如 resume 后 `Envelope.turn` 也从累计值续）需明确——否则 `/compact`、事件消费方会看到「回合号回退」。

### 可维护性补充

- `Session::meta()` 默认 `None` + `set_meta` 默认 `Ok(())`：`MemorySession` 零改动 ✅，但 `update_meta` 对 `MemorySession` 会**静默成功**（测试里若不换成 `JsonlSession` 会假绿）。
- `meta` 判别顺序（先 `Message` 后 `SessionMeta`）依赖 `Message` 无 `type` 字段（`crates/ys-core/src/message.rs:13`）——成立；但应加一条**契约快照测试**（`Message` 的 serde 形状不含 `type`），否则未来给 `Message` 加 `type` 会静默破坏判别。

---

## 改进建议

### 易修复（低风险，开工前补进设计）

1. **抽 `finalize_turn_meta(wiring, raw_user_text, usage)`**，`app_loop` 与 `run_single_turn`（`-p`/`--json`）三处共用；`run_print_mode`/`run_json_mode` 需保留 `RunResult.usage`。
2. **空列表**：打印「没有可恢复的会话」→ 走 `persistent_new`（与「默认新会话」语义一致）。
3. **picker 所有输出走 stderr**（TUI 交互屏、非 TTY 列表、错误提示统一）。
4. **`split_leading_flags` 循环剥离**，对 `--stats` / `--resume[=<id>]` 顺序无关；补解析矩阵（含两种顺序 + `-p --resume=x --stats "任务"`）。
5. **`persistent_new` 必须 `create_dir_all`**（沿用现状，否则惰性落盘时 `File::create` 失败）。
6. **显示层对 title 折叠空白、控制字符**（原文可能含换行，列表两列会破版）。
7. `Wiring::ephemeral` 标 `#[cfg(test)]` 或 `#[allow(dead_code)]`。

### 需讨论（团队决策）

1. **标题有界性**（T2）：推荐 **meta 存「截断后的标题」（例如 200 字符 + 省略号）**，同时保证列表 4 KiB 封顶成立；若坚持存原文，则必须放弃 4 KiB 死上限。
2. **`thinking_visible` 是否进入会话 meta**（T7）：显示偏好通常应全局；若确要按会话，需定义 `/new`、`/login`、`/model` 等非 turn 命令后如何同步 UI 与 meta，并明确 `CodingView.thinking_visible` 只是「启动初值、之后不消费」。
3. **`turns` 语义**（T8）：`meta.turns`（显示累计）与 `Envelope.turn`（本进程回合序）是否统一。
4. **模型恢复的注入点**（T3）：`parse_args`/resume 解析提前到配置恢复之前，还是把解析结果显式传入配置恢复函数。

### 架构级（跨 phase）

- **无。** 本设计在架构上是正确收敛：不引入 `SessionCatalog`/`SessionPicker`/`TitlePolicy`（否决理由与「不做假想消费者」一致）；meta 单文件而非 sidecar 与 `JsonlSession` 全量重写的一致性模型自洽；默认新会话 + 显式 resume 与三家业界实践一致。

---

## 总体评价

**整体健康度：中上。** 架构方向、抽象层级与取舍都是对的，候选 B 的否决、sidecar 的否决、`=` 形式消除歧义都体现了对既往 ADR 与铁律的尊重；实现面 12 个文件职责单一、测试计划分层清晰。

**最大风险点集中在「设计到实现的落点」而非「设计本身」**：

- T1 会让 D4「三模式统一持久化」名不副实（`-p` 会话无标题、不可辨识）；
- T2 会让核心的「列表带主题」在最需要它的场景（长首消息）下退化；
- T3 是一处未被实施步骤覆盖的 `main()` 顺序重构。

**推荐下一步**：先按「易修复」1–5 改 `design.md`（补 `-p`/`--json` 的 meta 写入点、标题有界性、空列表与输出流、`split_leading_flags` 顺序、`create_dir_all`），再进入实现。这三项改完后，本设计可视为**架构层无阻塞**。
