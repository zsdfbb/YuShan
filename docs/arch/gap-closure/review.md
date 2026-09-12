# 核心信道 + Actor 模型 — 架构质量分析报告

> 分析日期：2026-09-12
> 对象：`docs/arch/gap-closure/design-core-channel.md` + `docs/adr/0011-core-channel-design.md`
> 基线：`docs/arch/gap-closure/context.md`、ADR-0009/0010/0012

## 分析范围

- 对象：**设计方案文档**（未实现）
- 维度：全量（可行性 / 可维护性 / 可理解性 / 性能与可靠性）
- 来源：design.md + ADR + **对照实际代码核实**（`crates/agent-loop/src/basic.rs`、`crates/agent-event/`、各 crate 的 `Cargo.toml`）

**方法说明**：本报告对设计中的**每处代码级断言**都做了核实（而非照抄文档）。核出的 3 个 🔴 全部来自**设计自身的不自洽**，而非外部约束。

## 各维度判断

| 维度 | 判断 | 关键发现 |
|------|------|----------|
| 1.1 技术可实现性 | 🟢 | 全部为 std/tokio/serde，无黑科技；`try/await` 是既有模式（`mio`/`tokio::try_send` 同构） |
| 1.2 依赖成熟度 | 🟢 | tokio / async-trait / serde，无新增依赖（`ys-channel` 零依赖） |
| 1.3 实现周期 | 🟢 | 6 步均可独立编译验证；步 1（async emit）触碰约 10 个调用点，可控 |
| 2.1 模块边界 | 🟢 | `ys-channel` 契约与接线器实现分离，依赖单向无环，与 `design.md §9` 一致 |
| 2.2 接口稳定性 | 🟡 | `Envelope` 是 `--json` 的对外契约但**无版本字段**（perf 方案有，被裁掉）；`Source(String)` 每次 clone 一次堆分配 |
| 2.3 错误处理 | 🟡 | `try_emit` 返 `Err(AgentEvent)`、`emit` 返 `Err(EventError)`——**同一操作两种错误类型**；`SendFailed` 一名兼表"消费者消失"与"信道错误" |
| 2.4 并发安全 | 🔴 | **`Inbox` 的产消并发模型不自洽**——见风险 R1 |
| 2.5 资源管理 | 🟡 | 有背压（绿）；但 **`Forwarder` overflow 的 flush 点未定义，会静默丢事件**——见 R2；容量内存估算偏低（见 R5） |
| 3.1 概念一致性 | 🔴 | **"队列 = 日志 + 游标" 未被真正落实**——`Inbox.log` 与 `Session.messages` 重复持有消息——见 R3 |
| 3.2 抽象层次 | 🟡 | `try/await` 双路径对调用方是隐式的（必须记得用自由函数 `emit()`，无强制手段） |
| 3.3 文档 | 🟢 | design.md + ADR-0011 + context.md 三层齐备，且记录了未选方案与理由 |
| 3.4 上手成本 | 🟡 | `try_emit`/`emit`/自由函数 `emit()` 三名并存，新人易误用慢路径 |
| 4.1 性能模型 | 🟢 | back-of-envelope 完整（96 B/事件、20–200 events/s、0.02% CPU）；结论"信道非瓶颈"经复核成立 |
| 4.2 故障模式 | 🟡 | 消费者消失有策略（绿）；但**背压无观测**——队列满导致 agent 静默变慢，无指标——见 R6 |
| 4.3 可观测性 | 🔴 | 无 metrics/tracing 埋点；对"有背压的系统"而言，**背压发生与否不可见**是盲区 |

## 风险排序

