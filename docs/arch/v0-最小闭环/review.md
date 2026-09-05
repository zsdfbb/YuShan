# v0 最小闭环 — 架构质量分析报告

## 分析范围

- 对象：v0 架构设计（`docs/arch/v0-最小闭环/design.md` + ADR-0001..0004 + `docs/design.md` 修订），无代码
- 维度：全量（可行性 / 可维护性 / 可理解性 / 性能与可靠性）
- 来源：设计文档与 ADR 的对抗性审查，逐条对照 REFERENCE 检查清单

## 各维度判断

| 维度 | 判断 | 关键发现 |
|------|------|----------|
| 1.1 技术可实现性 | 🟢 | 全部为成熟 Rust 模式（trait object + async-trait + serde）；无未验证假设。实现期摩擦点：多处匿名生命周期与 async-trait 交互，必要时显式化即可 |
| 1.2 依赖成熟度 | 🟢 | serde/serde_json/async-trait/thiserror 均为生产级；版本锁定待 Cargo.toml 落地时补 |
| 1.3 实现周期 | 🟢 | 增量路径清晰（§12 测试矩阵即里程碑）；单人无外部依赖方，无时间承诺风险 |
| 2.1 模块边界 | 🟢 | 单一职责清晰；依赖单向由 crate 边界编译期强制；RuntimeContext 归位 agent-component 已解开 loop→runtime 环 |
| 2.2 接口稳定性 | 🟡 | trait 承诺稳定，但两处演进策略缺失：① `ToolResult` 等由第三方构造的类型**无法 #[non_exhaustive]**，字段演进只能是 0.x 破坏性变更（已接受，应写明）；② v0 缺 system prompt 注入点，v1 落地即需动 `AgentInput`/`Builder` 签名（见需讨论项） |
| 2.3 错误处理 | 🟡 | 双通道语义清晰；但发现 **P1 规格漏洞**：多工具轮次的部分失败配对未定义（已修复）；`Session.append` 失败后历史可能停在无配对 tool_use 状态，恢复策略未定义（v1 JSONL 时处理） |
| 2.4 并发安全 | 🟢 | 唯一共享状态是 `CancelToken`（原子布尔）；组件经独占借用进入 run，无锁无竞态；「单 Agent 同时一个 run」约束需文档化（已补） |
| 2.5 资源管理 | 🟢 | 无池化/FD；每轮 O(n) 历史克隆已量化（~2MB/千条消息）并设了优化触发条件；同步 emit 天然背压——但「慢 sink 阻塞执行器线程」的推论需写明（已补） |
| 3.1 概念一致性 | 🟢 | CONTEXT.md 术语与接口一一对应（Turn/rounds、StopReason 三值、错误双通道）；发现根 design.md §5 一处过时表述（已修复） |
| 3.2 上手成本 | 🟢 | 全部决策有 ADR 背书，无「秘密知识」；8-crate 仪式感是已知代价，prelude 缓解 |
| 4.1 性能模型 | 🟢 | back-of-envelope 已做：事件路径 ~0–50ns ≪ 模型 RTT（≥100ms），事件微优化无意义；O(n) 克隆有触发条件 |
| 4.2 故障模式 | 🟡 | 终局事件不变式消除了「流停了不知道为什么」；多工具部分失败（P1，已修复）与 session 部分写失败（P2，顺延 v1）是仅有的两个未闭合故障路径 |
| 4.3 可观测性 | 🟢 | 事件流即观测通道（设计初衷）；tracing/metrics 留 v2 Hook 阶段，方向正确 |

## 风险排序

| 风险 | 影响 | 可能性 | 优先级 | 状态 |
|------|------|--------|--------|------|
| 多工具轮次部分失败：ADR-0004 第 6 条只写了「补写失败工具的结果」，未覆盖同轮**已执行**和**未执行**的兄弟 tool_use——按原文实现会产出无配对历史，v1 真实 API 拒收续跑会话，且按原文写的测试也测不出来 | 高 | 中 | **P1** | ✅ 已修复：规则改为「每个 tool_use 都得到结果」（v0 design.md 步骤 7、ADR-0001 后果） |
| `#[non_exhaustive]` 结构体（RuntimeContext/ToolContext）不可跨 crate 字面构造，agent-loop/runtime 首次实现即编译失败 | 高 | 高（必现） | P2 | ✅ 已修复：实现注记要求提供 `new(...)` 伴生构造器（编译器也会兜底） |
| v0 无 system prompt 注入点：v1 真实适配器一落地就要改 `AgentInput`/`Builder`/`ModelRequest` | 中 | 高（必然） | P2 | ⏳ 需讨论（见下） |
| `EventError` 的归属 crate 未写明（应为 agent-core，供两口共用）；`ModelError` 缺 `Sink(EventError)` 变体 | 中 | 高 | P2 | ✅ 已修复：实现注记 |
| `Session.append` 失败（磁盘满等）后历史停在无配对 tool_use 状态，无恢复策略 | 中 | 低 | P2 | ⏳ 顺延 v1（JSONL 落地时定义启动修复或错误指引） |
| 根 design.md §5「终止于错误时为空」与 Err 通道矛盾（错误根本不产生 RunResult） | 低 | 已发生 | P2 | ✅ 已修复 |
| 文档级小项：max_rounds 语义、单 run 约束、慢 sink 阻塞执行线程、增量拼接断言进测试矩阵 | 低 | - | P3 | ✅ 已修复（实现注记 + §12） |

## 改进建议

### 易修复（本次已直接落入文档）

- P1 配对规则补全 → v0 design.md 步骤 7 + ADR-0001 后果
- 实现注记块（EventError 归属、构造器、ModelError::Sink、max_rounds 语义、单 run、慢 sink 线程阻塞、拼接断言）→ v0 design.md
- 根 design.md §5 过时句修正、§12 测试矩阵补一项

### 需讨论（需要决策后再动）

- **system prompt 注入点**：建议 v0 即在 `ModelRequest` 加 `system: Option<String>`、`AgentBuilder` 加 `.system()`——不违反任何锁定决策（prompts 归产品层指的是 Coding Prompt/Hook 机制，一个中性字符串字段不冲突），避免 v1 破坏性修改。反方理由：v0 Mock Model 用不上，YAGNI。倾向：加。
- **边界↔Hook 映射表**：v0 的离散步骤边界与 design.md §6 的 8 个 Hook 点目前只有部分对齐（如 `before_session_append` 无明确边界），建议 v2 Hook 设计时补全映射，v0 不动。

### 架构级

- 无。对抗性审查未发现结构缺陷：8-crate 分层、双事件口、同步推式事件、协作式取消经全部检查项检验成立。8-crate 对 ~2k 行的「仪式感偏重」是已知且已接受（ADR-0004）的代价，不构成风险。

## 总体评价

设计整体健康：11 个维度 8 绿 3 黄，无红。唯一 P1 是规格层面的一处下钻不足（多工具部分失败配对），已随审查修复；其余黄项均为文档补全或顺延到对应阶段的已知项。三个值得肯定的强点：错误双通道 + 终局事件不变式让故障路径完全可枚举；依赖单向由 crate 边界机械保证而非约定；全部决策有 ADR 可溯。**建议**：把「需讨论」的 system prompt 注入点定下来（10 分钟决策），然后直接落地 workspace 骨架——设计与实现之间已无阻塞项。

后续：本次审查产生的文档修订随下次提交入库。