| # | 风险 | 影响 | 可能性 | 优先级 | 说明 |
|---|------|------|--------|--------|------|
| **R1** | `Inbox` 产消并发模型不自洽 | 高 | **确定** | **P0** | 设计里 `run(&mut self, inbox: &Inbox, …)` 取**不可变**引用，而 `push`/`take_*` 均需 `&mut self`——**编译不过**。若改 `&mut Inbox`，接线器就无法在 run 期间 push，**"中途插话"（steering）直接失效**，即本设计的核心需求被破坏 |
| **R2** | `Forwarder` overflow 无 flush 点 | 高 | 高 | **P0** | 见下「核实结果」。overflow 是设计的背压缓冲，但它随 `Forwarder` 在 `complete()` 返回后**即被 drop**，缓冲内事件永久丢失。违背 ADR-0004 点 5「终局事件不变式」 |
| **R3** | "队列 = 日志 + 游标" 未落实 | 中 | **确定** | **P1** | `Inbox.log` 与 `Session.messages` 各存一份对话；`take_*` 后又 `session.append(msg)` **复制一份**。声称的"一条日志 + 游标"实为**两个容器**，`/new` 时仍要处理"pending 要不要迁移"——正是移植该设计想消除的问题 |
| **R4** | `turn` 由 `UserMessage` 推导会被 steering 污染 | 中 | 高 | **P1** | 设计自相矛盾：既说"每回合恰好一个 `UserMessage` → 据此推 turn"，又说轮边界注入 steering 时 `emit(AgentEvent::UserMessage{…})`。**注入即误增 turn**，`--json` 的 `turn` 分组错乱 |
| **R5** | `QueueMode::All` 语义与"每消息一回合"矛盾 | 中 | 高 | **P1** | `All` 描述为"一次全灌（同来源合并处理）"，但 `run()` 的 `for message in batch { run_turn(message) }` 是**每条各自一回合**。需明确：合并成一条 user 消息，还是顺序跑 N 回合 |
| **R6** | 背压不可观测 | 中 | 中 | **P2** | 队列满 → agent 静默变慢，无计数、无日志。排障时无法区分"模型慢"与"消费者慢" |
| **R7** | `Envelope` 尺寸估算与所选表示不一致 | 低 | **确定** | **P2** | design §8 写 96 B，但那是在 `SourceId(u32)` 下算的；实际采纳 `Source(String)`（24 B + 每事件 clone）→ **约 120 B**，容量 1024 对应约 123 KiB 而非 96 KiB |
| **R8** | `LifecyclePolicy` 指定在两处 | 低 | 中 | **P2** | `ChannelSink` 持一份、`Agent::run(…, policy)` 又传一份，**优先级不明**（谁说了算？） |

### 核实结果（三处对照代码）

**① `Forwarder` 生命周期**（`basic.rs:129-140`）

```rust
let mut forwarder = Forwarder { sink: ctx.events };     // 129：创建
let response = ctx.model.complete(request, &mut forwarder).await  // 132：唯一使用点
    .map_err(|e| { … })?;                                // 136：此处已用回 ctx.events
```

`forwarder` 在 132 行后不再被使用，作用域结束时 drop。**若给它加 overflow 字段，缓冲区随之一并丢弃**——设计里"回合边界由 loop 用 `emit().await` 冲掉 overflow"这句话，**在 `basic.rs` 中找不到落点**。

**② `UserMessage` 的发出点**（`basic.rs:67-71`）

确实每回合恰好一次（Step 2），——**在无 steering 注入时成立**。设计新增的轮边界注入会再发一次，R4 由此而来。

**③ `Inbox` 签名**（design §4 vs §4 的 `run()`）

```rust
pub fn push(&mut self, …);              // 需 &mut
pub fn take_steering(&mut self) -> …;   // 需 &mut
pub async fn run(&mut self, inbox: &Inbox, …)   // ← 只有 &Inbox
```

**矛盾**。根因：骨架取自方案 A（`Inbox = Arc<Mutex<…>>`，`push(&self)`，内可变），而移植自方案 C 的 `Inbox` 是**普通结构体**（`push(&mut self)`）——**移植时未同步调整 `run()` 签名**。

## 改进建议

### 易修复（低风险快速改进）

1. **R7** 重算 `Envelope` 尺寸；或改用 `Arc<str>` 作为 `Source`（消掉每事件一次 `String` clone，且保持 `--json` 可读）。
2. **R8** 把 `LifecyclePolicy` **只留在一处**——建议留在 `Agent::run`（策略是"怎么跑"的事），`ChannelSink` 只报"消费者是否在场"（如 `Result<(), ConsumerGone>`），由 `run()` 依策略决定。
3. **R6** 在 `ChannelSink` 加两个 `AtomicU64`：`try_emit` 失败次数（背压发生）、`emit` 等待总时长。成本极低，排障价值高。
4. **R4** `turn` 不再从事件推导，改为显式：`Agent::run` 每回合开始调 `sink.begin_turn(n)`（或把 `turn` 放进 `RuntimeContext`，因 `run()` 已知回合号）。顺带消除 `--json` 的分组歧义。

### 需讨论（需要决策）

5. **R5** 明确 `QueueMode::All` 语义。建议：`All` = **合成一条** user 消息（多段文本拼接或保留为多条 ContentBlock）→ **一个回合**；`OneAtATime` = 一条一回合。pi 的 `"all"` 即前者。
6. **R2** overflow 的两个选项：
   - **(a) 保留 overflow，但把 flush 写进 `basic.rs`**：`complete()` 返回后立刻 `forwarder.flush().await` 把缓冲冲入 sink。改动小，但要求 `Forwarder` 显式实现 `flush()`，且**在错误路径（136 行 `?`）也要 flush**——容易被漏。
   - **(b) 取消 overflow**：让 `EventSink` 提供**同步**的 `try_emit`，满时**直接丢弃并计数**（对有损消费者），背压只由回合边界的 `emit().await` 承担。更简单，但"流式文本在消费者慢时丢字"。
   - **倾向 (a)**——它保住了 ADR-0004 点 5 的终局事件不变式；但必须在设计里把 flush 点写死，不能留给实现者。
7. **R3** 二选一：
   - **统一**：让 `Inbox` 与 `Session` **共用同一条日志**（`Inbox` 持有 `Arc<Session>` 或 session 暴露 `cursor`）。真正落实"队列 = 日志 + 游标"，`/new` = 换 session。
   - **承认两容器**：那就在文档里**撤回**"队列 = 日志 + 游标"的表述，诚实地说"队列是独立的待处理缓冲，消费时追加进 session"。
   - **倾向统一**，但注意这会牵动 ADR-0010 的所有权收敛（会话归接线器）——**该问题与 R1 是同一个问题的两面**。

### 架构级（影响面大）

8. **R1（P0）`Inbox` 的产消并发模型必须先定**。这是整个设计的承重墙——steering（中途插话）能否成立全系于此。三个选项：

   | 选项 | 做法 | 代价 |
   |---|---|---|
   | **A. 内可变**（回到骨架方案 A） | `Inbox = Arc<Mutex<Inner>>`；`push(&self)`；`take_*` 返回 **owned `Vec<Message>`** | 丢失零拷贝切片；`Mutex` 在 agent 轮边界加锁（低频，可接受） |
   | **B. 拆分句柄** | `InboxProducer`（接线器持）/ `InboxConsumer`（agent 持），共享 `Arc<Mutex<Inner>>` | 类型多一层，但职责清晰 |
   | **C. 入站也走 mpsc** | 接线器 `Sender`，agent `Receiver`（`try_recv` 批量 drain） | 复用出站同款机制，一致性最好；但 `Inbox` 就不再是"日志+游标"（回到 R3 的两容器） |

   **倾向 A**——它同时最贴合"队列是共享的待处理视图"这一语义，且 `Mutex` 的争用只在用户输入到达时（低频），不进入模型热路径。
   **注意**：无论选哪个，"队列 = 日志 + 游标"（R3）的落法都要随之重定，两项**必须一起决**。

9. **4.3 可观测性**：设计通篇无 metrics/tracing。对一个**内建背压**的系统，建议至少在 `ys-channel` 的契约层留出 `ChannelStats { queued, backpressure_events, dropped }` 的读法（实现可放接线器），否则线上问题只能靠猜。

## 总体评价

**整体健康度：良好，但有 2 个必须先解决的承重墙问题。**

**做对的地方**：模块边界与依赖方向干净（🟢）；三方案对比 + 未选理由记录完整，决策可追溯；back-of-envelope 扎实，"信道非瓶颈"的结论经复核成立；迁移路径分 6 步且各自可验证；移植 `try/await` 以保住 `ModelEventSink` 同步性的判断**成立且重要**（ADR-0004 点 2 的 ABI 面确实需要它）。

**最大风险**：**R1（`Inbox` 产消并发模型）与 R3（日志/游标落实）是同一个问题的两面**——都源于"移植方案 C 的 `Inbox` 结构时，只搬了数据结构、没搬它的产消模型"。设计在**核心需求（中途插话）的承载机制上不自洽**，这是 P0。

**次要但确定的问题**：R2（overflow 丢事件）、R4（turn 被 steering 污染）都是**设计内部矛盾**，不是外部约束导致的——说明这部分是"写下来时想得不够细"，而非"被什么限制了"。

**推荐下一步行动**：

1. **先决 R1 + R3**（合并为一个决策：`Inbox` 的形态与所有权）——用 `design-an-interface` 细化，产出接口后回来更新 design.md。
2. **顺手修 4 个易修复项**（R4/R6/R7/R8）——成本极低。
3. **R2 定选项 (a) 并把 flush 点写死**。
4. 以上完成后，设计才具备进入实现（迁移步 1）的条件。**当前不建议开工**。

**一句话**：骨架选对了，但**移植方案 C 的那两处动了承重结构，未回检承重**——修好 R1/R3 后这份设计是扎实的。
